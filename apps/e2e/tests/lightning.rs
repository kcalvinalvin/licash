//! Wallets pay and get paid over Lightning through their hubs' gateways,
//! on the stack of `apps/e2e/run.sh`, which runs these.

use licash_api::ReceiveState;
use licash_e2e::{received, until, Eco, Wallet, OUTSIDE};
use licash_wallet::{InvoiceState, Lock, LockState};

fn available(wallet: &Wallet) -> u64 {
    let balance = wallet.balance().unwrap();
    balance.iter().map(|b| b.available).sum()
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
    assert_eq!(available(&alice), 50_000 - 20_102);
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

    // A payment that fails refunds its lock.
    let before = available(&alice);
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
    assert_eq!(available(&alice), before);

    // hub1's operator sees them in its dashboard, read beside it running.
    let dashboard = eco.dashboard("hub1", "licash-hub").await?;
    assert_eq!(dashboard.check("Service").map(|c| c.0), Some("good".into()));
    assert!(dashboard.card("Lightning in") >= Some(1.0));
    assert!(dashboard.card("Lightning out") >= Some(2.0));
    assert!(dashboard.card("Failed") >= Some(1.0));
    assert!(dashboard.card("Fees") > Some(0.0));
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

fn hex_32(text: &str) -> [u8; 32] {
    let mut bytes = [0; 32];
    hex::decode_to_slice(text, &mut bytes).unwrap();
    bytes
}
