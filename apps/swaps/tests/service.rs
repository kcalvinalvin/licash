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
use licash_api::{swaps, Code, Problem};
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
    let wallet = Wallet::create(&dir.path().join("wallet.sqlite"), &[5; 32], Offline).unwrap();
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

    let (status, body) = call(&router, licash_swaps_service::HEALTH, None).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"ok".as_slice())
    );
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
    assert_eq!(
        address.trim().len(),
        2 * licash::address::ADDRESS_BYTES,
        "{address}"
    );
    let again = licash_swaps(&dir, &["init"]);
    assert!(!again.status.success());

    let status = licash_swaps(&dir, &["status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["address"], address.trim());
    assert_eq!(status["stats"]["open"], 0);
    assert_eq!(status["swaps"], serde_json::json!([]));
    let text = licash_swaps(&dir, &["status"]);
    assert!(String::from_utf8(text.stdout).unwrap().contains("earned"));
    let events = licash_swaps(&dir, &["events"]);
    assert!(events.status.success() && events.stdout.is_empty());

    let bad = licash_swaps(&dir, &["run", "--margin", "4"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8(bad.stderr).unwrap().contains("--margin"));
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
