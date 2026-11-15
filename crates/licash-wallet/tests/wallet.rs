mod net;

use std::{path::Path, sync::Arc};

use licash::spend::ChainProfile;
use licash_api::{Code, Fee, Hex, Payment};
use licash_wallet::{Balance, Error, HubId, State, Wallet};
use net::{Fault, Link, Net, HUB};

const FEE: Fee = Fee { base: 2, ppm: 0 };
const OFFER: &str = licash_api::OFFER;
const RELEASE: &str = licash_api::RELEASE;
const UNLINK: &str = licash_api::UNLINK;

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

/// A wallet of the owner of `[seed; 32]` that added the hub at [`HUB`].
async fn party(net: &Arc<Net>, seed: u8) -> Party {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("w.sqlite"),
        &[seed; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    wallet.add_hub(&format!("{HUB}/")).await.unwrap();
    Party { _dir: dir, wallet }
}

fn available(wallet: &Wallet<Link>) -> (u64, u64) {
    let balance = wallet.balance().unwrap();
    balance
        .iter()
        .fold((0, 0), |(a, p), b| (a + b.available, p + b.pending))
}

fn same(a: &Payment, b: &Payment) -> bool {
    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
}

fn network() -> Arc<Net> {
    let net = Net::new();
    net.start(HUB, FEE);
    net
}

#[tokio::test]
async fn a_payment_pays_its_payee_and_the_hub_its_fee() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    let received = alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap();
    assert_eq!((received.amount, received.hub.as_str()), (100, HUB));
    assert_eq!(
        alice.balance().unwrap(),
        [Balance {
            hub: HUB.into(),
            available: 100,
            pending: 0
        }]
    );

    let paid = alice.send(HUB, &bob.address(), 30).await.unwrap();
    let payment = paid.payment;
    assert_eq!(payment.hub, HUB);
    let received = bob.receive(&payment).unwrap();
    assert_eq!(
        (received.id.as_str(), received.amount),
        (paid.id.as_str(), 30)
    );
    assert_eq!(bob.receipts().unwrap(), [received]);
    assert_eq!(available(&alice), (68, 0));
    assert_eq!(available(&bob), (30, 0));

    let sends = alice.sends().unwrap();
    assert_eq!(sends.len(), 1);
    let sent = &sends[0];
    assert_eq!(sent.id, paid.id);
    assert_eq!((sent.amount, sent.fee, &sent.state), (30, 2, &State::Done));
    assert_eq!(sent.payee, bob.address());
    assert!(same(&alice.payment(&sent.id).unwrap(), &payment));

    // Received once only, and by its payee only.
    assert!(matches!(bob.receive(&payment), Err(Error::AlreadyReceived)));
    let carol = party(&net, 3).await;
    assert!(matches!(carol.receive(&payment), Err(Error::PaysNothing)));
    let mut forged = payment.clone();
    forged.unlock = Hex(vec![7; 32]);
    assert!(matches!(carol.receive(&forged), Err(Error::Invalid(_))));

    // Bob pays on what he received, Alice on her change.
    let back = bob.send(HUB, &alice.address(), 28).await.unwrap();
    assert_eq!(alice.receive(&back.payment).unwrap().amount, 28);
    assert_eq!(available(&bob), (0, 0));
    let all = alice.send(HUB, &bob.address(), 94).await.unwrap();
    assert_eq!(bob.receive(&all.payment).unwrap().amount, 94);
    assert_eq!(available(&alice), (0, 0));
    assert_eq!(alice.receipts().unwrap().len(), 2);
}

#[tokio::test]
async fn the_notes_counted_are_those_spendable_by_denomination() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    assert_eq!(alice.notes().unwrap(), []);
    alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap();
    alice.receive(&net.pay(HUB, &alice.address(), 4)).unwrap();
    let notes = |wallet: &Wallet<Link>| -> Vec<(u64, u64)> {
        let notes = wallet.notes().unwrap();
        assert!(notes.iter().all(|n| n.hub == HUB));
        notes.iter().map(|n| (n.denomination, n.count)).collect()
    };
    assert_eq!(notes(&alice), [(64, 1), (32, 1), (4, 2)]);

    // Notes held by a payment under way are not spendable.
    net.fail(RELEASE, Fault::Lost);
    assert!(matches!(
        alice.send(HUB, &bob.address(), 30).await,
        Err(Error::Pending { .. })
    ));
    let (available, pending) = available(&alice);
    let counted: u64 = notes(&alice).iter().map(|(d, c)| d * c).sum();
    assert_eq!((counted, pending), (available, 104 - available));
    assert!(pending > 0);
}

