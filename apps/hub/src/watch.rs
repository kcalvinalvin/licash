//! The hub's scan of Bitcoin: it follows the best chain of a node or an
//! Esplora server, block by block, rewinds where that chain left the
//! scanned one, adds checkpoints as blocks get buried and tells the hub
//! when its scan reached the tip.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use bitcoin::hashes::Hash;
use licash_chain_client::Source;
use licash_hub::Hub;

/// Most blocks between checkpoints: fresh ones keep a holder's proof short.
const CHECKPOINT_EVERY: u32 = 144;

/// Follow `source` into `hub`, a round every `every`, for good. Stops only
/// if `source` follows a chain the hub cannot: another network than its
/// checkpoints', or one without a checkpoint profile.
pub async fn watch(hub: Arc<Hub>, source: Source, every: Duration) {
    let mut failing = Failing::default();
    let network = loop {
        match source.network().await {
            Ok(network) => break network,
            Err(e) => failing.failed(&source, &e),
        }
        tokio::time::sleep(every).await;
    };
    let profile = match licash_chain_client::profile(network) {
        Ok(profile) => profile,
        Err(e) => {
            tracing::error!(chain = %source, "{e:#}: not scanning");
            return;
        }
    };
    if hub.checkpoint().is_some_and(|c| c.profile() != profile) {
        tracing::error!(chain = %source, %network, "the hub follows another network: not scanning");
        return;
    }
    tracing::info!(chain = %source, %network, "following Bitcoin");
    let mut reached = false;
    loop {
        match step(&hub, &source).await {
            Ok(now) => {
                failing.recovered(&source);
                if now != reached {
                    tracing::info!(synced = now, "the scan reached the chain's tip");
                }
                reached = now;
            }
            Err(e) => {
                failing.failed(&source, &e);
                reached = false;
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// The last failure, logged once until it changes or the scan recovers.
#[derive(Default)]
struct Failing(Option<String>);

impl Failing {
    fn failed(&mut self, source: &Source, e: &anyhow::Error) {
        let text = format!("{e:#}");
        if self.0.as_ref() != Some(&text) {
            tracing::warn!(chain = %source, "following Bitcoin: {text}");
        } else {
            tracing::debug!(chain = %source, "following Bitcoin: {text}");
        }
        self.0 = Some(text);
    }

    fn recovered(&mut self, source: &Source) {
        if self.0.take().is_some() {
            tracing::info!(chain = %source, "following Bitcoin again");
        }
    }
}

/// One round: scan what `source`'s best chain has beyond the hub's scan,
/// rewinding first where it left it, and tell the hub whether its scan
/// reached that chain's tip. Whether it did. The first round takes a
/// checkpoint [`licash_hub::Timing::bury`] blocks deep.
pub async fn step(hub: &Arc<Hub>, source: &Source) -> anyhow::Result<bool> {
    let timing = hub.info().policy.timing;
    let tip = source.tip().await.context("the chain's tip")?;
    let theirs = tip.hash.to_byte_array();
    if blocking(hub, Hub::scanned).await?.is_none() {
        let height = (tip.height + 1).saturating_sub(timing.bury);
        let checkpoint = source
            .checkpoint(height)
            .await
            .context("the first checkpoint")?;
        blocking(hub, move |hub| hub.add_checkpoint(&checkpoint)).await?;
    }
    loop {
        let (height, ours) = blocking(hub, Hub::scanned).await?.context("no scan")?;
        if height < tip.height {
            let next = source.block(&source.hash(height + 1).await?).await?;
            if next.header.prev_blockhash.to_byte_array() == ours {
                blocking(hub, move |hub| hub.observe(height + 1, &next)).await?;
                tracing::debug!(height = height + 1, "scanned");
            } else {
                rewind(hub, source, height, height).await?;
            }
            continue;
        }
        // At or beyond the tip: the same chain, one the backend left, or
        // a backend behind the scan.
        let at = blocking(hub, move |hub| hub.scanned_at(tip.height)).await?;
        if at != Some(theirs) {
            rewind(hub, source, tip.height, height).await?;
            continue;
        }
        break;
    }
    let reached = blocking(hub, move |hub| hub.synced(tip.height, &theirs)).await?;
    if reached {
        checkpoint(hub, source, tip.height).await?;
    }
    Ok(reached)
}

/// Rewind the hub's scan, to `top`, to the highest block from `from` down
/// that `source`'s best chain shares. A reorganization of the newest
/// checkpoint is refused by the hub, which alerts.
async fn rewind(hub: &Arc<Hub>, source: &Source, from: u32, top: u32) -> anyhow::Result<()> {
    let floor = hub.checkpoint().context("no checkpoint")?.height();
    let mut height = from;
    loop {
        let ours = blocking(hub, move |hub| hub.scanned_at(height)).await?;
        if ours.is_some() && ours == Some(source.hash(height).await?.to_byte_array()) {
            break;
        }
        if height <= floor {
            // Refused, and alerted, by the hub.
            return blocking(hub, move |hub| hub.rewind(height)).await;
        }
        height -= 1;
    }
    // The backend changed its chain back meanwhile.
    if height >= top {
        return Ok(());
    }
    blocking(hub, move |hub| hub.rewind(height + 1)).await
}

/// Add a checkpoint at the block [`licash_hub::Timing::bury`] deep below
/// `tip`, once the newest is far enough below it that the locks it admits
/// stay within [`licash_hub::Timing::max_headers`] of their checkpoint.
async fn checkpoint(hub: &Arc<Hub>, source: &Source, tip: u32) -> anyhow::Result<()> {
    let timing = hub.info().policy.timing;
    let newest = hub.checkpoint().context("no checkpoint")?.height();
    let budget = timing
        .max_headers
        .saturating_sub(timing.window.saturating_add(timing.bury));
    let every = (budget / 2).clamp(1, CHECKPOINT_EVERY);
    let buried = (tip + 1).saturating_sub(timing.bury);
    if buried < newest.saturating_add(every) {
        return Ok(());
    }
    let checkpoint = source.checkpoint(buried).await.context("a checkpoint")?;
    blocking(hub, move |hub| hub.add_checkpoint(&checkpoint)).await
}

/// Run `op` off the async workers: it waits on the ledger's fsync.
async fn blocking<T: Send + 'static>(
    hub: &Arc<Hub>,
    op: impl FnOnce(&Hub) -> Result<T, licash_hub::Error> + Send + 'static,
) -> anyhow::Result<T> {
    let hub = hub.clone();
    Ok(tokio::task::spawn_blocking(move || op(&hub)).await??)
}
