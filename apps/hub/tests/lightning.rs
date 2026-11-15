//! The Lightning gateway over a fake node, in process: wallets pay
//! invoices and get paid through it, and its failures end in refunds.

mod chain;

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    http::{header, Method, Request},
    Router,
};
use http_body_util::BodyExt;
use licash::spend::{lock_hash, ChainProfile, ProposalId};
use licash_api::{Fee, Info, Pay, PayState, Paying, Problem, ReceiveState};
use licash_hub::{Hub, Policy, Timing};
use licash_hub_service::lightning::{
    fake::{Fake, Outcome},
    Config, Gateway, HeldState,
};
use licash_wallet::{InvoiceState, LockState, Response, Transport, Wallet};
use lightning_invoice::Bolt11Invoice;
use tower::ServiceExt;

const HUB: &str = "http://hub.test";
const MAX_POW: u32 = 6;

/// Short, to expire locks in a test.
const TIMING: Timing = Timing {
    window: 40,
    reaction: 2,
    min_depth: 1,
    max_headers: 4032,
    bury: 6,
    max_lag: 120,
};

fn config() -> Config {
    Config {
        pay: Fee { base: 2, ppm: 0 },
        receive: Fee { base: 1, ppm: 0 },
        max_pay: 100,
        min_receive: 1,
        max_receive: 100,
        pay_slack: 4,
        hold_delta: 3,
        downtime: 2,
        receive_blocks: 6,
        grace: 0,
        ..Config::default()
    }
}

/// A hub with a gateway over a fake node, both following one chain.
struct Net {
    hub: Arc<Hub>,
    node: Arc<Fake>,
    gateway: Arc<Gateway>,
    app: Router,
    chain: Mutex<chain::Chain>,
    dir: tempfile::TempDir,
}

impl Net {
    /// A hub without fees, its gateway's node known.
    async fn new() -> Self {
        let net = Self::with(Policy {
            timing: TIMING,
            ..Policy::default()
        });
        net.gateway.round().await;
        net
    }

    fn with(policy: Policy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, MAX_POW).unwrap();
        let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
        let chain = chain::Chain::new();
        hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
        let node = Arc::new(Fake::new());
        let path = dir.path().join("lightning.sqlite");
        let gateway = Gateway::open(hub.clone(), node.clone(), config(), &path).unwrap();
        let gateway = Arc::new(gateway);
        let net = Self {
            app: licash_hub_service::router(
                hub.clone(),
                licash_hub_service::Services {
                    gateway: Some(gateway.clone()),
                    ..Default::default()
                },
            ),
            hub,
            node,
            gateway,
            chain: Mutex::new(chain),
            dir,
        };
        net.mine(3, None);
        net
    }

    /// Mine `blocks`, the first with an activation of `secret` if given,
    /// which the hub scans and the node follows.
    fn mine(&self, blocks: u32, mut secret: Option<[u8; 32]>) {
        let mut chain = self.chain.lock().unwrap();
        for _ in 0..blocks {
            let block = chain.mine(secret.take()).clone();
            self.hub.observe(chain.height(), &block).unwrap();
        }
        assert!(self.hub.synced(chain.height(), &chain.tip()).unwrap());
        self.node.tip(chain.height(), chain.tip());
    }

    fn height(&self) -> u32 {
        self.chain.lock().unwrap().height()
    }

    fn wallet(&self, name: &str) -> Wallet<Local> {
        let path = self.dir.path().join(format!("{name}.sqlite"));
        let mut seed = [0; 32];
        seed[..name.len()].copy_from_slice(name.as_bytes());
        Wallet::create(
            &path,
            &seed,
            ChainProfile::Core30DefaultRegtest,
            Local(self.app.clone()),
        )
        .unwrap()
    }

    /// A wallet holding `amount`, paid to it over Lightning.
    async fn funded(&self, name: &str, amount: u64) -> Wallet<Local> {
        let wallet = self.wallet(name);
        let invoice = wallet.invoice(HUB, amount).await.unwrap();
        self.pay_held(&invoice.hash);
        self.gateway.round().await;
        let invoice = wallet.follow_invoice(&invoice.hash).await.unwrap();
        assert_eq!(invoice.state, InvoiceState::Received);
        self.gateway.round().await;
        wallet
    }

    /// Another node pays the held invoice of `hash`, its HTLCs lasting
    /// the invoice's final delta.
    fn pay_held(&self, hash: &str) {
        let hash = bytes(hash);
        let cltv = self.node.held_cltv(hash).unwrap() as u32;
        self.node.pay_held(hash, self.height() + cltv);
    }

    async fn info(&self) -> Info {
        let response = Local(self.app.clone())
            .request(&format!("{HUB}{}", licash_api::INFO), None)
            .await
            .unwrap();
        serde_json::from_slice(&response.body).unwrap()
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

/// The service's router, reached at [`HUB`].
#[derive(Clone)]
struct Local(Router);

impl Transport for Local {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
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
            Ok(Response {
                status,
                body: body.to_vec(),
            })
        }
    }
}

