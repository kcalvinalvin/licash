mod support;

use std::{path::Path, process::Output};

use licash::{address::Address, spend::lock_hash};
use licash_api::Fee;
use licash_hub_service::lightning::{fake::Outcome, HeldState};
use serde_json::Value;
use support::{swaps::Swaps, wallet, Server};

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

#[test]
fn a_lock_pays_on_its_preimage_or_returns_once_expired() {
    let hub = Server::start(FEE);
    let dir = tempfile::tempdir().unwrap();
    let (alice, alice_at) = party(dir.path(), "alice", Some(&hub));
    let (bob, bob_at) = party(dir.path(), "bob", Some(&hub));
    let funds = serde_json::to_string(&hub.pay(&address(&alice_at), 64)).unwrap();
    ok(wallet(&alice, &["receive"], Some(&funds)));

    // Bob keeps a secret and hands Alice its hash.
    let secret = json(&bob, &["secret"]);
    let preimage = secret["preimage"].as_str().unwrap();
    let hash = secret["hash"].as_str().unwrap();
    let lock = json(&alice, &["lock", &bob_at, "10", hash]);
    let id = lock["id"].as_str().unwrap();
    assert_eq!(
        (lock["state"].as_str(), lock["amount"].as_u64()),
        (Some("locked"), Some(10))
    );
    // The latest expiry the hub admits at height 3: its window of 20, less
    // the depth of 1 and its reaction of 2.
    assert_eq!(
        (lock["fee"].as_u64(), lock["expiry"].as_u64()),
        (Some(1), Some(20))
    );
    assert_eq!(available(&alice), (0, 64));
    failed(
        wallet(&bob, &["claim", id, &"00".repeat(32)], None),
        1,
        "not the preimage",
    );
    let claimed = json(&bob, &["claim", id, preimage]);
    assert_eq!(
        (claimed["id"].as_str(), claimed["amount"].as_u64()),
        (Some(id), Some(10))
    );
    assert_eq!(available(&bob), (10, 0));
    let resumed = json(&alice, &["resume"]);
    let paid = &resumed["locks"][0];
    assert_eq!(
        (paid["state"].as_str(), paid["preimage"].as_str()),
        (Some("paid"), Some(preimage))
    );
    assert_eq!(available(&alice), (53, 0));

    // Unclaimed, a lock returns to its payer once expired, less the hub's
    // refund fee.
    let lock = json(&alice, &["lock", &bob_at, "10", hash, "--expiry", "8"]);
    let id = lock["id"].as_str().unwrap();
    hub.mine(6, None);
    failed(wallet(&bob, &["claim", id, preimage], None), 1, "too late");
    let resumed = json(&alice, &["resume"]);
    assert_eq!(resumed["locks"][0]["state"], "refunded");
    assert_eq!(available(&alice), (52, 0));
    let text = ok(wallet(&alice, &["locks"], None));
    assert!(
        text.contains("lock refunded  10 + 1  until 8") && text.contains("lock paid"),
        "{text}"
    );
    failed(
        wallet(&alice, &["lock", &bob_at, "1", "abc"], None),
        1,
        "not 32 bytes in hex",
    );
}