#[tokio::test]
async fn a_payment_cut_off_at_any_step_finishes_on_resume() {
    for (path, fault) in [
        (OFFER, Fault::Lost),
        (OFFER, Fault::Dropped),
        (RELEASE, Fault::Lost),
        (RELEASE, Fault::Dropped),
    ] {
        let net = network();
        let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
        alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
        net.fail(path, fault);
        let id = match alice.send(HUB, &bob.address(), 10).await {
            Err(Error::Pending { id, .. }) => id,
            other => panic!("{path} {fault:?}: {other:?}"),
        };
        assert_eq!(available(&alice), (0, 64), "{path} {fault:?}");
        let state = if path == OFFER {
            State::Offered
        } else {
            State::Signed
        };
        assert_eq!(alice.sends().unwrap()[0].state, state);

        let resumed = alice.resume().await.unwrap();
        assert_eq!(resumed.paid.len(), 1, "{path} {fault:?}: {resumed:?}");
        assert_eq!(resumed.paid[0].id, id);
        assert_eq!(bob.receive(&resumed.paid[0].payment).unwrap().amount, 10);
        assert_eq!(available(&alice), (52, 0));
        assert!(alice.resume().await.unwrap().paid.is_empty());
    }
}

#[tokio::test]
async fn a_wallet_reopened_resumes_where_it_stopped() {
    let net = network();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.sqlite");
    let bob = party(&net, 2).await;
    {
        let alice = Wallet::create(
            &path,
            &[1; 32],
            ChainProfile::Core30DefaultRegtest,
            Link(net.clone()),
        )
        .unwrap();
        alice.add_hub(HUB).await.unwrap();
        alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
        net.fail(RELEASE, Fault::Dropped);
        assert!(matches!(
            alice.send(HUB, &bob.address(), 10).await,
            Err(Error::Pending { .. })
        ));
    }
    assert!(matches!(
        Wallet::create(
            &path,
            &[1; 32],
            ChainProfile::Core30DefaultRegtest,
            Link(net.clone())
        ),
        Err(Error::Setup(_))
    ));
    let alice = Wallet::open(&path, Link(net.clone())).unwrap();
    let owner = licash::address::OwnerKeys::from_seed(&[1; 32]).unwrap();
    assert_eq!(alice.address(), owner.address());
    let resumed = alice.resume().await.unwrap();
    assert_eq!(bob.receive(&resumed.paid[0].payment).unwrap().amount, 10);
    assert_eq!(available(&alice), (52, 0));
    assert!(matches!(
        Wallet::open(Path::new("/nonexistent/w.sqlite"), Link(net)),
        Err(Error::Setup(_))
    ));
}

#[tokio::test]
async fn fees_changed_since_the_hub_was_added_are_learned_and_paid_once() {
    let net = network();
    *net.stale.lock().unwrap() = Some(Fee { base: 0, ppm: 0 });
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    *net.stale.lock().unwrap() = None;
    alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();

    let payment = alice.send(HUB, &bob.address(), 10).await.unwrap().payment;
    assert_eq!(bob.receive(&payment).unwrap().amount, 10);
    assert_eq!(available(&alice), (52, 0));
    let sends = alice.sends().unwrap();
    assert_eq!((sends[0].fee, &sends[0].state), (2, &State::Done));
    assert!(matches!(&sends[1].state, State::Abandoned(why) if why.contains("Fee")));
    assert_eq!(alice.hubs().unwrap()[0].1.fees.spend, FEE);
}

