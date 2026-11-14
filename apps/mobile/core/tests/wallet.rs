//! The mobile wallet against a hub in this process, over HTTP: paid on
//! Bitcoin at its silent payment address, found through a light client,
//! moved to notes, paying another wallet in them through its mailbox or by
//! hand; and what it refuses.

#[path = "../../../hub/tests/chain/mod.rs"]
mod chain;
#[path = "../../../hub/tests/esplora/mod.rs"]
mod esplora;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use bitcoin::{
    absolute::LockTime,
    bip158::BlockFilter,
    consensus::{deserialize, serialize},
    hashes::Hash,
    key::TweakedPublicKey,
    secp256k1::{Secp256k1, SecretKey},
    transaction, Amount, Block, BlockHash, CompressedPublicKey, OutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use licash_api::Fee;
use licash_chain_client::Source;
use licash_hub::{Hub, Policy, Timing};
use licash_hub_service::{
    deposit,
    tweaks::{step, Tweaks},
    Services,
};
use licash_mobile::{
    set_logger, wallet_exists, Activity, Balance, ChainError, ChainSource, Kind, LicashError,
    Logger, Network, Range, Scanned, State, Wallet,
};
use licash_wallet::PaymentCode;
use nostr_sdk::prelude::MockRelay;

const MAX_POW: u32 = 6;

/// Short, as the wallet CLI's tests have it.
const TIMING: Timing = Timing {
    window: 20,
    reaction: 2,
    min_depth: 1,
    max_headers: 4032,
    bury: 6,
    max_lag: 120,
};

/// A hub charging 1 on a payment, taking deposits of 1000 to 4000 for 100
/// and indexing tweaks from block 3, over a mock Esplora; mailbox relays
/// if asked.
struct Net {
    runtime: tokio::runtime::Runtime,
    hub: Arc<Hub>,
    deposits: Arc<deposit::Gateway>,
    tweaks: Arc<Tweaks>,
    source: Source,
    backend: esplora::Shared,
    url: String,
    relay: Option<MockRelay>,
    dir: tempfile::TempDir,
}

impl Net {
    fn new(relays: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay = relays.then(|| relay(&runtime));
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, MAX_POW).unwrap();
        let fee = Fee { base: 1, ppm: 0 };
        let policy = Policy {
            spend: fee,
            lock: fee,
            refund: fee,
            timing: TIMING,
        };
        let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
        let chain = chain::Chain::new();
        hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
        let backend = esplora::backend(chain);
        let source: Source = esplora::serve(backend.clone()).parse().unwrap();
        let config = deposit::Config {
            fee: Fee { base: 100, ppm: 0 },
            min: 1_000,
            max: 4_000,
            depth: 2,
            window: 10,
            lock_blocks: 6,
            downtime: 2,
            sweep_blocks: 6,
            min_fee_rate: bitcoin::FeeRate::from_sat_per_vb_u32(1),
            ..deposit::Config::new(sweep())
        };
        let profile = licash::spend::ChainProfile::Core30DefaultRegtest;
        let path = dir.path().join("deposits.sqlite");
        let gateway = deposit::Gateway::open(hub.clone(), source.clone(), profile, config, &path);
        let deposits = Arc::new(gateway.unwrap());
        let tweaks = Arc::new(Tweaks::open(&dir.path().join("tweaks.sqlite")).unwrap());
        let mut net = Self {
            runtime,
            hub,
            deposits,
            tweaks,
            source,
            backend,
            url: String::new(),
            relay,
            dir,
        };
        net.mine(3, Vec::new());
        net.round();
        let relays = match &net.relay {
            Some(relay) => vec![net.runtime.block_on(relay.url()).to_string()],
            None => Vec::new(),
        };
        let services = Services {
            deposits: Some(net.deposits.clone()),
            tweaks: Some(net.tweaks.clone()),
            relays,
            ..Default::default()
        };
        let app = licash_hub_service::router(net.hub.clone(), services);
        let listener = net
            .runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        net.url = format!("http://{}", listener.local_addr().unwrap());
        net.runtime
            .spawn(async move { axum::serve(listener, app).await.unwrap() });
        net
    }