fn proposal_id(id: &str) -> ProposalId {
    ProposalId::from_bytes(bytes(id))
}

async fn call(net: &Net, path: &str, body: &impl serde::Serialize) -> Response {
    let body = serde_json::to_vec(body).unwrap();
    let url = format!("{HUB}{path}");
    Local(net.app.clone())
        .request(&url, Some(body))
        .await
        .unwrap()
}

async fn post<T: serde::de::DeserializeOwned>(
    net: &Net,
    path: &str,
    body: &impl serde::Serialize,
) -> T {
    let response = call(net, path, body).await;
    let text = String::from_utf8_lossy(&response.body);
    assert_eq!(response.status, 200, "{text}");
    serde_json::from_slice(&response.body).unwrap()
}

async fn refused(net: &Net, path: &str, body: &impl serde::Serialize) -> Problem {
    let response = call(net, path, body).await;
    let problem: Problem = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(response.status, problem.code.status(), "{problem:?}");
    problem
}

fn alerts(hub: &Hub) -> usize {
    let events = hub.events(0, 1000).unwrap();
    events.iter().filter(|e| e.kind == "alert").count()
}

/// The preimage `wallet` drew for its invoice of `hash`.
fn preimage_of(net: &Net, name: &str, hash: &str) -> [u8; 32] {
    let db = rusqlite::Connection::open(net.dir.path().join(format!("{name}.sqlite"))).unwrap();
    db.query_row(
        "SELECT preimage FROM invoices WHERE hash = ?1",
        [bytes(hash)],
        |r| r.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn a_payment_received_is_claimed_and_settles_its_invoice() {
    let net = Net::new().await;
    let lightning = net.info().await.lightning.unwrap();
    assert_eq!(lightning.node, net.node.pubkey());
    assert_eq!(lightning.receive, Fee { base: 1, ppm: 0 });
    let wallet = net.wallet("alice");
    assert!(matches!(
        wallet.invoice(HUB, 101).await,
        Err(licash_wallet::Error::Invalid(_))
    ));
    let invoice = wallet.invoice(HUB, 20).await.unwrap();
    assert_eq!(invoice.state, InvoiceState::Open);
    let request: Bolt11Invoice = invoice.request.clone().unwrap().parse().unwrap();
    assert_eq!(request.amount_milli_satoshis(), Some(21_000));
    let hash = bytes(&invoice.hash);
    // Its life, depth, downtime and hold delta, and 4.
    assert_eq!(net.node.held_cltv(hash), Some(6 + 1 + 2 + 3 + 4));
    net.gateway.round().await;
    let open = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert_eq!(open.state, InvoiceState::Open);
    assert_eq!(open.request, invoice.request);

    net.pay_held(&invoice.hash);
    net.gateway.round().await;
    let status = net.gateway.receive_status(&hash).unwrap();
    assert_eq!(status.state, ReceiveState::Locked);
    let received = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert_eq!(received.state, InvoiceState::Received);
    assert_eq!(available(&wallet), 20);
    assert_eq!(net.node.held(hash).unwrap().0, HeldState::Accepted);
    net.gateway.round().await;
    assert_eq!(net.node.held(hash).unwrap(), (HeldState::Settled, 21_000));
    assert_eq!(
        net.gateway.receive_status(&hash).unwrap().state,
        ReceiveState::Settled
    );
    let resumed = wallet.resume().await.unwrap();
    assert!(resumed.invoices.is_empty() && resumed.pending.is_empty());
    assert_eq!(wallet.invoices().unwrap(), vec![received]);
}

#[tokio::test]
async fn an_invoice_paid_through_the_hub_settles_its_lock() {
    let net = Net::new().await;
    let wallet = net.funded("alice", 40).await;
    let preimage = [5; 32];
    let request = net
        .node
        .invoice(20_000, lock_hash(&preimage), 18, Outcome::Pay(preimage));
    let lock = wallet.pay_invoice(HUB, &request).await.unwrap();
    // The invoice and the gateway's fee.
    assert_eq!((lock.amount, lock.state.clone()), (22, LockState::Locked));
    assert_eq!(lock.invoice.as_deref(), Some(request.as_str()));
    let sent = net.node.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].cltv_limit, lock.expiry - net.height() - 4);
    assert_eq!(sent[0].fee_limit, 2);
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Paid(preimage));
    assert_eq!(lock.failure, None);
    assert_eq!(available(&wallet), 18);
    // Asked again, the same; for another invoice, refused.
    let id = proposal_id(&lock.id);
    let paying: Paying = post(&net, licash_api::LN_PAY, &Pay::new(&id, &request)).await;
    assert_eq!(paying.state, PayState::Paid);
    assert_eq!(paying.preimage.unwrap().0, preimage.to_vec());
    let other = net.node.invoice(20_000, [9; 32], 18, Outcome::Hang);
    let problem = refused(&net, licash_api::LN_PAY, &Pay::new(&id, &other)).await;
    assert_eq!(problem.code, licash_api::Code::Forward);
    assert_eq!(net.node.sent().len(), 1);
}

