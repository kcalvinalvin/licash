//! Silent payments to a wallet, found with a hub's tweaks and a light
//! client's compact filters on an in-process chain of real payments: found,
//! spent with change found again, through reorganizations, and refused from
//! a hub that names another block.

#[allow(dead_code)]
mod net;

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};

use bitcoin::{
    absolute::LockTime,
    bip158::{self, BlockFilter},
    block::{Header, Version},
    hashes::{sha256d, Hash},
    key::{Secp256k1, TweakedPublicKey},
    pow::CompactTarget,
    secp256k1::{Message, SecretKey, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache},
    transaction, Amount, Block, BlockHash, CompressedPublicKey, FeeRate, Network, OutPoint,
    ScriptBuf, Sequence, TapSighashType, Transaction, TxIn, TxMerkleNode, TxOut, Txid, Witness,
};
use licash::spend::ChainProfile;
use licash_api::{BlockTweaks, Fee, Hex, Info};
use licash_onchain::silent::{self, SpAddress};
use licash_wallet::{Blocks, Error, PaymentCode, Response, SilentSpend, Transport, Wallet};
use net::{Link, Net, HUB};

const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

/// A best chain from genesis, the outputs of every transaction ever made
/// (fictional coins of payers among them), and what the wallet read.
struct Chain {
    blocks: Vec<Block>,
    outputs: HashMap<OutPoint, TxOut>,
    /// Serve compact filters.
    filters: bool,
    /// The blocks the wallet fetched whole.
    fetched: Vec<BlockHash>,
    /// The filters it read.
    filtered: u32,
    /// Serve blocks without their last transaction.
    tampered: bool,
    /// Makes each coinbase and payer coin unique.
    made: u32,
}

type Shared = Arc<Mutex<Chain>>;

impl Chain {
    fn new() -> Shared {
        Arc::new(Mutex::new(Chain {
            blocks: vec![bitcoin::constants::genesis_block(Network::Regtest)],
            outputs: HashMap::new(),
            filters: true,
            fetched: Vec::new(),
            filtered: 0,
            tampered: false,
            made: 0,
        }))
    }

    fn height(&self) -> u32 {
        self.blocks.len() as u32 - 1
    }

    /// Mine `txs` in a block on the tip.
    fn mine(&mut self, txs: Vec<Transaction>) -> BlockHash {
        self.made += 1;
        let coinbase = Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(self.made.to_be_bytes().to_vec()),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new_op_return([1]),
            }],
        };
        let mut block = Block {
            header: Header {
                version: Version::from_consensus(0x2000_0000),
                prev_blockhash: self.blocks.last().unwrap().block_hash(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 1_700_000_000 + self.made,
                bits: CompactTarget::from_consensus(0x207f_ffff),
                nonce: 0,
            },
            txdata: [vec![coinbase], txs].concat(),
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        for tx in &block.txdata {
            let txid = tx.compute_txid();
            for (vout, output) in tx.output.iter().enumerate() {
                self.outputs
                    .insert(OutPoint::new(txid, vout as u32), output.clone());
            }
        }
        let hash = block.block_hash();
        self.blocks.push(block);
        hash
    }

    /// Mine `count` empty blocks.
    fn mine_empty(&mut self, count: u32) {
        for _ in 0..count {
            self.mine(Vec::new());
        }
    }

    /// Drop the blocks from height `at` on.
    fn fork(&mut self, at: u32) {
        self.blocks.truncate(at as usize);
    }

    /// A payer's transaction from a fresh coin of its own, of key-hash
    /// inputs, paying `values` to `to` in order.
    fn pay(&mut self, to: &[SpAddress], values: &[u64]) -> Transaction {
        self.made += 1;
        let secp = Secp256k1::new();
        let mut secret = [0x11; 32];
        secret[..4].copy_from_slice(&self.made.to_be_bytes());
        let secret = SecretKey::from_slice(&secret).unwrap();
        let key = CompressedPublicKey(secret.public_key(&secp));
        let coin = OutPoint::new(
            Txid::from_raw_hash(sha256d::Hash::hash(&self.made.to_be_bytes())),
            0,
        );
        self.outputs.insert(
            coin,
            TxOut {
                value: Amount::from_sat(values.iter().sum::<u64>() + 1_000),
                script_pubkey: ScriptBuf::new_p2wpkh(&key.wpubkey_hash()),
            },
        );
        let keys = silent::outputs(&[(secret, false)], &[coin], to).unwrap();
        let mut witness = Witness::new();
        witness.push([0x30; 71]);
        witness.push(key.to_bytes());
        Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: coin,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: keys
                .iter()
                .zip(values)
                .map(|(key, value)| TxOut {
                    value: Amount::from_sat(*value),
                    script_pubkey: ScriptBuf::new_p2tr_tweaked(
                        TweakedPublicKey::dangerous_assume_tweaked(*key),
                    ),
                })
                .collect(),
        }
    }

    /// A payer's transaction from a fresh coin of its own, of a script-hash
    /// input, which silent payments skip (no tweak), paying `value` to `to`.
    fn pay_plain(&mut self, to: &ScriptBuf, value: u64) -> Transaction {
        self.made += 1;
        let script = ScriptBuf::from_bytes(vec![0x51]);
        let coin = OutPoint::new(
            Txid::from_raw_hash(sha256d::Hash::hash(&self.made.to_be_bytes())),
            0,
        );
        self.outputs.insert(
            coin,
            TxOut {
                value: Amount::from_sat(value + 1_000),
                script_pubkey: ScriptBuf::new_p2wsh(&script.wscript_hash()),
            },
        );
        let mut witness = Witness::new();
        witness.push(script.as_bytes());
        Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: coin,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value),
                script_pubkey: to.clone(),
            }],
        }
    }

    /// What an index server serves of the block at `height`.
    fn tweaks(&self, height: u32) -> BlockTweaks {
        let block = &self.blocks[height as usize];
        let tweaks = block.txdata[1..].iter().filter_map(|tx| {
            let prevouts: Option<Vec<&bitcoin::Script>> = tx
                .input
                .iter()
                .map(|i| {
                    self.outputs
                        .get(&i.previous_output)
                        .map(|o| o.script_pubkey.as_script())
                })
                .collect();
            silent::tweak(tx, &prevouts?)
        });
        BlockTweaks {
            height,
            hash: block.block_hash().to_string(),
            tweaks: tweaks.map(|t| Hex(t.serialize().to_vec())).collect(),
        }
    }
}

/// A light client's view of a [`Chain`].
struct Light(Shared);

impl Blocks for Light {
    fn height(&self) -> impl Future<Output = Result<u32, String>> + Send {
        std::future::ready(Ok(self.0.lock().unwrap().height()))
    }