    /// Mine `blocks`, the first with `txs`, which the hub scans and
    /// indexes the tweaks of.
    fn mine(&self, blocks: u32, mut txs: Vec<Transaction>) {
        {
            let mut backend = self.backend.lock().unwrap();
            let ids: Vec<Txid> = txs.iter().map(Transaction::compute_txid).collect();
            backend.mempool.retain(|t| !ids.contains(&t.compute_txid()));
            let chain = &mut backend.chain;
            for _ in 0..blocks {
                let block = chain.mine_with(None, std::mem::take(&mut txs)).clone();
                self.hub.observe(chain.height(), &block).unwrap();
            }
            assert!(self.hub.synced(chain.height(), &chain.tip()).unwrap());
        }
        let indexed = step(&self.tweaks, &self.source, None);
        self.runtime.block_on(indexed).unwrap();
    }

    /// Mine what was sent, in one block.
    fn confirm_sent(&self) {
        let sent = self.sent();
        self.mine(1, sent);
    }

    fn sent(&self) -> Vec<Transaction> {
        self.backend.lock().unwrap().mempool.clone()
    }

    /// One round of the deposit gateway.
    fn round(&self) {
        self.runtime.block_on(self.deposits.round());
    }

    fn height(&self) -> u32 {
        self.backend.lock().unwrap().chain.height()
    }

    fn dir(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// A new wallet `name`, made at `birthday`, trusting the hub.
    fn wallet(&self, name: &str, birthday: u32) -> Arc<Wallet> {
        let wallet = Wallet::create(self.dir(name), Network::Regtest, birthday, false).unwrap();
        wallet.add_hub(self.url.clone()).unwrap();
        wallet
    }

    /// Pay `value` to the silent payment address of the payment code
    /// `code`, from a P2WPKH coin of the `n`th payer: unconfirmed.
    fn pay_silent(&self, code: &str, value: u64, n: u8) -> Transaction {
        let code: PaymentCode = code.parse().unwrap();
        let to = code.silent.expect("a silent payment address");
        let secp = Secp256k1::new();
        let sender = SecretKey::from_slice(&[n; 32]).unwrap();
        let key = CompressedPublicKey(sender.public_key(&secp));
        let previous_output = OutPoint::new(Txid::from_byte_array([n; 32]), 0);
        let spent = ScriptBuf::new_p2wpkh(&key.wpubkey_hash());
        let mut backend = self.backend.lock().unwrap();
        backend.outside.insert(previous_output, spent);
        let paid = licash_onchain::silent::outputs(&[(sender, false)], &[previous_output], &[to]);
        let output = TweakedPublicKey::dangerous_assume_tweaked(paid.unwrap()[0]);
        let mut witness = Witness::new();
        witness.push([0x30; 71]);
        witness.push(key.to_bytes());
        Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value),
                script_pubkey: ScriptBuf::new_p2tr_tweaked(output),
            }],
        }
    }

    /// A payment of `value` to `address` from coin `n` of a payer's, of a
    /// script-hash input, which silent payments skip.
    fn pay_plain(&self, address: &str, value: u64, n: u8) -> Transaction {
        let address: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
            address.parse().unwrap();
        let script = ScriptBuf::from_bytes(vec![0x51]);
        let previous_output = OutPoint::new(Txid::from_byte_array([n; 32]), 0);
        let spent = ScriptBuf::new_p2wsh(&script.wscript_hash());
        self.backend
            .lock()
            .unwrap()
            .outside
            .insert(previous_output, spent);
        let mut witness = Witness::new();
        witness.push(script.as_bytes());
        Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value),
                script_pubkey: address.assume_checked().script_pubkey(),
            }],
        }
    }
}

/// A Nostr relay in this process, tried again should another socket take
/// the port it probed.
fn relay(runtime: &tokio::runtime::Runtime) -> MockRelay {
    let mut failed = None;
    for _ in 0..5 {
        match runtime.block_on(MockRelay::run()) {
            Ok(relay) => return relay,
            Err(e) => failed = Some(e),
        }
    }
    panic!("no mock relay: {failed:?}");
}

/// Where the hub's deposit claims pay.
fn sweep() -> ScriptBuf {
    let key = SecretKey::from_slice(&[9; 32]).unwrap();
    let key = key.x_only_public_key(&Secp256k1::new()).0;
    ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(key))
}

/// The mock's chain as a phone's light client serves it: the blocks'
/// compact filters, the blocks it asks for, counted, and a broadcast that
/// can be made to fail.
struct Light {
    backend: esplora::Shared,
    fetched: Mutex<Vec<u32>>,
    failing: AtomicBool,
}

