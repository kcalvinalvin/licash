//! The observatory against a fake hub and provider on loopback: probed,
//! their terms kept, and shown on the page and in the JSON API.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{body::Body, http::Request, routing::get, Json, Router};
use http_body_util::BodyExt;
use licash_nostr::Route;
use licash_observatory::{
    budget::{self, Budget, Kind},
    mover::{Mover, Moving, Skipped},
    observer::{self, Watching},
    probe::Dialer,
    store::{self, Action, Outcome, Role, Service, Store},
    view::{self, Range, Records},
    web::{self, App},
};
use serde_json::{json, Value};
use tower::ServiceExt;

/// A valid point: the generator, compressed.
const G: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
const HUB_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const PROVIDER_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn address() -> String {
    format!("{G}{G}{G}")
}

fn hub_info() -> Value {
    json!({
        "keysets": [],
        "address": address(),
        "fees": {
            "spend": {"base": 1, "ppm": 100},
            "lock": {"base": 1, "ppm": 100},
            "refund": {"base": 1, "ppm": 0},
            "output_msat": 100
        },
        "locks": null,
        "lightning": {
            "node": "02aa", "pay": {"base": 2, "ppm": 1000}, "receive": {"base": 1, "ppm": 100},
            "pay_slack": 40, "max_pay": 1000000, "min_receive": 100, "max_receive": 500000
        },
        "deposits": {
            "fee": {"base": 300, "ppm": 0}, "min": 10000, "max": 1000000,
            "depth": 1, "window": 144, "timeout": 1008
        },
        "nostr": HUB_KEY
    })
}

fn provider_info() -> Value {
    json!({
        "address": address(),
        "fee": {"base": 10, "ppm": 1000},
        "min": 1000,
        "max": 1000000,
        "margin": 200,
        "hubs": [{"address": address(), "max": 250000}],
        "nostr": PROVIDER_KEY,
        "chain": {"fee": {"base": 100, "ppm": 1000}, "miner": 400, "rate": 2000,
                  "min": 10000, "max": 500000, "margin": 72}
    })
}

/// A service on loopback answering `/health` and its info at `path`.
async fn serve(path: &'static str, info: Value) -> String {
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(path, get(move || async move { Json(info) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{at}")
}

/// An address nothing answers at.
async fn closed() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{at}")
}

fn announced(id: &str, role: Role, urls: Vec<String>) -> Service {
    let now = store::now();
    Service {
        id: id.into(),
        role,
        key: Some(id.into()),
        name: None,
        urls,
        first_seen: now,
        last_seen: now,
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    hub: String,
    provider: String,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("observatory.sqlite")).unwrap());
    let hub = serve(licash_api::INFO, hub_info()).await;
    let provider = serve(licash_api::swaps::SWAPS_INFO, provider_info()).await;
    let down = closed().await;
    store
        .seen(&announced(HUB_KEY, Role::Hub, vec![hub.clone(), down]))
        .unwrap();
    store
        .seen(&announced(
            PROVIDER_KEY,
            Role::Provider,
            vec![provider.clone()],
        ))
        .unwrap();
    World {
        _dir: dir,
        store,
        hub,
        provider,
    }
}

fn watching() -> Watching {
    Watching {
        network: "regtest".into(),
        relays: Vec::new(),
        route: Route::Direct,
        dialer: Dialer::direct(),
        hubs: Vec::new(),
        providers: Vec::new(),
        probe_every: Duration::from_millis(20),
        discover_every: Duration::from_secs(600),
        timeout: Duration::from_secs(5),
    }
}

fn view(store: &Store, budget: Option<Budget>) -> view::View {
    let now = store::now();
    let records = Records::read(store, now - Range::Week.millis()).unwrap();
    view::compute(
        records,
        view::Context {
            network: "regtest".into(),
            tor: false,
            now,
            range: Range::Week,
            fresh: 60_000,
            budget,
        },
    )
}

#[tokio::test]
async fn a_round_probes_every_url_and_keeps_the_terms() {
    let w = world().await;
    for _ in 0..3 {
        observer::round(w.store.clone(), watching()).await;
    }
    let probes = w.store.probes_since(0).unwrap();
    assert_eq!(
        probes.len(),
        9,
        "two URLs of the hub and one of the provider, 3 times"
    );
    let answered = probes.iter().filter(|p| p.ok).count();
    assert_eq!(answered, 6);
    let failed = probes.iter().find(|p| !p.ok).unwrap();
    assert!(failed.open_ms.is_none() && failed.error.is_some());
    let ok = probes.iter().find(|p| p.ok).unwrap();
    assert!(ok.open_ms.is_some() && ok.rtt_ms.is_some() && ok.info_ms.is_some());
    // Terms are kept once while unchanged.
    assert_eq!(w.store.latest_terms().unwrap().len(), 2);

    let view = view(&w.store, None);
    let hub = &view.hubs[0];
    assert_eq!(hub.health.up, Some(true), "one URL answers");
    assert_eq!(hub.health.uptime, Some(0.5));
    assert_eq!(hub.health.probes, 6);
    assert_eq!(hub.health.urls.len(), 2);
    assert_eq!(hub.inbound, 250_000, "the provider moves into it");
    let provider = &view.providers[0];
    assert_eq!(provider.health.uptime, Some(1.0));
    assert_eq!(provider.hubs, vec![(HUB_KEY.to_owned(), 250_000)]);

    // Ranked: the provider first by uptime.
    assert_eq!(view.boards.uptime[0].id, PROVIDER_KEY);
    assert_eq!(view.boards.uptime.len(), 2);
    assert_eq!(view.boards.latency.len(), 2);

    let fee = |what: &str| view.fees.iter().find(|f| f.what == what).unwrap().clone();
    // 1 sat + 100 ppm of 10,000 sat.
    assert_eq!(fee("Pay in Licash").median, Some(2));
    // 100 sat + 1,000 ppm of 10,000, and 400 for miners.
    assert_eq!(fee("Pay on Bitcoin").median, Some(510));
    assert_eq!(fee("Protection, opening").offered, 0);
    let _ = (&w.hub, &w.provider);
}

#[tokio::test]
async fn a_url_answering_for_another_key_is_not_the_service() {
    let w = world().await;
    let impostor = announced(
        "3333333333333333333333333333333333333333333333333333333333333333",
        Role::Hub,
        vec![w.hub.clone()],
    );
    let (probe, info) =
        observer::check(&Dialer::direct(), &impostor, &w.hub, Duration::from_secs(5)).await;
    assert!(!probe.ok);
    assert!(info.is_none());
    assert_eq!(
        probe.error.as_deref(),
        Some("info: not the service that announced it")
    );
}

#[tokio::test]
async fn a_configured_service_is_watched_by_its_url() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("o.sqlite")).unwrap();
    let mut watching = watching();
    watching.hubs = vec!["http://hub.example/".into()];
    observer::discover(&store, &watching).await.unwrap();
    let services = store.services().unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].id, "http://hub.example");
    assert_eq!(services[0].key, None);
}

