//! Extra codes: each of its own keys, from the wallet's seed and its
//! number, naming no mailbox, paid at its hubs' inboxes, and what it is paid
//! paid on to the wallet's own code, less its hub's fee.

#[allow(dead_code)]
mod net;

use std::sync::Arc;

use licash::spend::ChainProfile;
use licash_api::Fee;
use licash_wallet::{Error, LicashCode, PaymentNote, Wallet};
use net::{Conduct, Link, Net, HUB};

const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

fn wallet(net: &Arc<Net>, dir: &tempfile::TempDir, name: &str, seed: u8) -> Wallet<Link> {
    let path = dir.path().join(format!("{name}.sqlite"));
    Wallet::create(&path, &[seed; 32], REGTEST, Link(net.clone())).unwrap()
}

struct Pair {
    net: Arc<Net>,
    dir: tempfile::TempDir,
    alice: Wallet<Link>,
    bob: Wallet<Link>,
}

/// Alice, holding 100 at a hub keeping payments, and Bob, both of it.
async fn pair() -> Pair {
    let net = Net::new();
    net.start(HUB, Fee { base: 1, ppm: 0 });
    net.conduct(HUB, Conduct::Keeps);
    *net.relays.lock().unwrap() = vec!["wss://relay.example".into()];
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (wallet(&net, &dir, "alice", 1), wallet(&net, &dir, "bob", 2));
    alice.add_hub(HUB).await.unwrap();
    bob.add_hub(HUB).await.unwrap();
    alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap();
    Pair {
        net,
        dir,
        alice,
        bob,
    }
}

fn code(wallet: &Wallet<Link>, index: u32) -> LicashCode {
    let code = wallet.extra_code(index, &[HUB.into()]).unwrap();
    code.licash.unwrap()
}

fn held(wallet: &Wallet<Link>) -> u64 {
    wallet.balance().unwrap().iter().map(|b| b.available).sum()
}