impl Light {
    fn new(net: &Net) -> Arc<Self> {
        Arc::new(Self {
            backend: net.backend.clone(),
            fetched: Mutex::new(Vec::new()),
            failing: AtomicBool::new(false),
        })
    }

    /// The heights of the blocks fetched since last asked.
    fn fetched(&self) -> Vec<u32> {
        std::mem::take(&mut self.fetched.lock().unwrap())
    }

    fn block_of(&self, hash: &[u8]) -> Result<Block, ChainError> {
        let hash = BlockHash::from_byte_array(hash.try_into().unwrap());
        let backend = self.backend.lock().unwrap();
        let block = backend.chain.blocks.iter().find(|b| b.block_hash() == hash);
        block.cloned().ok_or_else(|| ChainError::Failed {
            reason: format!("no block {hash}"),
        })
    }
}

impl ChainSource for Light {
    fn height(&self) -> Result<u32, ChainError> {
        Ok(self.backend.lock().unwrap().chain.height())
    }

    fn hash(&self, height: u32) -> Result<Vec<u8>, ChainError> {
        let backend = self.backend.lock().unwrap();
        match backend.chain.blocks.get(height as usize) {
            Some(block) => Ok(block.block_hash().to_byte_array().to_vec()),
            None => Err(ChainError::Failed {
                reason: format!("no block {height}"),
            }),
        }
    }

    fn filter(&self, hash: Vec<u8>) -> Result<Vec<u8>, ChainError> {
        let block = self.block_of(&hash)?;
        let backend = self.backend.lock().unwrap();
        let spent = |outpoint: &OutPoint| {
            let mut made = backend.chain.blocks.iter().flat_map(|b| &b.txdata);
            let made = made.find(|tx| tx.compute_txid() == outpoint.txid);
            let made = made.and_then(|tx| tx.output.get(outpoint.vout as usize));
            let made = made.map(|output| output.script_pubkey.clone());
            let outside = backend.outside.get(outpoint).cloned();
            outside
                .or(made)
                .ok_or(bitcoin::bip158::Error::UtxoMissing(*outpoint))
        };
        Ok(BlockFilter::new_script_filter(&block, spent)
            .unwrap()
            .content)
    }

    fn block(&self, hash: Vec<u8>) -> Result<Vec<u8>, ChainError> {
        let block = self.block_of(&hash)?;
        let backend = self.backend.lock().unwrap();
        let height = backend.chain.blocks.iter().position(|b| *b == block);
        self.fetched.lock().unwrap().push(height.unwrap() as u32);
        Ok(serialize(&block))
    }

    fn broadcast(&self, tx: Vec<u8>) -> Result<(), ChainError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(ChainError::Failed {
                reason: "no peers".into(),
            });
        }
        let tx: Transaction = deserialize(&tx).unwrap();
        let mut backend = self.backend.lock().unwrap();
        if !backend.mempool.contains(&tx) {
            backend.mempool.push(tx);
        }
        Ok(())
    }

    fn unconfirmed(&self, script: Vec<u8>) -> Result<Vec<u8>, ChainError> {
        let backend = self.backend.lock().unwrap();
        let paying = backend.mempool.iter().filter(|tx| {
            let mut outputs = tx.output.iter();
            outputs.any(|o| o.script_pubkey.as_bytes() == script.as_slice())
        });
        Ok(paying.flat_map(serialize).collect())
    }
}

/// The lines logged, by level.
#[derive(Default)]
struct Lines(Mutex<Vec<(String, String)>>);

impl Logger for Lines {
    fn log(&self, level: String, line: String) {
        self.0.lock().unwrap().push((level, line));
    }
}

/// Look until caught up with the chain: the coins found, and their worth.
fn scan(wallet: &Wallet, light: &Arc<Light>) -> (u32, u64) {
    let (mut found, mut value) = (0, 0);
    loop {
        let scan = wallet.scan(light.clone()).unwrap();
        found += scan.found;
        value += scan.found_value;
        if scan.next > scan.height {
            return (found, value);
        }
    }
}

fn invalid(result: Result<impl std::fmt::Debug, LicashError>) -> String {
    match result {
        Err(LicashError::Invalid { reason }) => reason,
        other => panic!("not refused as invalid: {other:?}"),
    }
}

fn notes(wallet: &Wallet) -> u64 {
    wallet.balance().unwrap().notes
}

