#[allow(dead_code)]
mod net;

use std::sync::Arc;

use licash::spend::{lock_hash, ChainProfile};
use licash_api::{Code, Fee};
use licash_wallet::{Error, LockState, PartTo, Wallet};
use net::{Fault, Link, Net, HUB};

const FEE: Fee = Fee { base: 2, ppm: 0 };
const PREIMAGE: [u8; 32] = [7; 32];

struct Party {
    _dir: tempfile::TempDir,
    wallet: Wallet<Link>,
}

impl std::ops::Deref for Party {
    type Target = Wallet<Link>;
    fn deref(&self) -> &Wallet<Link> {
        &self.wallet
    }
}

async fn party(net: &Arc<Net>, seed: u8) -> Party {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("w.sqlite"),
        &[seed; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    wallet.add_hub(HUB).await.unwrap();
    Party { _dir: dir, wallet }
}

/// Alice, holding one note of 64, and Bob, holding nothing.
async fn parties() -> (Arc<Net>, Party, Party) {
    let net = Net::new();
    net.start(HUB, FEE);
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
    (net, alice, bob)
}

fn available(wallet: &Wallet<Link>) -> (u64, u64) {
    let balance = wallet.balance().unwrap();
    (balance[0].available, balance[0].pending)
}

fn hash() -> [u8; 32] {
    lock_hash(&PREIMAGE)
}

#[tokio::test]
async fn a_lock_pays_its_payee_on_the_preimage_and_returns_the_change() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), None)
        .await
        .unwrap();
    // The latest expiry the hub admits at height 3: its window of 20, less
    // the depth of 1 and its reaction of 2.
    assert_eq!(
        (lock.amount, lock.fee, lock.expiry, &lock.state),
        (10, 2, 20, &LockState::Locked)
    );
    assert_eq!(available(&alice), (0, 64));
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Locked
    );

    let received = bob.claim(HUB, &lock.id, &PREIMAGE).await.unwrap();
    assert_eq!(
        (received.id.as_str(), received.amount),
        (lock.id.as_str(), 10)
    );
    assert_eq!(available(&bob), (10, 0));
    assert!(matches!(
        bob.claim(HUB, &lock.id, &PREIMAGE).await,
        Err(Error::AlreadyReceived)
    ));
    let paid = alice.follow_lock(&lock.id).await.unwrap();
    assert_eq!(paid.state, LockState::Paid(PREIMAGE));
    assert_eq!(available(&alice), (52, 0));
    assert_eq!(alice.locks().unwrap()[0].state, LockState::Paid(PREIMAGE));

    // Paid, it stays so past its expiry.
    net.mine(HUB, 20, None);
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Paid(PREIMAGE)
    );
    let payment = bob.send(HUB, &alice.address(), 8).await.unwrap().payment;
    assert_eq!(alice.receive(&payment).unwrap().amount, 8);
}

#[tokio::test]
async fn a_lock_unpaid_is_refunded_once_expired_at_depth() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(8))
        .await
        .unwrap();
    net.mine(HUB, 5, None);
    // At its expiry, but not at its depth.
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Locked
    );
    assert!(matches!(
        bob.claim(HUB, &lock.id, &PREIMAGE).await,
        Err(Error::Invalid(why)) if why.contains("too late")
    ));
    net.mine(HUB, 1, None);
    let refunded = alice.resume().await.unwrap();
    assert_eq!(refunded.locks[0].state, LockState::Refunded);
    assert_eq!(available(&alice), (62, 0));
    assert!(matches!(
        bob.claim(HUB, &lock.id, &PREIMAGE).await,
        Err(Error::Invalid(why)) if why.contains("refunded")
    ));
}