    fn hash(&self, height: u32) -> impl Future<Output = Result<BlockHash, String>> + Send {
        let chain = self.0.lock().unwrap();
        let hash = chain.blocks.get(height as usize).map(Block::block_hash);
        std::future::ready(hash.ok_or_else(|| format!("no block {height}")))
    }

    fn filter(
        &self,
        hash: &BlockHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send {
        let mut chain = self.0.lock().unwrap();
        chain.filtered += 1;
        let chain = &*chain;
        let block = chain.blocks.iter().find(|b| b.block_hash() == *hash);
        let filter = match (chain.filters, block) {
            (false, _) => Ok(None),
            (true, None) => Err(format!("no block {hash}")),
            (true, Some(block)) => BlockFilter::new_script_filter(block, |coin| {
                let output = chain.outputs.get(coin);
                let script = output.map(|o| o.script_pubkey.clone());
                script.ok_or(bip158::Error::UtxoMissing(*coin))
            })
            .map(|f| Some(f.content))
            .map_err(|e| e.to_string()),
        };
        std::future::ready(filter)
    }

    fn block(&self, hash: &BlockHash) -> impl Future<Output = Result<Block, String>> + Send {
        let mut chain = self.0.lock().unwrap();
        chain.fetched.push(*hash);
        let mut block = chain
            .blocks
            .iter()
            .find(|b| b.block_hash() == *hash)
            .cloned();
        if let Some(block) = block.as_mut().filter(|_| chain.tampered) {
            block.txdata.pop();
        }
        std::future::ready(block.ok_or_else(|| format!("no block {hash}")))
    }
}

/// A hub of the [`Net`] that indexes the tweaks of a [`Chain`].
struct Indexer {
    net: Arc<Net>,
    chain: Shared,
    /// The first block whose tweaks it serves.
    first: Mutex<Option<u32>>,
    /// The height at which it names another block than the chain's.
    forged: Mutex<Option<u32>>,
    /// How often it was asked for tweaks.
    asked: Mutex<u32>,
}

impl Indexer {
    fn serve(&self, url: &str, body: Option<Vec<u8>>) -> Result<Response, String> {
        let tweaks = format!("{HUB}{}?", licash_api::TWEAKS);
        if let Some(query) = url.strip_prefix(&tweaks) {
            let number = |name: &str| -> u32 {
                let pair = query.split('&').find_map(|p| p.strip_prefix(name)).unwrap();
                pair.trim_start_matches('=').parse().unwrap()
            };
            let (from, count) = (number("from"), number("count"));
            *self.asked.lock().unwrap() += 1;
            assert!(count <= licash_api::MAX_TWEAK_BLOCKS);
            let chain = self.chain.lock().unwrap();
            let mut blocks: Vec<BlockTweaks> = (from..from + count)
                .take_while(|h| *h <= chain.height())
                .map(|h| chain.tweaks(h))
                .collect();
            let forged = *self.forged.lock().unwrap();
            for block in blocks.iter_mut().filter(|b| Some(b.height) == forged) {
                block.hash = BlockHash::all_zeros().to_string();
            }
            return Ok(Response {
                status: 200,
                body: serde_json::to_vec(&blocks).unwrap(),
            });
        }
        let mut response = self.net.serve(url, body)?;
        if url.ends_with(licash_api::INFO) && response.status == 200 {
            let mut info: Info = serde_json::from_slice(&response.body).unwrap();
            info.tweaks = *self.first.lock().unwrap();
            response.body = serde_json::to_vec(&info).unwrap();
        }
        Ok(response)
    }
}

struct Via(Arc<Indexer>);

impl Transport for Via {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl Future<Output = Result<Response, String>> + Send {
        std::future::ready(self.0.serve(url, body))
    }
}

struct Setup {
    wallet: Wallet<Via>,
    chain: Shared,
    hub: Arc<Indexer>,
    dir: tempfile::TempDir,
}

/// A wallet trusting a hub that serves the tweaks of a chain of two empty
/// blocks, from the first.
async fn setup() -> Setup {
    let net = Net::new();
    net.start(HUB, Fee { base: 0, ppm: 0 });
    let chain = Chain::new();
    chain.lock().unwrap().mine_empty(2);
    let hub = Arc::new(Indexer {
        net,
        chain: chain.clone(),
        first: Mutex::new(Some(1)),
        forged: Mutex::new(None),
        asked: Mutex::new(0),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.sqlite");
    let wallet = Wallet::create(&path, &[7; 32], REGTEST, Via(hub.clone())).unwrap();
    wallet.add_hub(HUB).await.unwrap();
    Setup {
        wallet,
        chain,
        hub,
        dir,
    }
}

impl Setup {
    fn address(&self) -> SpAddress {
        self.wallet.silent_keys().address()
    }

    async fn scan(&self) -> Result<licash_wallet::Scanned, Error> {
        self.wallet
            .scan_silent(HUB, &Light(self.chain.clone()), u32::MAX)
            .await
    }

    fn mine(&self, txs: Vec<Transaction>) -> BlockHash {
        self.chain.lock().unwrap().mine(txs)
    }

    fn pay(&self, to: &[SpAddress], values: &[u64]) -> Transaction {
        self.chain.lock().unwrap().pay(to, values)
    }

    /// The fetched blocks so far, forgotten.
    fn fetched(&self) -> Vec<BlockHash> {
        std::mem::take(&mut self.chain.lock().unwrap().fetched)
    }

    fn pay_plain(&self, to: &ScriptBuf, value: u64) -> Transaction {
        self.chain.lock().unwrap().pay_plain(to, value)
    }

    /// The fallback address a payment code hands out now, as a script.
    fn fallback(&self) -> ScriptBuf {
        let code = self.wallet.payment_code(&[HUB.into()], true).unwrap();
        code.address.unwrap().assume_checked().script_pubkey()
    }
}

/// The script of `wallet`'s fallback address `index`.
fn fallback_of<T: Transport>(wallet: &Wallet<T>, index: u32) -> ScriptBuf {
    let found = wallet.silent_keys().scanner([]).fallback(index).unwrap();
    ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(found.output))
}

/// Someone else's silent payment address.
fn stranger() -> SpAddress {
    silent::Keys::from_seed(&[9; 32], bitcoin::NetworkKind::Test)
        .unwrap()
        .address()
}

/// A key-hash script outside the wallet.
fn elsewhere() -> ScriptBuf {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[5; 32]).unwrap().public_key(&secp);
    ScriptBuf::new_p2wpkh(&CompressedPublicKey(key).wpubkey_hash())
}

/// Every input of `tx` is signed for the coin it spends, as `chain` holds.
fn assert_signed(chain: &Chain, tx: &Transaction) {
    let secp = Secp256k1::verification_only();
    let prevouts: Vec<TxOut> = tx
        .input
        .iter()
        .map(|i| chain.outputs[&i.previous_output].clone())
        .collect();
    let mut cache = SighashCache::new(tx);
    for (i, input) in tx.input.iter().enumerate() {
        let sighash = cache
            .taproot_key_spend_signature_hash(i, &Prevouts::All(&prevouts), TapSighashType::Default)
            .unwrap();
        let signature = bitcoin::taproot::Signature::from_slice(&input.witness[0]).unwrap();
        let key = XOnlyPublicKey::from_slice(&prevouts[i].script_pubkey.as_bytes()[2..]).unwrap();
        let message = Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&signature.signature, &message, &key)
            .unwrap();
        assert_eq!(input.witness.len(), 1);
    }
}