#[test]
fn a_phone_paid_on_bitcoin_moves_it_to_notes_and_pays_in_them() {
    let lines = Arc::new(Lines::default());
    set_logger(lines.clone(), "info".into());
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = net.wallet("alice", net.height());
    let bob = net.wallet("bob", net.height());
    let hubs = alice.hubs().unwrap();
    assert_eq!(hubs.len(), 1);
    assert_eq!((hubs[0].fee_base, hubs[0].fee_ppm), (1, 0));
    assert_eq!(
        hubs[0].deposits,
        Some(Range {
            min: 1_000,
            max: 4_000
        })
    );
    assert!(hubs[0].silent && hubs[0].mailbox);

    // Paid at the silent payment address in her code: found in the one
    // block whose filter matches, and shown first in her history.
    let code = alice.payment_code(None).unwrap();
    assert!(code.qr.starts_with("BITCOIN:BCRT1P"), "{}", code.qr);
    assert!(code.qr.contains("?SP=TSP1Q"), "{}", code.qr);
    assert!(code.qr.contains("&LICASH=TLICASH1Q"), "{}", code.qr);
    net.mine(2, Vec::new());
    let payment = net.pay_silent(&code.qr, 3_000, 1);
    net.mine(1, vec![payment]);
    net.mine(2, Vec::new());
    assert_eq!(scan(&alice, &light), (1, 3_000));
    assert_eq!(light.fetched(), [6]);
    let onchain = Balance {
        onchain: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), onchain);
    let history = alice.history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!((history[0].kind, history[0].amount), (Kind::Onchain, 3_000));
    // Nothing new: nothing fetched.
    assert_eq!(scan(&alice, &light), (0, 0));
    assert_eq!(light.fetched(), [0; 0]);

    // Moved to notes: all of it but the hub's fee and the transaction's,
    // under way until the deposit confirms at depth.
    let moving = alice.move_to_licash(light.clone()).unwrap();
    assert_eq!((moving.kind, moving.state), (Kind::Moved, State::Pending));
    assert_eq!(moving.fee, 100);
    let sent = net.sent();
    assert_eq!(
        (sent.len(), sent[0].input.len(), sent[0].output.len()),
        (1, 1, 1)
    );
    assert_eq!(sent[0].output[0].value.to_sat(), moving.amount + 100);
    let fee = 3_000 - moving.amount - 100;
    assert_eq!(fee, 2 * sent[0].vsize() as u64);
    let moving_balance = Balance {
        moving: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), moving_balance);
    net.confirm_sent();
    net.round();
    net.mine(1, Vec::new());
    net.round();
    alice.resume().unwrap();
    assert_eq!(scan(&alice, &light), (0, 0));
    let held = Balance {
        notes: moving.amount,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), held);
    let history = alice.history().unwrap();
    assert_eq!(history.len(), 1, "{history:#?}");
    assert_eq!(
        (history[0].kind, history[0].state),
        (Kind::Moved, State::Done)
    );
    let logged = lines.0.lock().unwrap().clone();
    assert!(
        logged
            .iter()
            .any(|(level, line)| level == "INFO" && line.contains("moving to notes")),
        "{logged:?}"
    );

    // Bob asks for 500 with a code naming his mailbox: she pays it there.
    let asked = bob.payment_code(Some(500)).unwrap();
    let scanned = alice.scanned(asked.qr.clone()).unwrap();
    let wanted = Scanned::Code {
        amount: Some(500),
        notes: true,
    };
    assert_eq!(scanned, wanted);
    let paid = alice.pay(asked.qr, None).unwrap();
    assert!(paid.delivered, "{paid:?}");
    assert_eq!((paid.amount, &paid.hub), (500, &hubs[0].url));
    assert_eq!((paid.handover, paid.undelivered), (None, None));
    assert_eq!(notes(&alice), moving.amount - 501);
    let got = bob.collect().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].kind, got[0].amount), (Kind::Received, 500));
    // Each payment is taken once.
    assert!(bob.collect().unwrap().is_empty());
    assert_eq!(notes(&bob), 500);
    let history = alice.history().unwrap();
    let sent = Activity {
        id: paid.id.clone(),
        kind: Kind::Sent,
        state: State::Done,
        amount: 500,
        fee: 1,
        detail: hubs[0].url.clone(),
        at: history[0].at,
    };
    assert_eq!(history[0], sent);

    // The mailbox's relay down: a payment to hand over, which Bob takes
    // once, and someone else not at all.
    net.relay.as_ref().unwrap().shutdown();
    let code = bob.payment_code(None).unwrap();
    let paid = alice.pay(code.text, Some(100)).unwrap();
    assert!(!paid.delivered && paid.undelivered.is_some(), "{paid:?}");
    let handover = paid.handover.unwrap();
    assert_eq!(bob.scanned(handover.clone()).unwrap(), Scanned::Payment);
    let carol = Wallet::create(net.dir("carol"), Network::Regtest, 0, false).unwrap();
    let unknown = carol.receive(handover.clone(), false);
    assert!(
        matches!(&unknown, Err(LicashError::UnknownHub { url }) if *url == hubs[0].url),
        "{unknown:?}"
    );
    let refused = invalid(carol.receive(handover.clone(), true));
    assert!(refused.contains("pays this wallet nothing"), "{refused}");
    assert_eq!(carol.hubs().unwrap().len(), 1);
    let received = bob.receive(handover.clone(), false).unwrap();
    assert_eq!((received.kind, received.amount), (Kind::Received, 100));
    assert!(invalid(bob.receive(handover, false)).contains("received already"));
    assert_eq!(notes(&bob), 600);
    assert_eq!(notes(&alice), moving.amount - 602);
}