async fn get_page(app: Router, path: &str) -> (u16, String) {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn action(kind: Kind, source: &str, outcome: Outcome, cost: u64) -> Action {
    Action {
        id: 0,
        kind: kind.key().into(),
        started: store::now() - 5_000,
        ended: None,
        source: Some(source.into()),
        target: Some(source.into()),
        via: None,
        amount: 1_000,
        quoted: Some(cost),
        cost: None,
        outcome,
        detail: None,
    }
}

#[tokio::test]
async fn the_pages_show_what_was_seen() {
    let w = world().await;
    observer::round(w.store.clone(), watching()).await;
    let started = action(Kind::Hub, HUB_KEY, Outcome::Running, 2);
    let id = w.store.action_started(&started).unwrap();
    w.store
        .action_ended(id, store::now(), Outcome::Done, Some(2), None)
        .unwrap();
    let id = w.store.action_started(&started).unwrap();
    w.store
        .action_ended(
            id,
            store::now(),
            Outcome::Failed,
            Some(1),
            Some("the hub refused"),
        )
        .unwrap();
    let month = w.store.actions_since(0).unwrap();
    let budget = Budget::new(
        10_000,
        &month,
        Some(5_000),
        &[(Kind::Hub, true, Some(store::now() + 600_000))],
    );
    assert_eq!(budget.spent, 3);
    assert_eq!(budget::spent(&month, Some(Kind::Hub)), 3);
    let app = Arc::new(App::new(
        w.store.clone(),
        "signet".into(),
        true,
        Duration::from_secs(60),
        Arc::new(Mutex::new(Some(budget))),
    ));
    let router = web::router(vec![app]);

    let (status, page) = get_page(router.clone(), "/signet").await;
    assert_eq!(status, 200);
    assert!(page.contains("Test network"), "a test network is said");
    assert!(page.contains("Leaderboards"));
    assert!(page.contains("Fees on 10,000 sat"));
    assert!(page.contains("1 of 1"), "the provider is up");
    assert!(page.contains("50%"), "half the moves succeeded");

    let (status, page) = get_page(router.clone(), "/signet/hubs?range=24h").await;
    assert_eq!(status, 200);
    assert!(page.contains("127.0.0.1"));
    assert!(page.contains("1 sat + 0.01%"), "the hub's fee, as terms");
    assert!(page.contains("up to 250,000 sat"));

    let (status, page) = get_page(router.clone(), "/signet/providers").await;
    assert_eq!(status, 200);
    assert!(page.contains("10 sat + 0.10%"));

    let (status, page) = get_page(router.clone(), &format!("/signet/service?id={HUB_KEY}")).await;
    assert_eq!(status, 200);
    assert!(page.contains("Receive on Bitcoin"));
    assert!(page.contains("the hub refused"));
    assert!(page.contains("<svg"));

    let (status, _) = get_page(router.clone(), "/signet/service?id=nobody").await;
    assert_eq!(status, 404);

    let (status, page) = get_page(router.clone(), "/signet/moves").await;
    assert_eq!(status, 200);
    assert!(page.contains("Pay in Licash"));
    assert!(page.contains("5,000 sat"));

    let (status, body) = get_page(router.clone(), "/signet/api/v1/view?range=30d").await;
    assert_eq!(status, 200);
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["range"], "30d");
    assert_eq!(json["hubs"][0]["id"], HUB_KEY);
    assert_eq!(json["budget"]["spent"], 3);

    let (status, body) = get_page(router, "/health").await;
    assert_eq!((status, body.as_str()), (200, "ok"));
}

