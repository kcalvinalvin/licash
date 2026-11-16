//! Payments to payment codes reaching their payees: kept at their hub for
//! the payee to collect there, acknowledged through it, and mailed only
//! once no acknowledgment came in time, or at once if the hub keeps none.

#[allow(dead_code)]
mod net;

use std::sync::Arc;

use licash::{address::OwnerKeys, spend::ChainProfile};
use licash_api::{inbox, Fee};
use licash_wallet::{Delivery, Error, LicashCode, Wallet, ACK_WITHIN};
use net::{Conduct, Fault, Link, Net, HUB};

const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

struct Pair {
    net: Arc<Net>,
    dir: tempfile::TempDir,
    alice: Wallet<Link>,
    bob: Wallet<Link>,
    /// Bob's code: his hub and mailbox.
    code: LicashCode,
}

fn wallet(net: &Arc<Net>, dir: &tempfile::TempDir, seed: u8) -> Wallet<Link> {
    let path = dir.path().join(format!("{seed}.sqlite"));
    Wallet::create(&path, &[seed; 32], REGTEST, Link(net.clone())).unwrap()
}

/// Alice, holding 50 at the hub, and Bob, whose code names it and his
/// mailbox on its relays.
async fn pair(conduct: Conduct) -> Pair {
    let net = Net::new();
    net.start(HUB, Fee { base: 1, ppm: 0 });
    net.conduct(HUB, conduct);
    *net.relays.lock().unwrap() = vec!["wss://relay.example".into()];
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (wallet(&net, &dir, 1), wallet(&net, &dir, 2));
    alice.add_hub(HUB).await.unwrap();
    bob.add_hub(HUB).await.unwrap();
    alice.receive(&net.pay(HUB, &alice.address(), 50)).unwrap();
    let code = bob.payment_code(&[HUB.into()], false).unwrap();
    let code = code.licash.unwrap();
    assert!(code.mailbox.is_some());
    Pair {
        net,
        dir,
        alice,
        bob,
        code,
    }
}

/// Time passes for the deliveries of the wallet of `seed`: `by` seconds.
fn later(dir: &tempfile::TempDir, seed: u8, by: i64) {
    let db = rusqlite::Connection::open(dir.path().join(format!("{seed}.sqlite"))).unwrap();
    db.execute_batch(&format!(
        "UPDATE deliveries SET kept_at = kept_at - {by}, mail_at = mail_at - {by};
         UPDATE sends SET created_at = created_at - {by}, done_at = done_at - {by};"
    ))
    .unwrap();
}

fn held(wallet: &Wallet<Link>) -> u64 {
    wallet.balance().unwrap().iter().map(|b| b.available).sum()
}

#[tokio::test]
async fn a_payment_to_a_code_is_collected_at_its_hub_and_never_mailed() {
    let Pair {
        net,
        dir,
        alice,
        bob,
        code,
    } = pair(Conduct::Keeps).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    assert!(paid.kept);
    let Some(Delivery::Kept { mail_at }) = alice.delivery(&paid.id).unwrap() else {
        panic!("{:?}", alice.delivery(&paid.id));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((now + ACK_WITHIN - 5..=now + ACK_WITHIN).contains(&mail_at));
    assert_eq!(alice.delivering().unwrap(), 1);
    assert!(alice.due().await.unwrap().is_empty(), "not before the hour");
    assert_eq!(net.kept(HUB), (1, 0));

    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.failed.is_empty(), "{:?}", collected.failed);
    assert_eq!(collected.served, 1);
    assert_eq!(collected.received.len(), 1);
    assert_eq!(collected.received[0].id, paid.id);
    assert_eq!(collected.received[0].amount, 8);
    assert_eq!(held(&bob), 8);
    assert_eq!(net.kept(HUB), (1, 1), "Bob told his ack");
    // Once: the inbox is looked through from where it was.
    let again = bob.collect_inbox().await.unwrap();
    assert!(again.received.is_empty() && again.failed.is_empty());

    // Alice hears of it, and mails nothing.
    assert!(alice.due().await.unwrap().is_empty());
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::Collected));
    assert_eq!(alice.delivering().unwrap(), 0);
    // Not even once the hour is over.
    later(&dir, 1, ACK_WITHIN + 1);
    assert!(alice.due().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_payment_not_collected_within_the_hour_is_mailed_again_until_it_is() {
    let Pair {
        dir,
        alice,
        bob,
        code,
        ..
    } = pair(Conduct::Keeps).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    later(&dir, 1, ACK_WITHIN + 1);
    let due = alice.due().await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, paid.id);
    assert_eq!(Some(due[0].mailbox), code.mailbox);
    assert_eq!(due[0].relays, ["wss://relay.example"]);
    assert_eq!(due[0].payment.hub, HUB);

    // The relays fail: mailed again a minute later, not before.
    alice.mailed(&paid.id, Err("relays down".into())).unwrap();
    let failed = Delivery::Mailing {
        tries: 1,
        error: Some("relays down".into()),
    };
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(failed));
    assert!(alice.due().await.unwrap().is_empty());
    assert_eq!(alice.delivering().unwrap(), 1);
    later(&dir, 1, 61);
    let due = alice.due().await.unwrap();
    assert_eq!(due.len(), 1);
    alice.mailed(&paid.id, Ok(1)).unwrap();
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::Mailed));
    assert_eq!(alice.delivering().unwrap(), 0);
    later(&dir, 1, ACK_WITHIN);
    assert!(alice.due().await.unwrap().is_empty(), "mailed once");

    // Bob, who takes it from his mailbox, has it once.
    assert_eq!(bob.receive(&due[0].payment).unwrap().amount, 8);
    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.received.is_empty() && collected.failed.is_empty());
}

