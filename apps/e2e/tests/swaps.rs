//! Wallets swap notes between the stack's hubs through its swap provider,
//! which they discover on Nostr, on the stack of `apps/e2e/run.sh`, which
//! runs these.

use licash::spend::lock_hash;
use licash_api::{
    swaps::{Request, State, Swap},
    Info,
};
use licash_e2e::{until, Eco, Wallet};
use licash_nostr::{discover, Role, Route};
use licash_wallet::{Lock, LockState, SwapLimits, SwapState};
use rand_core::{OsRng, RngCore};

/// What the provider holds at hub 2 for the tests.
const STOCK: u64 = 60_000;

fn held(wallet: &Wallet, hub: &str) -> u64 {
    let balance = wallet.balance().unwrap();
    balance
        .iter()
        .find(|b| b.hub == hub)
        .map_or(0, |b| b.available)
}

/// Wait for the provider's swap of the client's lock `id` to reach `state`.
async fn reaches(eco: &Eco, id: &str, state: State) -> anyhow::Result<Swap> {
    until(&format!("swap {id} {state:?}"), 120, || async {
        let swap = eco.swap_state(id).await?;
        Ok((swap.state == state).then_some(swap))
    })
    .await
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn wallets_swap_between_hubs_through_a_provider_they_discover() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    let relays = [eco.relay().to_owned()];
    let found = until("the provider announced", 120, || async {
        let found = discover(&relays, &Route::Direct, Role::Swaps, "regtest").await?;
        Ok((!found.is_empty()).then_some(found))
    })
    .await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].announcement.urls, [eco.swaps()]);
    let info = eco.swaps_info().await?;
    assert!(found[0].named_by(info.nostr.as_deref()), "{found:?}");
    assert_eq!((info.fee.base, info.fee.ppm, info.margin), (1, 5000, 12));

    eco.stock(STOCK).await?;
    let alice = eco.wallet("swapper")?;
    eco.pay_in(&alice, hub1, 30_000).await?;
    alice.add_hub(hub2).await?;
    let swap = alice
        .swap(eco.swaps(), hub1, hub2, 10_000, SwapLimits::usual(10_000))
        .await?;
    assert_eq!(swap.state, SwapState::Done, "{swap:?}");
    // 1 + 5000 ppm of 10 000.
    assert_eq!(swap.fee, 51);
    assert_eq!(held(&alice, hub2), 10_000);

    // The provider claims Alice's lock with the preimage she revealed.
    let id = swap.lock.clone().unwrap();
    reaches(&eco, &id, State::Done).await?;
    alice.resume().await?;
    let ours = alice.locks()?.into_iter().find(|l| l.id == id).unwrap();
    assert!(matches!(ours.state, LockState::Paid(_)), "{ours:?}");
    assert_eq!(ours.amount, 10_051);

    // Beyond what it holds at hub 2: Alice locks nothing.
    let e = alice
        .swap(eco.swaps(), hub1, hub2, 190_000, SwapLimits::usual(190_000))
        .await
        .unwrap_err();
    assert!(matches!(e, licash_wallet::Error::Invalid(_)), "{e}");
    assert_eq!(alice.swaps()?.len(), 1);
    Ok(())
}

/// Bob's own lock of `amount` and the provider's fee to the provider at
/// hub 1 under the hash of `preimage`, expiring at `expiry`, and the swap
/// he asks for with it.
async fn ask(
    eco: &Eco,
    bob: &Wallet,
    hubs: (&Info, &Info),
    preimage: &[u8; 32],
    expiry: u32,
) -> anyhow::Result<(Lock, Swap)> {
    let info = eco.swaps_info().await?;
    let amount = 5_000;
    let total = amount + info.fee.due(amount);
    let lock = bob
        .lock(
            eco.hub(1),
            &info.address,
            total,
            lock_hash(preimage),
            Some(expiry),
        )
        .await?;
    let id = licash_api::lock_id(&lock.id).map_err(|p| anyhow::anyhow!(p.message))?;
    let request = Request::new(
        &hubs.0.address,
        &id,
        &hubs.1.address,
        &bob.address(),
        amount,
    );
    let swap = eco.ask_swap(&request).await?;
    assert_eq!(swap.state, State::Locked, "{swap:?}");
    // The provider's lock expires its margin earlier.
    assert_eq!(swap.expiry, expiry - 12);
    Ok((lock, swap))
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_provider_restarted_takes_its_swaps_on_and_unclaimed_locks_return() -> anyhow::Result<()>
{
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    eco.stock(STOCK).await?;
    let bob = eco.wallet("bob-swaps")?;
    // Two payments in, for two locks at once.
    eco.pay_in(&bob, hub1, 8_000).await?;
    eco.pay_in(&bob, hub1, 8_000).await?;
    bob.add_hub(hub2).await?;
    let hubs = (eco.info(hub1).await?, eco.info(hub2).await?);
    let expiry = eco.height().await? + 40;
    let mut preimages = [[0; 32]; 2];
    for preimage in &mut preimages {
        OsRng.fill_bytes(preimage);
    }
    let (claimed, theirs) = ask(&eco, &bob, (&hubs.0, &hubs.1), &preimages[0], expiry).await?;
    let (unclaimed, other) = ask(&eco, &bob, (&hubs.0, &hubs.1), &preimages[1], expiry).await?;

    // Bob claims the provider's first lock while the provider restarts:
    // started again, it claims his lock with the preimage he revealed.
    eco.restart("swaps").await?;
    let lock = hex::encode(&theirs.lock.unwrap().0);
    let received = bob.claim(hub2, &lock, &preimages[0]).await?;
    assert_eq!(received.amount, 5_000);
    reaches(&eco, &claimed.id, State::Done).await?;

    // Unclaimed, the provider's second lock returns to it once expired at
    // the hub's depth of 3.
    let height = eco.height().await?;
    eco.mine(other.expiry + 3 + 1 - height).await?;
    reaches(&eco, &unclaimed.id, State::Refunded).await?;
    // Bob's returns to him once his expired.
    let height = eco.height().await?;
    eco.mine(expiry + 3 + 1 - height).await?;
    let refunded = until("Bob's lock refunded", 120, || async {
        let lock = bob.follow_lock(&unclaimed.id).await;
        Ok(lock.ok().filter(|l| l.state == LockState::Refunded))
    })
    .await?;
    assert_eq!(refunded.amount, 5_026);
    assert_eq!(held(&bob, hub2), 5_000);

    // The operator sees both, in its status and its events.
    let status = eco
        .exec("swaps", &["licash-swaps", "status", "--json"])
        .await?;
    let status: serde_json::Value = serde_json::from_slice(&status)?;
    assert!(status["stats"]["done"].as_u64() >= Some(1), "{status}");
    assert!(status["stats"]["refunded"].as_u64() >= Some(1), "{status}");
    let events = eco.exec("swaps", &["licash-swaps", "events"]).await?;
    let events = String::from_utf8(events)?;
    for kind in ["accepted", "done", "refunded"] {
        assert!(events.contains(&format!("\"kind\":\"{kind}\"")), "{events}");
    }
    Ok(())
}