#[test]
fn a_later_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.sqlite");
    drop(Store::open(&path).unwrap());
    assert!(Store::open(&path).is_ok(), "its own version opens again");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", 2).unwrap();
    drop(db);
    let refused = Store::open(&path).err().unwrap().to_string();
    assert!(refused.contains("later version"), "{refused}");
}

#[tokio::test]
async fn an_unfunded_mover_moves_nothing_and_says_so() {
    let w = world().await;
    observer::round(w.store.clone(), watching()).await;
    let mut seed = [7; 32];
    seed[0] = 1;
    let wallet = licash_wallet::Wallet::create(
        &w._dir.path().join("wallet.sqlite"),
        &seed,
        licash::spend::ChainProfile::Core30DefaultRegtest,
        licash_transport::Http::new(None).unwrap(),
    )
    .unwrap();
    let shown = Arc::new(Mutex::new(None));
    let moving = Moving {
        store: w.store.clone(),
        monthly: 10_000,
        amount: 1_000,
        fresh: 60_000,
        shown: shown.clone(),
        chain: None,
    };
    let mut mover = Mover::new(wallet, moving).unwrap();
    for kind in [Kind::Hub, Kind::Swap, Kind::Lightning] {
        let skipped = mover.move_now(kind).await.unwrap_err();
        assert!(
            matches!(skipped, Skipped::NoRoute(_)),
            "{kind:?}: {skipped:?}"
        );
    }
    assert!(
        w.store.actions_since(0).unwrap().is_empty(),
        "nothing recorded"
    );
    let budget = shown.lock().unwrap().clone().unwrap();
    assert_eq!(budget.balance, Some(0));
    assert!(
        budget.notes.iter().any(|n| n.contains("fund it")),
        "{:?}",
        budget.notes
    );
    // The fake hub's info is not a hub the wallet can use.
    assert!(
        budget.notes.iter().any(|n| n.contains("can't use")),
        "{:?}",
        budget.notes
    );
    let chain = budget.kinds.iter().find(|k| k.kind == Kind::Chain).unwrap();
    assert!(!chain.enabled);
}

#[tokio::test]
async fn several_networks_are_served_each_under_its_name() {
    let w = world().await;
    observer::round(w.store.clone(), watching()).await;
    let other = tempfile::tempdir().unwrap();
    let empty = Arc::new(Store::open(&other.path().join("o.sqlite")).unwrap());
    let app = |store: &Arc<Store>, network: &str| {
        Arc::new(App::new(
            store.clone(),
            network.into(),
            false,
            Duration::from_secs(60),
            Arc::new(Mutex::new(None)),
        ))
    };
    let router = web::router(vec![app(&w.store, "mutinynet"), app(&empty, "bitcoin")]);

    let response = router
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 307);
    assert_eq!(response.headers()["location"], "/mutinynet");

    let (status, page) = get_page(router.clone(), "/mutinynet/hubs").await;
    assert_eq!(status, 200);
    assert!(page.contains("127.0.0.1"), "its own records");
    assert!(
        page.contains("href=\"/bitcoin/hubs?range=7d\""),
        "a switch to the other"
    );
    assert!(
        page.contains("href=\"/mutinynet/service?id="),
        "links stay in its network"
    );

    let (status, page) = get_page(router.clone(), "/bitcoin/hubs/").await;
    assert_eq!(status, 404, "no such page");
    let (status, page_bitcoin) = get_page(router.clone(), "/bitcoin").await;
    assert_eq!(status, 200);
    assert!(!page_bitcoin.contains("Test network"), "{page}");
    assert!(
        !page_bitcoin.contains("127.0.0.1"),
        "not the other's records"
    );
    let (status, _) = get_page(router.clone(), "/nowhere").await;
    assert_eq!(status, 404);
    let (status, _) = get_page(router, "/bitcoin/").await;
    assert_eq!(status, 200);
}