#[tokio::test]
async fn a_send_refused_as_spent_is_given_up_and_drops_the_spent_notes() {
    // Two copies of one wallet, as after restoring an old backup: what one
    // spends, the other learns from the hub. Each copy holds 64 they share
    // and the copy 32 of its own.
    let net = network();
    let bob = party(&net, 2).await;
    for signed in [false, true] {
        let seed = if signed { 4 } else { 1 };
        let (alice, copy) = (party(&net, seed).await, party(&net, seed).await);
        let funds = net.pay(HUB, &alice.address(), 64);
        alice.receive(&funds).unwrap();
        copy.receive(&funds).unwrap();
        copy.receive(&net.pay(HUB, &copy.address(), 32)).unwrap();

        if signed {
            // The hub spent an input otherwise, so it never releases this.
            net.fail(RELEASE, Fault::Lost);
            let Err(Error::Pending { id, .. }) = copy.send(HUB, &bob.address(), 10).await else {
                panic!("not pending");
            };
            assert!(matches!(copy.abandon(&id), Err(Error::Invalid(_))));
            alice.send(HUB, &bob.address(), 10).await.unwrap();
            let resumed = copy.resume().await.unwrap();
            assert_eq!((resumed.paid.len(), resumed.pending.len()), (0, 0));
            assert!(resumed.abandoned[0].1.contains("Spent"), "{resumed:?}");
            assert!(matches!(copy.payment(&id), Err(Error::Abandoned { .. })));
        } else {
            alice.send(HUB, &bob.address(), 10).await.unwrap();
            let refused = copy.send(HUB, &bob.address(), 10).await;
            assert!(
                matches!(&refused, Err(Error::Refused { problem, .. }) if problem.code == Code::Spent),
                "{refused:?}"
            );
        }
        assert!(matches!(
            copy.sends().unwrap()[0].state,
            State::Abandoned(_)
        ));
        // The shared note is dropped, not chosen again.
        assert_eq!(available(&copy), (32, 0));
        let paid = copy.send(HUB, &bob.address(), 10).await.unwrap();
        assert_eq!(bob.receive(&paid.payment).unwrap().amount, 10);
        assert_eq!(available(&copy), (20, 0));
        assert_eq!(copy.check().await.unwrap(), 0);
    }
}

#[tokio::test]
async fn a_restored_copy_drops_the_notes_spent_since() {
    let net = network();
    let (alice, copy, bob) = (
        party(&net, 1).await,
        party(&net, 1).await,
        party(&net, 2).await,
    );
    let funds = net.pay(HUB, &alice.address(), 100);
    alice.receive(&funds).unwrap();
    copy.receive(&funds).unwrap();
    alice.send(HUB, &bob.address(), 60).await.unwrap();
    assert_eq!(available(&copy), (100, 0));
    assert_eq!(copy.check().await.unwrap(), 64);
    assert_eq!(available(&copy), (36, 0));
    assert_eq!(copy.check().await.unwrap(), 0);
    assert_eq!(alice.check().await.unwrap(), 0);
    assert_eq!(available(&alice), (38, 0));
    // A note dropped is not unlinked either.
    assert_eq!(copy.unlink().await.unwrap(), 2);
}

#[tokio::test]
async fn a_signed_send_refused_otherwise_stays_pending_and_finishes() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
    net.fail(RELEASE, Fault::Refused(Code::UnknownOffer));
    let Err(Error::Pending { id, reason }) = alice.send(HUB, &bob.address(), 10).await else {
        panic!("not pending");
    };
    assert!(reason.contains("UnknownOffer"), "{reason}");
    assert_eq!(alice.sends().unwrap()[0].state, State::Signed);
    assert_eq!(available(&alice), (0, 64));
    assert!(matches!(alice.payment(&id), Err(Error::Pending { .. })));
    let resumed = alice.resume().await.unwrap();
    assert_eq!(resumed.paid[0].id, id);
    assert_eq!(bob.receive(&resumed.paid[0].payment).unwrap().amount, 10);
    assert!(same(&alice.payment(&id).unwrap(), &resumed.paid[0].payment));
}

#[tokio::test]
async fn an_unsigned_send_refused_is_given_up() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
    net.fail(OFFER, Fault::Refused(Code::Invalid));
    let refused = alice.send(HUB, &bob.address(), 10).await;
    assert!(
        matches!(&refused, Err(Error::Refused { problem, .. }) if problem.code == Code::Invalid),
        "{refused:?}"
    );
    assert_eq!(available(&alice), (64, 0));
    let id = alice.sends().unwrap()[0].id.clone();
    assert!(matches!(alice.payment(&id), Err(Error::Abandoned { .. })));
    let resumed = alice.resume().await.unwrap();
    assert!(resumed.paid.is_empty() && resumed.abandoned.is_empty());
    assert!(matches!(
        alice.payment(&"00".repeat(32)),
        Err(Error::UnknownPayment(_))
    ));
}

