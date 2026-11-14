//! The mobile wallet against a hub in this process, over HTTP: paid on
//! Bitcoin at its silent payment address, found through a light client,
//! moved to notes, paying another wallet in them through its mailbox or by
//! hand; and what it refuses.

#[path = "../../../hub/tests/chain/mod.rs"]
mod chain;
#[path = "../../../hub/tests/esplora/mod.rs"]
mod esplora;

use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc, Mutex,
};

use axum::response::IntoResponse;
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
    set_logger, wallet_exists, Activity, Balance, ChainError, ChainSource, Details, Kind,
    LicashError, Logger, Network, Onchain, Range, Scanned, State, Wallet,
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
    /// The hub answers 503 to everything while set.
    down: Arc<AtomicBool>,
    dir: tempfile::TempDir,
}

impl Net {
    fn new(relays: bool) -> Self {
        Self::with(relays, true)
    }

    /// The hub serving tweaks only if `tweaks`.
    fn with(relays: bool, tweaks: bool) -> Self {
        let serves_tweaks = tweaks;
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
            down: Arc::new(AtomicBool::new(false)),
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
            tweaks: serves_tweaks.then(|| net.tweaks.clone()),
            relays,
            ..Default::default()
        };
        let down = net.down.clone();
        let app = licash_hub_service::router(net.hub.clone(), services).layer(
            axum::middleware::from_fn(move |request, next: axum::middleware::Next| {
                let down = down.load(Ordering::SeqCst);
                async move {
                    if down {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    next.run(request).await
                }
            }),
        );
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
    /// Added to the times blocks claim.
    skew: AtomicI64,
}

impl Light {
    fn new(net: &Net) -> Arc<Self> {
        Arc::new(Self {
            backend: net.backend.clone(),
            fetched: Mutex::new(Vec::new()),
            failing: AtomicBool::new(false),
            skew: AtomicI64::new(0),
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

    /// Ten minutes apart, the tip's now: the times its headers claim are
    /// of 2011.
    fn time(&self, height: u32) -> Result<i64, ChainError> {
        let tip = self.backend.lock().unwrap().chain.height();
        if height > tip {
            return Err(ChainError::Failed {
                reason: format!("no block {height}"),
            });
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Ok(now - i64::from(tip - height) * 600 + self.skew.load(Ordering::SeqCst))
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

    // More than she holds: what is needed and held.
    let held = notes(&alice);
    let open = bob.payment_code(None).unwrap();
    match alice.pay(open.text, Some(held + 1)) {
        Err(LicashError::Insufficient { needed, available }) => {
            assert_eq!((needed, available), (held + 2, held));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(notes(&alice), held);

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

#[test]
fn a_phone_whose_hubs_serve_no_tweaks_finds_its_fallback_payments_from_the_day_before_it_was_made()
{
    let net = Net::with(false, false);
    let light = Light::new(&net);
    // Its birthday unknown.
    let alice = net.wallet("alice", 0);
    let made = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(!alice.hubs().unwrap()[0].silent);
    // No silent payment address, which no hub's tweaks would find paid.
    let code = alice.payment_code(None).unwrap();
    assert!(!code.text.contains("sp="), "{}", code.text);
    let address = code.address.unwrap();
    let early = net.pay_plain(&address, 1_000, 1);
    net.mine(1, vec![early]);
    let before = net.height();
    let late = net.pay_plain(&address, 6_000, 2);
    net.mine(1, vec![late]);
    // Block `before` claims a time just before the day before the wallet
    // was made, the next just after.
    let claimed = light.time(before).unwrap();
    let skew = made - 86_400 - 300 - claimed;
    light.skew.store(skew, Ordering::SeqCst);
    assert_eq!(scan(&alice, &light), (1, 6_000));
    assert_eq!(light.fetched(), [before + 1]);
    assert_eq!(alice.balance().unwrap().onchain, 6_000);

    // Moved to notes, more than the hub takes at once: the change found.
    let moving = alice.move_to_licash(light.clone()).unwrap();
    let sent = net.sent();
    let outputs: u64 = sent[0].output.iter().map(|o| o.value.to_sat()).sum();
    let change = outputs - (moving.amount + moving.fee);
    assert!(change > 0);
    net.confirm_sent();
    assert_eq!(scan(&alice, &light), (1, change));
    assert_eq!(alice.balance().unwrap().onchain, change);
}

#[test]
fn a_phone_born_before_its_hubs_tweaks_looks_in_each_block_once() {
    let net = Net::new(false);
    let light = Light::new(&net);
    // Born at block 1, the hub's tweaks from block 3 on: blocks 1 and 2
    // looked in without them.
    let alice = net.wallet("alice", 1);
    let code = alice.payment_code(None).unwrap();
    let silent = net.pay_silent(&code.text, 2_000, 1);
    net.mine(1, vec![silent]);
    let paid = net.height();
    let plain = net.pay_plain(&code.address.unwrap(), 3_000, 2);
    net.mine(1, vec![plain]);
    assert_eq!(scan(&alice, &light), (2, 5_000));
    assert_eq!(light.fetched(), [paid, paid + 1]);
    net.mine(1, Vec::new());
    assert_eq!(scan(&alice, &light), (0, 0));
    assert!(light.fetched().is_empty());
    assert_eq!(alice.balance().unwrap().onchain, 5_000);
}

/// A wallet `name` holding the notes moved in from a payment of 3000 to
/// its silent payment address.
fn funded(net: &Net, light: &Arc<Light>, name: &str) -> Arc<Wallet> {
    let wallet = net.wallet(name, net.height());
    let code = wallet.payment_code(None).unwrap();
    let payment = net.pay_silent(&code.qr, 3_000, 1);
    net.mine(1, vec![payment]);
    assert_eq!(scan(&wallet, light), (1, 3_000));
    wallet.move_to_licash(light.clone()).unwrap();
    net.confirm_sent();
    net.round();
    net.mine(1, Vec::new());
    net.round();
    wallet.resume().unwrap();
    assert!(notes(&wallet) > 0);
    wallet
}

#[test]
fn a_phone_paying_while_its_hub_is_down_sends_the_payment_to_the_mailbox_once_made() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = funded(&net, &light, "alice");
    let held = notes(&alice);
    let bob = net.wallet("bob", net.height());
    let asked = bob.payment_code(Some(500)).unwrap();

    // Its hub down: under way, and still so when resumed.
    net.down.store(true, Ordering::SeqCst);
    let paid = alice.pay(asked.qr.clone(), None);
    assert!(matches!(paid, Err(LicashError::Pending { .. })), "{paid:?}");
    alice.resume().unwrap();
    assert!(notes(&alice) < held);

    // Back up: made when resumed, and sent to Bob's mailbox, once.
    net.down.store(false, Ordering::SeqCst);
    alice.resume().unwrap();
    assert_eq!(notes(&alice), held - 501);
    let got = bob.collect().unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].kind, got[0].amount), (Kind::Received, 500));
    alice.resume().unwrap();
    assert!(bob.collect().unwrap().is_empty());
    assert_eq!(notes(&bob), 500);
    let kept = std::path::Path::new(&net.dir("alice")).join("deliveries.json");
    assert_eq!(std::fs::read_to_string(kept).unwrap(), "[]");

    // Paid with its hub up: sent at once, nothing kept to send later.
    let paid = alice.pay(asked.qr, None).unwrap();
    assert!(paid.delivered, "{paid:?}");
    alice.resume().unwrap();
    assert_eq!(bob.collect().unwrap().len(), 1);
    assert_eq!(notes(&bob), 1_000);
}

#[test]
fn a_phone_stopped_while_paying_sends_the_payment_to_the_mailbox_when_back() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = funded(&net, &light, "alice");
    let held = notes(&alice);
    let bob = net.wallet("bob", net.height());
    let asked = bob.payment_code(Some(500)).unwrap();
    net.down.store(true, Ordering::SeqCst);
    let paid = alice.pay(asked.qr, None);
    assert!(matches!(paid, Err(LicashError::Pending { .. })), "{paid:?}");

    // As if the app stopped before keeping which payment it was, and the
    // payment was made after, before the app sent it on.
    let dir = std::path::PathBuf::from(net.dir("alice"));
    let kept = dir.join("deliveries.json");
    let mut entries: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&kept).unwrap()).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].as_object_mut().unwrap().remove("send").is_some());
    std::fs::write(&kept, serde_json::to_vec(&entries).unwrap()).unwrap();
    net.down.store(false, Ordering::SeqCst);
    let http = licash_transport::Http::new(None).unwrap();
    let sdk = licash_wallet::Wallet::open(&dir.join("wallet.sqlite"), http).unwrap();
    let resumed = net.runtime.block_on(sdk.resume()).unwrap();
    assert_eq!(resumed.paid.len(), 1);
    drop(sdk);

