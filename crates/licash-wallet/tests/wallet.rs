mod net;

use std::{path::Path, sync::Arc};

use licash_api::{Code, Fee, Hex, Payment};
use licash_wallet::{Balance, Error, State, Wallet};
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
    let wallet =
        Wallet::create(&dir.path().join("w.sqlite"), &[seed; 32], Link(net.clone())).unwrap();
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
    assert_eq!(
        alice.receive(&net.pay(HUB, &alice.address(), 100)).unwrap(),
        100
    );
    assert_eq!(
        alice.balance().unwrap(),
        [Balance {
            hub: HUB.into(),
            available: 100,
            pending: 0
        }]
    );

    let payment = alice.send(HUB, &bob.address(), 30).await.unwrap();
    assert_eq!(payment.hub, HUB);
    assert_eq!(bob.receive(&payment).unwrap(), 30);
    assert_eq!(available(&alice), (68, 0));
    assert_eq!(available(&bob), (30, 0));

    let sends = alice.sends().unwrap();
    assert_eq!(sends.len(), 1);
    let sent = &sends[0];
    assert_eq!((sent.amount, sent.fee, &sent.state), (30, 2, &State::Done));
    assert_eq!(sent.payee, bob.address());
    assert!(same(&alice.payment(&sent.id).unwrap(), &payment));

    // Received once only, and by its payee only.
    assert!(matches!(bob.receive(&payment), Err(Error::Invalid(_))));
    let carol = party(&net, 3).await;
    assert!(matches!(carol.receive(&payment), Err(Error::Invalid(_))));
    let mut forged = payment.clone();
    forged.unlock = Hex(vec![7; 32]);
    assert!(matches!(carol.receive(&forged), Err(Error::Invalid(_))));

    // Bob pays on what he received, Alice on her change.
    let back = bob.send(HUB, &alice.address(), 28).await.unwrap();
    assert_eq!(alice.receive(&back).unwrap(), 28);
    assert_eq!(available(&bob), (0, 0));
    let all = alice.send(HUB, &bob.address(), 94).await.unwrap();
    assert_eq!(bob.receive(&all).unwrap(), 94);
    assert_eq!(available(&alice), (0, 0));
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
        assert_eq!(resumed.paid[0].0, id);
        assert_eq!(bob.receive(&resumed.paid[0].1).unwrap(), 10);
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
        let alice = Wallet::create(&path, &[1; 32], Link(net.clone())).unwrap();
        alice.add_hub(HUB).await.unwrap();
        alice.receive(&net.pay(HUB, &alice.address(), 64)).unwrap();
        net.fail(RELEASE, Fault::Dropped);
        assert!(matches!(
            alice.send(HUB, &bob.address(), 10).await,
            Err(Error::Pending { .. })
        ));
    }
    assert!(matches!(
        Wallet::create(&path, &[1; 32], Link(net.clone())),
        Err(Error::Setup(_))
    ));
    let alice = Wallet::open(&path, Link(net.clone())).unwrap();
    let owner = licash::address::OwnerKeys::from_seed(&[1; 32]).unwrap();
    assert_eq!(alice.address(), owner.address());
    let resumed = alice.resume().await.unwrap();
    assert_eq!(bob.receive(&resumed.paid[0].1).unwrap(), 10);
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

    let payment = alice.send(HUB, &bob.address(), 10).await.unwrap();
    assert_eq!(bob.receive(&payment).unwrap(), 10);
    assert_eq!(available(&alice), (52, 0));
    let sends = alice.sends().unwrap();
    assert_eq!((sends[0].fee, &sends[0].state), (2, &State::Done));
    assert!(matches!(&sends[1].state, State::Abandoned(why) if why.contains("Fee")));
    assert_eq!(alice.hubs().unwrap()[0].1.fees.spend, FEE);
}

#[tokio::test]
async fn a_send_its_hub_refuses_frees_its_notes() {
    // Two copies of one wallet, as after restoring an old backup: what one
    // spends, the other learns from the hub.
    let net = network();
    let (alice, copy, bob) = (
        party(&net, 1).await,
        party(&net, 1).await,
        party(&net, 2).await,
    );
    let funds = net.pay(HUB, &alice.address(), 64);
    alice.receive(&funds).unwrap();
    copy.receive(&funds).unwrap();

    // Refused unsigned: given up at once.
    alice.send(HUB, &bob.address(), 10).await.unwrap();
    let refused = copy.send(HUB, &bob.address(), 10).await;
    assert!(matches!(refused, Err(Error::Refused { problem, .. }) if problem.code == Code::Spent));
    assert_eq!(available(&copy), (64, 0));
    assert!(matches!(
        copy.sends().unwrap()[0].state,
        State::Abandoned(_)
    ));

    // Refused signed: the hub spent an input otherwise, so it never
    // releases this one.
    let (alice, copy) = (party(&net, 4).await, party(&net, 4).await);
    let funds = net.pay(HUB, &alice.address(), 64);
    alice.receive(&funds).unwrap();
    copy.receive(&funds).unwrap();
    net.fail(RELEASE, Fault::Lost);
    assert!(matches!(
        copy.send(HUB, &bob.address(), 10).await,
        Err(Error::Pending { .. })
    ));
    assert!(matches!(
        copy.abandon(&copy.sends().unwrap()[0].id),
        Err(Error::Invalid(_))
    ));
    alice.send(HUB, &bob.address(), 10).await.unwrap();
    let resumed = copy.resume().await.unwrap();
    assert_eq!((resumed.paid.len(), resumed.pending.len()), (0, 0));
    assert_eq!(resumed.abandoned[0].1.code, Code::Spent);
    assert_eq!(available(&copy), (64, 0));
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
    assert!(matches!(alice.payment(&id), Err(Error::Invalid(_))));
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
    assert_eq!(bob.receive(&payment).unwrap(), 8);
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

    let payment = alice.send(HUB, &bob.address(), 98).await.unwrap();
    assert_eq!(bob.receive(&payment).unwrap(), 98);
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
    let payment = alice.send(HUB, &bob.address(), 4).await.unwrap();
    assert_eq!(bob.receive(&payment).unwrap(), 4);
}

#[test]
fn its_futures_run_on_any_runtime_thread() {
    fn send<F: std::future::Future + Send>(_: F) {}
    let net = network();
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(&dir.path().join("w"), &[1; 32], Link(net)).unwrap();
    let to = wallet.address();
    send(wallet.add_hub(HUB));
    send(wallet.send(HUB, &to, 1));
    send(wallet.resume());
    send(wallet.unlink());
}
