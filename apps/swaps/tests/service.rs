//! The provider's API and notifications over a provider whose wallet
//! reaches no hub, and its binary's commands that need none.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use licash::spend::ChainProfile;
use licash_api::{swaps, Code, Problem};
use licash_lightning::fake::{Fake, Outcome};
use licash_swaps::{Config, Provider};
use licash_wallet::{Response, Transport, Wallet};
use tower::ServiceExt;

struct Offline;

impl Transport for Offline {
    fn request(
        &self,
        _: &str,
        _: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        std::future::ready(Err("offline".into()))
    }
}

fn router(dir: &tempfile::TempDir) -> (Router, Arc<Provider<Offline>>) {
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[5; 32],
        ChainProfile::Core30DefaultRegtest,
        Offline,
    )
    .unwrap();
    let provider = Provider::create(&dir.path().join("swaps.sqlite"), wallet, Config::default());
    let provider = Arc::new(provider.unwrap());
    let router = licash_swaps_service::router(provider.clone(), Some("ab".repeat(32)));
    (router, provider)
}

async fn call(router: &Router, path: &str, body: Option<&str>) -> (StatusCode, Vec<u8>) {
    let request = match body {
        Some(body) => Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned())),
        None => Request::get(path).body(Body::empty()),
    };
    let response = router.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

fn problem(body: &[u8]) -> Problem {
    serde_json::from_slice(body).unwrap()
}