    // Back: the payment made found, and sent to Bob's mailbox.
    alice.resume().unwrap();
    assert_eq!(notes(&alice), held - 501);
    let got = bob.collect().unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].kind, got[0].amount), (Kind::Received, 500));
    assert_eq!(std::fs::read_to_string(kept).unwrap(), "[]");
}

/// Alice's payments kept for mailboxes, each changed by `change`.
fn change_deliveries(net: &Net, change: impl Fn(&mut serde_json::Map<String, serde_json::Value>)) {
    let kept = std::path::Path::new(&net.dir("alice")).join("deliveries.json");
    let mut entries: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&kept).unwrap()).unwrap();
    for entry in &mut entries {
        change(entry.as_object_mut().unwrap());
    }
    std::fs::write(&kept, serde_json::to_vec(&entries).unwrap()).unwrap();
}

#[test]
fn a_phone_sends_a_payment_made_weeks_later_to_the_mailbox_still() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = funded(&net, &light, "alice");
    let bob = net.wallet("bob", net.height());
    let asked = bob.payment_code(Some(500)).unwrap();
    net.down.store(true, Ordering::SeqCst);
    let paid = alice.pay(asked.qr, None);
    assert!(matches!(paid, Err(LicashError::Pending { .. })), "{paid:?}");
    // Paid 15 days ago, its hub down since.
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 15 * 86_400;
    change_deliveries(&net, |entry| {
        entry.insert("at".into(), ago.into());
    });
    net.down.store(false, Ordering::SeqCst);
    alice.resume().unwrap();
    let got = bob.collect().unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].amount, 500);
}

