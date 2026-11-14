//! Deposits on Bitcoin over a mock Esplora, in process: wallets pay them,
//! claim the hub's notes and the hub claims the coins; payments not as
//! asked, late or reorganized away lock nothing, and return to the payer.

mod chain;
mod esplora;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::{
    body::Body,
    http::{header, Method, Request},
    Router,
};
use bitcoin::{
    absolute::LockTime,
    hashes::Hash,
    key::TweakedPublicKey,
    secp256k1::{Secp256k1, SecretKey},
    transaction, Address, Amount, FeeRate, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Txid, Witness,
};
use http_body_util::BodyExt;
use licash::spend::ChainProfile;
use licash_api::{Code, Fee, Info, Problem, ReceiveState};
use licash_chain_client::Source;
use licash_hub::{Hub, Policy, Timing};
use licash_hub_service::{deposit, Services};
use licash_wallet::{DepositState, Transport, Wallet};
use serde_json::json;
use tower::ServiceExt;

const HUB: &str = "http://hub.test";

/// Short, to expire locks in a test.
const TIMING: Timing = Timing {
    window: 40,
    reaction: 2,
    min_depth: 1,
    max_headers: 4032,
    bury: 6,
    max_lag: 120,
};

/// Where the hub's claims pay.
fn sweep_to() -> ScriptBuf {
    let key = SecretKey::from_slice(&[9; 32]).unwrap();
    let key = key.x_only_public_key(&Secp256k1::new()).0;
    ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(key))
}

fn config() -> deposit::Config {
    deposit::Config {
        fee: Fee { base: 100, ppm: 0 },
        min: 1_000,
        max: 4_000,
        depth: 2,
        window: 10,
        lock_blocks: 6,
        downtime: 2,
        sweep_blocks: 6,
        min_fee_rate: FeeRate::from_sat_per_vb_u32(1),
        ..deposit::Config::new(sweep_to())
    }
}

/// Its window, depth, lock blocks, the lock's depth, downtime and sweep
/// blocks.
const TIMEOUT: u32 = 10 + 2 + 6 + 1 + 2 + 6;

/// A hub taking deposits, it and its gateway following one chain.
struct Net {
    hub: Arc<Hub>,
    deposits: Arc<deposit::Gateway>,
    source: Source,
    app: Router,
    backend: esplora::Shared,
    dir: tempfile::TempDir,
}