#[tokio::test]
async fn an_extra_code_shares_nothing_with_the_wallets_own_and_comes_again_from_its_seed() {
    let Pair { net, dir, bob, .. } = pair().await;
    assert!(bob.codes().unwrap().is_empty());
    let first = bob.add_code("  Bob  online ").await.unwrap();
    let second = bob.add_code("Bob at work").await.unwrap();
    assert_eq!((first.index, first.label.as_str()), (1, "Bob online"));
    assert_eq!(second.index, 2);
    let own = bob.payment_code(&[HUB.into()]).unwrap().licash.unwrap();
    assert!(own.mailbox.is_some(), "the wallet's own names its mailbox");
    let extra = code(&bob, 1);
    assert_eq!(extra.address, first.address);
    assert_eq!(extra.mailbox, None, "an extra code names no mailbox");
    assert_eq!(extra.hubs, own.hubs);
    let addresses = [own.address, first.address, second.address];
    for (i, a) in addresses.iter().enumerate() {
        assert!(!addresses[i + 1..].contains(a), "two codes alike");
    }
    // Its seed's again, in another file: the same codes, by number.
    let again = wallet(&net, &dir, "bob-again", 2);
    again.add_hub(HUB).await.unwrap();
    again.add_code("anything").await.unwrap();
    assert_eq!(again.codes().unwrap()[0].address, first.address);
    assert!(matches!(
        bob.extra_code(3, &[HUB.into()]),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(bob.add_code(" ").await, Err(Error::Invalid(_))));
}

#[tokio::test]
async fn what_an_extra_code_is_paid_is_collected_and_paid_on_to_the_wallet_less_its_fee() {
    let Pair { alice, bob, .. } = pair().await;
    let pseudonym = bob.add_code("Bob online").await.unwrap();
    let to = code(&bob, pseudonym.index);
    let note = PaymentNote {
        text: Some("for the pizza".into()),
        from: None,
    };
    let paid = alice.pay_code_saying(HUB, &to, 40, &note).await.unwrap();
    assert!(paid.kept);

    // Collected at the code first, paid on to the wallet's own code, then
    // taken there as any payment.
    let inboxed = bob.collect_inbox().await.unwrap();
    let collected = &inboxed.codes;
    assert!(collected.failed.is_empty(), "{:?}", collected.failed);
    assert_eq!(collected.received.len(), 1);
    let (index, received) = &collected.received[0];
    assert_eq!((*index, received.amount, &received.id), (1, 40, &paid.id));
    assert_eq!(collected.swept.len(), 1);
    let swept = &collected.swept[0].1;
    assert_eq!(inboxed.received.len(), 1);
    assert_eq!(inboxed.received[0].id, swept.id);
    let activity = bob.code_activity().unwrap();
    assert_eq!(activity.len(), 1);
    let at = &activity[0];
    assert_eq!(at.code, pseudonym);
    assert_eq!(at.received.len(), 1);
    assert_eq!(at.notes.get(&paid.id), Some(&note));
    assert_eq!(at.swept.len(), 1);
    let fee = at.swept[0].fee;
    assert!(fee > 0, "the hub's fee");
    assert_eq!(at.swept[0].amount + fee, 40);
    assert_eq!(held(&bob), 40 - fee);

    // Nothing more to collect or pay on.
    let again = bob.collect_inbox().await.unwrap();
    assert!(again.received.is_empty() && again.codes.received.is_empty());
    assert!(again.codes.swept.is_empty());
    // The code's payment to the wallet, once taken, is done.
    let activity = bob.code_activity().unwrap();
    assert_eq!(activity[0].swept[0].state, licash_wallet::State::Done);
}

#[tokio::test]
async fn a_note_shares_an_extra_code_proved_by_its_keys_not_the_wallets_own() {
    let Pair { alice, bob, .. } = pair().await;
    let alices = alice.add_code("Alice online").await.unwrap();
    let from = code(&alice, alices.index);
    let bobs = bob.payment_code(&[HUB.into()]).unwrap().licash.unwrap();
    let note = PaymentNote {
        text: None,
        from: Some(from.clone()),
    };
    let paid = alice.pay_code_saying(HUB, &bobs, 8, &note).await.unwrap();
    bob.collect_inbox().await.unwrap();
    let said = bob.payment_note(&paid.id).unwrap().expect("who paid");
    assert_eq!(said.from, Some(from));
    assert_ne!(said.from.unwrap().address, alice.address());
}

#[tokio::test]
async fn a_code_whose_file_is_gone_has_it_made_again_and_looks_through_its_inboxes_anew() {
    let Pair {
        dir, alice, bob, ..
    } = pair().await;
    bob.add_code("Bob online").await.unwrap();
    let to = code(&bob, 1);
    let paid = alice.pay_code(HUB, &to, 20).await.unwrap();
    std::fs::remove_dir_all(dir.path().join("bob.sqlite.codes")).unwrap();
    assert!(bob.code_activity().unwrap()[0].received.is_empty());
    // Made again, its hubs added, their inboxes looked through from the
    // start: the payment found.
    let collected = bob.collect_codes().await.unwrap();
    assert_eq!(collected.received.len(), 1, "{:?}", collected.failed);
    assert_eq!(collected.received[0].1.id, paid.id);
}

#[tokio::test]
async fn a_file_of_a_later_version_of_codes_has_none_and_makes_none() {
    let Pair { dir, bob, .. } = pair().await;
    let db = rusqlite::Connection::open(dir.path().join("bob.sqlite")).unwrap();
    db.execute(
        "INSERT INTO schema_versions (component, version, created_at) VALUES ('codes', 2, 0)",
        [],
    )
    .unwrap();
    assert!(bob.codes().unwrap().is_empty());
    assert!(matches!(bob.add_code("x").await, Err(Error::Invalid(_))));
    assert!(bob.collect_codes().await.unwrap().received.is_empty());
}

#[tokio::test]
async fn each_payment_is_taken_and_paid_on_in_a_lane_of_its_own_and_feeds_in_none() {
    use licash_api::inbox::{ACK, INBOX, TAKE};
    let Pair {
        net, alice, bob, ..
    } = pair().await;
    bob.add_code("a").await.unwrap();
    bob.add_code("b").await.unwrap();
    bob.collect_inbox().await.unwrap();
    let asked = || std::mem::take(&mut *net.asked.lock().unwrap());
    asked();

    // Idle: the public feeds alone, in no lane, whatever codes it has.
    bob.collect_inbox().await.unwrap();
    let idle = asked();
    assert!(!idle.is_empty());
    assert!(
        idle.iter()
            .all(|(url, lane)| lane.is_none() && url.contains(INBOX) && !url.ends_with(TAKE)),
        "{idle:?}"
    );

    for index in [1, 2] {
        alice.pay_code(HUB, &code(&bob, index), 30).await.unwrap();
    }
    asked();
    // Within an operation: its feeds still in none.
    let collecting = bob.collect_inbox();
    let inboxed = licash_wallet::in_lane(licash_wallet::Lane::Operation, collecting)
        .await
        .unwrap();
    assert_eq!(inboxed.codes.swept.len(), 2, "{inboxed:?}");
    assert_eq!(inboxed.received.len(), 2);
    let all = asked();
    let lane_of = |pick: &dyn Fn(&str) -> bool| -> Vec<Option<u64>> {
        all.iter()
            .filter(|(url, _)| pick(url))
            .map(|(_, lane)| *lane)
            .collect()
    };
    let feeds = lane_of(&|url| url.contains(INBOX) && !url.ends_with(TAKE) && !url.ends_with(ACK));
    assert!(feeds.iter().all(Option::is_none), "{all:?}");
    // Two taken at the codes, two paid on taken by the wallet: four takes,
    // in four lanes.
    let takes = lane_of(&|url| url.ends_with(TAKE));
    assert_eq!(takes.len(), 4, "{all:?}");
    let mut apart: Vec<u64> = takes
        .iter()
        .map(|lane| lane.expect("a take in a lane"))
        .collect();
    apart.sort_unstable();
    apart.dedup();
    assert_eq!(apart.len(), 4, "two takes in one lane: {all:?}");
    // Each acknowledged in its take's lane; the payments on in others.
    let acks = lane_of(&|url| url.ends_with(ACK));
    assert!(
        acks.iter()
            .all(|lane| lane.is_some_and(|l| apart.contains(&l))),
        "{all:?}"
    );
    let rest: Vec<u64> = (all.iter())
        .filter(|(url, _)| !url.contains(INBOX))
        .filter_map(|(_, lane)| *lane)
        .collect();
    let mut paying = rest.clone();
    paying.sort_unstable();
    paying.dedup();
    assert!(
        paying.len() >= 2,
        "the payments on in one lane, or none: {all:?}"
    );
    assert!(
        rest.iter().all(|lane| !apart.contains(lane)),
        "a payment on in a take's lane: {all:?}"
    );
}