#[test]
fn owners_pay_invoices_and_get_paid_over_lightning() {
    let hub = Server::with_lightning(FEE);
    let dir = tempfile::tempdir().unwrap();
    let (alice, _) = party(dir.path(), "alice", Some(&hub));
    assert_eq!(json(&alice, &["hubs"])[0]["lightning"]["max_pay"], 100);

    // Paid 40 over Lightning, the gateway's fee on top, once the payer
    // pays the invoice and the wallet claims what the hub locks to it.
    let asked = wallet(&alice, &["invoice", "40"], None);
    let stderr = String::from_utf8_lossy(&asked.stderr).into_owned();
    let hash = stderr.trim().strip_prefix("invoice ").unwrap().to_owned();
    let request = ok(asked);
    let invoices = json(&alice, &["invoices"]);
    assert_eq!(
        (
            invoices[0]["state"].as_str(),
            invoices[0]["request"].as_str()
        ),
        (Some("open"), Some(request.trim()))
    );
    assert_eq!(json(&alice, &["resume"])["invoices"][0]["state"], "open");
    hub.pay_held(&hash);
    let resumed = json(&alice, &["resume"]);
    assert_eq!(resumed["invoices"][0]["state"], "received");
    assert_eq!(available(&alice), (40, 0));
    hub.round();
    let mut held = [0; 32];
    hex::decode_to_slice(&hash, &mut held).unwrap();
    assert_eq!(hub.node().held(held).unwrap().0, HeldState::Settled);
    let text = ok(wallet(&alice, &["invoices"], None));
    assert!(text.contains("invoice received  40"), "{text}");
    failed(
        wallet(&alice, &["invoice", "101"], None),
        1,
        "takes 1 to 100",
    );

    // Paying an invoice of 20 locks notes of 22 to the hub, its gateway's
    // fee on top, until the payment buys the preimage. The notes are made
    // exact by a payment to the wallet itself, for the hub's fee of 1.
    let preimage = [5; 32];
    let request = hub
        .node()
        .invoice(20_000, lock_hash(&preimage), 6, Outcome::Pay(preimage));
    let paid = ok(wallet(&alice, &["pay-invoice", &request], None));
    assert_eq!(
        paid,
        format!("paid 22 + 0\npreimage {}\n", hex::encode(preimage))
    );
    assert_eq!(available(&alice), (17, 0));
    let locks = json(&alice, &["locks"]);
    assert_eq!(locks[0]["invoice"].as_str(), Some(request.as_str()));

    // A failed payment refunds its lock at once, less the hub's refund
    // fee.
    let reason = "FAILURE_REASON_NO_ROUTE";
    let request = hub
        .node()
        .invoice(10_000, lock_hash(&[6; 32]), 6, Outcome::Fail(reason.into()));
    failed(wallet(&alice, &["pay-invoice", &request], None), 1, reason);
    assert_eq!(available(&alice), (15, 0));
    let text = ok(wallet(&alice, &["locks"], None));
    assert!(
        text.contains(&format!("lock refunded ({reason})  12 + 0")),
        "{text}"
    );

    // One under way past the wait is pending, and followed by resume.
    let preimage = [7; 32];
    let hash = lock_hash(&preimage);
    let request = hub.node().invoice(5_000, hash, 6, Outcome::Hang);
    failed(
        wallet(&alice, &["pay-invoice", &request, "--wait", "1"], None),
        75,
        "under way",
    );
    // Exact notes were at hand.
    assert_eq!(available(&alice), (8, 7));
    hub.node().resolve(hash, Outcome::Pay(preimage));
    let resumed = json(&alice, &["resume"]);
    assert_eq!(resumed["locks"][0]["state"], "paid");
    assert_eq!(available(&alice), (8, 0));

    failed(
        wallet(&alice, &["pay-invoice", "lnbcrt1nope"], None),
        1,
        "not a Lightning invoice",
    );
    let request = hub
        .node()
        .invoice(101_000, lock_hash(&[8; 32]), 6, Outcome::Hang);
    failed(
        wallet(&alice, &["pay-invoice", &request], None),
        1,
        "at most 100 sat",
    );
}

#[test]
fn hubs_announced_on_nostr_are_discovered_where_they_answer_as_the_announcer() {
    use licash_nostr::{Announcement, Announcer, Role};
    use nostr_sdk::prelude::MockRelay;

    let hub = Server::start(FEE);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let relay = runtime.block_on(MockRelay::run()).unwrap();
    let relays = [runtime.block_on(relay.url()).to_string()];
    let announce = |secret: [u8; 32], urls: &[&str], network: &str| {
        let announcement = Announcement {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            network: network.into(),
            name: Some(format!("hub {}", secret[0])),
        };
        runtime.block_on(async {
            let announcer = Announcer::new(&secret, Role::Hub, &relays).await.unwrap();
            announcer.announce(&announcement).await.unwrap();
        });
    };
    announce(
        hub.hub.nostr_secret(),
        &["http://127.0.0.1:1", &hub.url()],
        "regtest",
    );
    // Another key announcing the hub's URL, a hub down, and one of
    // another network.
    announce([3; 32], &[&hub.url()], "regtest");
    announce([4; 32], &["http://127.0.0.1:1"], "regtest");
    announce([5; 32], &[&hub.url()], "bitcoin");

    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("none.sqlite");
    let args = ["discover", "--relay", &relays[0], "--network", "regtest"];
    let found = json(&nowhere, &args);
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    let named = hub.hub.advertise().unwrap().nostr.unwrap();
    assert_eq!(found[0]["key"], named);
    assert_eq!(found[0]["urls"], serde_json::json!([hub.url()]));
    assert_eq!(
        found[0]["name"],
        format!("hub {}", hub.hub.nostr_secret()[0])
    );
    assert_eq!(found[0]["fees"]["spend"]["base"], 1);
    assert!(!nowhere.exists());
    let text = ok(wallet(&nowhere, &args, None));
    assert!(
        text.starts_with(&format!("{}  1 + 0 ppm  hub ", hub.url())),
        "{text}"
    );

    // Then trusted like any other.
    let (alice, _) = party(dir.path(), "alice", None);
    ok(wallet(
        &alice,
        &["add-hub", found[0]["urls"][0].as_str().unwrap()],
        None,
    ));
    assert_eq!(json(&alice, &["hubs"])[0]["url"], hub.url());

    failed(
        wallet(&nowhere, &["discover", "--relay", "ws://127.0.0.1:1"], None),
        1,
        "no Nostr relay answered",
    );
    failed(
        wallet(
            &nowhere,
            &[
                "--proxy",
                "http://127.0.0.1:1",
                "discover",
                "--relay",
                &relays[0],
            ],
            None,
        ),
        1,
        "socks5h://",
    );
}