#[tokio::test]
async fn an_owner_gives_up_an_unsigned_send_only() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
    net.fail(OFFER, Fault::Lost);
    let Err(Error::Pending { id, .. }) = alice.send(HUB, &bob.address(), 10).await else {
        panic!("not pending");
    };
    alice.abandon(&id).unwrap();
    assert_eq!(available(&alice), (64, 0));
    assert!(alice.abandon(&id).is_err());
    assert!(alice.resume().await.unwrap().paid.is_empty());
    assert!(matches!(alice.payment(&id), Err(Error::Abandoned { .. })));
    alice.send(HUB, &bob.address(), 10).await.unwrap();
}

#[tokio::test]
async fn only_hubs_added_are_trusted() {
    const OTHER: &str = "http://other.test";
    let net = network();
    net.start(OTHER, FEE);
    let bob = party(&net, 2).await;
    let payment = net.pay(OTHER, &bob.address(), 8);
    assert!(matches!(bob.receive(&payment), Err(Error::UnknownHub(hub)) if hub == OTHER));
    bob.add_hub(OTHER).await.unwrap();
    assert_eq!(bob.receive(&payment).unwrap().amount, 8);
    assert_eq!(bob.hubs().unwrap().len(), 2);

    // Another URL claiming a hub's keysets is not that hub.
    net.alias("http://mirror.test", HUB);
    assert!(matches!(
        bob.add_hub("http://mirror.test").await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(bob.hubs().unwrap().len(), 2);
    assert!(matches!(
        bob.send("http://mirror.test", &bob.address(), 1).await,
        Err(Error::UnknownHub(_))
    ));
    assert!(matches!(
        bob.add_hub("ftp://hub.test").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        bob.add_hub("http://down.test").await,
        Err(Error::Unavailable { .. })
    ));
}

#[tokio::test]
async fn a_hub_proving_another_identity_is_refused_and_one_proving_none_taken() {
    const OTHER: &str = "http://other.test";
    let net = network();
    net.start(OTHER, FEE);
    let bob = party(&net, 2).await;
    let regtest = ChainProfile::Core30DefaultRegtest;
    assert_eq!(bob.hubs().unwrap()[0].1.proved(regtest), Ok(true));
    let proof = |url: &str| net.hub(url).advertise().unwrap().proof;
    let refused = |result: Result<_, Error>| {
        assert!(
            matches!(&result, Err(Error::Invalid(m)) if m.contains("proof")),
            "{result:?}"
        );
    };

    // A proof of another identity, whoever made it: not the hub's info,
    // neither added nor refreshing what is kept.
    let mut forged = proof(OTHER).unwrap();
    forged.0[0] ^= 1;
    *net.proof.lock().unwrap() = Some(Some(forged));
    refused(bob.add_hub(OTHER).await);
    refused(bob.add_hub(HUB).await);
    *net.proof.lock().unwrap() = Some(proof(OTHER));
    refused(bob.add_hub(HUB).await);
    assert_eq!(bob.hubs().unwrap().len(), 1);
    assert_eq!(bob.hubs().unwrap()[0].1.proof, proof(HUB));

    // None, as from a hub older than proofs: taken, as ever.
    *net.proof.lock().unwrap() = Some(None);
    bob.add_hub(OTHER).await.unwrap();
    assert_eq!(bob.hubs().unwrap().len(), 2);
}

#[tokio::test]
async fn a_hub_visited_proves_it_is_the_one_a_code_names_and_is_not_trusted() {
    const OTHER: &str = "http://other.test";
    let net = network();
    net.start(OTHER, FEE);
    let bob = party(&net, 2).await;
    let other = HubId::of(&net.hub(OTHER).advertise().unwrap().address);
    let added = HubId::of(&bob.hubs().unwrap()[0].1.address);
    fn refused<T: std::fmt::Debug>(result: Result<T, Error>, why: &str) {
        assert!(
            matches!(&result, Err(Error::Invalid(m)) if m.contains(why)),
            "{result:?}"
        );
    }

    // Not the hub the code names, or proving none: refused, nothing kept.
    refused(bob.visit(OTHER, added).await, "not the hub the code names");
    *net.proof.lock().unwrap() = Some(None);
    refused(bob.visit(OTHER, other).await, "proves no identity");
    *net.proof.lock().unwrap() = None;
    assert!(bob.visited().unwrap().is_empty());

    // Visited: known apart, its payments not taken, and in no code.
    let info = bob.visit(&format!("{OTHER}/"), other).await.unwrap();
    assert_eq!(HubId::of(&info.address), other);
    assert_eq!(bob.hubs().unwrap().len(), 1);
    assert_eq!(bob.visited().unwrap()[0].0, OTHER);
    let payment = net.pay(OTHER, &bob.address(), 8);
    assert!(matches!(bob.receive(&payment), Err(Error::UnknownHub(hub)) if hub == OTHER));
    assert_eq!(bob.balance().unwrap().len(), 1);
    let apart = Balance {
        hub: OTHER.into(),
        available: 0,
        pending: 0,
    };
    assert_eq!(bob.visited_balance().unwrap(), [apart]);
    let code = bob.payment_code(&[OTHER.into()], false);
    assert!(matches!(code, Err(Error::UnknownHub(_))), "{code:?}");
    // Nor asked for what its proof does not cover.
    refused(bob.deposit(OTHER, 1_000).await, "only visited");
    refused(bob.invoice(OTHER, 1_000).await, "only visited");

    // Learned again only as the hub it proved it is: else unavailable,
    // what is under way there waiting for it to prove it again.
    *net.proof.lock().unwrap() = Some(None);
    let lock = bob.lock(OTHER, &bob.address(), 1, [0; 32], None).await;
    assert!(
        matches!(&lock, Err(Error::Unavailable { reason, .. }) if reason.contains("no longer proves")),
        "{lock:?}"
    );
    *net.proof.lock().unwrap() = None;
    assert_eq!(bob.visited().unwrap()[0].1.proof, info.proof);

    // A hub added stays trusted when visited; one visited, once added, is.
    bob.visit(HUB, added).await.unwrap();
    assert_eq!(bob.hubs().unwrap().len(), 1);
    bob.add_hub(OTHER).await.unwrap();
    assert!(bob.visited().unwrap().is_empty());
    assert_eq!(bob.receive(&payment).unwrap().amount, 8);
    assert_eq!(bob.balance().unwrap().len(), 2);
}

#[tokio::test]
async fn a_hub_visited_is_added_at_the_url_the_owner_gives() {
    const OTHER: &str = "http://other.test";
    const MIRROR: &str = "http://mirror.test";
    let net = network();
    net.start(OTHER, FEE);
    net.alias(MIRROR, OTHER);
    let bob = party(&net, 2).await;
    let other = HubId::of(&net.hub(OTHER).advertise().unwrap().address);
    bob.visit(OTHER, other).await.unwrap();

    // The same hub, proving it is, at another URL: reached there, trusted.
    bob.add_hub(MIRROR).await.unwrap();
    assert!(bob.visited().unwrap().is_empty());
    let hubs = bob.hubs().unwrap();
    assert_eq!(hubs.len(), 2);
    assert!(hubs
        .iter()
        .any(|(url, i)| url == MIRROR && HubId::of(&i.address) == other));
    let payment = net.pay(OTHER, &bob.address(), 8);
    assert_eq!(bob.receive(&payment).unwrap().amount, 8);
    let held = bob.balance().unwrap();
    assert!(
        held.iter().any(|b| b.hub == MIRROR && b.available == 8),
        "{held:?}"
    );
}

#[test]
fn a_wallet_file_of_version_5_trusts_every_hub_it_had() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!("fixtures/wallet-v5.sql"))
        .unwrap();
    let wallet = Wallet::open(&path, Link(Net::new())).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6);
    assert_eq!(wallet.hubs().unwrap().len(), 1);
    assert_eq!(wallet.balance().unwrap().len(), 1);
    assert!(wallet.visited().unwrap().is_empty());
    assert!(wallet.visited_balance().unwrap().is_empty());
}