fn rate() -> FeeRate {
    FeeRate::from_sat_per_vb(2).unwrap()
}

#[tokio::test]
async fn scanning_needs_a_first_block_and_a_hub_serving_tweaks() {
    let s = setup().await;
    assert_eq!(s.wallet.silent_next().unwrap(), None);
    match s.scan().await {
        Err(Error::Invalid(why)) => assert!(why.contains("watch_silent"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.chain.lock().unwrap().filtered, 0);
    s.wallet.watch_silent(HUB, 2).unwrap();
    // Never later than it stands.
    s.wallet.watch_silent(HUB, 5).unwrap();
    assert_eq!(s.wallet.silent_next().unwrap(), Some(2));
    for (first, says) in [(Some(3), "from block 3 on"), (None, "serves no")] {
        *s.hub.first.lock().unwrap() = first;
        s.wallet.add_hub(HUB).await.unwrap();
        match s.scan().await {
            Err(Error::Invalid(why)) => assert!(why.contains(says), "{why}"),
            other => panic!("{other:?}"),
        }
        // Nor can it look earlier than the hub serves.
        match s.wallet.watch_silent(HUB, 1) {
            Err(Error::Invalid(why)) => assert!(why.contains(says), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.wallet.silent_next().unwrap(), Some(2));
    }
    *s.hub.first.lock().unwrap() = Some(1);
    s.wallet.add_hub(HUB).await.unwrap();
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (3, 2));
    assert!(scanned.found.is_empty());
    // At most `max` blocks at a time.
    s.chain.lock().unwrap().mine_empty(5);
    let light = Light(s.chain.clone());
    let scanned = s.wallet.scan_silent(HUB, &light, 2).await.unwrap();
    assert_eq!((scanned.next, scanned.height), (5, 7));
    assert_eq!(s.scan().await.unwrap().next, 8);
}

#[tokio::test]
async fn payments_are_found_and_spent_with_change_found_again() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let me = s.address();
    let paying = s.pay(&[me, me], &[30_000, 20_000]);
    let other = s.pay(&[stranger()], &[10_000]);
    s.mine(vec![other]);
    let paid = s.mine(vec![paying.clone()]);
    s.chain.lock().unwrap().mine_empty(1);
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (6, 5));
    let mut values: Vec<u64> = scanned.found.iter().map(|c| c.value).collect();
    values.sort();
    assert_eq!(values, [20_000, 30_000]);
    // The filters spared the block paying someone else, and the empty
    // blocks before any coin were not even filtered.
    assert_eq!(s.fetched(), [paid]);
    assert_eq!(scanned.fetched, 1);
    assert_eq!(s.chain.lock().unwrap().filtered, 3);
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 2);
    assert!(coins
        .iter()
        .all(|c| c.height == 4 && !c.change && c.spending.is_none()));
    assert!(coins
        .iter()
        .all(|c| c.outpoint.txid == paying.compute_txid()));
    // Looked in already: nothing again.
    assert!(s.scan().await.unwrap().found.is_empty());

    let spend = s
        .wallet
        .spend_silent(elsewhere(), Some(35_000), rate())
        .unwrap();
    assert_signed(&s.chain.lock().unwrap(), &spend);
    assert_eq!(spend.input.len(), 2);
    assert_eq!(spend.output.len(), 2);
    assert!(spend.input.iter().all(|i| i.sequence.is_rbf()));
    let out: u64 = spend.output.iter().map(|o| o.value.to_sat()).sum();
    let fee = 50_000 - out;
    assert_eq!(fee, rate().fee_wu(spend.weight()).unwrap().to_sat());
    let pays = |tx: &Transaction, script: &ScriptBuf| {
        tx.output
            .iter()
            .filter(|o| o.script_pubkey == *script)
            .map(|o| o.value.to_sat())
            .sum::<u64>()
    };
    assert_eq!(pays(&spend, &elsewhere()), 35_000);
    assert_eq!(
        s.wallet.silent_spends().unwrap(),
        std::slice::from_ref(&spend)
    );
    let txid = spend.compute_txid();
    assert!(s
        .wallet
        .silent_coins()
        .unwrap()
        .iter()
        .all(|c| c.spending == Some(txid)));
    match s.wallet.spend_silent(elsewhere(), Some(1_000), rate()) {
        Err(Error::Insufficient { available: 0, .. }) => {}
        other => panic!("{other:?}"),
    }

    s.mine(vec![spend.clone()]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.spent.len(), 2);
    assert_eq!(scanned.found.len(), 1);
    let change = &scanned.found[0];
    assert!(change.change);
    assert_eq!(change.value, 50_000 - 35_000 - fee);
    assert_eq!(change.outpoint.txid, txid);
    assert!(s.wallet.silent_spends().unwrap().is_empty());
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 1);
    assert_eq!(coins[0].outpoint, change.outpoint);

    // Everything: no change.
    let all = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    assert_signed(&s.chain.lock().unwrap(), &all);
    assert_eq!(all.output.len(), 1);
    let fee = rate().fee_wu(all.weight()).unwrap().to_sat();
    assert_eq!(pays(&all, &elsewhere()), change.value - fee);
}