#[tokio::test]
async fn a_locks_parts_are_what_it_pays_until_refunded_then_the_refund() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(8))
        .await
        .unwrap();
    let parts = |alice: &Party| {
        let parts = alice.operation_parts(&lock.id).unwrap();
        for part in &parts {
            assert_eq!(part.notes.iter().sum::<u64>(), part.amount, "{part:?}");
        }
        parts
            .into_iter()
            .map(|p| (p.to, p.amount))
            .collect::<Vec<_>>()
    };
    // Its one note of 64: 10 to Bob if paid, its fee of 2, 52 back.
    let pays = vec![
        (PartTo::Payee(bob.address()), 10),
        (PartTo::Hub, 2),
        (PartTo::Kept, 52),
    ];
    assert_eq!(parts(&alice), pays);
    net.mine(HUB, 6, None);
    assert_eq!(
        alice.resume().await.unwrap().locks[0].state,
        LockState::Refunded
    );
    // Refunded: the hub's refund fee, and the rest back.
    assert_eq!(parts(&alice), [(PartTo::Hub, 2), (PartTo::Kept, 62)]);
}

#[tokio::test]
async fn a_claim_reveals_the_preimage_only_well_before_expiry() {
    let (net, alice, bob) = parties().await;
    let late = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(6))
        .await
        .unwrap();
    let refused = bob.claim(HUB, &late.id, &PREIMAGE).await;
    assert!(
        matches!(&refused, Err(Error::Invalid(why)) if why.contains("too late")),
        "{refused:?}"
    );
    let id = licash_api::lock_id(&late.id).unwrap();
    assert_eq!(net.hub(HUB).locked(&id).unwrap().preimage, None);

    alice.receive(&net.pay(HUB, &alice.address(), 8)).unwrap();
    let lock = alice
        .lock(HUB, &bob.address(), 4, hash(), None)
        .await
        .unwrap();
    assert!(matches!(
        bob.claim(HUB, &lock.id, &[8; 32]).await,
        Err(Error::Invalid(_))
    ));
    let carol = party(&net, 3).await;
    assert!(matches!(
        carol.claim(HUB, &lock.id, &PREIMAGE).await,
        Err(Error::PaysNothing)
    ));
    assert_eq!(bob.claim(HUB, &lock.id, &PREIMAGE).await.unwrap().amount, 4);
}

#[tokio::test]
async fn a_lock_its_hub_refuses_is_given_up() {
    let (net, alice, bob) = parties().await;
    net.fail(licash_api::LOCK, Fault::Refused(Code::Timing));
    let refused = alice.lock(HUB, &bob.address(), 10, hash(), None).await;
    assert!(
        matches!(&refused, Err(Error::Refused { problem, .. }) if problem.code == Code::Timing),
        "{refused:?}"
    );
    assert_eq!(available(&alice), (64, 0));
    let lock = &alice.locks().unwrap()[0];
    assert!(matches!(&lock.state, LockState::Abandoned(_)));
    assert!(matches!(
        alice.follow_lock(&lock.id).await,
        Err(Error::Abandoned { .. })
    ));
    // Out of the hub's window.
    let refused = alice.lock(HUB, &bob.address(), 10, hash(), Some(40)).await;
    assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
}

#[tokio::test]
async fn a_lock_whose_answer_is_lost_is_taken_on_by_resume() {
    let (net, alice, bob) = parties().await;
    for fault in [Fault::Lost, Fault::Dropped] {
        net.fail(licash_api::LOCK, fault);
        let Err(Error::Pending { id, .. }) = alice.lock(HUB, &bob.address(), 4, hash(), None).await
        else {
            panic!("not pending");
        };
        assert_eq!(alice.locks().unwrap()[0].state, LockState::Requested);
        assert!(matches!(alice.abandon(&id), Err(Error::Invalid(_))));
        let resumed = alice.resume().await.unwrap();
        assert_eq!(resumed.locks.len(), 1);
        assert_eq!(
            (resumed.locks[0].id.as_str(), &resumed.locks[0].state),
            (id.as_str(), &LockState::Locked)
        );
        assert_eq!(bob.claim(HUB, &id, &PREIMAGE).await.unwrap().amount, 4);
        assert_eq!(
            alice.follow_lock(&id).await.unwrap().state,
            LockState::Paid(PREIMAGE)
        );
    }
    assert_eq!(available(&alice), (64 - 2 * (4 + 2), 0));
}