#[tokio::test]
async fn a_hub_keeping_no_payments_has_them_mailed_at_once() {
    let Pair {
        net,
        alice,
        bob,
        code,
        ..
    } = pair(Conduct::None).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    assert!(!paid.kept);
    let due = alice.due().await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, paid.id);
    assert_eq!(net.kept(HUB), (0, 0));
    // Bob finds no inbox there, which is no failure.
    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.received.is_empty() && collected.failed.is_empty());
    assert_eq!(collected.served, 0);
}

#[tokio::test]
async fn a_hub_withholding_a_payment_only_delays_it_by_the_hour() {
    let Pair {
        dir,
        alice,
        bob,
        code,
        ..
    } = pair(Conduct::Withholds).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    assert!(paid.kept, "as the hub says");
    assert!(bob.collect_inbox().await.unwrap().received.is_empty());
    assert!(alice.due().await.unwrap().is_empty());
    later(&dir, 1, ACK_WITHIN + 1);
    let due = alice.due().await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(bob.receive(&due[0].payment).unwrap().amount, 8);
}

#[tokio::test]
async fn an_ack_the_hub_makes_up_is_not_the_payees() {
    let Pair {
        dir, alice, code, ..
    } = pair(Conduct::Forges).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    assert!(alice.due().await.unwrap().is_empty());
    assert!(matches!(
        alice.delivery(&paid.id).unwrap(),
        Some(Delivery::Kept { .. })
    ));
    later(&dir, 1, ACK_WITHIN + 1);
    assert_eq!(alice.due().await.unwrap().len(), 1, "mailed all the same");
}

#[tokio::test]
async fn a_hub_not_answering_for_acks_does_not_have_payments_mailed_early() {
    let Pair {
        net, alice, code, ..
    } = pair(Conduct::Keeps).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    net.fail(inbox::ACKS, Fault::Lost);
    assert!(alice.due().await.unwrap().is_empty());
    assert!(matches!(
        alice.delivery(&paid.id).unwrap(),
        Some(Delivery::Kept { .. })
    ));
}

#[tokio::test]
async fn entries_others_put_in_the_way_do_not_hold_the_inbox() {
    let Pair { net, bob, .. } = pair(Conduct::Keeps).await;
    let stranger = OwnerKeys::from_seed(&[0xee; 32]).unwrap().address();
    // Under Bob's tag: one he cannot take, one paying him nothing.
    net.keep_junk(HUB, &bob.address());
    net.pay_kept(HUB, &stranger, 4, &bob.address());
    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.received.is_empty() && collected.failed.is_empty());
    assert_eq!(net.kept(HUB), (2, 0), "no ack of either");
    // Passed: what comes after them is collected, once.
    net.pay_kept(HUB, &bob.address(), 8, &bob.address());
    let collected = bob.collect_inbox().await.unwrap();
    assert_eq!(collected.received.len(), 1);
    assert_eq!(collected.received[0].amount, 8);
    assert_eq!(net.kept(HUB), (3, 1));
    assert!(bob.collect_inbox().await.unwrap().received.is_empty());
}

