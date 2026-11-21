//! Notes with payments to codes: sealed for the payee alone, all of one
//! size, kept with the payment by a hub naming notes and mailed with it, and
//! never a reason for a payment not to be made or taken.

#[allow(dead_code)]
mod net;

use std::sync::Arc;

use licash::spend::ChainProfile;
use licash_api::{inbox, Fee};
use licash_wallet::{Error, LicashCode, PaymentNote, Wallet};
use net::{Conduct, Link, Net, HUB};

const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

struct Pair {
    dir: tempfile::TempDir,
    alice: Wallet<Link>,
    bob: Wallet<Link>,
    /// Alice's code, and Bob's.
    alices: LicashCode,
    bobs: LicashCode,
}

fn wallet(net: &Arc<Net>, dir: &tempfile::TempDir, seed: u8) -> Wallet<Link> {
    let path = dir.path().join(format!("{seed}.sqlite"));
    Wallet::create(&path, &[seed; 32], REGTEST, Link(net.clone())).unwrap()
}

/// Alice, holding 50 at a hub keeping payments as `conduct` says, and Bob.
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
    let code = |w: &Wallet<Link>| w.payment_code(&[HUB.into()]).unwrap().licash.unwrap();
    Pair {
        alices: code(&alice),
        bobs: code(&bob),
        dir,
        alice,
        bob,
    }
}

fn saying(text: &str, from: Option<&LicashCode>) -> PaymentNote {
    PaymentNote {
        text: Some(text.into()),
        from: from.cloned(),
    }
}

fn notes_version(dir: &tempfile::TempDir, seed: u8) -> Option<i64> {
    let db = rusqlite::Connection::open(dir.path().join(format!("{seed}.sqlite"))).unwrap();
    licash_store::component_version(&db, "payment_notes").unwrap()
}

#[tokio::test]
async fn a_note_kept_at_the_hub_tells_the_payee_who_paid() {
    let Pair {
        alice,
        bob,
        alices,
        bobs,
        ..
    } = pair(Conduct::Keeps).await;
    let note = saying("  for the  pizza ", Some(&alices));
    let paid = alice.pay_code_saying(HUB, &bobs, 8, &note).await.unwrap();
    assert!(paid.kept);
    let sealed = paid.payment.note.as_ref().expect("a sealed note");
    assert_eq!(sealed.0.len(), inbox::NOTE_SEALED);
    let said = saying("for the pizza", Some(&alices));
    assert_eq!(alice.payment_note(&paid.id).unwrap(), Some(said.clone()));

    let collected = bob.collect_inbox().await.unwrap();
    assert_eq!(collected.received.len(), 1);
    assert_eq!(bob.payment_note(&paid.id).unwrap(), Some(said));
    // The same payment handed over again, as mailed: kept once.
    assert!(matches!(
        bob.receive(&paid.payment),
        Err(Error::AlreadyReceived)
    ));
    assert_eq!(bob.payment_notes().unwrap().len(), 1);
}

#[tokio::test]
async fn a_mailed_payment_carries_its_note_and_the_payment_again_too() {
    let Pair {
        alice, bob, bobs, ..
    } = pair(Conduct::Keeps).await;
    let note = saying("rent", None);
    let paid = alice.pay_code_saying(HUB, &bobs, 8, &note).await.unwrap();
    assert_eq!(alice.payment(&paid.id).unwrap().note, paid.payment.note);
    bob.receive(&paid.payment).unwrap();
    assert_eq!(bob.payment_note(&paid.id).unwrap(), Some(note));
}

#[tokio::test]
async fn every_payment_to_a_code_goes_with_a_note_of_one_size() {
    let Pair {
        dir,
        alice,
        bob,
        alices,
        bobs,
    } = pair(Conduct::Keeps).await;
    assert_eq!(notes_version(&dir, 1), None, "made with the first note");
    let none = alice.pay_code(HUB, &bobs, 4).await.unwrap();
    assert_eq!(notes_version(&dir, 1), Some(1));
    let long = saying(&"x".repeat(licash_wallet::MAX_NOTE), Some(&alices));
    let full = alice.pay_code_saying(HUB, &bobs, 4, &long).await.unwrap();
    let sizes = [&none, &full].map(|p| p.payment.note.as_ref().unwrap().0.len());
    assert_eq!(sizes, [inbox::NOTE_SEALED; 2]);
    // One saying nothing is no note.
    bob.collect_inbox().await.unwrap();
    assert_eq!(bob.payment_note(&none.id).unwrap(), None);
    assert!(bob.payment_note(&full.id).unwrap().is_some());
    assert_eq!(alice.payment_note(&none.id).unwrap(), None);
    // A payment to an address is no payment to a code: no note.
    let plain = alice.send(HUB, &bob.address(), 2).await.unwrap();
    assert_eq!(plain.payment.note, None);
}