#[test]
fn a_phone_stopped_twice_while_paying_the_same_sends_both_to_the_mailbox() {
    let net = Net::new(true);
    let light = Light::new(&net);
    let alice = funded(&net, &light, "alice");
    let held = notes(&alice);
    let bob = net.wallet("bob", net.height());
    let asked = bob.payment_code(Some(500)).unwrap();
    net.down.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let paid = alice.pay(asked.qr.clone(), None);
        assert!(matches!(paid, Err(LicashError::Pending { .. })), "{paid:?}");
        // A second apart: the second kept after the first was made.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
    }
    // As if the app stopped each time before keeping which payment it was.
    change_deliveries(&net, |entry| {
        assert!(entry.remove("send").is_some());
    });
    net.down.store(false, Ordering::SeqCst);
    alice.resume().unwrap();
    assert_eq!(notes(&alice), held - 2 * 501);
    let got = bob.collect().unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(notes(&bob), 1_000);
}

/// HTTP to a hub, losing every request that posts.
struct Lossy(licash_transport::Http);

impl licash_wallet::Transport for Lossy {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<licash_wallet::Response, String>> + Send {
        let lost = body.is_some();
        let answer = self.0.request(url, body);
        async move {
            match lost {
                true => Err("connection reset".into()),
                false => answer.await,
            }
        }
    }
}

#[test]
fn a_phone_keeps_no_move_its_hub_never_answered_and_gives_up_those_kept_before() {
    let net = Net::new(false);
    let light = Light::new(&net);
    let alice = net.wallet("alice", 0);
    let code = alice.payment_code(None).unwrap();
    net.mine(1, vec![net.pay_silent(&code.text, 3_000, 1)]);
    assert_eq!(scan(&alice, &light), (1, 3_000));
    let moves = |wallet: &Wallet| {
        let history = wallet.history().unwrap();
        history.iter().filter(|a| a.kind == Kind::Moved).count()
    };

    // Its hub down: no move kept, however often asked, its coins still on
    // Bitcoin.
    net.down.store(true, Ordering::SeqCst);
    for _ in 0..3 {
        let moved = alice.move_to_licash(light.clone());
        assert!(
            matches!(moved, Err(LicashError::Unavailable { .. })),
            "{moved:?}"
        );
        alice.resume().unwrap();
    }
    net.down.store(false, Ordering::SeqCst);
    assert_eq!(moves(&alice), 0);
    let onchain = Balance {
        onchain: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), onchain);
    drop(alice);

    // Moves kept unanswered, as earlier versions did: given up on resuming,
    // never shown again, and the coins moved anew.
    let path = std::path::Path::new(&net.dir("alice")).join("wallet.sqlite");
    let http = licash_transport::Http::new(None).unwrap();
    let earlier = licash_wallet::Wallet::open(&path, Lossy(http)).unwrap();
    for _ in 0..2 {
        let kept = net.runtime.block_on(earlier.deposit(&net.url, 2_000));
        assert!(
            matches!(kept, Err(licash_wallet::Error::Pending { .. })),
            "{kept:?}"
        );
    }
    drop(earlier);
    let alice = Wallet::open(net.dir("alice"), false).unwrap();
    assert_eq!(moves(&alice), 2);
    alice.resume().unwrap();
    assert_eq!(moves(&alice), 0);
    alice.move_to_licash(light.clone()).unwrap();
    assert_eq!(moves(&alice), 1);
    let moving = Balance {
        moving: 3_000,
        ..Balance::default()
    };
    assert_eq!(alice.balance().unwrap(), moving);
}

