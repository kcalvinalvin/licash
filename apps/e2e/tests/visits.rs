//! Paying a payee at a hub the payer does not trust: visited where the
//! payee's code says to reach it, proving it is the hub the code names,
//! and swapped into from a hub the payer trusts.

use anyhow::Context;
use licash_e2e::{until, Eco};
use licash_nostr::{wallet, Route};
use licash_wallet::{SwapLimits, SwapState};

/// What the provider holds at hub 2 for the tests.
const STOCK: u64 = 60_000;

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_wallet_pays_a_code_at_a_hub_it_does_not_trust_once_it_proves_it_is_the_one_named(
) -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let (hub1, hub2) = (eco.hub(1), eco.hub(2));
    eco.stock(STOCK).await?;
    let alice = eco.wallet("visitor")?;
    eco.pay_in(&alice, hub1, 30_000).await?;
    let bob = eco.wallet("visited")?;
    bob.add_hub(hub2).await?;
    let code = bob.payment_code(&[hub2.to_owned()], false)?;
    let licash = code.licash.as_ref().context("a Licash code")?;
    assert_eq!(licash.locations.len(), 1, "{code}");

    // A code naming hub 2 where hub 1 answers: not visited, nothing paid.
    let mut forged = code.clone();
    if let Some(licash) = forged.licash.as_mut() {
        licash.locations[0].1 = hub1.to_owned();
    }
    let refused = wallet::plan(&alice, &forged, Some(5_000), &Route::Direct).await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(alice.visited()?.is_empty());

    // Alice trusts hub 1 only: hub 2, visited, is swapped into.
    let plan = wallet::plan(&alice, &code, Some(5_000), &Route::Direct).await?;
    assert!(!plan.trusted, "{plan:?}");
    assert_eq!(plan.hub, hub2);
    let swap = plan.swap.as_ref().context("a swap")?;
    assert_eq!(swap.from, hub1);
    assert!(
        swap.provider.quote.amount <= 2 * (5_000 + plan.fee),
        "{plan:?}"
    );
    assert_eq!(alice.hubs()?.len(), 1);
    assert_eq!(alice.visited()?.len(), 1);

    let paid = wallet::pay_planned(&alice, &code, &plan).await?;
    assert_eq!((paid.hub.as_str(), paid.amount), (hub2, 5_000));
    // Kept at hub 2, which Alice only visits, for Bob to collect there.
    assert!(paid.paid.kept, "{paid:?}");
    let got = until("Bob's payment in his mailbox", 60, || async {
        let collected = wallet::collect(&bob, &Route::Direct).await?;
        let sum: u64 = collected.received.iter().map(|r| r.amount).sum();
        Ok((sum > 0).then_some(sum))
    })
    .await?;
    assert_eq!(got, 5_000);

    // What the swap brought past it stays at hub 2, counted apart.
    let apart = alice.visited_balance()?;
    assert_eq!(apart.len(), 1);
    assert_eq!(apart[0].hub, hub2);
    // Less the payment and the hub's fee on it, at most as planned.
    let left = swap.provider.quote.amount - 5_000;
    let range = left - plan.fee..=left;
    assert!(range.contains(&apart[0].available), "{apart:?} {plan:?}");
    assert_eq!(alice.balance()?.len(), 1);

    // Held enough there, a code naming it is paid there without a swap.
    let swap = alice
        .swap(eco.swaps(), hub1, hub2, 3_000, SwapLimits::usual(3_000))
        .await?;
    assert_eq!(swap.state, SwapState::Done, "{swap:?}");
    let plan = wallet::plan(&alice, &code, Some(2_000), &Route::Direct).await?;
    assert!(!plan.trusted && plan.swap.is_none(), "{plan:?}");
    assert_eq!(plan.hub, hub2);
    let paid = wallet::pay_planned(&alice, &code, &plan).await?;
    assert_eq!((paid.hub.as_str(), paid.amount), (hub2, 2_000));
    let got = until("Bob's second payment", 60, || async {
        let collected = wallet::collect(&bob, &Route::Direct).await?;
        let sum: u64 = collected.received.iter().map(|r| r.amount).sum();
        Ok((sum > 0).then_some(sum))
    })
    .await?;
    assert_eq!(got, 2_000);
    Ok(())
}