impl Net {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, 12).unwrap();
        let policy = Policy {
            timing: TIMING,
            ..Policy::default()
        };
        let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
        let chain = chain::Chain::new();
        hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
        let backend = esplora::backend(chain);
        let source: Source = esplora::serve(backend.clone()).parse().unwrap();
        let deposits = Arc::new(gateway(&hub, &source, &dir.path().join("deposits.sqlite")));
        let services = Services {
            deposits: Some(deposits.clone()),
            ..Default::default()
        };
        let net = Self {
            app: licash_hub_service::router(hub.clone(), services),
            hub,
            deposits,
            source,
            backend,
            dir,
        };
        net.mine(3, Vec::new());
        // Scanning from the tip on.
        net.deposits.round().await;
        net
    }

    /// Mine `blocks`, the first with `txs`, which the hub scans.
    fn mine(&self, blocks: u32, mut txs: Vec<Transaction>) {
        let mut backend = self.backend.lock().unwrap();
        for _ in 0..blocks {
            let txs = std::mem::take(&mut txs);
            let ids: Vec<Txid> = txs.iter().map(Transaction::compute_txid).collect();
            backend.mempool.retain(|t| !ids.contains(&t.compute_txid()));
            let block = backend.chain.mine_with(None, txs).clone();
            self.hub.observe(backend.chain.height(), &block).unwrap();
        }
        let (height, tip) = (backend.chain.height(), backend.chain.tip());
        assert!(self.hub.synced(height, &tip).unwrap());
    }

    /// Mine what was sent, in one block.
    fn confirm_sent(&self) -> Vec<Transaction> {
        let sent = std::mem::take(&mut self.backend.lock().unwrap().mempool);
        self.mine(1, sent.clone());
        sent
    }

    /// Leave the blocks from `height` on, for others mined after.
    fn fork(&self, height: u32) {
        let mut backend = self.backend.lock().unwrap();
        backend.chain.blocks.truncate(height as usize);
        self.hub.rewind(height).unwrap();
    }

    fn height(&self) -> u32 {
        self.backend.lock().unwrap().chain.height()
    }

    fn sent(&self) -> Vec<Transaction> {
        self.backend.lock().unwrap().mempool.clone()
    }

    fn wallet(&self, name: &str) -> Wallet<Local> {
        let path = self.dir.path().join(format!("{name}.sqlite"));
        let mut seed = [0; 32];
        seed[..name.len()].copy_from_slice(name.as_bytes());
        let network = ChainProfile::Core30DefaultRegtest;
        Wallet::create(&path, &seed, network, Local(self.app.clone())).unwrap()
    }

    /// What the wallet `name` recorded of its deposit of `hash`: the
    /// preimage and the body it asked with.
    fn asked(&self, name: &str, hash: &str) -> ([u8; 32], licash_api::Deposit) {
        let path = self.dir.path().join(format!("{name}.sqlite"));
        let db = rusqlite::Connection::open(path).unwrap();
        let (preimage, body): ([u8; 32], String) = db
            .query_row(
                "SELECT preimage, body FROM deposits WHERE hash = ?1",
                [bytes(hash)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        (preimage, serde_json::from_str(&body).unwrap())
    }

    async fn info(&self) -> Info {
        let response = Local(self.app.clone())
            .request(&format!("{HUB}{}", licash_api::INFO), None)
            .await
            .unwrap();
        serde_json::from_slice(&response.body).unwrap()
    }
}

/// A gateway of `hub`'s deposits over `source`, kept at `path`.
fn gateway(hub: &Arc<Hub>, source: &Source, path: &std::path::Path) -> deposit::Gateway {
    let profile = ChainProfile::Core30DefaultRegtest;
    deposit::Gateway::open(hub.clone(), source.clone(), profile, config(), path).unwrap()
}

/// A payment of `value` to `address`, from coins of the `n`th payer.
fn payment(address: &str, value: u64, n: u8) -> Transaction {
    let address: Address<_> = address.parse().unwrap();
    Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([n; 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: address.assume_checked().script_pubkey(),
        }],
    }
}

fn bytes(hash: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hash, &mut bytes).unwrap();
    bytes
}

fn available(wallet: &Wallet<Local>) -> u64 {
    wallet.balance().unwrap().iter().map(|b| b.available).sum()
}

fn alerts(hub: &Hub) -> Vec<String> {
    let events = hub.events(0, 1000).unwrap();
    let alerts = events.into_iter().filter(|e| e.kind == "alert");
    alerts.map(|e| e.data.to_string()).collect()
}

/// The service's router, reached at [`HUB`].
#[derive(Clone)]
struct Local(Router);

impl Transport for Local {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<licash_wallet::Response, String>> + Send {
        let path = url.strip_prefix(HUB).unwrap_or(url).to_owned();
        let app = self.0.clone();
        async move {
            let request = match body {
                Some(body) => Request::builder()
                    .method(Method::POST)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body)),
                None => Request::builder().uri(path).body(Body::empty()),
            };
            let response = app.oneshot(request.unwrap()).await.unwrap();
            let status = response.status().as_u16();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            Ok(licash_wallet::Response {
                status,
                body: body.to_vec(),
            })
        }
    }
}

