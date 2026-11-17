//! Wallets pay and get paid over Lightning through their hubs' gateways,
//! and through a swap provider paying invoices from its own node, on the
//! stack of `apps/e2e/run.sh`, which runs these.

use licash::spend::lock_hash;
use licash_api::{swaps::State, ReceiveState};
use licash_e2e::{received, until, Eco, Wallet, OUTSIDE, SWAPS_LN};
use licash_wallet::{InvoiceState, Lock, LockState};

fn available(wallet: &Wallet) -> u64 {
    let balance = wallet.balance().unwrap();
    balance.iter().map(|b| b.available).sum()
}

/// What `wallet` paid its hubs on its payments to itself, which make the
/// exact notes a lock to a hub locks.
fn exact_fees(wallet: &Wallet) -> u64 {
    let me = wallet.address();
    let sends = wallet.sends().unwrap();
    sends.iter().filter(|s| s.payee == me).map(|s| s.fee).sum()
}

/// Follow `lock` until it is paid or refunded.
async fn settled(wallet: &Wallet, lock: &Lock) -> anyhow::Result<Lock> {
    until(&format!("lock {}", lock.id), 180, || async {
        Ok(match wallet.follow_lock(&lock.id).await {
            Ok(lock) if matches!(lock.state, LockState::Paid(_) | LockState::Refunded) => {
                Some(lock)
            }
            Ok(_) | Err(licash_wallet::Error::Pending { .. }) => None,
            Err(e) => return Err(e.into()),
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn wallets_pay_and_get_paid_over_lightning_across_hubs() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let alice = eco.wallet("alice")?;
    let bob = eco.wallet("bob")?;

    // Alice is paid from outside.
    eco.pay_in(&alice, hub1, 50_000).await?;
    assert_eq!(available(&alice), 50_000);

    // She pays an outside invoice: its preimage settles her lock.
    let (request, hash) = eco.invoice(OUTSIDE, 20_000).await?;
    let lock = alice.pay_invoice(hub1, &request).await?;
    let lock = settled(&alice, &lock).await?;
    let (state, preimage) = eco.invoice_state(OUTSIDE, &hash).await?;
    assert_eq!(state, "SETTLED");
    assert_eq!(lock.state, LockState::Paid(hex_32(&preimage)));
    // The invoice, and the gateway's fee of 2 + 5000 ppm.
    assert_eq!(lock.amount, 20_000 + 2 + 100);
    // Her notes of 50 000 held none adding up to it: she paid hub1 its fee
    // to make them first.
    let fees = exact_fees(&alice);
    assert!(fees > 0);
    assert_eq!(available(&alice), 50_000 - 20_102 - fees);
    // Its route ended its timelock before the lock's expiry, by the
    // gateway's slack of 12 blocks at least.
    let timelocks = eco.timelocks("hub1-ln", &hash).await?;
    assert!(!timelocks.is_empty());
    assert!(
        timelocks.iter().all(|t| *t + 12 <= u64::from(lock.expiry)),
        "{timelocks:?}"
    );

    // She pays Bob, at the other hub: hub1's node pays hub2's held
    // invoice, which settles once Bob claims what hub2 locks to him.
    let invoice = bob.invoice(hub2, 10_000).await?;
    let lock = alice
        .pay_invoice(hub1, invoice.request.as_deref().unwrap())
        .await?;
    assert_eq!(received(&bob, &invoice.hash).await?, InvoiceState::Received);
    let lock = settled(&alice, &lock).await?;
    assert!(matches!(lock.state, LockState::Paid(_)), "{lock:?}");
    assert_eq!(available(&bob), 10_000);
    let status = until("hub2 settles its invoice", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Settled).then_some(status))
    })
    .await?;
    assert_eq!(status.state, ReceiveState::Settled);

    // A payment that fails refunds its lock, less the hub's refund fee.
    let (before, fees) = (available(&alice), exact_fees(&alice));
    let (request, hash, _) = eco.hold_invoice(OUTSIDE, 5_000).await?;
    let lock = alice.pay_invoice(hub1, &request).await?;
    until("the outside node holds the payment", 60, || async {
        let (state, _) = eco.invoice_state(OUTSIDE, &hash).await?;
        Ok((state == "ACCEPTED").then_some(()))
    })
    .await?;
    eco.lncli(OUTSIDE, &["cancelinvoice", &hash]).await?;
    let lock = settled(&alice, &lock).await?;
    assert_eq!(lock.state, LockState::Refunded);
    assert!(lock.failure.is_some());
    let refund_fee = lock.cost.unwrap();
    let exact = exact_fees(&alice) - fees;
    assert_eq!(available(&alice), before - exact - refund_fee);

    // hub1's operator sees them in its dashboard, read beside it running.
    let dashboard = eco.dashboard("hub1", "licash-hub").await?;
    assert_eq!(dashboard.check("Service").map(|c| c.0), Some("good".into()));
    assert!(dashboard.card("Lightning in") >= Some(1.0));
    assert!(dashboard.card("Lightning out") >= Some(2.0));
    assert!(dashboard.card("Failed") >= Some(1.0));
    assert!(dashboard.card("Fees") > Some(0.0));
    // Among its recent events, the newest payment out: the one refused.
    let events = dashboard.texts("Recent events");
    let out = events.iter().find(|e| e[0] == "lightning out");
    assert_eq!(out.map(|e| &e[1]), Some(&"failed".to_owned()), "{events:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_payment_its_payee_never_claims_returns_to_its_payer() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub2 = eco.hub(2);
    let carol = eco.wallet("carol")?;
    let invoice = carol.invoice(hub2, 7_000).await?;
    let paying = eco.pay(OUTSIDE, invoice.request.as_deref().unwrap());
    until("hub2 locks the payment to Carol", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Locked).then_some(()))
    })
    .await?;
    // Unclaimed until the lock expired at depth: its blocks, 12, and the
    // hub's depth, 3.
    eco.mine(12 + 3 + 1).await?;
    until("hub2 cancels the invoice", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Canceled).then_some(()))
    })
    .await?;
    assert!(!paying.await??, "the outside node's payment failed");
    let state = received(&carol, &invoice.hash).await?;
    assert!(matches!(state, InvoiceState::Canceled(_)), "{state:?}");
    assert_eq!(available(&carol), 0);
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn hubs_restarted_mid_payment_finish_it() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let dave = eco.wallet("dave")?;
    eco.pay_in(&dave, hub1, 30_000).await?;

    // Paying out: the outside node holds the payment while hub1 restarts.
    let (request, hash, preimage) = eco.hold_invoice(OUTSIDE, 10_000).await?;
    let lock = dave.pay_invoice(hub1, &request).await?;
    until("the outside node holds the payment", 60, || async {
        let (state, _) = eco.invoice_state(OUTSIDE, &hash).await?;
        Ok((state == "ACCEPTED").then_some(()))
    })
    .await?;
    eco.restart("hub1").await?;
    eco.lncli(OUTSIDE, &["settleinvoice", &preimage]).await?;
    let lock = settled(&dave, &lock).await?;
    assert_eq!(lock.state, LockState::Paid(hex_32(&preimage)));

    // Paid in: hub2 restarts once it locked the payment to Erin.
    let erin = eco.wallet("erin")?;
    let invoice = erin.invoice(hub2, 4_000).await?;
    let paying = eco.pay(OUTSIDE, invoice.request.as_deref().unwrap());
    until("hub2 locks the payment to Erin", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Locked).then_some(()))
    })
    .await?;
    eco.restart("hub2").await?;
    assert_eq!(
        received(&erin, &invoice.hash).await?,
        InvoiceState::Received
    );
    assert!(paying.await??, "the outside node's payment succeeded");
    assert_eq!(available(&erin), 4_000);
    Ok(())
}