#[tokio::test]
async fn an_invoice_is_read_and_what_paying_it_costs_said_before_paying() {
    let net = Net::with(Policy {
        spend: Fee { base: 1, ppm: 0 },
        timing: TIMING,
        ..Policy::default()
    });
    net.gateway.round().await;
    let wallet = net.funded("alice", 40).await;
    let preimage = [5; 32];
    let request = net
        .node
        .invoice(20_500, lock_hash(&preimage), 18, Outcome::Pay(preimage));
    // Bare, as a URI, or in a Bitcoin URI, any case.
    for text in [
        request.clone(),
        format!("lightning:{request}"),
        format!("LIGHTNING:{}", request.to_uppercase()),
        format!("bitcoin:bcrt1qexample?amount=0.00000021&lightning={request}"),
    ] {
        let payable = wallet.read_invoice(&text).unwrap();
        assert_eq!(payable.request, request, "{text}");
        assert_eq!((payable.amount, payable.description), (21, None));
        assert!(payable.expires_at > 0);
    }
    for text in ["bitcoin:bcrt1qexample?amount=1", "lnbcrt1nonsense", ""] {
        assert!(wallet.read_invoice(text).is_err(), "{text}");
    }
    // Not for a wallet of another network.
    let path = net.dir.path().join("signet.sqlite");
    let signet = Wallet::create(
        &path,
        &[7; 32],
        ChainProfile::Core30Signet,
        Local(net.app.clone()),
    )
    .unwrap();
    let refused = signet.read_invoice(&request).unwrap_err().to_string();
    assert!(refused.contains("another network"), "{refused}");
    // Read as it is, as for the history, by any wallet.
    let read = licash_wallet::Payable::read(&format!("lightning:{request}")).unwrap();
    assert_eq!(read, wallet.read_invoice(&request).unwrap());

    // The gateway's 2 on the 21 asked, and 1 to the hub: the lock of 23
    // needs exact notes, made by a payment to itself.
    assert_eq!(wallet.invoice_fee(HUB, &request).unwrap(), 3);
    let lock = wallet.pay_invoice(HUB, &request).await.unwrap();
    assert_eq!(lock.amount, 23);
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Paid(preimage));
    assert_eq!(available(&wallet), 40 - 23 - 1);
    // The lock's own cost: the notes made for it exactly.
    assert_eq!(lock.cost, Some(23));
    // Exact notes held, 8: the gateway's fee alone.
    let exact = net.node.invoice(6_000, [4; 32], 18, Outcome::Hang);
    assert_eq!(wallet.invoice_fee(HUB, &exact).unwrap(), 2);
    // Too much for the wallet, then for the gateway.
    let more = net.node.invoice(30_000, [6; 32], 18, Outcome::Hang);
    assert!(matches!(
        wallet.invoice_fee(HUB, &more),
        Err(licash_wallet::Error::Insufficient { .. })
    ));
    let most = net.node.invoice(101_000, [8; 32], 18, Outcome::Hang);
    assert!(wallet.invoice_fee(HUB, &most).is_err());
    // A final delta longer than the hub's locks leave its gateway.
    let long = net.node.invoice(6_000, [3; 32], 40, Outcome::Hang);
    let refused = wallet.invoice_fee(HUB, &long).unwrap_err().to_string();
    assert!(refused.contains("final delta"), "{refused}");
}