#[tokio::test]
async fn a_deposit_paid_at_depth_is_received_and_its_coins_claimed() {
    let net = Net::new().await;
    let asked_at = net.height();
    let deposits = net.info().await.deposits.unwrap();
    assert_eq!(
        (deposits.fee, deposits.min, deposits.max),
        (Fee { base: 100, ppm: 0 }, 1_000, 4_000)
    );
    assert_eq!(
        (deposits.depth, deposits.window, deposits.timeout),
        (2, 10, TIMEOUT)
    );
    let alice = net.wallet("alice");
    // Outside what the hub takes, refused before asking.
    for amount in [999, 4_001] {
        let refused = alice.deposit(HUB, amount).await;
        assert!(
            matches!(refused, Err(licash_wallet::Error::Invalid(_))),
            "{refused:?}"
        );
    }
    let deposit = alice.deposit(HUB, 3_000).await.unwrap();
    assert_eq!(deposit.state, DepositState::Open);
    let terms = deposit.terms.clone().unwrap();
    assert!(terms.address.starts_with("bcrt1p"), "{}", terms.address);
    assert_eq!(terms.pays, 3_100);
    assert_eq!(
        (terms.fund_by, terms.timeout),
        (asked_at + 10, asked_at + TIMEOUT)
    );
    // The same deposit asked again: the same terms; another under its
    // hash, refused.
    let (preimage, body) = net.asked("alice", &deposit.hash);
    let again = net.deposits.deposit(&body).await.unwrap();
    assert_eq!(
        (again.address.as_str(), again.amount),
        (terms.address.as_str(), 3_100)
    );
    let mut other = body.clone();
    other.refund = licash_api::Hex(vec![2; 32]);
    let refused = net.deposits.deposit(&other).await.unwrap_err();
    assert_eq!(refused.0.code, Code::Forward);

    // Paid: seen, but not at depth.
    net.mine(1, vec![payment(&terms.address, 3_100, 1)]);
    net.deposits.round().await;
    let paid = alice.follow_deposit(&deposit.hash).await.unwrap();
    assert_eq!(paid.state, DepositState::Open);
    let (funding, height) = paid.funding.unwrap();
    assert_eq!(height, asked_at + 1);
    assert_eq!(available(&alice), 0);
    // At depth: the hub's notes, claimed.
    net.mine(1, Vec::new());
    net.deposits.round().await;
    let hash = bytes(&deposit.hash);
    assert_eq!(
        net.deposits.deposit_status(&hash).unwrap().status.state,
        ReceiveState::Locked
    );
    let received = alice.follow_deposit(&deposit.hash).await.unwrap();
    assert_eq!(received.state, DepositState::Received);
    assert_eq!(available(&alice), 3_000);
    // Claimed, its coins are the hub's.
    let refund = alice.refund_deposit(
        &deposit.hash,
        funding,
        Amount::from_sat(3_100),
        sweep_to(),
        FeeRate::from_sat_per_vb_u32(1),
    );
    assert!(
        matches!(refund, Err(licash_wallet::Error::Invalid(_))),
        "{refund:?}"
    );

    // The hub settles, then claims the coins with the preimage, at the
    // rate the chain estimates.
    net.deposits.round().await;
    assert_eq!(
        net.deposits.deposit_status(&hash).unwrap().status.state,
        ReceiveState::Settled
    );
    net.deposits.round().await;
    let sent = net.sent();
    assert_eq!(sent.len(), 1);
    let claim = &sent[0];
    assert_eq!(claim.input[0].previous_output, funding);
    assert_eq!(claim.input[0].witness.nth(1).unwrap(), preimage);
    assert_eq!(claim.output[0].script_pubkey, sweep_to());
    let fee = 3_100 - claim.output[0].value.to_sat();
    assert_eq!(
        FeeRate::from_sat_per_vb_u32(3)
            .fee_wu(claim.weight())
            .unwrap()
            .to_sat(),
        fee
    );
    // Sent again each round until confirmed, the same.
    net.deposits.round().await;
    assert_eq!(net.sent(), sent);
    net.confirm_sent();
    net.deposits.round().await;
    net.deposits.round().await;
    assert!(net.sent().is_empty());
    assert_eq!(
        net.deposits.deposit_status(&hash).unwrap().status.state,
        ReceiveState::Settled
    );
    assert!(alerts(&net.hub).is_empty(), "{:?}", alerts(&net.hub));
    let resumed = alice.resume().await.unwrap();
    assert!(resumed.deposits.is_empty() && resumed.pending.is_empty());
    assert_eq!(alice.deposits().unwrap(), vec![received]);
}

