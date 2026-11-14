//! The operators of the stack's hubs and swap provider are notified on its
//! Gotify server, on the stack of `apps/e2e/run.sh`, which runs these.

use anyhow::Context;
use licash_e2e::{until, Eco};

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn operators_are_notified_on_gotify() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let heard = eco.notifications().await?;
    let find = |title: &str, part: &str| {
        let found = heard
            .iter()
            .find(|n| n.title == title && n.message.contains(part));
        found.with_context(|| format!("no {title:?} of {part:?} in {heard:#?}"))
    };
    // Each hub's start, at low priority.
    for hub in ["hub1", "hub2"] {
        assert_eq!(find(&format!("{hub}: Hub started"), "Boot")?.priority, 3);
    }
    // Started with no channels, and the provider with nothing to swap with:
    // alerts, at high priority.
    let channels = find("hub1: Hub alert", "Lightning node can")?;
    assert_eq!(channels.priority, 8);
    let stock = find("swaps: Swaps alert", "No swaps to")?;
    assert_eq!(stock.priority, 8);

    // A payment received at hub 2: its lock is notified as it is paid.
    let last = heard.first().map_or(0, |n| n.id);
    let frank = eco.wallet("frank")?;
    eco.pay_in(&frank, eco.hub(2), 4_321).await?;
    let paid = until("hub2 notifies the lock paid", 60, || async {
        let heard = eco.notifications().await?;
        Ok(heard
            .into_iter()
            .find(|n| n.id > last && n.title == "hub2: Lock paid"))
    })
    .await?;
    assert!(paid.message.starts_with("4321 paid"), "{paid:?}");
    assert_eq!(paid.priority, 3);
    Ok(())
}