#[tokio::test]
async fn a_spend_shows_its_fee_and_its_block_as_far_as_the_wallet_knows() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address(), s.address()], &[30_000, 20_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    let paying = |script: &ScriptBuf| s.wallet.silent_spends_paying(script).unwrap();
    assert!(paying(&elsewhere()).is_empty());
    let spend = s
        .wallet
        .spend_silent(elsewhere(), Some(35_000), rate())
        .unwrap();
    let out: u64 = spend.output.iter().map(|o| o.value.to_sat()).sum();
    let sent = SilentSpend {
        tx: spend.clone(),
        fee: Some(50_000 - out),
        height: None,
    };
    assert_eq!(paying(&elsewhere()), std::slice::from_ref(&sent));
    assert!(paying(&ScriptBuf::new_op_return([1])).is_empty());

    // In block 4, once a scan saw it there.
    s.mine(vec![spend]);
    assert_eq!(paying(&elsewhere()), std::slice::from_ref(&sent));
    s.scan().await.unwrap();
    let confirmed = SilentSpend {
        height: Some(4),
        ..sent.clone()
    };
    assert_eq!(paying(&elsewhere()), [confirmed]);

    // Its block replaced: in none again.
    s.chain.lock().unwrap().fork(4);
    s.chain.lock().unwrap().mine_empty(2);
    s.scan().await.unwrap();
    assert_eq!(paying(&elsewhere()), std::slice::from_ref(&sent));

    // Its coins' block replaced too: what they held is unknown.
    s.chain.lock().unwrap().fork(3);
    s.chain.lock().unwrap().mine_empty(4);
    s.scan().await.unwrap();
    assert!(s.wallet.silent_coins().unwrap().is_empty());
    let unknown = SilentSpend { fee: None, ..sent };
    assert_eq!(paying(&elsewhere()), [unknown]);
}

#[tokio::test]
async fn amounts_too_small_or_too_large_are_refused() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    match s.wallet.spend_silent(elsewhere(), None, rate()) {
        Err(Error::Insufficient { available: 0, .. }) => {}
        other => panic!("{other:?}"),
    }
    let paying = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    match s.wallet.spend_silent(elsewhere(), Some(100), rate()) {
        Err(Error::Invalid(why)) => assert!(why.contains("dust"), "{why}"),
        other => panic!("{other:?}"),
    }
    match s.wallet.spend_silent(elsewhere(), Some(10_000), rate()) {
        Err(Error::Insufficient {
            available: 10_000, ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // Change too small to keep goes to the fee.
    let spend = s
        .wallet
        .spend_silent(elsewhere(), Some(9_500), rate())
        .unwrap();
    assert_eq!(spend.output.len(), 1);
    assert_eq!(spend.output[0].value.to_sat(), 9_500);
    assert!(s.wallet.silent_spends().unwrap().len() == 1);
}

#[tokio::test]
async fn an_abandoned_spend_frees_its_coins() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    let spend = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    let txid = spend.compute_txid();
    s.wallet.abandon_silent(&txid).unwrap();
    assert!(s.wallet.silent_spends().unwrap().is_empty());
    assert_eq!(s.wallet.silent_coins().unwrap()[0].spending, None);
    assert!(matches!(
        s.wallet.abandon_silent(&txid),
        Err(Error::UnknownPayment(_))
    ));
    // Should it reach a block after all, a scan sees its coin spent.
    let again = s
        .wallet
        .spend_silent(elsewhere(), Some(5_000), rate())
        .unwrap();
    s.mine(vec![spend]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.spent.len(), 1);
    assert!(s.wallet.silent_coins().unwrap().is_empty());
    // And the spend it replaced is no longer offered for broadcast.
    assert!(!s.wallet.silent_spends().unwrap().contains(&again));
    // That spend, not the wallet's own any more, replaced: the coin is the
    // wallet's again.
    s.chain.lock().unwrap().fork(4);
    s.chain.lock().unwrap().mine_empty(2);
    s.scan().await.unwrap();
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 1);
    assert_eq!(coins[0].spending, None);
}

/// A transaction spending another party's coin, then the wallet's coin
/// `coin`, paying `value` elsewhere, and the coins it spends: a contract's
/// funding, as far as the wallet sees it.
fn shared(chain: &Chain, coin: OutPoint, value: u64) -> (Transaction, Vec<TxOut>) {
    let theirs = OutPoint::new(Txid::from_byte_array([3; 32]), 0);
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: [theirs, coin]
            .map(|previous_output| TxIn {
                previous_output,
                ..Default::default()
            })
            .to_vec(),
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: elsewhere(),
        }],
    };
    let prevouts = vec![
        TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: elsewhere(),
        },
        chain.outputs[&coin].clone(),
    ];
    (tx, prevouts)
}

#[tokio::test]
async fn a_coin_signed_into_anothers_transaction_is_reserved_for_it() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    let coin = s.wallet.silent_coins().unwrap()[0].outpoint;
    let (tx, prevouts) = shared(&s.chain.lock().unwrap(), coin, 55_000);
    let txid = tx.compute_txid();

    // Its input signed as the coin it spends, committing to every coin:
    // the other party's left as it was.
    let signed = s.wallet.sign_input(&tx, &prevouts, 1).unwrap();
    assert_eq!(signed.compute_txid(), txid);
    assert!(signed.input[0].witness.is_empty());
    let mut cache = SighashCache::new(&signed);
    let sighash = cache
        .taproot_key_spend_signature_hash(1, &Prevouts::All(&prevouts), TapSighashType::Default)
        .unwrap();
    let signature = bitcoin::taproot::Signature::from_slice(&signed.input[1].witness[0]).unwrap();
    let key = XOnlyPublicKey::from_slice(&prevouts[1].script_pubkey.as_bytes()[2..]).unwrap();
    let message = Message::from_digest(sighash.to_byte_array());
    Secp256k1::verification_only()
        .verify_schnorr(&signature.signature, &message, &key)
        .unwrap();

    // Reserved for it, nothing to broadcast: no spend takes the coin.
    assert_eq!(s.wallet.silent_coins().unwrap()[0].spending, Some(txid));
    assert!(s.wallet.silent_spends().unwrap().is_empty());
    let spend = s.wallet.spend_silent(elsewhere(), None, rate());
    assert!(
        matches!(spend, Err(Error::Insufficient { .. })),
        "{spend:?}"
    );
    // Signed again for it alone.
    s.wallet.sign_input(&tx, &prevouts, 1).unwrap();
    let (other, other_prevouts) = shared(&s.chain.lock().unwrap(), coin, 54_000);
    let again = s.wallet.sign_input(&other, &other_prevouts, 1);
    assert!(matches!(again, Err(Error::NoteTaken)), "{again:?}");

    // Never the other party's coin, nor the wallet's for another value.
    let theirs = s.wallet.sign_input(&tx, &prevouts, 0);
    assert!(matches!(theirs, Err(Error::Invalid(_))), "{theirs:?}");
    let mut lied = prevouts.clone();
    lied[1].value = Amount::from_sat(20_000);
    let lied = s.wallet.sign_input(&tx, &lied, 1);
    assert!(matches!(lied, Err(Error::Invalid(_))), "{lied:?}");
    let short = s.wallet.sign_input(&tx, &prevouts[1..], 1);
    assert!(matches!(short, Err(Error::Invalid(_))), "{short:?}");

    // Given up, the coin is free again.
    s.wallet.abandon_silent(&txid).unwrap();
    assert_eq!(s.wallet.silent_coins().unwrap()[0].spending, None);
    s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
}