#[tokio::test]
async fn deposits_paid_wrong_late_or_never_lock_nothing_and_are_refunded() {
    let net = Net::new().await;
    let alice = net.wallet("alice");
    let short = alice.deposit(HUB, 2_000).await.unwrap();
    let never = alice.deposit(HUB, 2_000).await.unwrap();
    let late = alice.deposit(HUB, 2_000).await.unwrap();
    let address = |d: &licash_wallet::Deposit| d.terms.clone().unwrap().address;
    let underpaid = payment(&address(&short), 2_099, 1);
    net.mine(1, vec![underpaid.clone()]);
    // Paid one block after the last it may confirm in.
    let fund_by = late.terms.clone().unwrap().fund_by;
    net.mine(fund_by - net.height(), Vec::new());
    net.deposits.round().await;
    net.mine(1, vec![payment(&address(&late), 2_100, 2)]);
    net.mine(2, Vec::new());
    net.deposits.round().await;
    for deposit in [&short, &never, &late] {
        let hash = bytes(&deposit.hash);
        let status = net.deposits.deposit_status(&hash).unwrap();
        assert_eq!(
            status.status.state,
            ReceiveState::Canceled,
            "{}",
            deposit.hash
        );
        assert_eq!(status.funding, None);
        let followed = alice.follow_deposit(&deposit.hash).await.unwrap();
        assert_eq!(
            followed.state,
            DepositState::Canceled("canceled by its hub".into())
        );
    }
    assert_eq!(available(&alice), 0);
    assert!(alice.resume().await.unwrap().deposits.is_empty());

    // Taken back after the timeout, again at a higher rate.
    let funding = OutPoint {
        txid: underpaid.compute_txid(),
        vout: 0,
    };
    let to = payment(&address(&never), 0, 0).output[0]
        .script_pubkey
        .clone();
    let refund = |rate| {
        let value = Amount::from_sat(2_099);
        alice.refund_deposit(
            &short.hash,
            funding,
            value,
            to.clone(),
            FeeRate::from_sat_per_vb_u32(rate),
        )
    };
    let first = refund(1).unwrap();
    let timeout = short.terms.clone().unwrap().timeout;
    assert_eq!(first.lock_time.to_consensus_u32(), timeout);
    assert_eq!(first.input[0].previous_output, funding);
    assert_eq!(first.output[0].script_pubkey, to);
    assert_eq!(first.input[0].witness.len(), 3);
    let state = alice.deposit_of(&short.hash).unwrap().state;
    assert_eq!(state, DepositState::Refunded(first.compute_txid()));
    let second = refund(5).unwrap();
    assert!(second.output[0].value < first.output[0].value);
    let state = alice.deposit_of(&short.hash).unwrap().state;
    assert_eq!(state, DepositState::Refunded(second.compute_txid()));
    // Too small to pay its fee.
    let dust = alice.refund_deposit(
        &never.hash,
        funding,
        Amount::from_sat(400),
        to,
        FeeRate::from_sat_per_vb_u32(5),
    );
    assert!(
        matches!(dust, Err(licash_wallet::Error::Invalid(_))),
        "{dust:?}"
    );
}