#[tokio::test]
async fn a_hub_over_plain_http_is_reached_over_https_once_it_answers_the_same_there() {
    const SECURE: &str = "https://hub.test";
    let net = network();
    let bob = party(&net, 2).await;
    bob.receive(&net.pay(HUB, &bob.address(), 8)).unwrap();
    net.alias(SECURE, HUB);
    // Asked over HTTPS once since opened: not again.
    bob.add_hub(HUB).await.unwrap();
    assert_eq!(bob.hubs().unwrap()[0].0, HUB);

    // Added over HTTPS: the same hub, its notes with it, reached there from
    // now on, and still known at the other URL.
    bob.add_hub(SECURE).await.unwrap();
    let hubs = bob.hubs().unwrap();
    assert_eq!((hubs.len(), hubs[0].0.as_str()), (1, SECURE));
    let balance = bob.balance().unwrap();
    assert_eq!((balance[0].hub.as_str(), balance[0].available), (SECURE, 8));
    bob.add_hub(HUB).await.unwrap();
    assert_eq!(bob.hubs().unwrap().len(), 1);

    // A wallet adding it over plain HTTP: reached over HTTPS at once.
    let alice = party(&net, 1).await;
    assert_eq!(alice.hubs().unwrap()[0].0, SECURE);
    let paid = bob.send(HUB, &alice.address(), 4).await.unwrap();
    assert_eq!(paid.payment.hub, SECURE);
    assert_eq!(alice.receive(&paid.payment).unwrap().amount, 4);
    // Never back to plain HTTP from HTTPS.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.sqlite");
    let network = ChainProfile::Core30DefaultRegtest;
    let carol = Wallet::create(&path, &[3; 32], network, Link(net.clone())).unwrap();
    carol.add_hub(SECURE).await.unwrap();
    assert!(matches!(carol.add_hub(HUB).await, Err(Error::Invalid(_))));
    // Another host's URL over HTTPS is not the hub.
    net.alias("https://mirror.test", HUB);
    let mirror = carol.add_hub("https://mirror.test").await;
    assert!(matches!(mirror, Err(Error::Invalid(_))), "{mirror:?}");
    assert_eq!(carol.hubs().unwrap().len(), 1);

    // Asked over HTTPS only, even at the other URL: another hub answering
    // over plain HTTP there is not taken for it.
    let keysets = bob.hubs().unwrap()[0].1.keysets.clone();
    net.start(HUB, FEE);
    bob.add_hub(HUB).await.unwrap();
    let hubs = bob.hubs().unwrap();
    assert_eq!(hubs.len(), 1);
    assert!(hubs[0].1.keysets == keysets);
}