/// What `wallet` can spend at `hub`.
fn available_at(wallet: &Wallet, hub: &str) -> u64 {
    let balance = wallet.balance().unwrap();
    balance
        .iter()
        .find(|b| b.hub == hub)
        .map_or(0, |b| b.available)
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_wallet_moves_its_notes_to_another_hub_over_lightning() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let frank = eco.wallet("frank")?;
    eco.pay_in(&frank, hub1, 40_000).await?;
    assert_eq!(available_at(&frank, hub1), 40_000);

    // Frank asks hub2 for an invoice under a preimage only he knows, and
    // pays it from hub1: no swap provider, no notes of either hub held by
    // the other.
    let invoice = frank.invoice(hub2, 10_000).await?;
    let request = invoice.request.as_deref().unwrap();
    let payable = frank.read_invoice(request)?;
    assert!(payable.amount > 10_000, "hub2's receive fee: {payable:?}");
    let lock = frank.pay_invoice(hub1, request).await?;
    assert_eq!(hex::encode(lock.hash), invoice.hash);
    // The invoice, and hub1's gateway fee of 2 + 5000 ppm.
    assert_eq!(
        lock.amount,
        payable.amount + 2 + payable.amount.div_ceil(200)
    );

    // He claims at hub2 with his own preimage, which settles hub2's
    // invoice, and Lightning carries it back to hub1 to settle his lock.
    assert_eq!(
        received(&frank, &invoice.hash).await?,
        InvoiceState::Received
    );
    let lock = settled(&frank, &lock).await?;
    let LockState::Paid(preimage) = lock.state else {
        panic!("{lock:?}");
    };
    assert_eq!(hex::encode(lock_hash(&preimage)), invoice.hash);
    let status = until("hub2 settles its invoice", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Settled).then_some(status))
    })
    .await?;
    assert_eq!(status.state, ReceiveState::Settled);

    assert_eq!(available_at(&frank, hub2), 10_000);
    // Hub1 kept what the lock cost, and its fee for making the exact
    // notes the lock spent.
    let cost = lock.cost.expect("a settled lock's cost");
    assert!(cost >= lock.amount, "{lock:?}");
    assert_eq!(
        available_at(&frank, hub1),
        40_000 - cost - exact_fees(&frank)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_move_never_claimed_at_the_other_hub_returns_to_the_first() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let grace = eco.wallet("grace")?;
    eco.pay_in(&grace, hub1, 30_000).await?;

    // Grace pays her own hub2 invoice from hub1, then never claims.
    let invoice = grace.invoice(hub2, 6_000).await?;
    let lock = grace
        .pay_invoice(hub1, invoice.request.as_deref().unwrap())
        .await?;
    until("hub2 locks the payment to Grace", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Locked).then_some(()))
    })
    .await?;

    // Hub2's lock expires at depth: its blocks, 12, and the hub's depth,
    // 3. Without the preimage hub2 cannot settle the invoice, so the
    // HTLC fails back and hub1 refunds her lock.
    eco.mine(12 + 3 + 1).await?;
    until("hub2 cancels the invoice", 60, || async {
        let status = eco.receipt(hub2, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Canceled).then_some(()))
    })
    .await?;
    let lock = settled(&grace, &lock).await?;
    assert_eq!(lock.state, LockState::Refunded);
    assert!(lock.failure.is_some());

    let state = received(&grace, &invoice.hash).await?;
    assert!(matches!(state, InvoiceState::Canceled(_)), "{state:?}");
    assert_eq!(available_at(&grace, hub2), 0);
    // Only hub1's refund fee, and its fee for the exact notes, are lost.
    let refund_fee = lock.cost.expect("a refunded lock's cost");
    assert_eq!(
        available_at(&grace, hub1),
        30_000 - refund_fee - exact_fees(&grace)
    );
    Ok(())
}

