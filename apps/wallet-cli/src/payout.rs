//! `licash-wallet payout` and `payouts`: notes paid out to an address on
//! Bitcoin through a swap provider, which locks a coin on chain for the
//! wallet's lock, and the wallet's claims of those coins.

use anyhow::{bail, Context};
use licash_chain_client::Source;
use licash_nostr::{wallet::payout_providers, Route};
use licash_transport::Http;
use licash_wallet::{Payout, PayoutState, Wallet};
use serde_json::json;

use crate::{onchain, show};

/// The payout's costs at most on top of its amount by default: 1% and
/// this, room for the coin's claim at a few times the fee rate.
const USUAL_MAX_FEE: u64 = 2_000;

fn source(chain: &str) -> anyhow::Result<Source> {
    chain.parse().context("--chain")
}

/// The provider to pay out through: at its URL, or the one announced on
/// relays, reached through a route, asking least.
pub enum Provider {
    At(String),
    Announced(Vec<String>, Route),
}

/// Pay `amount` to `to` from notes at `from` through `provider`, the
/// claim budgeted at `fee_rate` or the chain's estimate, and take the
/// payout on once.
pub async fn pay(
    wallet: &Wallet<Http>,
    chain: &str,
    (provider, from, to): (Provider, &str, &str),
    amount: u64,
    (max_fee, fee_rate): (Option<u64>, Option<u64>),
    json: bool,
) -> anyhow::Result<()> {
    let source = source(chain)?;
    let rate = onchain::rate(&source, fee_rate).await?;
    let max_fee = max_fee.unwrap_or(amount / 100 + USUAL_MAX_FEE);
    let provider = match provider {
        Provider::At(url) => url,
        Provider::Announced(relays, route) => {
            let relays = match relays {
                relays if !relays.is_empty() => relays,
                _ => licash_nostr::wallet::hub_relays(wallet)?,
            };
            if relays.is_empty() {
                bail!("no relay to find a provider on: name one with --relay, or a provider");
            }
            let limits = (max_fee, rate);
            let found = payout_providers(wallet, &relays, &route, (from, to), amount, limits);
            let found = found.await?.into_iter().next().with_context(|| {
                format!("no provider announced pays {amount} out from {from} within --max-fee")
            })?;
            let (costs, known) = (found.quote.costs(), found.known);
            tracing::info!(provider = %found.url, costs, known, "provider found");
            found.url
        }
    };
    let payout = wallet
        .payout(&provider, from, to, amount, max_fee, rate)
        .await?;
    if let PayoutState::Failed(reason) = &payout.state {
        show(json, payout_json(&payout), payout_line(&payout));
        bail!("the payout failed ({reason}): its lock, if made, returns once expired");
    }
    let payout = follow(wallet, &source, &payout.hash).await?;
    show(
        json,
        payout_json(&payout),
        format!(
            "{}\nfollow it with `licash-wallet payouts --chain` until done",
            payout_line(&payout)
        ),
    );
    Ok(())
}

/// Take the payout `hash` on, then broadcast the wallet's claims.
async fn follow(wallet: &Wallet<Http>, source: &Source, hash: &str) -> anyhow::Result<Payout> {
    let blocks = onchain::Blocks(source.clone());
    let followed = wallet.follow_payout(hash, &blocks).await;
    broadcast(wallet, source).await?;
    match followed {
        Ok(payout) => Ok(payout),
        Err(licash_wallet::Error::Pending { reason, .. }) => {
            tracing::info!(hash, reason, "payout pending");
            let payouts = wallet.payouts()?;
            Ok(payouts
                .into_iter()
                .find(|p| p.hash == hash)
                .context("the payout is not recorded")?)
        }
        Err(e) => Err(e.into()),
    }
}

/// Broadcast the claims of payouts not done: one the chain refuses is
/// broadcast again next time, as its coin may not be in the chain's view
/// yet.
async fn broadcast(wallet: &Wallet<Http>, source: &Source) -> anyhow::Result<()> {
    for claim in wallet.payout_claims()? {
        match source.broadcast(&claim).await {
            Ok(txid) => tracing::info!(%txid, "claim broadcast"),
            Err(e) => tracing::warn!(txid = %claim.compute_txid(), "claim not broadcast: {e:#}"),
        }
    }
    Ok(())
}

/// The payouts, newest first: taken on first with `chain`, their claims
/// broadcast.
pub async fn list(wallet: &Wallet<Http>, chain: Option<&str>, json: bool) -> anyhow::Result<()> {
    if let Some(chain) = chain {
        let source = source(chain)?;
        let (_, pending) = wallet
            .follow_payouts(&onchain::Blocks(source.clone()))
            .await?;
        for (hash, reason) in pending {
            tracing::info!(hash, reason, "payout pending");
        }
        broadcast(wallet, &source).await?;
    }
    let payouts = wallet.payouts()?;
    let lines: Vec<String> = payouts.iter().map(payout_line).collect();
    show(
        json,
        payouts.iter().map(payout_json).collect(),
        lines.join("\n"),
    );
    Ok(())
}

fn state(state: &PayoutState) -> String {
    match state {
        PayoutState::Open => "open".into(),
        PayoutState::Claimed => "claimed".into(),
        PayoutState::Done => "done".into(),
        PayoutState::Failed(reason) => format!("failed ({reason})"),
        _ => "unknown".into(),
    }
}

fn payout_json(payout: &Payout) -> serde_json::Value {
    json!({
        "hash": payout.hash, "provider": payout.provider, "from": payout.from,
        "address": payout.address, "amount": payout.amount, "budget": payout.budget,
        "fee": payout.fee, "lock": payout.lock,
        "funding": payout.funding.map(|o| o.to_string()),
        "claim": payout.claim.map(|t| t.to_string()),
        "state": state(&payout.state), "created_at": payout.created_at,
    })
}

fn payout_line(payout: &Payout) -> String {
    format!(
        "{}  payout {}  {} + {}  {} -> {}  {}",
        payout.created_at,
        state(&payout.state),
        payout.amount,
        payout.budget + payout.fee,
        payout.from,
        payout.address,
        payout.hash
    )
}