#[test]
fn a_phone_refuses_what_it_cannot_do_and_frees_coins_it_could_not_send() {
    let net = Net::new(false);
    let light = Light::new(&net);
    let dir = net.dir("alice");
    assert!(!wallet_exists(dir.clone()));
    let alice = Wallet::create(dir.clone(), Network::Regtest, 0, false).unwrap();
    assert!(wallet_exists(dir.clone()));
    let again = Wallet::create(dir.clone(), Network::Regtest, 0, false).map(|_| ());
    assert!(invalid(again).contains("already"));
    drop(alice);
    let alice = Wallet::open(dir, false).unwrap();
    assert_eq!((alice.network(), alice.tor()), (Network::Regtest, false));

    // No hub: nothing to scan with or move to.
    assert!(invalid(alice.scan(light.clone())).contains("no hub trusted serves"));
    assert!(invalid(alice.move_to_licash(light.clone())).contains("no hub trusted takes"));
    assert!(alice.collect().unwrap().is_empty());

    // Onion hubs need Tor; hubs down are unavailable.
    let onion = "http://licashhubv3licashhubv3licashhubv3licashhubv3licashhubqd.onion";
    assert!(matches!(
        alice.add_hub(onion.into()),
        Err(LicashError::NeedsTor)
    ));
    let down = alice.add_hub("http://127.0.0.1:1".into());
    assert!(
        matches!(down, Err(LicashError::Unavailable { .. })),
        "{down:?}"
    );
    assert!(alice.hubs().unwrap().is_empty());
    let hub = alice.add_hub(net.url.clone()).unwrap();
    assert!(hub.silent && !hub.mailbox);
    // A hub naming no relays: no mailbox to look in.
    assert!(alice.collect().unwrap().is_empty());

    // Nothing paid on Bitcoin yet: too little to move.
    match alice.move_to_licash(light.clone()) {
        Err(LicashError::Insufficient { needed, available }) => {
            assert_eq!(available, 0);
            assert!(needed > 1_100, "{needed}");
        }
        other => panic!("{other:?}"),
    }

    // Codes and payments it cannot take.
    assert!(invalid(alice.scanned("garbage".into())).contains("neither"));
    let elsewhere = Wallet::create(net.dir("mainnet"), Network::Bitcoin, 0, false).unwrap();
    let mainnet = elsewhere.payment_code(None).unwrap();
    assert!(invalid(alice.scanned(mainnet.text.clone())).contains("not on"));
    assert!(invalid(alice.pay(mainnet.text, Some(10))).contains("not on"));
    assert!(!invalid(alice.pay("garbage".into(), Some(10))).is_empty());
    let bob = net.wallet("bob", 0);
    let asking = bob.payment_code(Some(500)).unwrap();
    assert!(invalid(alice.pay(asking.text.clone(), Some(400))).contains("asks for 500, not 400"));
    let open = bob.payment_code(None).unwrap();
    assert!(invalid(alice.pay(open.text.clone(), None)).contains("does not say"));
    // No notes to pay with: the payee's Bitcoin address instead.
    let unfunded = invalid(alice.pay(open.text, Some(10)));
    assert!(unfunded.contains("on Bitcoin"), "{unfunded}");
    assert!(invalid(alice.receive("{}".into(), false)).contains("not a payment"));
    assert!(alice.history().unwrap().is_empty());

    // A move whose broadcast fails frees its coins to move again.
    let code = alice.payment_code(None).unwrap();
    let payment = net.pay_silent(&code.text, 3_000, 1);
    net.mine(1, vec![payment]);
    assert_eq!(scan(&alice, &light), (1, 3_000));
    light.failing.store(true, Ordering::SeqCst);
    let failed = alice.move_to_licash(light.clone());
    assert!(
        matches!(failed, Err(LicashError::Unavailable { .. })),
        "{failed:?}"
    );
    let onchain = Balance {
        onchain: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), onchain);
    assert!(net.sent().is_empty());
    light.failing.store(false, Ordering::SeqCst);
    alice.move_to_licash(light.clone()).unwrap();
    let moving = Balance {
        moving: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), moving);
    assert_eq!(net.sent().len(), 1);
}