/// Why `node`'s payment of `hash` failed, if it did.
async fn failure(eco: &Eco, node: &str, hash: &str) -> anyhow::Result<Option<String>> {
    let payments = eco
        .lncli(node, &["listpayments", "--include_incomplete"])
        .await?;
    let payments = payments["payments"].as_array().cloned().unwrap_or_default();
    let payment = payments.iter().find(|p| p["payment_hash"] == hash);
    Ok(payment
        .filter(|p| p["status"] == "FAILED")
        .and_then(|p| p["failure_reason"].as_str().map(str::to_owned)))
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn an_invoice_of_a_wallet_at_the_same_hub_is_paid_inside_it() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub1 = eco.hub(1);
    let lena = eco.wallet("lena")?;
    let mike = eco.wallet("mike")?;
    eco.pay_in(&lena, hub1, 30_000).await?;

    // Mike asks hub1 for an invoice; Lena pays it through hub1, which
    // pays it inside the hub: no gateway fee, nothing over Lightning.
    let invoice = mike.invoice(hub1, 10_000).await?;
    let request = invoice.request.as_deref().unwrap();
    let payable = lena.read_invoice(request)?;
    // Cheaper than an invoice of the outside node, by the gateway's fee:
    // only the hub's fee on making notes worth the lock exactly is left.
    let (outside, _) = eco.invoice(OUTSIDE, 10_000).await?;
    let (inside, over) = (
        lena.invoice_fee(hub1, request)?,
        lena.invoice_fee(hub1, &outside)?,
    );
    assert!(inside < over, "{inside} inside, {over} over Lightning");
    let lock = lena.pay_invoice(hub1, request).await?;
    assert_eq!(lock.amount, payable.amount);
    // LND canceled the invoice while open: no HTLC may back it any more.
    let (state, _) = eco.invoice_state("hub1-ln", &invoice.hash).await?;
    assert_eq!(state, "CANCELED");

    assert_eq!(
        received(&mike, &invoice.hash).await?,
        InvoiceState::Received
    );
    assert_eq!(available_at(&mike, hub1), 10_000);
    let lock = settled(&lena, &lock).await?;
    let LockState::Paid(preimage) = lock.state else {
        panic!("{lock:?}");
    };
    assert_eq!(hex::encode(lock_hash(&preimage)), invoice.hash);
    let status = until("hub1 settles the receipt", 60, || async {
        let status = eco.receipt(hub1, &invoice.hash).await?;
        Ok((status.state == ReceiveState::Settled).then_some(status))
    })
    .await?;
    assert_eq!(status.state, ReceiveState::Settled);
    let cost = lock.cost.expect("a settled lock's cost");
    assert_eq!(available_at(&lena, hub1), 30_000 - cost - exact_fees(&lena));
    // Hub1's node paid nothing for it.
    let payments = eco.lncli("hub1-ln", &["listpayments"]).await?;
    let paid = payments["payments"].as_array().cloned().unwrap_or_default();
    assert!(paid
        .iter()
        .all(|p| p["payment_hash"] != invoice.hash.as_str()));

    // Paid over Lightning afterwards, the canceled invoice fails back.
    let paying = eco.pay(OUTSIDE, request);
    assert!(!paying.await??, "the outside node's payment failed");
    let why = failure(&eco, OUTSIDE, &invoice.hash).await?;
    assert_eq!(
        why.as_deref(),
        Some("FAILURE_REASON_INCORRECT_PAYMENT_DETAILS")
    );
    assert_eq!(available_at(&mike, hub1), 10_000);
    Ok(())
}