#[tokio::test]
async fn the_block_a_coin_signed_into_anothers_transaction_is_spent_in_is_known() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    let coin = s.wallet.silent_coins().unwrap()[0].outpoint;
    let (tx, mut prevouts) = shared(&s.chain.lock().unwrap(), coin, 55_000);
    // The other party's coin, on the chain: of a script-hash, no tweak.
    let theirs = tx.input[0].previous_output;
    let script = ScriptBuf::from_bytes(vec![0x51]);
    prevouts[0].script_pubkey = ScriptBuf::new_p2wsh(&script.wscript_hash());
    let output = prevouts[0].clone();
    s.chain.lock().unwrap().outputs.insert(theirs, output);
    let signed = s.wallet.sign_input(&tx, &prevouts, 1).unwrap();

    // Signed, in no block yet; nor is a coin not the wallet's.
    assert_eq!(s.wallet.silent_spent_at(&coin).unwrap(), None);
    assert_eq!(s.wallet.silent_spent_at(&theirs).unwrap(), None);

    // In block 4, however deep.
    s.mine(vec![signed]);
    s.mine(Vec::new());
    s.scan().await.unwrap();
    assert_eq!(s.wallet.silent_spent_at(&coin).unwrap(), Some(4));

    // That block replaced, without it: in none again.
    s.chain.lock().unwrap().fork(4);
    s.chain.lock().unwrap().mine_empty(3);
    s.scan().await.unwrap();
    assert_eq!(s.wallet.silent_spent_at(&coin).unwrap(), None);
}

#[test]
fn a_wallets_contract_secrets_are_its_own_and_its_spending_roots() {
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str, seed: [u8; 32]| {
        let path = dir.path().join(name);
        Wallet::create(&path, &seed, REGTEST, Link(Net::new())).unwrap()
    };
    let a = open("a.sqlite", [7; 32]).contract_secrets();
    let again = open("again.sqlite", [7; 32]).contract_secrets();
    let b = open("b.sqlite", [8; 32]).contract_secrets();
    assert_eq!((a.seed, a.spend), (again.seed, again.spend));
    assert_ne!(a.seed, b.seed);
    assert_ne!(a.spend, b.spend);
    // Of a domain of its own: neither the seed nor the spending root's.
    assert_ne!(a.seed, [7; 32]);
    let owner = licash::address::OwnerKeys::from_seed(&[7; 32]).unwrap();
    assert_eq!(a.spend, *owner.spend_secret());
    assert_eq!(format!("{a:?}"), "ContractSecrets(..)");
}

#[tokio::test]
async fn a_spend_a_block_conflicts_with_is_given_up() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address(), s.address()], &[20_000, 10_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();
    // The larger coin alone, given up; then both.
    let first = s
        .wallet
        .spend_silent(elsewhere(), Some(5_000), rate())
        .unwrap();
    assert_eq!(first.input.len(), 1);
    s.wallet.abandon_silent(&first.compute_txid()).unwrap();
    let second = s
        .wallet
        .spend_silent(elsewhere(), Some(25_000), rate())
        .unwrap();
    assert_eq!(second.input.len(), 2);
    // The first confirms after all: the second never can.
    s.mine(vec![first.clone()]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.spent, [first.input[0].previous_output]);
    assert!(s.wallet.silent_spends().unwrap().is_empty());
    // The smaller coin is free, beside the first's change.
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 2);
    assert!(coins.iter().all(|c| c.spending.is_none()));
    assert!(coins.iter().any(|c| c.value == 10_000 && !c.change));
    assert!(coins.iter().any(|c| c.change));
    // Its block replaced: both coins are free.
    s.chain.lock().unwrap().fork(4);
    s.chain.lock().unwrap().mine_empty(2);
    s.scan().await.unwrap();
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 2);
    assert!(coins.iter().all(|c| c.spending.is_none() && !c.change));
}

#[tokio::test]
async fn a_block_unlike_its_header_is_refused() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[40_000]);
    s.mine(vec![paying]);
    s.chain.lock().unwrap().tampered = true;
    match s.scan().await {
        Err(Error::Chain(why)) => assert!(why.contains("header"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.wallet.silent_next().unwrap(), Some(1));
    s.chain.lock().unwrap().tampered = false;
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
}

#[tokio::test]
async fn without_filters_the_blocks_that_could_pay_or_spend_are_fetched() {
    let s = setup().await;
    s.chain.lock().unwrap().filters = false;
    s.wallet.watch_silent(HUB, 3).unwrap();
    let other = s.pay(&[stranger()], &[10_000]);
    let theirs = s.mine(vec![other]);
    s.mine(Vec::new());
    let scanned = s.scan().await.unwrap();
    assert!(scanned.found.is_empty());
    assert_eq!(s.fetched(), [theirs]);
    // Once it holds a coin, any block may spend it.
    let paying = s.pay(&[s.address()], &[10_000]);
    let paid = s.mine(vec![paying]);
    let empty = s.mine(Vec::new());
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    assert_eq!(s.fetched(), [paid, empty]);
    assert_eq!(scanned.fetched, 2);
}

#[tokio::test]
async fn a_reorganization_drops_what_the_replaced_blocks_held() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[40_000]);
    s.mine(vec![paying.clone()]);
    s.mine(Vec::new());
    assert_eq!(s.scan().await.unwrap().found[0].height, 3);

    // Blocks 3 and 4 replaced, without the payment: a chain behind the
    // blocks looked in proves nothing yet.
    s.chain.lock().unwrap().fork(3);
    s.chain.lock().unwrap().mine_empty(1);
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (5, 3));
    assert_eq!(s.wallet.silent_coins().unwrap().len(), 1);
    s.chain.lock().unwrap().mine_empty(2);
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (6, 5));
    assert!(scanned.found.is_empty());
    assert!(s.wallet.silent_coins().unwrap().is_empty());

    // Mined again later: found again.
    s.mine(vec![paying.clone()]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    assert_eq!(scanned.found[0].height, 6);

    // The wallet's spend, in no block yet, outlives its coin's block.
    let spend = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    let txid = spend.compute_txid();
    s.chain.lock().unwrap().fork(6);
    s.mine(Vec::new());
    s.mine(vec![paying.clone()]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.next, 8);
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 1);
    assert_eq!(coins[0].height, 7);
    assert_eq!(coins[0].spending, Some(txid));
    assert_eq!(
        s.wallet.silent_spends().unwrap(),
        std::slice::from_ref(&spend)
    );

    // Its spend's block replaced: the coin is the wallet's again, its
    // spend still to broadcast.
    s.mine(vec![spend.clone()]);
    s.scan().await.unwrap();
    assert!(s.wallet.silent_coins().unwrap().is_empty());
    s.chain.lock().unwrap().fork(8);
    s.chain.lock().unwrap().mine_empty(2);
    s.scan().await.unwrap();
    let coins = s.wallet.silent_coins().unwrap();
    assert_eq!(coins.len(), 1);
    assert_eq!(coins[0].spending, Some(txid));
    assert_eq!(s.wallet.silent_spends().unwrap(), [spend]);
}