#[test]
fn a_phone_looks_for_payments_from_its_birthday_or_all_the_hub_indexed() {
    let net = Net::new(false);
    let light = Light::new(&net);
    let born = net.height() + 2;
    let young = net.wallet("young", born);
    // Its birthday lost: everything the hub indexed is looked in.
    let unsure = net.wallet("unsure", born);
    std::fs::remove_file(net.dir.path().join("unsure/app.json")).unwrap();
    let pay = |n: u8| {
        let to_young = net.pay_silent(&young.payment_code(None).unwrap().text, 2_000, n);
        let to_unsure = net.pay_silent(&unsure.payment_code(None).unwrap().text, 2_000, n + 1);
        net.mine(1, vec![to_young, to_unsure]);
        net.height()
    };
    let before = pay(1);
    net.mine(1, Vec::new());
    let after = pay(3);
    assert!(before < born && born < after);
    assert_eq!(scan(&young, &light), (1, 2_000));
    assert_eq!(light.fetched(), [after]);
    assert_eq!(scan(&unsure, &light), (2, 4_000));
    assert_eq!(light.fetched(), [before, after]);
    // A scan goes on from where it stood.
    let next = pay(5);
    assert_eq!(scan(&young, &light), (1, 2_000));
    assert_eq!(light.fetched(), [next]);
}

#[test]
fn a_phone_paid_at_its_fallback_address_finds_it_and_shows_the_next() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = net.wallet("alice", net.height());
    let code = alice.payment_code(None).unwrap();
    let address = code.address.clone().expect("a fallback address");
    assert!(address.starts_with("bcrt1p"), "{address}");
    let uri = format!("bitcoin:{address}?sp=");
    assert!(code.text.starts_with(&uri), "{}", code.text);
    // The same until paid.
    assert_eq!(alice.payment_code(None).unwrap(), code);
    let payment = net.pay_plain(&address, 3_000, 1);
    net.mine(1, vec![payment]);
    assert_eq!(scan(&alice, &light), (1, 3_000));
    assert_eq!(alice.balance().unwrap().onchain, 3_000);
    let next = alice.payment_code(None).unwrap().address.unwrap();
    assert_ne!(next, address);
}

#[test]
fn a_phone_sees_payments_to_its_fallback_address_on_their_way() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = net.wallet("alice", net.height());
    // No code shown yet: nothing to watch.
    assert_eq!(alice.incoming(light.clone()).unwrap(), []);
    let address = alice.payment_code(None).unwrap().address.unwrap();
    let payment = net.pay_plain(&address, 3_000, 1);
    net.backend.lock().unwrap().mempool.push(payment.clone());
    let incoming = alice.incoming(light.clone()).unwrap();
    assert_eq!(incoming.len(), 1);
    let on_its_way = &incoming[0];
    assert_eq!(
        (on_its_way.kind, on_its_way.state, on_its_way.amount),
        (Kind::Onchain, State::Pending, 3_000)
    );
    assert!(on_its_way
        .id
        .starts_with(&payment.compute_txid().to_string()));
    // Not a coin until found in a block.
    assert_eq!(alice.balance().unwrap().onchain, 0);
    net.mine(1, vec![payment]);
    assert_eq!(scan(&alice, &light), (1, 3_000));
    assert_eq!(alice.incoming(light.clone()).unwrap(), []);
}