/// Follow the provider's payment for the client's lock `id` until it
/// reaches `state`.
async fn paid_as(
    eco: &Eco,
    id: &str,
    state: State,
) -> anyhow::Result<licash_api::swaps::LightningSwap> {
    until(
        &format!("the provider's payment for {id} {state:?}"),
        180,
        || async {
            let payment = eco.lightning_payment(id).await?;
            Ok((payment.state == state).then_some(payment))
        },
    )
    .await
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_provider_pays_a_hub_invoice_for_a_lock_at_another_hub() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let hal = eco.wallet("hal")?;
    let ivy = eco.wallet("ivy")?;
    eco.pay_in(&hal, hub1, 30_000).await?;

    // Ivy asks hub2 for an invoice under a preimage only she knows; Hal
    // locks notes at hub1 to the provider, which pays it from its node. No
    // contract with either hub or the provider.
    let invoice = ivy.invoice(hub2, 10_000).await?;
    let request = invoice.request.as_deref().unwrap();
    let payable = hal.read_invoice(request)?;
    let offer = eco.swaps_info().await?;
    let terms = offer
        .lightning
        .as_ref()
        .expect("the provider pays invoices");
    let lock = hal
        .pay_invoice_via(eco.swaps(), hub1, request, 1_000)
        .await?;
    assert_eq!(lock.payee, offer.address);
    assert_eq!(hex::encode(lock.hash), invoice.hash);
    assert_eq!(lock.amount, terms.pays(payable.amount * 1000));

    // Ivy claims at hub2 with her preimage, which settles hub2's invoice;
    // the provider learns it and claims Hal's lock with it.
    assert_eq!(received(&ivy, &invoice.hash).await?, InvoiceState::Received);
    assert_eq!(available_at(&ivy, hub2), 10_000);
    let lock = settled(&hal, &lock).await?;
    let LockState::Paid(preimage) = lock.state else {
        panic!("{lock:?}");
    };
    assert_eq!(hex::encode(lock_hash(&preimage)), invoice.hash);
    let payment = paid_as(&eco, &lock.id, State::Done).await?;
    assert_eq!(payment.preimage()?, Some(preimage));
    assert_eq!(payment.amount, payable.amount);

    // Its routes ended at least its slack of 80 blocks before Hal's lock
    // expires.
    let timelocks = eco.timelocks(SWAPS_LN, &invoice.hash).await?;
    assert!(!timelocks.is_empty());
    assert!(
        timelocks.iter().all(|t| *t + 80 <= u64::from(lock.expiry)),
        "{timelocks:?} for {}",
        lock.expiry
    );
    // Hal paid the lock, and hub1 its fee for the exact notes it spent.
    let cost = lock.cost.expect("a settled lock's cost");
    assert!(cost >= lock.amount, "{lock:?}");
    assert_eq!(available_at(&hal, hub1), 30_000 - cost - exact_fees(&hal));

    // The same invoice asked again answers the same lock, locking nothing.
    let again = hal
        .pay_invoice_via(eco.swaps(), hub1, request, 1_000)
        .await?;
    assert_eq!(again.id, lock.id);
    assert_eq!(available_at(&hal, hub1), 30_000 - cost - exact_fees(&hal));
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_payment_the_provider_fails_returns_once_its_lock_expires() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub1 = eco.hub(1);
    let jack = eco.wallet("jack")?;
    eco.pay_in(&jack, hub1, 20_000).await?;

    let (request, hash, _) = eco.hold_invoice(OUTSIDE, 5_000).await?;
    let lock = jack
        .pay_invoice_via(eco.swaps(), hub1, &request, 1_000)
        .await?;
    until("the outside node holds the payment", 60, || async {
        let (state, _) = eco.invoice_state(OUTSIDE, &hash).await?;
        Ok((state == "ACCEPTED").then_some(()))
    })
    .await?;
    eco.lncli(OUTSIDE, &["cancelinvoice", &hash]).await?;
    let payment = paid_as(&eco, &lock.id, State::Failed).await?;
    assert!(payment.reason.is_some(), "{payment:?}");
    assert_eq!(payment.preimage, None);

    // The provider releases nothing early: the lock stands until expired.
    let locked = jack.follow_lock(&lock.id).await?;
    assert_eq!(locked.state, LockState::Locked);
    let depth = eco
        .info(hub1)
        .await?
        .locks
        .expect("hub1 takes locks")
        .min_depth;
    let height = eco.height().await?;
    eco.mine(lock.expiry - height + depth + 1).await?;
    let lock = settled(&jack, &lock).await?;
    assert_eq!(lock.state, LockState::Refunded);
    // Only hub1's refund fee, and its fee for the exact notes, are lost.
    let refund_fee = lock.cost.expect("a refunded lock's cost");
    assert_eq!(
        available_at(&jack, hub1),
        20_000 - refund_fee - exact_fees(&jack)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_provider_restarted_mid_payment_claims_its_lock() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub1 = eco.hub(1);
    let kate = eco.wallet("kate")?;
    eco.pay_in(&kate, hub1, 20_000).await?;

    // The outside node holds the payment while the provider restarts.
    let (request, hash, preimage) = eco.hold_invoice(OUTSIDE, 6_000).await?;
    let lock = kate
        .pay_invoice_via(eco.swaps(), hub1, &request, 1_000)
        .await?;
    until("the outside node holds the payment", 60, || async {
        let (state, _) = eco.invoice_state(OUTSIDE, &hash).await?;
        Ok((state == "ACCEPTED").then_some(()))
    })
    .await?;
    eco.restart("swaps").await?;
    eco.lncli(OUTSIDE, &["settleinvoice", &preimage]).await?;
    let lock = settled(&kate, &lock).await?;
    assert_eq!(lock.state, LockState::Paid(hex_32(&preimage)));
    let payment = paid_as(&eco, &lock.id, State::Done).await?;
    assert_eq!(payment.preimage()?, Some(hex_32(&preimage)));
    Ok(())
}

fn hex_32(text: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    hex::decode_to_slice(text, &mut bytes).unwrap();
    bytes
}