#[tokio::test]
async fn an_inbox_not_looked_through_is_looked_through_again_from_where_it_was() {
    let Pair { net, bob, .. } = pair(Conduct::Keeps).await;
    net.pay_kept(HUB, &bob.address(), 8, &bob.address());
    net.fail(inbox::TAKE, Fault::Lost);
    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.received.is_empty());
    assert_eq!(collected.failed.len(), 1);
    assert_eq!(collected.failed[0].0, HUB);
    let collected = bob.collect_inbox().await.unwrap();
    assert_eq!(collected.received.len(), 1);

    // Its ack lost, Bob has it and tells it again next time.
    net.pay_kept(HUB, &bob.address(), 4, &bob.address());
    net.fail(inbox::ACK, Fault::Lost);
    let collected = bob.collect_inbox().await.unwrap();
    assert_eq!(collected.received.len(), 1);
    assert_eq!(collected.failed.len(), 1);
    assert_eq!(net.kept(HUB), (2, 1));
    let collected = bob.collect_inbox().await.unwrap();
    assert!(collected.received.is_empty() && collected.failed.is_empty());
    assert_eq!(net.kept(HUB), (2, 2));
    assert_eq!(held(&bob), 12);
}

#[tokio::test]
async fn a_payment_whose_release_went_unanswered_is_kept_once_resumed() {
    let Pair {
        net,
        alice,
        bob,
        code,
        ..
    } = pair(Conduct::Keeps).await;
    net.fail(licash_api::RELEASE, Fault::Dropped);
    let id = match alice.pay_code(HUB, &code, 8).await {
        Err(Error::Pending { id, .. }) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(alice.delivery(&id).unwrap(), None, "not released yet");
    assert_eq!(alice.delivering().unwrap(), 0);
    assert!(alice.due().await.unwrap().is_empty());
    let resumed = alice.resume().await.unwrap();
    assert_eq!(resumed.paid.len(), 1);
    assert!(resumed.paid[0].kept);
    assert_eq!(net.kept(HUB), (1, 0), "kept once");
    assert!(matches!(
        alice.delivery(&id).unwrap(),
        Some(Delivery::Kept { .. })
    ));
    assert_eq!(bob.collect_inbox().await.unwrap().received.len(), 1);
    assert!(alice.due().await.unwrap().is_empty());
    assert_eq!(alice.delivery(&id).unwrap(), Some(Delivery::Collected));
}

#[tokio::test]
async fn a_payment_to_a_code_naming_no_mailbox_is_handed_over_unless_collected() {
    let Pair {
        net,
        dir,
        alice,
        bob,
        mut code,
    } = pair(Conduct::Keeps).await;
    code.mailbox = None;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    assert!(paid.kept);
    later(&dir, 1, ACK_WITHIN + 1);
    assert!(alice.due().await.unwrap().is_empty(), "nowhere to mail it");
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::HandOver));
    assert_eq!(alice.delivering().unwrap(), 0);
    // Collected late all the same.
    assert_eq!(bob.collect_inbox().await.unwrap().received.len(), 1);
    alice.due().await.unwrap();
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::Collected));

    // At a hub keeping none, at once.
    net.conduct(HUB, Conduct::None);
    let paid = alice.pay_code(HUB, &code, 4).await.unwrap();
    assert!(!paid.kept);
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::HandOver));
}

#[tokio::test]
async fn a_payment_neither_collected_nor_mailed_in_the_days_kept_is_given_up() {
    let Pair {
        dir, alice, code, ..
    } = pair(Conduct::Keeps).await;
    let paid = alice.pay_code(HUB, &code, 8).await.unwrap();
    later(&dir, 1, inbox::KEPT_FOR + 1);
    assert!(alice.due().await.unwrap().is_empty());
    assert_eq!(alice.delivery(&paid.id).unwrap(), Some(Delivery::GivenUp));
    assert_eq!(alice.delivering().unwrap(), 0);
    // To hand over still.
    assert_eq!(alice.payment(&paid.id).unwrap().hub, HUB);
}

#[tokio::test]
async fn a_payment_sent_plainly_is_neither_kept_nor_mailed() {
    let Pair {
        net, alice, bob, ..
    } = pair(Conduct::Keeps).await;
    let paid = alice.send(HUB, &bob.address(), 8).await.unwrap();
    assert!(!paid.kept);
    assert_eq!(alice.delivery(&paid.id).unwrap(), None);
    assert!(alice.due().await.unwrap().is_empty());
    assert_eq!(net.kept(HUB), (0, 0));
}