#[test]
fn notes_swap_between_hubs_through_a_provider() {
    let (a, b) = (Server::start(FEE), Server::start(FEE));
    let swaps = Swaps::start(&[&a, &b], 200);
    let dir = tempfile::tempdir().unwrap();
    let (alice, alice_at) = party(dir.path(), "alice", Some(&a));
    ok(wallet(&alice, &["add-hub", &b.url()], None));
    let funds = serde_json::to_string(&a.pay(&address(&alice_at), 100)).unwrap();
    ok(wallet(&alice, &["receive"], Some(&funds)));
    let provider = swaps.url();
    let swap = |amount: &str| {
        wallet(
            &alice,
            &["swap", amount, "--to", &b.url(), "--provider", &provider],
            None,
        )
    };

    // 20 at B for 20, the provider's fee of 1 + 1% and A's lock fee at A.
    let text = ok(swap("20"));
    assert_eq!(
        text.trim(),
        format!("swapped 20 + 2 at {} for 20 at {}", a.url(), b.url())
    );
    let listed = json(&alice, &["swaps"]);
    assert_eq!(
        (listed[0]["state"].as_str(), listed[0]["fee"].as_u64()),
        (Some("done"), Some(2))
    );
    let record = swaps.until(|r| r.stage == licash_swaps::Stage::Done);
    assert_eq!((record.amount, record.fee, record.paid), (20, 2, 22));
    assert_eq!((swaps.held(&a), swaps.held(&b)), (22, 200 - 20 - 1));
    ok(wallet(&alice, &["resume"], None));
    assert_eq!(available(&alice), (100 - 22 - 1 + 20, 0));

    // Beyond what the provider swaps: nothing is locked.
    failed(swap("151"), 1, "swaps 5 to");
    assert_eq!(json(&alice, &["swaps"]).as_array().unwrap().len(), 1);

    // The provider does not answer once Alice locked: pending until it does.
    swaps
        .refuse
        .store(true, std::sync::atomic::Ordering::Relaxed);
    failed(swap("10"), 75, "refused as asked");
    assert_eq!(json(&alice, &["swaps"])[0]["state"], "open");
    failed(wallet(&alice, &["resume"], None), 75, "refused as asked");
    swaps
        .refuse
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let resumed = json(&alice, &["resume"]);
    assert_eq!(resumed["swaps"][0]["state"], "done", "{resumed}");
    assert_eq!(resumed["swaps"][0]["amount"], 10);
    let text = ok(wallet(&alice, &["swaps"], None));
    assert!(text.contains("swap done  10 + 2"), "{text}");
    swaps.until(|r| r.amount == 10 && r.stage == licash_swaps::Stage::Done);
}

#[test]
fn swap_providers_announced_on_nostr_are_discovered_where_they_answer() {
    use licash_nostr::{Announcement, Announcer, Role};
    use nostr_sdk::prelude::MockRelay;

    let (a, b) = (Server::start(FEE), Server::start(FEE));
    let swaps = Swaps::start(&[&a, &b], 200);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let relay = runtime.block_on(MockRelay::run()).unwrap();
    let relays = [runtime.block_on(relay.url()).to_string()];
    let announce = |secret: [u8; 32], role: Role, url: String| {
        let announcement = Announcement {
            urls: vec![url],
            network: "regtest".into(),
            name: Some("swaps".into()),
        };
        runtime.block_on(async {
            let announcer = Announcer::new(&secret, role, &relays).await.unwrap();
            announcer.announce(&announcement).await.unwrap();
        });
    };
    announce(
        swaps.provider.nostr_secret().unwrap(),
        Role::Swaps,
        swaps.url(),
    );
    // The hub announced as a hub, and another key announcing the provider.
    announce(a.hub.nostr_secret(), Role::Hub, a.url());
    announce([3; 32], Role::Swaps, swaps.url());

    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("none.sqlite");
    let args = [
        "discover",
        "--swaps",
        "--relay",
        &relays[0],
        "--network",
        "regtest",
    ];
    let found = json(&nowhere, &args);
    assert_eq!(found.as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found[0]["urls"], serde_json::json!([swaps.url()]));
    assert_eq!(
        (found[0]["min"].as_u64(), found[0]["max"].as_u64()),
        (Some(5), Some(150))
    );
    // It locks only at B, where it holds 200 less the lock fee.
    let hubs = found[0]["hubs"].as_array().unwrap();
    let mut max: Vec<u64> = hubs.iter().map(|h| h["max"].as_u64().unwrap()).collect();
    max.sort();
    assert_eq!(max, [0, 150]);
    let text = ok(wallet(&nowhere, &args, None));
    assert_eq!(
        text.trim(),
        format!("{}  1 + 10000 ppm  5 to 150  2 hubs  swaps", swaps.url())
    );
}
