#[allow(dead_code)]
mod support;

use std::{path::Path, process::Output};

use axum::{extract::State, http::StatusCode, routing::post, Router};
use licash_api::Fee;
use licash_chain_client::Source;
use serde_json::Value;
use support::{wallet, Server};

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn json(file: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(wallet(file, &all, None))).unwrap()
}

#[test]
fn an_accepted_spend_with_a_lost_response_keeps_its_inputs_and_retries_identically() {
    let hub = Server::with_tweaks(Fee::default());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("wallet.sqlite");
    ok(wallet(&file, &["init"], None));
    ok(wallet(&file, &["add-hub", &hub.url()], None));
    let code = json(&file, &["code", "--sp"]);
    let code: licash_wallet::PaymentCode = code["code"].as_str().unwrap().parse().unwrap();
    let funded = hub.pay_silent(&code.silent.unwrap().to_string(), 3_000, 1);
    hub.round();
    ok(wallet(
        &file,
        &["onchain", "--chain", hub.esplora(), "scan"],
        None,
    ));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let source: Source = hub.esplora().parse().unwrap();
    let app = Router::new()
        .route(
            "/tx",
            post(|State(source): State<Source>, body: String| async move {
                let tx: bitcoin::Transaction =
                    bitcoin::consensus::deserialize(&hex::decode(body).unwrap()).unwrap();
                source.broadcast(&tx).await.unwrap();
                (StatusCode::SERVICE_UNAVAILABLE, "response lost after relay")
            }),
        )
        .with_state(source);
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("esplora+http://{}", listener.local_addr().unwrap());
    let serving = runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let failed = wallet(
        &file,
        &[
            "onchain",
            "--chain",
            &url,
            "send",
            "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080",
            "1000",
            "--fee-rate",
            "2",
        ],
        None,
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("broadcast outcome unknown"));
    let accepted = hub.sent();
    assert_eq!(accepted.len(), 1);
    let txid = accepted[0].compute_txid().to_string();
    assert!(!accepted[0].input[0].witness.is_empty());
    let coins = json(&file, &["onchain", "coins"]);
    assert_eq!(coins[0]["spending"], txid);
    ok(wallet(
        &file,
        &["onchain", "--chain", hub.esplora(), "scan"],
        None,
    ));
    assert_eq!(hub.sent(), accepted);
    hub.confirm_sent();
    hub.round();
    ok(wallet(
        &file,
        &["onchain", "--chain", hub.esplora(), "scan"],
        None,
    ));
    let coins = json(&file, &["onchain", "coins"]);
    assert!(coins
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["outpoint"] != funded.to_string()));
    serving.abort();
}