#[test]
fn a_phone_reaches_a_hub_named_by_its_host_alone_over_https() {
    let net = Net::new(false);
    let alice = Wallet::create(net.dir("alice"), Network::Regtest, 0, false).unwrap();
    // Not over plain HTTP, which the hub here serves on its port.
    let host = net.url.strip_prefix("http://").unwrap();
    let secure = alice.add_hub(host.into());
    assert!(
        matches!(secure, Err(LicashError::Unavailable { .. })),
        "{secure:?}"
    );
    assert!(alice.hubs().unwrap().is_empty());
    alice.add_hub(net.url.clone()).unwrap();
    assert_eq!(alice.hubs().unwrap()[0].url, net.url);
}

#[test]
fn a_phone_shows_what_a_move_cost_and_how_deep_its_payment_is() {
    let net = Net::new(false);
    let light = Light::new(&net);
    let alice = net.wallet("alice", net.height());
    let url = alice.hubs().unwrap()[0].url.clone();
    let code = alice.payment_code(None).unwrap();
    net.mine(2, Vec::new());
    let paying = net.pay_silent(&code.qr, 3_000, 1);
    net.mine(1, vec![paying.clone()]);
    let paid_in = net.height();
    net.mine(1, Vec::new());
    scan(&alice, &light);
    let id = OutPoint::new(paying.compute_txid(), 0).to_string();
    let found = Onchain {
        txid: paying.compute_txid().to_string(),
        fee: None,
        height: Some(paid_in),
        confirmations: 2,
    };
    let coin = Details::Coin {
        tx: found.clone(),
        vout: 0,
        change: false,
        moving: None,
    };
    assert_eq!(alice.details(Kind::Onchain, id.clone()).unwrap(), coin);

    // Moved: the hub's fee and the miners', and its payment in no block.
    let moving = alice.move_to_licash(light.clone()).unwrap();
    let sent = net.sent().remove(0);
    let txid = sent.compute_txid().to_string();
    let coin = Details::Coin {
        tx: found,
        vout: 0,
        change: false,
        moving: Some(txid.clone()),
    };
    assert_eq!(alice.details(Kind::Onchain, id).unwrap(), coin);
    let paid = |confirmations, height| Onchain {
        txid: txid.clone(),
        fee: Some(3_000 - moving.amount - 100),
        height,
        confirmations,
    };
    let moved = |payment| match alice.details(Kind::Moved, moving.id.clone()).unwrap() {
        Details::Moved {
            hub,
            hash,
            pays,
            address,
            payment: shown,
            depth,
            fund_by,
            timeout,
            refund,
        } => {
            assert_eq!((hub, hash), (url.clone(), moving.id.clone()));
            assert_eq!(pays, Some(moving.amount + 100));
            let address: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
                address.unwrap().parse().unwrap();
            let script = address.assume_checked().script_pubkey();
            assert_eq!(script, sent.output[0].script_pubkey);
            assert_eq!(depth, Some(2));
            assert!(fund_by.unwrap() < timeout.unwrap());
            assert_eq!(refund, None);
            assert_eq!(shown, Some(payment));
        }
        other => panic!("{other:?}"),
    };
    moved(paid(0, None));

    // In a block once the phone looked in it, deeper with each block.
    net.confirm_sent();
    let confirmed = net.height();
    moved(paid(0, None));
    scan(&alice, &light);
    moved(paid(1, Some(confirmed)));
    net.mine(1, Vec::new());
    scan(&alice, &light);
    moved(paid(2, Some(confirmed)));
    net.round();
    alice.resume().unwrap();
    let history = alice.history().unwrap();
    assert_eq!(
        (history[0].kind, history[0].state),
        (Kind::Moved, State::Done)
    );
    moved(paid(2, Some(confirmed)));

    let none = invalid(alice.details(Kind::Moved, "00".repeat(32)));
    assert!(none.contains("nothing in the history"), "{none}");
    assert!(invalid(alice.details(Kind::Onchain, "no".into())).contains("nothing"));
    assert!(invalid(alice.details(Kind::Sent, "no".into())).contains("nothing"));
}