#[tokio::test]
async fn a_failed_payment_refunds_its_lock_at_once() {
    let net = Net::new().await;
    let wallet = net.funded("alice", 40).await;
    let reason = "FAILURE_REASON_NO_ROUTE".to_owned();
    let request = net.node.invoice(
        20_000,
        lock_hash(&[6; 32]),
        18,
        Outcome::Fail(reason.clone()),
    );
    let lock = wallet.pay_invoice(HUB, &request).await.unwrap();
    assert_eq!(lock.state, LockState::Locked);
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Refunded);
    assert_eq!(lock.failure, Some(reason));
    assert_eq!(available(&wallet), 40);
}

#[tokio::test]
async fn an_invoice_of_the_hubs_own_node_is_refused_and_refunded() {
    let net = Net::new().await;
    let alice = net.funded("alice", 40).await;
    let bob = net.wallet("bob");
    let invoice = bob.invoice(HUB, 10).await.unwrap();
    let lock = alice
        .pay_invoice(HUB, invoice.request.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(lock.state, LockState::Refunded);
    assert!(lock.failure.unwrap().contains("own node"));
    assert!(net.node.sent().is_empty());
    assert_eq!(available(&alice), 40);
}

#[tokio::test]
async fn a_payment_in_flight_holds_its_lock_past_expiry_until_resolved() {
    let net = Net::new().await;
    let wallet = net.funded("alice", 40).await;
    let preimage = [7; 32];
    let hash = lock_hash(&preimage);
    let request = net.node.invoice(20_000, hash, 18, Outcome::Hang);
    // The node is down: the lock waits, and is paid once asked again.
    net.node.down(true);
    let lock = wallet.pay_invoice(HUB, &request).await.unwrap();
    assert_eq!(
        (lock.state.clone(), lock.failure.clone()),
        (LockState::Locked, None)
    );
    assert!(net.node.sent().is_empty());
    net.node.down(false);
    wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(net.node.sent().len(), 1);
    net.gateway.round().await;
    // Expired at depth, it is not refunded while the payment is in flight.
    net.mine(lock.expiry + 1 - net.height(), None);
    assert_eq!(alerts(&net.hub), 0);
    net.gateway.round().await;
    net.gateway.round().await;
    assert_eq!(alerts(&net.hub), 1, "alerted once of a late payment");
    assert!(matches!(
        wallet.follow_lock(&lock.id).await,
        Err(licash_wallet::Error::Pending { .. })
    ));
    net.node.resolve(hash, Outcome::Pay(preimage));
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Paid(preimage));
    assert_eq!(net.node.sent().len(), 1);
}