#[tokio::test]
async fn the_api_answers_what_the_provider_swaps_and_declines_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let (router, provider) = router(&dir);

    let (status, body) = call(&router, swaps::SWAPS_INFO, None).await;
    assert_eq!(status, StatusCode::OK);
    let info: swaps::Info = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.address, provider.wallet().address());
    assert_eq!((info.fee, info.margin), (Config::default().fee, 12));
    assert!(info.hubs.is_empty());
    assert_eq!(info.nostr, Some("ab".repeat(32)));

    let address = provider.wallet().address();
    let id = licash::spend::ProposalId::from_bytes([1; 32]);
    let request = swaps::Request::new(&address, &id, &address, &address, 5000);
    let body = serde_json::to_string(&request).unwrap();
    let (status, answer) = call(&router, swaps::SWAPS, Some(&body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(problem(&answer).code, Code::Declined);

    let (status, answer) = call(&router, swaps::SWAPS, Some("{\"from\": 1}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem(&answer).code, Code::Invalid);

    let (status, answer) = call(&router, &swaps::swap_state(&id), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem(&answer).code, Code::UnknownLock);
    let (status, _) = call(&router, "/v1/swaps/zz", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Paying nothing out on Bitcoin: no payout taken, none known.
    assert_eq!(info.chain, None);
    let payout = swaps::ChainRequest::new(&address, &id, [2; 32], 20_000);
    let body = serde_json::to_string(&payout).unwrap();
    let (status, answer) = call(&router, swaps::CHAIN_SWAPS, Some(&body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let declined = problem(&answer);
    assert_eq!(declined.code, Code::Declined);
    assert!(declined.message.contains("no payouts"), "{declined:?}");
    let (status, answer) = call(&router, &swaps::chain_swap_state(&id), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem(&answer).code, Code::UnknownLock);

    // Paying nothing over Lightning: no invoice paid, none known.
    assert_eq!(info.lightning, None);
    let node = Fake::new();
    let invoice = node.invoice(10_000_000, [3; 32], 18, Outcome::Hang);
    let pay = swaps::LightningRequest::new(&address, &id, &invoice);
    let body = serde_json::to_string(&pay).unwrap();
    let (status, answer) = call(&router, swaps::LIGHTNING_SWAPS, Some(&body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let declined = problem(&answer);
    assert!(declined.message.contains("no Lightning"), "{declined:?}");
    let garbled = swaps::LightningRequest::new(&address, &id, "lnbcrt1nothing");
    let body = serde_json::to_string(&garbled).unwrap();
    let (status, answer) = call(&router, swaps::LIGHTNING_SWAPS, Some(&body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem(&answer).code, Code::Invalid);
    let (status, answer) = call(&router, &swaps::lightning_swap_state(&id), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem(&answer).code, Code::UnknownLock);

    let (status, body) = call(&router, licash_swaps_service::HEALTH, None).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"ok".as_slice())
    );
}

#[tokio::test]
async fn a_provider_paying_over_lightning_says_so_and_takes_invoices_from_its_hubs_only() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[5; 32],
        ChainProfile::Core30DefaultRegtest,
        Offline,
    )
    .unwrap();
    let node = Arc::new(Fake::new());
    let terms = swaps::Lightning {
        node: node.pubkey(),
        fee: licash_api::Fee { base: 2, ppm: 5000 },
        max: 100_000,
        slack: 80,
    };
    let provider = Provider::create(&dir.path().join("swaps.sqlite"), wallet, Config::default())
        .unwrap()
        .pay_lightning(node.clone(), terms.clone())
        .unwrap();
    let address = provider.wallet().address();
    let router = licash_swaps_service::router(Arc::new(provider), None);
    let (status, body) = call(&router, swaps::SWAPS_INFO, None).await;
    assert_eq!(status, StatusCode::OK);
    let info: swaps::Info = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.lightning, Some(terms));

    // From a hub it does not know.
    let id = licash::spend::ProposalId::from_bytes([1; 32]);
    let invoice = node.invoice(10_000_000, [3; 32], 18, Outcome::Hang);
    let pay = swaps::LightningRequest::new(&address, &id, &invoice);
    let body = serde_json::to_string(&pay).unwrap();
    let (status, answer) = call(&router, swaps::LIGHTNING_SWAPS, Some(&body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let declined = problem(&answer);
    assert!(declined.message.contains("from this hub"), "{declined:?}");
    assert!(node.sent().is_empty());
}

fn licash_swaps(dir: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_licash-swaps"))
        .arg("--data")
        .arg(dir.path().join("data"))
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

#[test]
fn init_creates_the_provider_once_and_status_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let init = licash_swaps(&dir, &["init"]);
    assert!(init.status.success(), "{init:?}");
    let address = String::from_utf8(init.stdout).unwrap();
    let message = String::from_utf8_lossy(&init.stderr);
    assert!(
        message.starts_with("Provider created. Back up wallet.sqlite and swaps.sqlite in "),
        "{message}"
    );
    assert!(
        !message.contains(address.trim()) && !message.contains("schema migrated"),
        "{message}"
    );
    assert_eq!(
        address.trim().len(),
        2 * licash::address::ADDRESS_BYTES,
        "{address}"
    );
    let machine = tempfile::tempdir().unwrap();
    let created = licash_swaps(&machine, &["--log", "json", "init"]);
    assert!(created.status.success(), "{created:?}");
    let event = String::from_utf8_lossy(&created.stderr)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|line| line["fields"]["message"] == "provider created: back up its wallet file")
        .unwrap();
    assert_eq!(event["level"], "INFO");
    assert_eq!(
        event["fields"]["address"],
        String::from_utf8_lossy(&created.stdout).trim()
    );
    let again = licash_swaps(&dir, &["init"]);
    assert!(!again.status.success());

    let status = licash_swaps(&dir, &["status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["address"], address.trim());
    assert_eq!(status["network"], "bitcoin");
    assert_eq!(status["stats"]["open"], 0);
    assert_eq!(status["swaps"], serde_json::json!([]));
    // Read by a person: what it earned first, in sat; what it did, in words.
    let text = licash_swaps(&dir, &["status"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.starts_with("earned "), "{text}");
    let said = |label: &str| {
        let line = text.lines().find(|l| l.starts_with(label));
        line.map(|l| l[label.len()..].trim().to_owned())
    };
    assert_eq!(said("earned").as_deref(), Some("0 sat"), "{text}");
    assert_eq!(said("swaps").as_deref(), Some("none"), "{text}");
    assert_eq!(said("lost").as_deref(), Some("none"), "{text}");
    assert_eq!(said("balance").as_deref(), Some("0 sat"), "{text}");
    assert_eq!(said("address"), None, "{text}");
    assert_eq!(said("events"), None, "{text}");
    let detailed = licash_swaps(&dir, &["status", "--details"]);
    assert!(detailed.status.success(), "{detailed:?}");
    let detailed = String::from_utf8(detailed.stdout).unwrap();
    assert!(detailed.contains(address.trim()), "{detailed}");
    assert!(
        detailed.lines().any(|line| line.starts_with("events")),
        "{detailed}"
    );
    let missing = licash_swaps(&tempfile::tempdir().unwrap(), &["status"]);
    assert!(!missing.status.success());
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(error.starts_with("error: "), "{error}");
    assert_eq!(error.lines().count(), 1, "{error}");
    let events = licash_swaps(&dir, &["events"]);
    assert!(events.status.success() && events.stdout.is_empty());
    // An address on Bitcoin to fund payouts at, watched from a first block,
    // given once: the same until paid.
    let unwatched = licash_swaps(&dir, &["address"]);
    assert!(!unwatched.status.success());
    assert!(String::from_utf8_lossy(&unwatched.stderr).contains("--chain-from"));
    let funding = licash_swaps(&dir, &["address", "--chain-from", "900000"]);
    assert!(funding.status.success(), "{funding:?}");
    let funding = String::from_utf8(funding.stdout).unwrap();
    assert!(funding.starts_with("bc1p"), "{funding}");
    let again = licash_swaps(&dir, &["address"]);
    assert_eq!(String::from_utf8(again.stdout).unwrap(), funding);

    let signet = tempfile::tempdir().unwrap();
    let init = licash_swaps(&signet, &["init", "--network", "signet"]);
    assert!(init.status.success(), "{init:?}");
    let status = licash_swaps(&signet, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["network"], "signet");
    let unknown = licash_swaps(&signet, &["init", "--network", "testnet3"]);
    assert_eq!(unknown.status.code(), Some(2));

    let bad = licash_swaps(&dir, &["run", "--margin", "4"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8(bad.stderr).unwrap().contains("--margin"));
    let bad = licash_swaps(&dir, &["run", "--confirm", "0"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8(bad.stderr).unwrap().contains("--confirm"));
}

#[test]
fn the_dashboard_reads_the_provider_as_json_and_draws_on_a_terminal_only() {
    let dir = tempfile::tempdir().unwrap();
    assert!(licash_swaps(&dir, &["init", "--network", "regtest"])
        .status
        .success());
    let no_service = ["--url", "http://127.0.0.1:1"];
    let json = licash_swaps(
        &dir,
        &[&["dashboard", "--json", "--range", "7d"][..], &no_service].concat(),
    );
    assert!(json.status.success(), "{json:?}");
    let view: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(
        (&view["title"], &view["range"]),
        (&"licash swaps".into(), &"7d".into())
    );
    let pages: Vec<_> = view["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["title"])
        .collect();
    assert_eq!(pages, ["Overview", "Swaps", "Liquidity", "Fees", "Health"]);
    // A provider of no hub swaps nothing.
    assert_eq!(view["health"], "bad");
    let drawn = licash_swaps(&dir, &[&["dashboard"][..], &no_service].concat());
    assert!(!drawn.status.success());
    assert!(String::from_utf8(drawn.stderr).unwrap().contains("--json"));
}

#[test]
fn a_provider_runs_unprotected_by_contracts_only_when_told_to() {
    let dir = tempfile::tempdir().unwrap();
    assert!(licash_swaps(&dir, &["init"]).status.success());
    let listen = ["--listen", "127.0.0.1:0"];
    // None opens on Bitcoin yet: refused before it serves.
    let out = licash_swaps(&dir, &[&["run"], &listen[..]].concat());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("none opens on bitcoin") && stderr.contains("--no-contracts"),
        "{stderr}"
    );
    assert!(!stderr.contains("serving at"), "{stderr}");
    // Nor kept without a chain to follow.
    let regtest = tempfile::tempdir().unwrap();
    let init = licash_swaps(&regtest, &["init", "--network", "regtest"]);
    assert!(init.status.success());
    let out = licash_swaps(&regtest, &[&["run"], &listen[..]].concat());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("kept with --chain") && stderr.contains("--no-contracts"),
        "{stderr}"
    );
}

#[test]
fn lightning_payments_need_their_node_and_enough_slack() {
    let dir = tempfile::tempdir().unwrap();
    assert!(licash_swaps(&dir, &["init", "--network", "regtest"])
        .status
        .success());
    let listen = ["--listen", "127.0.0.1:0", "--no-contracts"];
    // Too little slack to claim the client's lock in.
    let out = licash_swaps(&dir, &[&["run", "--ln-slack", "17"], &listen[..]].concat());
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--ln-slack"));
    let out = licash_swaps(
        &dir,
        &[&["run", "--lnd-macaroon", "m"], &listen[..]].concat(),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--lnd"));

    // A payment under way, recorded by a provider with a node: refused
    // without one, before it serves.
    let swaps = rusqlite::Connection::open(dir.path().join("data/swaps.sqlite")).unwrap();
    swaps
        .execute_batch(
            "INSERT INTO swaps (id, hash, from_url, to_url, address, amount, fee, paid,
                expiry_from, expiry, state, kind, created_at, updated_at)
             VALUES (x'01', x'02', 'http://a.test', 'lightning:regtest', x'03', 1000, 7,
                1007, 300, 200, 'accepted', 'lightning', 0, 0);
             INSERT INTO invoices (id, request, node) VALUES (x'01', 'lnbcrt', '02');",
        )
        .unwrap();
    drop(swaps);
    let out = licash_swaps(&dir, &[&["run"], &listen[..]].concat());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("1 Lightning payments under way: they need --lnd"),
        "{stderr}"
    );
    assert!(!stderr.contains("serving at"), "{stderr}");
}

#[test]
fn announcements_are_checked_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    let relay = ["--nostr-relay", "ws://127.0.0.1:1"];
    for (args, says) in [
        (
            &["--announce-url", "https://swaps.example"][..],
            "needs --nostr-relay",
        ),
        (&relay[..], "--announce-url"),
        (
            &[relay[0], relay[1], "--onion", "--announce-url", "ftp://x"][..],
            "ftp://",
        ),
        // An onion service may be the only URL announced.
        (&[relay[0], relay[1], "--onion"][..], "no provider"),
    ] {
        let out = licash_swaps(&dir, &[&["run"], args].concat());
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    assert!(!dir.path().join("data").exists());
}

#[tokio::test]
async fn a_new_target_hears_of_what_the_provider_says_from_its_start_on() {
    let dir = tempfile::tempdir().unwrap();
    let (_, provider) = router(&dir);
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = {
        let heard = heard.clone();
        Router::new().route(
            "/",
            post(move |body: String| async move { heard.lock().unwrap().push(body) }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("json://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, sink).await });

    provider.alert("before its start").unwrap();
    let notifier = licash_swaps_service::notify::Notifier::new(
        provider.clone(),
        "swaps",
        vec![target.parse().unwrap()],
        &dir.path().join("notify.json"),
        Vec::new(),
    )
    .unwrap();
    // Raised as it starts, before its notifications run.
    provider.alert("as it starts").unwrap();
    tokio::spawn(notifier.run());
    for _ in 0..100 {
        if !heard.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let heard = heard.lock().unwrap();
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert!(heard[0].contains("as it starts"), "{}", heard[0]);
}