#[tokio::test]
async fn a_hub_keeping_no_notes_is_refused_a_note_before_anything_is_paid() {
    let Pair {
        alice, bob, bobs, ..
    } = pair(Conduct::Noteless).await;
    let note = saying("hi", None);
    let refused = alice.pay_code_saying(HUB, &bobs, 8, &note).await;
    assert!(
        matches!(&refused, Err(Error::NoteUnkept { hub }) if hub == HUB),
        "{refused:?}"
    );
    assert!(alice.sends().unwrap().is_empty());
    // Without the note it pays, the hub keeping it without one, as an
    // older hub does: Bob takes it there.
    let paid = (alice.pay_code_saying(HUB, &bobs, 8, &PaymentNote::default()))
        .await
        .unwrap();
    assert!(paid.kept);
    assert!(
        paid.payment.note.is_some(),
        "mailed with a note all the same"
    );
    assert_eq!(bob.collect_inbox().await.unwrap().received.len(), 1);
    assert_eq!(bob.payment_note(&paid.id).unwrap(), None);
}

#[tokio::test]
async fn a_note_shares_only_its_payers_own_code_and_valid_text() {
    let Pair { alice, bobs, .. } = pair(Conduct::Keeps).await;
    for note in [
        saying("not mine", Some(&bobs)),
        saying(&"x".repeat(licash_wallet::MAX_NOTE + 1), None),
        saying("a\u{202e}b", None),
        saying("a\u{7}b", None),
    ] {
        let refused = alice.pay_code_saying(HUB, &bobs, 8, &note).await;
        assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
    }
    assert!(alice.sends().unwrap().is_empty());
    // A blank note is none.
    let blank = saying(" \n ", None);
    let paid = alice.pay_code_saying(HUB, &bobs, 8, &blank).await.unwrap();
    assert_eq!(alice.payment_note(&paid.id).unwrap(), None);
}

#[tokio::test]
async fn a_tampered_note_is_no_note_and_the_payment_stands() {
    let Pair {
        alice,
        bob,
        alices,
        bobs,
        ..
    } = pair(Conduct::Keeps).await;
    let note = saying("hello", Some(&alices));
    let mut paid = alice.pay_code_saying(HUB, &bobs, 8, &note).await.unwrap();
    paid.payment.note.as_mut().unwrap().0[3] ^= 1;
    assert_eq!(bob.receive(&paid.payment).unwrap().amount, 8);
    assert_eq!(bob.payment_note(&paid.id).unwrap(), None);
    // One of another size too.
    let mut other = alice.pay_code(HUB, &bobs, 4).await.unwrap();
    other.payment.note = Some(licash_api::Hex(vec![0; 16]));
    assert_eq!(bob.receive(&other.payment).unwrap().amount, 4);
}

#[tokio::test]
async fn a_file_of_a_later_version_of_notes_refuses_a_note_yet_pays_and_takes() {
    let Pair {
        dir,
        alice,
        bob,
        bobs,
        ..
    } = pair(Conduct::Keeps).await;
    for seed in [1, 2] {
        let db = rusqlite::Connection::open(dir.path().join(format!("{seed}.sqlite"))).unwrap();
        db.execute(
            "INSERT INTO schema_versions (component, version, created_at) VALUES ('payment_notes', 2, 0)",
            [],
        )
        .unwrap();
    }
    // A note typed: refused, nothing paid, rather than dropped unsaid.
    let note = saying("hi", None);
    let refused = alice.pay_code_saying(HUB, &bobs, 8, &note).await;
    assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
    // None typed: paid, sealing nothing.
    let paid = alice
        .pay_code_saying(HUB, &bobs, 8, &PaymentNote::default())
        .await
        .unwrap();
    assert!(paid.payment.note.is_none(), "notes off");
    assert_eq!(bob.collect_inbox().await.unwrap().received.len(), 1);
    assert!(bob.payment_notes().unwrap().is_empty());
}