#[tokio::test]
async fn a_payment_the_node_lost_is_sent_again_or_given_up_once_too_late() {
    let net = Net::new().await;
    let wallet = net.funded("alice", 40).await;
    let hash = lock_hash(&[8; 32]);
    let request = net.node.invoice(20_000, hash, 18, Outcome::Hang);
    let lock = wallet.pay_invoice(HUB, &request).await.unwrap();
    let id = proposal_id(&lock.id);
    net.node.forget(hash);
    net.gateway.round().await;
    let sent = net.node.sent();
    assert_eq!(sent.len(), 2, "sent again while in time");
    assert_eq!(sent[1], sent[0]);
    // Too late to send: its route would outlast the lock.
    net.node.forget(hash);
    net.mine(lock.expiry - 24 - net.height(), None);
    net.gateway.round().await;
    assert_eq!(net.node.sent().len(), 2);
    let pending = net.hub.forward_of(&id).unwrap().unwrap().state;
    assert_eq!(
        pending,
        licash_hub::Forwarded::Pending,
        "one miss is not enough"
    );
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Refunded);
    assert_eq!(lock.failure.as_deref(), Some("not started in time"));
}

#[tokio::test]
async fn htlcs_expiring_too_soon_cancel_the_invoice() {
    let net = Net::new().await;
    let wallet = net.wallet("alice");
    let invoice = wallet.invoice(HUB, 20).await.unwrap();
    let hash = bytes(&invoice.hash);
    // The lock would outlast them, with its depth, downtime and hold delta.
    net.node.pay_held(hash, net.height() + 6 + 1 + 2 + 3);
    net.gateway.round().await;
    assert_eq!(net.node.held(hash).unwrap().0, HeldState::Canceled);
    let invoice = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert!(matches!(invoice.state, InvoiceState::Canceled(_)));
    assert_eq!(available(&wallet), 0);
}

#[tokio::test]
async fn a_lock_unclaimed_is_refunded_and_cancels_its_invoice() {
    let net = Net::new().await;
    let wallet = net.wallet("alice");
    let invoice = wallet.invoice(HUB, 20).await.unwrap();
    let hash = bytes(&invoice.hash);
    net.pay_held(&invoice.hash);
    net.gateway.round().await;
    assert_eq!(
        net.gateway.receive_status(&hash).unwrap().state,
        ReceiveState::Locked
    );
    // Expired at depth.
    net.mine(6 + 1, None);
    net.gateway.round().await;
    assert_eq!(net.node.held(hash).unwrap().0, HeldState::Canceled);
    assert_eq!(
        net.gateway.receive_status(&hash).unwrap().state,
        ReceiveState::Canceled
    );
    let invoice = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert!(matches!(invoice.state, InvoiceState::Canceled(_)));
    assert_eq!(available(&wallet), 0);
}

#[tokio::test]
async fn a_preimage_activated_on_bitcoin_settles_the_invoice() {
    let net = Net::new().await;
    let wallet = net.wallet("alice");
    let invoice = wallet.invoice(HUB, 20).await.unwrap();
    let hash = bytes(&invoice.hash);
    net.pay_held(&invoice.hash);
    net.gateway.round().await;
    net.mine(1, Some(preimage_of(&net, "alice", &invoice.hash)));
    net.gateway.round().await;
    assert_eq!(net.node.held(hash).unwrap().0, HeldState::Settled);
    let invoice = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert_eq!(invoice.state, InvoiceState::Received);
    assert_eq!(available(&wallet), 20);
}

#[tokio::test]
async fn a_lock_made_before_a_crash_is_taken_for_its_receipt() {
    let net = Net::new().await;
    let wallet = net.wallet("alice");
    let invoice = wallet.invoice(HUB, 20).await.unwrap();
    let hash = bytes(&invoice.hash);
    net.pay_held(&invoice.hash);
    net.gateway.round().await;
    let locked = net.gateway.receive_status(&hash).unwrap().lock;
    // As if the gateway stopped between locking and recording the lock.
    let db = rusqlite::Connection::open(net.dir.path().join("lightning.sqlite")).unwrap();
    let reset = db
        .execute(
            "UPDATE receipts SET state = 'open', lock = NULL WHERE hash = ?1",
            [hash],
        )
        .unwrap();
    assert_eq!(reset, 1);
    net.gateway.round().await;
    let status = net.gateway.receive_status(&hash).unwrap();
    assert_eq!((status.state, status.lock), (ReceiveState::Locked, locked));
    let invoice = wallet.follow_invoice(&invoice.hash).await.unwrap();
    assert_eq!(invoice.state, InvoiceState::Received);
    net.gateway.round().await;
    assert_eq!(net.node.held(hash).unwrap().0, HeldState::Settled);
}