#[tokio::test]
async fn a_lock_to_its_hub_spends_exact_notes_and_pays_the_hub_only() {
    let (net, alice, _) = parties().await;
    let hub = alice.hubs().unwrap()[0].1.address;
    let lock = alice.lock(HUB, &hub, 10, hash(), None).await.unwrap();
    assert_eq!((lock.amount, lock.fee), (10, 0));
    // A payment to itself made the notes: 10, its fee of 2 and the change.
    assert_eq!(alice.sends().unwrap().len(), 1);
    assert_eq!(available(&alice), (52, 10));
    // The hub settles a lock paying it only whenever it learns the
    // preimage, past the lock's expiry too.
    net.mine(HUB, 30, None);
    let id = licash_api::lock_id(&lock.id).unwrap();
    net.hub(HUB).settle(&id, &PREIMAGE).unwrap();
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Paid(PREIMAGE)
    );
    assert_eq!(available(&alice), (52, 0));
}

#[tokio::test]
async fn an_activation_on_bitcoin_pays_the_lock() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(8))
        .await
        .unwrap();
    net.mine(HUB, 1, Some(PREIMAGE));
    net.mine(HUB, 10, None);
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Paid(PREIMAGE)
    );
    assert_eq!(available(&alice), (52, 0));
    // Paid, the lock is claimed past its expiry too.
    assert_eq!(
        bob.claim(HUB, &lock.id, &PREIMAGE).await.unwrap().amount,
        10
    );
}

#[tokio::test]
async fn a_preimage_revealed_past_expiry_does_not_pay_the_lock() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(8))
        .await
        .unwrap();
    net.mine(HUB, 6, None);
    net.mine(HUB, 1, Some(PREIMAGE));
    let id = licash_api::lock_id(&lock.id).unwrap();
    assert_eq!(net.hub(HUB).locked(&id).unwrap().preimage, Some(PREIMAGE));
    assert_eq!(
        alice.follow_lock(&lock.id).await.unwrap().state,
        LockState::Refunded
    );
    assert_eq!(available(&alice), (62, 0));
}

#[tokio::test]
async fn a_payee_sees_what_a_lock_pays_it_before_revealing_anything() {
    let (net, alice, bob) = parties().await;
    let lock = alice
        .lock(HUB, &bob.address(), 10, hash(), Some(12))
        .await
        .unwrap();
    let incoming = bob.incoming(HUB, &lock.id).await.unwrap();
    assert_eq!(
        (
            incoming.amount,
            incoming.hash,
            incoming.expiry,
            incoming.height
        ),
        (10, hash(), 12, 3)
    );
    assert_eq!((incoming.settled, incoming.preimage), (None, None));
    // It pays Alice her change, and Carol nothing.
    assert_eq!(alice.incoming(HUB, &lock.id).await.unwrap().amount, 52);
    let carol = party(&net, 3).await;
    assert!(matches!(
        carol.incoming(HUB, &lock.id).await,
        Err(Error::PaysNothing)
    ));
    assert!(matches!(
        bob.incoming(HUB, &"00".repeat(32)).await,
        Err(Error::Refused { problem, .. }) if problem.code == Code::UnknownLock
    ));

    bob.claim(HUB, &lock.id, &PREIMAGE).await.unwrap();
    net.mine(HUB, 1, None);
    let paid = bob.incoming(HUB, &lock.id).await.unwrap();
    assert_eq!(
        (paid.settled, paid.preimage, paid.height),
        (Some(licash_api::Settled::Pay), Some(PREIMAGE), 4)
    );
}