#[tokio::test]
async fn a_payment_reorganized_away_locks_nothing_and_one_locked_alerts() {
    let net = Net::new().await;
    let alice = net.wallet("alice");
    let deposit = alice.deposit(HUB, 3_000).await.unwrap();
    let hash = bytes(&deposit.hash);
    let address = deposit.terms.clone().unwrap().address;
    let paid_at = net.height() + 1;
    net.mine(1, vec![payment(&address, 3_100, 1)]);
    net.deposits.round().await;
    assert!(net
        .deposits
        .deposit_status(&hash)
        .unwrap()
        .funding
        .is_some());
    // Its block left the best chain, for two others: forgotten.
    net.fork(paid_at);
    net.mine(2, Vec::new());
    net.deposits.round().await;
    let status = net.deposits.deposit_status(&hash).unwrap();
    assert_eq!(
        (status.status.state, status.funding),
        (ReceiveState::Open, None)
    );
    // Paid again on the new chain, at depth: locked, then received.
    let paid_at = net.height() + 1;
    net.mine(1, vec![payment(&address, 3_100, 1)]);
    net.mine(1, Vec::new());
    net.deposits.round().await;
    let funding = net.deposits.deposit_status(&hash).unwrap().funding.unwrap();
    assert_eq!(funding.height, paid_at);
    assert_eq!(
        alice.follow_deposit(&deposit.hash).await.unwrap().state,
        DepositState::Received
    );
    assert!(alerts(&net.hub).is_empty());
    // Its payment reorganized away once its notes are out: alerted, once.
    net.fork(paid_at);
    net.mine(3, Vec::new());
    net.deposits.round().await;
    net.deposits.round().await;
    let alerts = alerts(&net.hub);
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("left the best chain"), "{alerts:?}");
}

async fn call(net: &Net, method: Method, path: &str, body: Option<String>) -> (u16, Problem) {
    let request = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body)),
        None => request.body(Body::empty()),
    };
    let response = net.app.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn deposits_outside_the_hubs_terms_are_refused() {
    let net = Net::new().await;
    let alice = net.wallet("alice");
    let deposit = alice.deposit(HUB, 3_000).await.unwrap();
    let (_, body) = net.asked("alice", &deposit.hash);
    // Of more than another gateway takes.
    let path = net.dir.path().join("other.sqlite");
    let mut other = config();
    other.max = 2_000;
    let profile = ChainProfile::Core30DefaultRegtest;
    let smaller =
        deposit::Gateway::open(net.hub.clone(), net.source.clone(), profile, other, &path).unwrap();
    assert_eq!(
        smaller.deposit(&body).await.unwrap_err().0.code,
        Code::Forward
    );
    // Not a key to refund with, or of no keyset of the hub's.
    let mut wrong = body.clone();
    wrong.refund = licash_api::Hex(vec![0xff; 32]);
    assert_eq!(
        net.deposits.deposit(&wrong).await.unwrap_err().0.code,
        Code::Invalid
    );
    let mut wire = serde_json::to_value(&body).unwrap();
    wire["keyset"] = json!(hex::encode([7; 32]));
    let (status, problem) = call(
        &net,
        Method::POST,
        licash_api::DEPOSIT,
        Some(wire.to_string()),
    )
    .await;
    assert_eq!(
        (status, problem.code),
        (problem.code.status(), Code::UnknownKeyset)
    );
    let (status, problem) = call(&net, Method::POST, licash_api::DEPOSIT, Some("{}".into())).await;
    assert_eq!((status, problem.code), (400, Code::Invalid));
    let unknown = licash_api::deposit_state(&[3; 32]);
    let (status, problem) = call(&net, Method::GET, &unknown, None).await;
    assert_eq!(
        (status, problem.code),
        (problem.code.status(), Code::UnknownLock)
    );
}

#[tokio::test]
async fn deposits_are_kept_in_a_file_of_their_own_and_timed_within_the_hubs_locks() {
    let net = Net::new().await;
    let profile = ChainProfile::Core30DefaultRegtest;
    let open = |config, path: &std::path::Path| {
        deposit::Gateway::open(net.hub.clone(), net.source.clone(), profile, config, path)
            .err()
            .map(|e| e.to_string())
    };
    // Not a file of deposits, nor of a schema this hub reads.
    let foreign = net.dir.path().join("foreign.sqlite");
    let db = rusqlite::Connection::open(&foreign).unwrap();
    db.execute_batch("CREATE TABLE t (x)").unwrap();
    assert!(open(config(), &foreign)
        .unwrap()
        .contains("not the hub's deposits"));
    let path = net.dir.path().join("old.sqlite");
    assert_eq!(open(config(), &path), None);
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 2)
        .unwrap();
    assert!(open(config(), &path).unwrap().contains("deposits schema 2"));
    // A lock lasting beyond the window of the hub's, with its depth and
    // reaction; nothing locked; no confirmation asked; fewer than none.
    let fresh = net.dir.path().join("fresh.sqlite");
    let long = deposit::Config {
        lock_blocks: 38,
        ..config()
    };
    assert!(open(long, &fresh)
        .unwrap()
        .contains("within their window of 40"));
    let none = deposit::Config {
        lock_blocks: 0,
        ..config()
    };
    assert!(open(none, &fresh).is_some());
    let shallow = deposit::Config {
        depth: 0,
        ..config()
    };
    assert!(open(shallow, &fresh).unwrap().contains("positive"));
    let empty = deposit::Config {
        min: 5_000,
        ..config()
    };
    assert!(open(empty, &fresh).unwrap().contains("at most the largest"));
    assert!(!fresh.exists());
    let fits = deposit::Config {
        lock_blocks: 37,
        ..config()
    };
    assert_eq!(open(fits, &fresh), None);
}