#[tokio::test]
async fn a_failure_of_an_earlier_payment_of_its_hash_is_not_the_forwards() {
    let net = Net::new().await;
    let wallet = net.funded("alice", 60).await;
    let preimage = [10; 32];
    let hash = lock_hash(&preimage);
    let failing = net.node.invoice(
        20_000,
        hash,
        18,
        Outcome::Fail("FAILURE_REASON_TIMEOUT".into()),
    );
    let first = wallet.pay_invoice(HUB, &failing).await.unwrap();
    net.gateway.round().await;
    let first = wallet.follow_lock(&first.id).await.unwrap();
    assert_eq!(first.state, LockState::Refunded);
    // The node keeps that failed attempt; a later payment of the hash
    // starts its own.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let paying = net.node.invoice(21_000, hash, 18, Outcome::Pay(preimage));
    let second = wallet.pay_invoice(HUB, &paying).await.unwrap();
    assert_eq!(second.state, LockState::Locked);
    assert_eq!(net.node.sent().len(), 2);
    net.gateway.round().await;
    let second = wallet.follow_lock(&second.id).await.unwrap();
    assert_eq!(second.state, LockState::Paid(preimage));
}

#[tokio::test]
async fn the_operator_is_alerted_once_of_each_way_the_node_moves_too_little() {
    let net = Net::new().await;
    let said = || -> Vec<String> {
        let events = net.hub.events(0, 1000).unwrap();
        let notices = events.iter().filter_map(|e| e.notice());
        notices
            .filter_map(|n| match n {
                licash_hub::Notice::Alert { message } => Some(message),
                _ => None,
            })
            .collect()
    };
    assert!(said().is_empty());
    // config() takes invoices and receipts of up to 100 sat.
    net.node.channels(99, 100);
    net.gateway.round().await;
    net.gateway.round().await;
    let alerts = said();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("can pay 99 sat"), "{}", alerts[0]);
    assert!(alerts[0].contains("--ln-max-pay"), "{}", alerts[0]);

    net.node.channels(100, 0);
    net.gateway.round().await;
    net.node.channels(50, 0);
    net.gateway.round().await;
    let alerts = said();
    assert_eq!(alerts.len(), 3, "{alerts:?}");
    assert!(alerts[1].contains("can receive 0 sat"), "{}", alerts[1]);
    assert!(alerts[2].contains("can pay 50 sat"), "{}", alerts[2]);

    // Unread channels say nothing.
    net.node.down(true);
    net.gateway.round().await;
    assert_eq!(said().len(), 3);
}

/// The value of the facts of `series` under `label`, in `report`.
fn summed(report: &licash_dashboard::Report, series: &str, label: &str) -> (usize, i64) {
    let series = report.series.iter().find(|s| s.id == series).unwrap();
    let Some(label) = series.labels.iter().position(|l| l == label) else {
        return (0, 0);
    };
    let facts = series.facts.iter().filter(|f| f.label as usize == label);
    facts.fold((0, 0), |(n, sum), f| (n + 1, sum + f.value))
}