#[tokio::test]
async fn a_reorganization_deeper_than_the_blocks_kept_needs_a_look_again() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[40_000]);
    s.mine(vec![paying]);
    s.chain.lock().unwrap().mine_empty(297);
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
    s.chain.lock().unwrap().fork(3);
    s.chain.lock().unwrap().mine_empty(299);
    match s.scan().await {
        Err(Error::Invalid(why)) => assert!(why.contains("deeper"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.wallet.silent_coins().unwrap().len(), 1);
    s.wallet.watch_silent(HUB, 1).unwrap();
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (302, 301));
    assert!(s.wallet.silent_coins().unwrap().is_empty());
}

#[tokio::test]
async fn tweaks_of_another_block_than_the_chains_are_refused() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[40_000]);
    s.mine(vec![paying]);
    *s.hub.forged.lock().unwrap() = Some(2);
    match s.scan().await {
        Err(Error::Unavailable { hub, reason }) => {
            assert_eq!(hub, HUB);
            assert!(reason.contains("block 2"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // Nothing of the batch was recorded.
    assert_eq!(s.wallet.silent_next().unwrap(), Some(1));
    *s.hub.forged.lock().unwrap() = None;
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
}

#[tokio::test]
async fn a_wallet_file_of_the_first_version_gains_silent_payments() {
    let Setup {
        wallet,
        chain,
        hub,
        dir,
    } = setup().await;
    drop(wallet);
    let path = dir.path().join("v1.sqlite");
    let version = |path: &std::path::Path| -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!("fixtures/wallet-v1.sql"))
        .unwrap();
    assert_eq!(version(&path), 1);
    let wallet = Wallet::open(&path, Via(hub.clone())).unwrap();
    let address = wallet.silent_keys().address();
    assert_eq!(version(&path), 11);
    assert_eq!(wallet.hubs().unwrap().len(), 1);
    assert_eq!(wallet.silent_next().unwrap(), None);
    wallet.watch_silent(HUB, 1).unwrap();
    let paying = chain.lock().unwrap().pay(&[address], &[10_000]);
    chain.lock().unwrap().mine(vec![paying]);
    let scanned = wallet
        .scan_silent(HUB, &Light(chain.clone()), u32::MAX)
        .await
        .unwrap();
    assert_eq!(scanned.found.len(), 1);
    drop(wallet);
    // A newer version's file is refused, untouched.
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 12)
        .unwrap();
    match Wallet::open(&path, Via(hub)) {
        Err(Error::Setup(why)) => assert!(why.contains("schema 12"), "{why}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("opened"),
    }
    assert_eq!(version(&path), 12);
}

#[tokio::test]
async fn a_fallback_address_is_watched_once_handed_out_and_the_next_once_paid() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let first = fallback_of(&s.wallet, 0);
    // None handed out: none watched, blocks without tweaks not even filtered.
    let plain = s.wallet.payment_code(&[HUB.into()], false).unwrap();
    assert_eq!(plain.address, None);
    let early = s.pay_plain(&first, 10_000);
    let paid = s.mine(vec![early]);
    s.mine(Vec::new());
    assert!(s.scan().await.unwrap().found.is_empty());
    assert_eq!(s.chain.lock().unwrap().filtered, 0);

    // Handed out in a code, its QR's too, and the same until paid.
    let code = s.wallet.payment_code(&[HUB.into()], true).unwrap();
    assert!(code.qr().starts_with("BITCOIN:BCRT1P"), "{}", code.qr());
    assert!(code.qr().contains("?SP=TSP1Q"), "{}", code.qr());
    assert_eq!(code.to_string().parse::<PaymentCode>().unwrap(), code);
    assert_eq!(s.fallback(), first);
    // Looked for again: found, in the one block paying it.
    s.wallet.watch_silent(HUB, 1).unwrap();
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    let coin = &scanned.found[0];
    assert_eq!((coin.value, coin.height, coin.change), (10_000, 3, false));
    assert_eq!(s.fetched(), [paid]);

    // Paid: the next is handed out, the first still watched.
    let second = s.fallback();
    assert_eq!(second, fallback_of(&s.wallet, 1));
    assert_eq!(s.fallback(), second);
    let again = s.pay_plain(&first, 20_000);
    let to_second = s.pay_plain(&second, 30_000);
    s.mine(vec![again, to_second]);
    assert_eq!(s.scan().await.unwrap().found.len(), 2);
    assert_eq!(s.fallback(), fallback_of(&s.wallet, 2));

    // Spent with the spend key, like any coin of the address.
    let all = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    assert_eq!(all.input.len(), 3);
    assert_signed(&s.chain.lock().unwrap(), &all);
}

#[tokio::test]
async fn the_fallback_address_handed_out_never_goes_back() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let first = s.fallback();
    let paying = s.pay_plain(&first, 10_000);
    s.mine(vec![paying]);
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
    let second = s.fallback();
    assert_eq!(second, fallback_of(&s.wallet, 1));
    // Its payment reorganized away: the second is still the one handed out.
    s.chain.lock().unwrap().fork(3);
    s.chain.lock().unwrap().mine_empty(2);
    s.scan().await.unwrap();
    assert!(s.wallet.silent_coins().unwrap().is_empty());
    assert_eq!(s.fallback(), second);
    // A copy of the wallet handed out more: paid there, found here, and
    // handed out past it.
    let paying = s.pay_plain(&fallback_of(&s.wallet, 5), 10_000);
    s.mine(vec![paying]);
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
    assert_eq!(s.fallback(), fallback_of(&s.wallet, 6));
    // Beyond the 20 watched past it: not found.
    let paying = s.pay_plain(&fallback_of(&s.wallet, 6 + 20 + 1), 10_000);
    s.mine(vec![paying]);
    assert!(s.scan().await.unwrap().found.is_empty());
}