/// The mock's chain as a wallet's light client sees it, filters aside.
struct Light(esplora::Shared);

impl licash_wallet::Blocks for Light {
    fn height(&self) -> impl std::future::Future<Output = Result<u32, String>> + Send {
        std::future::ready(Ok(self.0.lock().unwrap().chain.height()))
    }

    fn hash(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = Result<bitcoin::BlockHash, String>> + Send {
        let blocks = &self.0.lock().unwrap().chain.blocks;
        let hash = blocks.get(height as usize).map(|b| b.block_hash());
        std::future::ready(hash.ok_or_else(|| format!("no block {height}")))
    }

    fn filter(
        &self,
        _: &bitcoin::BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, String>> + Send {
        std::future::ready(Ok(None))
    }

    fn block(
        &self,
        hash: &bitcoin::BlockHash,
    ) -> impl std::future::Future<Output = Result<bitcoin::Block, String>> + Send {
        let blocks = &self.0.lock().unwrap().chain.blocks;
        let block = blocks.iter().find(|b| b.block_hash() == *hash).cloned();
        std::future::ready(block.ok_or_else(|| format!("no block {hash}")))
    }
}

/// A silent payment of `value` to `to`, from a P2WPKH coin of the `n`th
/// payer that the mock knows the script of.
fn pay_silent(net: &Net, to: &licash_onchain::silent::SpAddress, value: u64, n: u8) -> Transaction {
    let secp = Secp256k1::new();
    let sender = SecretKey::from_slice(&[n; 32]).unwrap();
    let key = bitcoin::CompressedPublicKey(sender.public_key(&secp));
    let previous_output = OutPoint {
        txid: Txid::from_byte_array([n; 32]),
        vout: 0,
    };
    let spent = ScriptBuf::new_p2wpkh(&key.wpubkey_hash());
    let backend = &mut net.backend.lock().unwrap();
    backend.outside.insert(previous_output, spent);
    let paid = licash_onchain::silent::outputs(&[(sender, false)], &[previous_output], &[*to]);
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

/// What `tx` spends of the coins `paid`, less what it pays: its fee.
fn fee_of(tx: &Transaction, paid: u64) -> u64 {
    paid - tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>()
}

#[tokio::test]
async fn silent_payment_coins_are_deposited_whole_up_to_the_most_a_hub_takes() {
    use licash_hub_service::tweaks::{step, Tweaks};
    let net = Net::new().await;
    let index = Arc::new(Tweaks::open(&net.dir.path().join("tweaks.sqlite")).unwrap());
    step(&index, &net.source, None).await.unwrap();
    let services = Services {
        deposits: Some(net.deposits.clone()),
        tweaks: Some(index.clone()),
        ..Default::default()
    };
    let app = licash_hub_service::router(net.hub.clone(), services);
    let path = net.dir.path().join("alice.sqlite");
    let network = ChainProfile::Core30DefaultRegtest;
    let alice = Wallet::create(&path, &[1; 32], network, Local(app)).unwrap();
    alice.add_hub(HUB).await.unwrap();
    alice
        .watch_silent(HUB, index.first().unwrap().unwrap())
        .unwrap();
    let to = alice.silent_keys().address();
    let rate = FeeRate::from_sat_per_vb_u32(1);
    let light = Light(net.backend.clone());
    let paid = |value: u64, n: u8| {
        let payment = pay_silent(&net, &to, value, n);
        net.mine(1, vec![payment]);
    };
    let scan = || async {
        step(&index, &net.source, None).await.unwrap();
        alice.scan_silent(HUB, &light, u32::MAX).await.unwrap()
    };
    let insufficient = |e: licash_wallet::Error| match e {
        licash_wallet::Error::Insufficient { needed, available } => (needed, available),
        e => panic!("{e:?}"),
    };

    // Nothing to deposit, or too little for the least the hub takes: no
    // deposit asked.
    let none = alice.deposit_silent(HUB, rate).await.unwrap_err();
    let (needed, held) = insufficient(none);
    assert_eq!(held, 0);
    assert!(needed > 1_100, "{needed}");
    paid(1_000, 2);
    assert_eq!(scan().await.found.len(), 1);
    let (needed, held) = insufficient(alice.deposit_silent(HUB, rate).await.unwrap_err());
    assert_eq!(held, 1_000);
    assert!(needed > 1_100, "{needed}");
    assert!(alice.deposits().unwrap().is_empty());

    // Enough with another coin: both deposited, all of them but the fee.
    paid(2_600, 3);
    assert_eq!(scan().await.found.len(), 1);
    let (deposit, tx) = alice.deposit_silent(HUB, rate).await.unwrap();
    let terms = deposit.terms.clone().unwrap();
    assert_eq!(deposit.state, DepositState::Open);
    assert_eq!(terms.pays, deposit.amount + 100);
    assert_eq!((tx.input.len(), tx.output.len()), (2, 1));
    let address: Address<_> = terms.address.parse().unwrap();
    assert_eq!(
        tx.output[0].script_pubkey,
        address.assume_checked().script_pubkey()
    );
    assert_eq!(tx.output[0].value.to_sat(), terms.pays);
    assert_eq!(fee_of(&tx, 3_600), tx.vsize() as u64);
    // Its coins taken: nothing more to deposit.
    let (_, held) = insufficient(alice.deposit_silent(HUB, rate).await.unwrap_err());
    assert_eq!(held, 0);
    assert_eq!(alice.deposits().unwrap().len(), 1);

    // Broadcast, confirmed at depth: the notes.
    net.backend.lock().unwrap().mempool.push(tx.clone());
    net.confirm_sent();
    net.deposits.round().await;
    net.mine(1, Vec::new());
    net.deposits.round().await;
    let received = alice.follow_deposit(&deposit.hash).await.unwrap();
    assert_eq!(received.state, DepositState::Received);
    assert_eq!(available(&alice), deposit.amount);
    // Its notes received under its lock.
    assert!(deposit.lock.is_none());
    let receipts = alice.receipts().unwrap();
    assert_eq!(received.lock.as_ref(), Some(&receipts[0].id));
    let scanned = scan().await;
    assert_eq!(scanned.spent.len(), 2);
    assert!(alice.silent_coins().unwrap().is_empty());

    // More than the hub takes: its most, the rest back as change.
    paid(10_000, 4);
    scan().await;
    let (most, tx) = alice.deposit_silent(HUB, rate).await.unwrap();
    assert_eq!((most.amount, most.terms.unwrap().pays), (4_000, 4_100));
    assert_eq!(tx.output.len(), 2);
    let change: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>() - 4_100;
    net.backend.lock().unwrap().mempool.push(tx.clone());
    net.confirm_sent();
    scan().await;
    let coins = alice.silent_coins().unwrap();
    assert_eq!(coins.len(), 1);
    assert!(coins[0].change && coins[0].spending.is_none());
    assert_eq!(coins[0].value, change);
    assert_eq!(fee_of(&tx, 10_000), 10_000 - 4_100 - change);
}

/// The hub behind a front refusing what is posted while `.1` is set, with
/// a plain 400, as a server only redirecting to HTTPS may.
struct Front(Local, Arc<AtomicBool>);

impl Transport for Front {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<licash_wallet::Response, String>> + Send {
        let refused = body.is_some() && self.1.load(Ordering::SeqCst);
        let answer = self.0.request(url, body);
        async move {
            if refused {
                let body = b"Bad Request\n".to_vec();
                return Ok(licash_wallet::Response { status: 400, body });
            }
            answer.await
        }
    }
}

#[tokio::test]
async fn a_deposit_its_hub_never_answered_is_abandoned_and_a_silent_one_never_kept() {
    use licash_hub_service::tweaks::{step, Tweaks};
    use licash_wallet::Error;
    let net = Net::new().await;
    let index = Arc::new(Tweaks::open(&net.dir.path().join("tweaks.sqlite")).unwrap());
    step(&index, &net.source, None).await.unwrap();
    let services = Services {
        deposits: Some(net.deposits.clone()),
        tweaks: Some(index.clone()),
        ..Default::default()
    };
    let app = licash_hub_service::router(net.hub.clone(), services);
    let path = net.dir.path().join("alice.sqlite");
    let network = ChainProfile::Core30DefaultRegtest;
    let refusing = Arc::new(AtomicBool::new(true));
    let front = Front(Local(app), refusing.clone());
    let alice = Wallet::create(&path, &[1; 32], network, front).unwrap();
    alice.add_hub(HUB).await.unwrap();
    alice
        .watch_silent(HUB, index.first().unwrap().unwrap())
        .unwrap();
    let to = alice.silent_keys().address();
    net.mine(1, vec![pay_silent(&net, &to, 3_000, 2)]);
    step(&index, &net.source, None).await.unwrap();
    let light = Light(net.backend.clone());
    let found = alice.scan_silent(HUB, &light, u32::MAX).await.unwrap();
    assert_eq!(found.found.len(), 1);
    let rate = FeeRate::from_sat_per_vb_u32(1);

    // Its coins never asked a deposit the hub answered: none kept, however
    // often asked, and the coins free.
    for _ in 0..3 {
        let unanswered = alice.deposit_silent(HUB, rate).await.unwrap_err();
        assert!(
            matches!(unanswered, Error::Unavailable { .. }),
            "{unanswered:?}"
        );
    }
    assert!(alice.deposits().unwrap().is_empty());
    assert!(alice.silent_coins().unwrap()[0].spending.is_none());

    // One asked for by hand, kept before its hub hears of it, unanswered:
    // given up, as no address was named to pay.
    let pending = alice.deposit(HUB, 1_000).await.unwrap_err();
    let Error::Pending { id, .. } = pending else {
        panic!("{pending:?}");
    };
    let asked = alice.deposits().unwrap();
    assert_eq!(
        (asked[0].state.clone(), asked[0].terms.clone()),
        (DepositState::Open, None)
    );
    alice.abandon_deposit(&id).unwrap();
    let asked = alice.deposits().unwrap();
    assert!(
        matches!(&asked[0].state, DepositState::Canceled(_)),
        "{asked:?}"
    );
    assert!(matches!(alice.abandon_deposit(&id), Err(Error::Invalid(_))));
    refusing.store(false, Ordering::SeqCst);
    assert!(alice.resume().await.unwrap().deposits.is_empty());

    // Answered: kept with its terms, its coins spent on it, and not to be
    // given up.
    let (deposit, tx) = alice.deposit_silent(HUB, rate).await.unwrap();
    assert_eq!(deposit.state, DepositState::Open);
    assert_eq!(deposit.terms.as_ref().unwrap().pays, deposit.amount + 100);
    assert_eq!(fee_of(&tx, 3_000), tx.vsize() as u64);
    assert_eq!(alice.deposits().unwrap().len(), 2);
    let answered = alice.abandon_deposit(&deposit.hash);
    assert!(matches!(answered, Err(Error::Invalid(_))), "{answered:?}");
    let unknown = alice.abandon_deposit(&"00".repeat(32));
    assert!(
        matches!(unknown, Err(Error::UnknownPayment(_))),
        "{unknown:?}"
    );
}
