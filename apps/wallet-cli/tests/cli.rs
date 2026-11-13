mod support;

use std::{path::Path, process::Output};

use licash::address::Address;
use licash_api::Fee;
use serde_json::Value;
use support::{wallet, Server};

const FEE: Fee = Fee { base: 1, ppm: 0 };

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn json(wallet_file: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(wallet(wallet_file, &all, None))).unwrap()
}

fn failed(output: Output, code: i32, says: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{stderr}");
    assert!(stderr.contains(says), "{stderr:?} lacks {says:?}");
}

/// A new wallet trusting `hub`, and its address.
fn party(dir: &Path, name: &str, hub: Option<&Server>) -> (std::path::PathBuf, String) {
    let file = dir.join(name).join("wallet.sqlite");
    let created = json(&file, &["init"]);
    let address = created["address"].as_str().unwrap().to_owned();
    if let Some(hub) = hub {
        let added = json(&file, &["add-hub", &hub.url()]);
        assert_eq!(added["fees"]["spend"]["base"], 1);
    }
    (file, address)
}

fn address(hex: &str) -> Address {
    Address::from_bytes(&hex::decode(hex).unwrap()).unwrap()
}

fn available(file: &Path) -> (u64, u64) {
    let balance = json(file, &["balance"]);
    let rows = balance.as_array().unwrap();
    rows.iter().fold((0, 0), |(a, p), row| {
        (
            a + row["available"].as_u64().unwrap(),
            p + row["pending"].as_u64().unwrap(),
        )
    })
}

#[test]
fn owners_pay_and_get_paid_on_the_command_line() {
    let hub = Server::start(FEE);
    let dir = tempfile::tempdir().unwrap();
    let (alice, alice_at) = party(dir.path(), "alice", Some(&hub));
    let (bob, bob_at) = party(dir.path(), "bob", Some(&hub));
    assert_eq!(ok(wallet(&alice, &["address"], None)).trim(), alice_at);

    // Funded from outside, through a file; its notes unlinked at once.
    let funds = dir.path().join("funds.json");
    let payment = hub.pay(&address(&alice_at), 100);
    std::fs::write(&funds, serde_json::to_string(&payment).unwrap()).unwrap();
    let received = json(&alice, &["receive", funds.to_str().unwrap()]);
    assert_eq!(
        (received["amount"].as_u64(), received["unlinked"].as_u64()),
        (Some(100), Some(3))
    );
    assert_eq!(available(&alice), (100, 0));

    // Alice pays Bob; the payment goes from her stdout to his stdin.
    let sent = wallet(&alice, &["send", &bob_at, "30"], None);
    let id = String::from_utf8_lossy(&sent.stderr).trim().to_owned();
    let paid = ok(sent);
    let received = ok(wallet(&bob, &["receive"], Some(&paid)));
    assert_eq!(received.trim(), "received 30");
    let receipts = json(&bob, &["received"]);
    assert_eq!(
        format!("payment {}", receipts[0]["id"].as_str().unwrap()),
        id
    );
    assert_eq!(receipts[0]["amount"], 30);
    assert_eq!(available(&alice), (69, 0));
    assert_eq!(available(&bob), (30, 0));
    let text = ok(wallet(&alice, &["balance"], None));
    assert!(
        text.contains(&format!("{}  69", hub.url())) && text.contains("total  69"),
        "{text}"
    );

    let history = json(&alice, &["history"]);
    let sent = &history[0];
    assert_eq!(
        (sent["amount"].as_u64(), sent["fee"].as_u64()),
        (Some(30), Some(1))
    );
    assert_eq!(
        (sent["state"].as_str(), sent["payee"].as_str()),
        (Some("done"), Some(bob_at.as_str()))
    );
    let again = ok(wallet(
        &alice,
        &["payment", sent["id"].as_str().unwrap()],
        None,
    ));
    assert_eq!(again, paid);

    failed(
        wallet(&bob, &["receive"], Some(&paid)),
        1,
        "received already",
    );
    failed(wallet(&bob, &["receive"], Some("{}")), 1, "not a payment");
    failed(
        wallet(&alice, &["send", &bob_at, "69"], None),
        1,
        "70 needed, 69 available",
    );
    failed(
        wallet(&alice, &["send", "00ff", "1"], None),
        1,
        "not an address",
    );
    failed(wallet(&alice, &["init"], None), 1, "");
    failed(
        wallet(&dir.path().join("nobody"), &["balance"], None),
        1,
        "licash-wallet init",
    );

    // A hub not trusted yet, unless asked to.
    let (carol, carol_at) = party(dir.path(), "carol", None);
    let paid = serde_json::to_string(&hub.pay(&address(&carol_at), 8)).unwrap();
    failed(wallet(&carol, &["receive"], Some(&paid)), 1, "--trust");
    let received = ok(wallet(
        &carol,
        &["--json", "receive", "--trust"],
        Some(&paid),
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&received).unwrap()["amount"],
        8
    );
    let hubs = json(&carol, &["hubs"]);
    assert_eq!(hubs[0]["url"].as_str(), Some(hub.url().as_str()));
}