#[tokio::test]
async fn a_hub_over_plain_http_is_asked_over_https_again_once_its_wallet_is_opened_again() {
    let net = network();
    let bob = party(&net, 2).await;
    net.alias("https://hub.test", HUB);
    let path = bob._dir.path().join("w.sqlite");
    let again = Wallet::open_with_seed(&path, &[2; 32], Link(net.clone())).unwrap();
    assert_eq!(again.hubs().unwrap()[0].0, HUB);
    // Even unavailable over plain HTTP, as through Tor to a server only
    // redirecting to HTTPS.
    net.fail(licash_api::INFO, Fault::Lost);
    again.add_hub(HUB).await.unwrap();
    assert_eq!(again.hubs().unwrap()[0].0, "https://hub.test");
}

#[tokio::test]
async fn another_hub_over_https_is_neither_reached_nor_trusted_unasked() {
    let net = network();
    net.start("https://hub.test", FEE);
    let bob = party(&net, 2).await;
    let hubs = bob.hubs().unwrap();
    assert_eq!((hubs.len(), hubs[0].0.as_str()), (1, HUB));
}

#[tokio::test]
async fn a_payment_beyond_the_balance_is_refused_before_the_hub_hears_of_it() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 20)).unwrap();
    let short = alice.send(HUB, &bob.address(), 19).await;
    assert!(
        matches!(
            short,
            Err(Error::Insufficient {
                needed: 21,
                available: 20
            })
        ),
        "{short:?}"
    );
    assert!(matches!(
        alice.send(HUB, &bob.address(), 0).await,
        Err(Error::Invalid(_))
    ));
    assert!(alice.sends().unwrap().is_empty());
    assert_eq!(
        bob.send(HUB, &alice.address(), 1)
            .await
            .unwrap_err()
            .to_string(),
        "3 needed, 0 available in one payment"
    );
}

#[tokio::test]
async fn notes_unlink_and_stay_spendable() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap();

    // An answer lost: the same request is asked again.
    net.fail(UNLINK, Fault::Dropped);
    assert!(matches!(
        alice.unlink().await,
        Err(Error::Unavailable { .. })
    ));
    assert_eq!(alice.unlink().await.unwrap(), 3);
    assert_eq!(alice.unlink().await.unwrap(), 0);
    assert_eq!(available(&alice), (100, 0));

    let payment = alice.send(HUB, &bob.address(), 98).await.unwrap().payment;
    assert_eq!(bob.receive(&payment).unwrap().amount, 98);
    assert_eq!(available(&alice), (0, 0));
}