#[tokio::test]
async fn a_wallet_file_of_version_2_watches_no_fallback_until_it_hands_one_out() {
    let Setup {
        wallet,
        chain,
        hub,
        dir,
    } = setup().await;
    drop(wallet);
    let path = dir.path().join("v2.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!("fixtures/wallet-v2.sql"))
        .unwrap();
    let wallet = Wallet::open(&path, Via(hub)).unwrap();
    assert_eq!(wallet.silent_next().unwrap(), Some(1));
    let light = Light(chain.clone());
    let scanned = wallet.scan_silent(HUB, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.next, 3);
    assert_eq!(chain.lock().unwrap().filtered, 0);
    // Handed out, then watched.
    let code = wallet.payment_code(&[HUB.into()], true).unwrap();
    let script = code.address.unwrap().assume_checked().script_pubkey();
    assert_eq!(script, fallback_of(&wallet, 0));
    let paying = chain.lock().unwrap().pay_plain(&script, 10_000);
    chain.lock().unwrap().mine(vec![paying]);
    let scanned = wallet.scan_silent(HUB, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.found.len(), 1);
}

/// The outputs of `tx` that `keys` finds paying it, by the tweak of the
/// coins `chain` holds that it spends.
fn found_by(chain: &Chain, keys: &silent::Keys, tx: &Transaction) -> Vec<u64> {
    let spent: Vec<&bitcoin::Script> = tx
        .input
        .iter()
        .map(|i| chain.outputs[&i.previous_output].script_pubkey.as_script())
        .collect();
    let tweak = silent::tweak(tx, &spent).unwrap();
    let found = keys
        .scanner([silent::CHANGE])
        .scan(&tweak, &silent::taproot_outputs(tx));
    let value = |key: XOnlyPublicKey| {
        let script = ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(key));
        tx.output
            .iter()
            .find(|o| o.script_pubkey == script)
            .unwrap()
            .value
            .to_sat()
    };
    let mut values: Vec<u64> = found.iter().map(|f| value(f.output)).collect();
    values.sort();
    values
}

#[tokio::test]
async fn a_spend_pays_a_silent_payment_address_its_receiver_finds() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    let paying = s.pay(&[s.address()], &[50_000]);
    s.mine(vec![paying]);
    s.scan().await.unwrap();

    // Another network's address is refused, its coins left free.
    let main = silent::Keys::from_seed(&[9; 32], bitcoin::NetworkKind::Main).unwrap();
    match s.wallet.spend_silent(main.address(), Some(20_000), rate()) {
        Err(Error::Invalid(why)) => assert!(why.contains("not of regtest"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(s.wallet.silent_spends().unwrap().is_empty());

    // Someone else's: they find what it pays them, the wallet its change.
    let them = silent::Keys::from_seed(&[9; 32], bitcoin::NetworkKind::Test).unwrap();
    assert_eq!(them.address(), stranger());
    let spend = s
        .wallet
        .spend_silent(stranger(), Some(20_000), rate())
        .unwrap();
    assert_signed(&s.chain.lock().unwrap(), &spend);
    assert_eq!(spend.output.len(), 2);
    assert!(spend.output.iter().all(|o| o.script_pubkey.is_p2tr()));
    assert_eq!(found_by(&s.chain.lock().unwrap(), &them, &spend), [20_000]);
    s.mine(vec![spend.clone()]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    assert!(scanned.found[0].change);
    let fee = rate().fee_wu(spend.weight()).unwrap().to_sat();
    let change = 50_000 - 20_000 - fee;
    assert_eq!(scanned.found[0].value, change);

    // Its own address: the payment and the change, derived together, both
    // found again.
    let to_self = s
        .wallet
        .spend_silent(s.address(), Some(10_000), rate())
        .unwrap();
    assert_eq!(to_self.output.len(), 2);
    s.mine(vec![to_self.clone()]);
    let scanned = s.scan().await.unwrap();
    let mut found: Vec<(u64, bool)> = scanned.found.iter().map(|c| (c.value, c.change)).collect();
    found.sort();
    let fee = rate().fee_wu(to_self.weight()).unwrap().to_sat();
    assert_eq!(found, [(10_000, false), (change - 10_000 - fee, true)]);
}

#[tokio::test]
async fn payments_to_fallback_addresses_are_seen_on_their_way() {
    let s = setup().await;
    s.wallet.watch_silent(HUB, 1).unwrap();
    // None handed out: nothing to watch.
    assert!(s.wallet.fallback_scripts().unwrap().is_empty());
    let first = s.fallback();
    let watched = s.wallet.fallback_scripts().unwrap();
    // The one handed out and 20 after.
    assert_eq!(watched.len(), 21);
    assert!(watched.contains(&first));
    assert!(watched.contains(&fallback_of(&s.wallet, 20)));

    // On its way: its output to the address, once however often relayed,
    // and nothing of a payment elsewhere.
    let paying = s.pay_plain(&first, 10_000);
    let other = s.pay_plain(&elsewhere(), 5_000);
    let txs = [paying.clone(), other, paying.clone()];
    let seen = s.wallet.unconfirmed_payments(&txs).unwrap();
    let vout = paying.output.iter().position(|o| o.script_pubkey == first);
    let outpoint = OutPoint::new(paying.compute_txid(), vout.unwrap() as u32);
    assert_eq!(seen, [(outpoint, 10_000)]);

    // Found in a block: no longer on its way, even once spent.
    s.mine(vec![paying.clone()]);
    assert_eq!(s.scan().await.unwrap().found.len(), 1);
    assert!(s.wallet.unconfirmed_payments(&txs).unwrap().is_empty());
    let spend = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    s.mine(vec![spend]);
    s.scan().await.unwrap();
    assert!(s.wallet.unconfirmed_payments(&txs).unwrap().is_empty());
}

#[tokio::test]
async fn without_tweaks_fallback_payments_their_spends_and_change_are_found() {
    let s = setup().await;
    *s.hub.first.lock().unwrap() = None;
    s.wallet.add_hub(HUB).await.unwrap();
    assert!(s.wallet.watch_silent(HUB, 1).is_err());
    s.wallet.watch_silent(None, 1).unwrap();
    let light = Light(s.chain.clone());
    let paying = s.pay_plain(&s.fallback(), 30_000);
    let silent = s.pay(&[s.address()], &[20_000]);
    s.mine(vec![paying]);
    s.mine(vec![silent]);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!((scanned.next, scanned.height), (5, 4));
    // The fallback payment only: a silent payment needs its tweak.
    let values: Vec<u64> = scanned.found.iter().map(|c| c.value).collect();
    assert_eq!(values, [30_000]);

    // Its spend, and the change of it, found with the tweak of the coins
    // it spends.
    let spend = s
        .wallet
        .spend_silent(elsewhere(), Some(10_000), rate())
        .unwrap();
    assert_eq!(spend.output.len(), 2);
    s.mine(vec![spend.clone()]);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.spent.len(), 1);
    assert_eq!(scanned.found.len(), 1);
    let change = scanned.found[0].clone();
    assert!(change.change);
    assert_eq!(change.outpoint.txid, spend.compute_txid());
    // Spendable, and seen spent.
    let all = s.wallet.spend_silent(elsewhere(), None, rate()).unwrap();
    assert_signed(&s.chain.lock().unwrap(), &all);
    s.mine(vec![all]);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.spent, [change.outpoint]);
    assert!(s.wallet.silent_coins().unwrap().is_empty());
    assert_eq!(*s.hub.asked.lock().unwrap(), 0);
}

#[tokio::test]
async fn blocks_looked_in_without_tweaks_are_looked_in_again_once_a_hub_serves_them() {
    let s = setup().await;
    let light = Light(s.chain.clone());
    s.wallet.watch_silent(None, 1).unwrap();
    let plain = s.pay_plain(&s.fallback(), 30_000);
    let silent = s.pay(&[s.address()], &[20_000]);
    s.mine(vec![plain]);
    s.mine(vec![silent]);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    // The wallet's spend, in no block yet, outlives looking again.
    let spend = s
        .wallet
        .spend_silent(elsewhere(), Some(10_000), rate())
        .unwrap();
    let txid = spend.compute_txid();

    // From the first block looked in without tweaks: the silent payment
    // found, the fallback payment again, once.
    let scanned = s.scan().await.unwrap();
    assert_eq!((scanned.next, scanned.height), (5, 4));
    let mut values: Vec<u64> = scanned.found.iter().map(|c| c.value).collect();
    values.sort();
    assert_eq!(values, [20_000, 30_000]);
    let mut coins: Vec<_> = s
        .wallet
        .silent_coins()
        .unwrap()
        .into_iter()
        .map(|c| (c.value, c.spending))
        .collect();
    coins.sort();
    assert_eq!(coins, [(20_000, None), (30_000, Some(txid))]);
    assert_eq!(s.wallet.silent_spends().unwrap(), [spend]);
    // Once.
    s.chain.lock().unwrap().mine_empty(1);
    s.fetched();
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.next, 6);
    assert!(scanned.found.is_empty());
    assert!(s.fetched().is_empty());
}