#[test]
fn a_payment_while_its_hub_is_down_finishes_once_it_is_back() {
    let mut hub = Server::start(FEE);
    let dir = tempfile::tempdir().unwrap();
    let (alice, alice_at) = party(dir.path(), "alice", Some(&hub));
    let (bob, bob_at) = party(dir.path(), "bob", Some(&hub));
    let funds = serde_json::to_string(&hub.pay(&address(&alice_at), 64)).unwrap();
    ok(wallet(&alice, &["receive"], Some(&funds)));

    hub.down();
    failed(
        wallet(&alice, &["send", &bob_at, "10"], None),
        75,
        "pending",
    );
    assert_eq!(available(&alice), (0, 64));
    failed(wallet(&alice, &["resume"], None), 75, "pending");
    assert_eq!(json(&alice, &["history"])[0]["state"], "offered");

    hub.up();
    let resumed = json(&alice, &["resume"]);
    let payment = serde_json::to_string(&resumed["paid"][0]["payment"]).unwrap();
    assert_eq!(
        ok(wallet(&bob, &["receive"], Some(&payment))).trim(),
        "received 10"
    );
    assert_eq!(available(&alice), (53, 0));
    assert_eq!(json(&alice, &["history"])[0]["state"], "done");

    // Given up while its hub is down, the payment frees its notes.
    hub.down();
    failed(
        wallet(&alice, &["send", &bob_at, "10"], None),
        75,
        "pending",
    );
    let id = json(&alice, &["history"])[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(wallet(&alice, &["abandon", &id], None));
    assert_eq!(available(&alice), (53, 0));
    failed(wallet(&alice, &["abandon", &id], None), 1, "unsigned");
    hub.up();
    assert_eq!(json(&alice, &["resume"])["paid"], Value::Array(vec![]));
}

#[test]
fn a_restored_backup_drops_the_notes_spent_since() {
    let hub = Server::start(FEE);
    let dir = tempfile::tempdir().unwrap();
    let (alice, alice_at) = party(dir.path(), "alice", Some(&hub));
    let (_, bob_at) = party(dir.path(), "bob", Some(&hub));
    let funds = serde_json::to_string(&hub.pay(&address(&alice_at), 100)).unwrap();
    ok(wallet(&alice, &["receive"], Some(&funds)));
    let backup = dir.path().join("backup.sqlite");
    std::fs::copy(&alice, &backup).unwrap();

    ok(wallet(&alice, &["send", &bob_at, "60"], None));
    assert_eq!(available(&backup), (100, 0));
    assert_eq!(json(&backup, &["check"])["dropped"], 64);
    assert_eq!(available(&backup), (36, 0));
    assert_eq!(json(&backup, &["check"])["dropped"], 0);
    // Spent from the backup, the 32 is gone from the first file too.
    ok(wallet(&backup, &["send", &bob_at, "30"], None));
    assert_eq!(json(&alice, &["check"])["dropped"], 32);
    assert_eq!(available(&alice), (7, 0));
}