#[tokio::test]
async fn notes_a_restarted_hub_no_longer_unlinks_stay_linked_and_spendable() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 6)).unwrap();
    net.restart(HUB);
    assert_eq!(alice.unlink().await.unwrap(), 0);
    assert_eq!(alice.unlink().await.unwrap(), 0);
    let payment = alice.send(HUB, &bob.address(), 4).await.unwrap().payment;
    assert_eq!(bob.receive(&payment).unwrap().amount, 4);
}

#[tokio::test]
async fn a_note_its_hub_refuses_to_unlink_stays_linked_and_spendable() {
    let net = network();
    let (alice, bob) = (party(&net, 1).await, party(&net, 2).await);
    alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap();
    net.fail(UNLINK, Fault::Refused(Code::Invalid));
    assert_eq!(alice.unlink().await.unwrap(), 2);
    assert_eq!(alice.unlink().await.unwrap(), 0);
    let payment = alice.send(HUB, &bob.address(), 98).await.unwrap().payment;
    assert_eq!(bob.receive(&payment).unwrap().amount, 98);
}

#[tokio::test]
async fn a_wallet_whose_seed_is_kept_elsewhere_opens_with_it_only() {
    let net = network();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.sqlite");
    let link = || Link(net.clone());
    let address =
        Wallet::create_without_seed(&path, &[5; 32], ChainProfile::Core30DefaultRegtest, link())
            .unwrap()
            .address();
    let seed: Option<Vec<u8>> = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT seed FROM meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(seed, None);
    assert!(matches!(Wallet::open(&path, link()), Err(Error::Setup(_))));
    assert!(matches!(
        Wallet::open_with_seed(&path, &[6; 32], link()),
        Err(Error::Setup(_))
    ));
    let wallet = Wallet::open_with_seed(&path, &[5; 32], link()).unwrap();
    assert_eq!(wallet.address(), address);

    // A wallet holding its seed opens with it too.
    let path = dir.path().join("held.sqlite");
    drop(Wallet::create(&path, &[5; 32], ChainProfile::Core30DefaultRegtest, link()).unwrap());
    Wallet::open_with_seed(&path, &[5; 32], link()).unwrap();
    assert!(matches!(
        Wallet::open_with_seed(&path, &[6; 32], link()),
        Err(Error::Setup(_))
    ));
}

#[tokio::test]
async fn a_wallet_trusts_hubs_of_its_network_only() {
    let net = network();
    let dir = tempfile::tempdir().unwrap();
    let link = || Link(net.clone());
    let others = [
        ChainProfile::Core30Mainnet,
        ChainProfile::Core30Signet,
        ChainProfile::Mutinynet,
    ];
    for profile in others {
        let path = dir.path().join(profile.name());
        let wallet = Wallet::create(&path, &[1; 32], profile, link()).unwrap();
        assert_eq!(wallet.network(), profile);
        match wallet.add_hub(HUB).await {
            Err(Error::Invalid(reason)) => assert!(reason.contains("follows regtest"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(wallet.hubs().unwrap().is_empty());
        drop(wallet);
        assert_eq!(Wallet::open(&path, link()).unwrap().network(), profile);
    }
    let path = dir.path().join("regtest");
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create_without_seed(&path, &[1; 32], regtest, link()).unwrap();
    wallet.add_hub(HUB).await.unwrap();
    drop(wallet);
    let wallet = Wallet::open_with_seed(&path, &[1; 32], link()).unwrap();
    assert_eq!(wallet.network(), regtest);
    assert_eq!(wallet.hubs().unwrap().len(), 1);
}

#[test]
fn its_futures_run_on_any_runtime_thread() {
    fn send<F: std::future::Future + Send>(_: F) {}
    let net = network();
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("w"),
        &[1; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net),
    )
    .unwrap();
    let to = wallet.address();
    send(wallet.add_hub(HUB));
    send(wallet.send(HUB, &to, 1));
    send(wallet.resume());
    send(wallet.unlink());
    send(wallet.check());
}