#[tokio::test]
async fn looking_again_with_tweaks_starts_at_the_first_block_the_hub_serves() {
    let s = setup().await;
    let light = Light(s.chain.clone());
    s.wallet.watch_silent(None, 1).unwrap();
    let early = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![early]);
    s.chain.lock().unwrap().mine_empty(2);
    let late = s.pay(&[s.address()], &[20_000]);
    s.mine(vec![late]);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.next, 7);
    assert!(scanned.found.is_empty());

    // A hub serving tweaks from after the next block is refused, and what
    // was looked in without them is still to look in again.
    s.chain.lock().unwrap().mine_empty(3);
    *s.hub.first.lock().unwrap() = Some(8);
    s.wallet.add_hub(HUB).await.unwrap();
    match s.scan().await {
        Err(Error::Invalid(why)) => assert!(why.contains("from block 8 on"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.wallet.silent_next().unwrap(), Some(7));

    // One serving them from block 5: from it, not before.
    *s.hub.first.lock().unwrap() = Some(5);
    s.wallet.add_hub(HUB).await.unwrap();
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.next, 10);
    let values: Vec<u64> = scanned.found.iter().map(|c| c.value).collect();
    assert_eq!(values, [20_000]);
    // Once: the blocks before stay looked in without tweaks...
    *s.hub.first.lock().unwrap() = Some(1);
    s.wallet.add_hub(HUB).await.unwrap();
    assert!(s.scan().await.unwrap().found.is_empty());
    // ...unless looked in again from one.
    s.wallet.watch_silent(HUB, 1).unwrap();
    assert_eq!(s.scan().await.unwrap().found.len(), 2);
}

#[tokio::test]
async fn a_reorganization_of_blocks_looked_in_without_tweaks_keeps_them_to_look_in_again() {
    let s = setup().await;
    let light = Light(s.chain.clone());
    s.wallet.watch_silent(HUB, 1).unwrap();
    s.scan().await.unwrap();
    // Blocks 3 on looked in without tweaks.
    let silent = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![silent.clone()]);
    s.chain.lock().unwrap().mine_empty(2);
    s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    // Replaced from block 4, then from block 2, each looked in without
    // tweaks again.
    for at in [4, 2] {
        s.chain.lock().unwrap().fork(at);
        s.chain.lock().unwrap().mine_empty(6 - at);
        if at == 2 {
            s.mine(vec![silent.clone()]);
        }
        let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
        assert!(scanned.found.is_empty());
    }
    let scanned = s.scan().await.unwrap();
    let found: Vec<(u64, u32)> = scanned.found.iter().map(|c| (c.value, c.height)).collect();
    assert_eq!(found, [(10_000, 6)]);
    assert!(s.scan().await.unwrap().found.is_empty());
}

#[tokio::test]
async fn a_wallet_file_of_version_3_never_watching_finds_its_fallback_payment() {
    let Setup {
        wallet,
        chain,
        hub,
        dir,
    } = setup().await;
    drop(wallet);
    let path = dir.path().join("v3.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!("fixtures/wallet-v3.sql"))
        .unwrap();
    let wallet = Wallet::open(&path, Via(hub.clone())).unwrap();
    assert_eq!(wallet.silent_next().unwrap(), None);
    // The address it handed out, still.
    let code = wallet.payment_code(&[HUB.into()], true).unwrap();
    let script = code.address.unwrap().assume_checked().script_pubkey();
    assert_eq!(script, fallback_of(&wallet, 0));
    let paying = chain.lock().unwrap().pay_plain(&script, 10_000);
    chain.lock().unwrap().mine(vec![paying]);
    wallet.watch_silent(None, 1).unwrap();
    let light = Light(chain.clone());
    let scanned = wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.found.len(), 1);
    assert_eq!(*hub.asked.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_hub_serving_tweaks_from_the_next_block_looks_in_none_again() {
    let s = setup().await;
    let light = Light(s.chain.clone());
    s.wallet.watch_silent(None, 1).unwrap();
    s.chain.lock().unwrap().mine_empty(2);
    let scanned = s.wallet.scan_silent(None, &light, u32::MAX).await.unwrap();
    assert_eq!(scanned.next, 5);
    // Tweaks from block 5 on: what was looked in without them stays so,
    // and the first batch with them is looked in once.
    *s.hub.first.lock().unwrap() = Some(5);
    s.wallet.add_hub(HUB).await.unwrap();
    let paying = s.pay(&[s.address()], &[10_000]);
    s.mine(vec![paying]);
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.next, 6);
    assert_eq!(scanned.found.len(), 1);
    s.fetched();
    let scanned = s.scan().await.unwrap();
    assert_eq!(scanned.next, 6);
    assert!(scanned.found.is_empty());
    assert!(s.fetched().is_empty());
    assert_eq!(s.wallet.silent_coins().unwrap().len(), 1);
}