#[tokio::test]
async fn the_dashboard_reads_payments_fees_failures_and_notes_from_the_records() {
    let net = Net::with(Policy {
        spend: Fee { base: 1, ppm: 0 },
        timing: TIMING,
        ..Policy::default()
    });
    net.gateway.round().await;
    // In: 40 sat, of an invoice of 41.
    let wallet = net.funded("alice", 40).await;
    // Out: 20 sat, in a lock of 22 with the gateway's fee, of notes made
    // exact by a transfer to itself, paying the hub 1.
    let preimage = [5; 32];
    let paid = net
        .node
        .invoice(20_000, lock_hash(&preimage), 18, Outcome::Pay(preimage));
    let lock = wallet.pay_invoice(HUB, &paid).await.unwrap();
    net.gateway.round().await;
    let lock = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(lock.state, LockState::Paid(preimage));
    // Failed: refunded.
    let reason = "FAILURE_REASON_NO_ROUTE".to_owned();
    let failed = net
        .node
        .invoice(10_000, lock_hash(&[6; 32]), 18, Outcome::Fail(reason));
    let lock = wallet.pay_invoice(HUB, &failed).await.unwrap();
    net.gateway.round().await;
    let failed = wallet.follow_lock(&lock.id).await.unwrap();
    assert_eq!(failed.state, LockState::Refunded);

    let report = licash_hub_service::dashboard::report(net.dir.path(), None).unwrap();
    assert_eq!(summed(&report, "volume", "lightning in"), (1, 41));
    assert_eq!(summed(&report, "fees", "lightning in"), (1, 1));
    assert_eq!(summed(&report, "volume", "lightning out"), (1, 22));
    assert_eq!(summed(&report, "fees", "lightning out"), (1, 2));
    let (transfers, fees) = summed(&report, "fees", "transfers");
    assert!(
        transfers >= 1 && fees == transfers as i64,
        "{transfers} {fees}"
    );
    assert_eq!(
        summed(&report, "failures", "no route"),
        (1, failed.amount as i64)
    );
    // A lock paying out is no plain lock, settled or refunded.
    assert_eq!(summed(&report, "volume", "locks"), (0, 0));
    assert_eq!(summed(&report, "failures", "lock refunded"), (0, 0));
    // Every note the hub issued since its ledger began: the 40 paid in,
    // the wallet's and the hub's fees and payments out, none behind a lock.
    let (_, holders) = summed(&report, "ecash", "holders");
    let (_, own) = summed(&report, "ecash", "hub's own");
    assert_eq!(summed(&report, "ecash", "behind locks").1, 0);
    assert_eq!(holders, available(&wallet) as i64);
    assert_eq!(holders + own, 40);
    let notes = report.series.iter().find(|s| s.id == "notes").unwrap();
    assert!(notes.facts.iter().map(|f| f.value).sum::<i64>() > 0);
    let pages: Vec<_> = report.pages.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        pages,
        [
            "Overview",
            "Payments",
            "Fees",
            "Reserves",
            "Contracts",
            "Privacy",
            "Health"
        ]
    );
    assert!(report.since <= report.now && report.now - report.since < 600);

    // Below the overview, what happened, newest first: each payment out
    // as its forward ended, never again as its lock settling.
    use licash_dashboard::{Cell, Panel};
    let Some([Panel::Table(recent)]) = report.pages[0].rows.last().map(|r| &r[..]) else {
        panic!("no recent events below the overview");
    };
    assert_eq!(recent.title, "Recent events");
    let text = |c: &Cell| match c {
        Cell::Text(s) => s.clone(),
        Cell::Sat(v) => v.to_string(),
        _ => String::new(),
    };
    let said: Vec<[String; 4]> = recent
        .rows
        .iter()
        .map(|r| [&r.cells[1], &r.cells[2], &r.cells[3], &r.cells[4]].map(text))
        .collect();
    let has = |kind: &str, event: &str| said.iter().any(|s| s[0] == kind && s[1] == event);
    let out = |event: &'static str| {
        let said = said.iter();
        said.filter(move |s| s[0] == "lightning out" && s[1] == event)
    };
    assert_eq!(out("locked").count(), 2, "{said:?}");
    assert_eq!(out("paid").map(|s| &s[2]).collect::<Vec<_>>(), ["22"]);
    let failures: Vec<_> = out("failed").collect();
    assert_eq!(failures.len(), 1, "{said:?}");
    assert_eq!(failures[0][2], failed.amount.to_string());
    assert_eq!(failures[0][3], "no route");
    assert!(!has("lightning out", "settled") && !has("lightning out", "refunded"));
    assert!(has("lightning in", "asked") && has("lightning in", "settled"));
    assert!(
        has("transfer", "released") && has("hub", "started"),
        "{said:?}"
    );
    let at: Vec<_> = recent.rows.iter().map(|r| r.at.unwrap()).collect();
    assert!(at.is_sorted_by(|a, b| a >= b), "{at:?}");
}
