//! Notes paid out to addresses on Bitcoin through swap providers announced
//! on Nostr: how the app pays an address, or cashes out a hub's notes.

use std::sync::Arc;

use bitcoin::FeeRate;
use licash_nostr::wallet::{hub_relays, payout_providers};
use licash_transport::Http;
use licash_wallet::{payout_depth, Payout, PayoutState};

use super::Wallet;
use crate::{chain::Chain, Activity, ChainSource, Details, Kind, LicashError, PayoutQuote, State};

/// The fee rate a claim confirms at soon, in sat/vB, as the app judges it
/// knowing no estimates: a claim is budgeted at a few times this or the
/// provider's rate, the higher.
const CLAIM_RATE: u64 = 2;

/// Quotes fitted again at most, to pay out all a hub's notes pay.
const FIT_ROUNDS: usize = 6;

fn claim_rate() -> FeeRate {
    FeeRate::from_sat_per_vb(CLAIM_RATE).expect("a rate")
}

/// What a payout of `amount` may cost on top to be quoted at all: 5% and
/// 5000 sat. The user sees what it costs before paying.
fn max_fee(amount: u64) -> u64 {
    amount / 20 + 5_000
}

/// A first guess at what `held` pays out, fees aside: 2% and 2000 less.
fn all_but_fees(held: u64) -> u64 {
    held.saturating_sub(held / 50 + 2_000)
}

/// What the lock of a payout quoted pays its provider: the coin's value
/// and the provider's fee.
fn locked(quote: &licash_wallet::PayoutQuote) -> u64 {
    quote.value().saturating_add(quote.fee)
}

/// Payouts on Bitcoin.
#[uniffi::export]
impl Wallet {
    /// What paying `amount` to `address`, on Bitcoin, costs from notes at
    /// `hub`, else at the hub trusted holding the most, through the swap
    /// provider announced on the relays the hubs name that asks least: of
    /// all the notes there, what they pay out once its fees are paid if
    /// `amount` is none. Refused unless the notes there pay it all.
    pub fn payout_quote(
        &self,
        address: String,
        amount: Option<u64>,
        hub: Option<String>,
    ) -> Result<PayoutQuote, LicashError> {
        let address = address.trim();
        let hub = match hub {
            Some(hub) => hub,
            None => self.richest()?,
        };
        let held = self.held(&hub)?;
        let first = amount.unwrap_or_else(|| all_but_fees(held));
        if first == 0 {
            return Err(LicashError::Insufficient {
                needed: held + 1,
                available: held,
            });
        }
        let relays = hub_relays(&self.wallet)?;
        if relays.is_empty() {
            return Err(LicashError::invalid(
                "no hub you trust names relays to find swap providers on",
            ));
        }
        let limits = (max_fee(first), claim_rate());
        let found = payout_providers(
            &self.wallet,
            &relays,
            &self.route,
            (&hub, address),
            first,
            limits,
        );
        let found = self
            .runtime
            .block_on(found)
            .map_err(|e| LicashError::from_any(e, |reason| LicashError::Unavailable { reason }))?;
        let Some(best) = found.into_iter().next() else {
            return Err(LicashError::invalid(format!(
                "no swap provider found pays {first} sat out on Bitcoin now"
            )));
        };
        let quote = match amount {
            Some(_) => {
                self.wallet.lock_fee(&hub, locked(&best.quote))?;
                best.quote
            }
            None => self.fit(&best.url, (&hub, held), address, best.quote)?,
        };
        Ok(PayoutQuote {
            address: address.to_owned(),
            amount: quote.amount,
            fee: quote.costs(),
            hub,
            provider: best.url,
            provider_name: best.name,
            depth: payout_depth(self.wallet.network()),
        })
    }

    /// Pay out as `quote` says, costing at most its fee: the payout, under
    /// way until [`Wallet::follow_payouts`] finds its claim confirmed. A
    /// payout whose lock was made but which fails gets its notes back once
    /// the lock expires.
    pub fn pay_out(&self, quote: PayoutQuote) -> Result<Activity, LicashError> {
        let pay = self.wallet.payout(
            &quote.provider,
            &quote.hub,
            &quote.address,
            quote.amount,
            quote.fee,
            claim_rate(),
        );
        let payout = self.runtime.block_on(pay)?;
        tracing::info!(hash = %payout.hash, address = %payout.address, "paying out");
        Ok(activity(&payout, self.lock_fee(&payout)?))
    }

    /// Take the payouts under way on, on `chain`: claim each provider's
    /// coin once confirmed, then broadcast through `chain` the claims of
    /// those not done, again each time, as a claim dropped loses its coin
    /// once it times out. Whether any is under way still.
    pub fn follow_payouts(&self, chain: Arc<dyn ChainSource>) -> Result<bool, LicashError> {
        let followed = self
            .runtime
            .block_on(self.wallet.follow_payouts(&Chain(chain.clone())));
        // Whatever the follow made, even if it failed after.
        for claim in self.wallet.payout_claims()? {
            if let Err(e) = chain.broadcast(bitcoin::consensus::serialize(&claim)) {
                tracing::debug!(txid = %claim.compute_txid(), %e, "broadcasting a claim");
            }
        }
        let (payouts, pending) = followed?;
        for (id, reason) in pending {
            tracing::info!(%id, %reason, "payout pending");
        }
        Ok(payouts
            .iter()
            .any(|p| matches!(p.state, PayoutState::Open | PayoutState::Claimed)))
    }
}

impl Wallet {
    /// The hub trusted holding the most.
    fn richest(&self) -> Result<String, LicashError> {
        let balance = self.wallet.balance()?.into_iter();
        let richest = balance.max_by_key(|b| b.available).map(|b| b.hub);
        richest.ok_or_else(|| LicashError::invalid("no hub trusted holds notes"))
    }

    /// What the wallet holds at `hub` to pay with.
    fn held(&self, hub: &str) -> Result<u64, LicashError> {
        let balance = self.wallet.balance()?.into_iter();
        let held = balance.filter(|b| b.hub == hub).map(|b| b.available);
        held.max()
            .ok_or_else(|| LicashError::invalid(format!("no hub trusted is {hub}")))
    }

    /// The quote of `provider` paying out the most the notes `held` at
    /// `hub` pay with its fees: from `quote`, fitted again to what the
    /// lock spends.
    fn fit(
        &self,
        provider: &str,
        (hub, held): (&str, u64),
        address: &str,
        mut quote: licash_wallet::PayoutQuote,
    ) -> Result<licash_wallet::PayoutQuote, LicashError> {
        let mut best: Option<licash_wallet::PayoutQuote> = None;
        let mut short = None;
        for _ in 0..FIT_ROUNDS {
            let amount = match self.wallet.lock_fee(hub, locked(&quote)) {
                Ok(lock_fee) => {
                    let spends = locked(&quote).saturating_add(lock_fee);
                    let room = held.saturating_sub(spends);
                    if best.as_ref().is_none_or(|b| b.amount < quote.amount) {
                        best = Some(quote.clone());
                    }
                    quote.amount + room
                }
                Err(licash_wallet::Error::Insufficient { needed, available }) => {
                    short = Some(licash_wallet::Error::Insufficient { needed, available });
                    let over = needed.saturating_sub(available).max(1);
                    quote.amount.saturating_sub(over)
                }
                Err(e) => return Err(e.into()),
            };
            // No more to find: what fits already pays as much.
            let fitting = best.as_ref().is_some_and(|b| b.amount >= amount);
            if amount == quote.amount || amount == 0 || fitting {
                break;
            }
            let asked = self.wallet.payout_quote(
                provider,
                hub,
                address,
                amount,
                max_fee(amount),
                claim_rate(),
            );
            quote = match self.runtime.block_on(asked) {
                Ok(quote) => quote,
                // Past what the provider pays out, say.
                Err(e) if best.is_some() => {
                    tracing::debug!(amount, %e, "paying out all: no more");
                    break;
                }
                Err(e) => return Err(e.into()),
            };
        }
        match (best, short) {
            (Some(best), _) => Ok(best),
            (None, Some(short)) => Err(short.into()),
            (None, None) => Err(LicashError::Insufficient {
                needed: held + 1,
                available: held,
            }),
        }
    }

    /// The hub's fee on the lock of `payout`, once made.
    fn lock_fee(&self, payout: &Payout) -> Result<u64, LicashError> {
        let Some(id) = &payout.lock else {
            return Ok(0);
        };
        let locks = self.wallet.locks()?;
        Ok(locks.iter().find(|l| &l.id == id).map_or(0, |l| l.fee))
    }
}

/// The payouts, as lines of the history.
pub(super) fn activities(
    wallet: &licash_wallet::Wallet<Http>,
) -> Result<Vec<Activity>, LicashError> {
    let locks = wallet.locks()?;
    let fee = |p: &Payout| {
        let lock = p
            .lock
            .as_ref()
            .and_then(|id| locks.iter().find(|l| &l.id == id));
        lock.map_or(0, |l| l.fee)
    };
    Ok(wallet
        .payouts()?
        .iter()
        .map(|p| activity(p, fee(p)))
        .collect())
}

/// More of the payout of `hash`.
pub(super) fn details(
    wallet: &licash_wallet::Wallet<Http>,
    hash: &str,
) -> Result<Details, LicashError> {
    let payout = wallet.payouts()?.into_iter().find(|p| p.hash == hash);
    let payout =
        payout.ok_or_else(|| LicashError::invalid(format!("nothing in the history is {hash}")))?;
    let failure = match &payout.state {
        PayoutState::Failed(why) => Some(why.clone()),
        _ => None,
    };
    Ok(Details::Payout {
        hub: payout.from,
        provider: payout.provider,
        address: payout.address,
        coin: payout.funding.map(|o| o.txid.to_string()),
        claim: payout.claim.map(|t| t.to_string()),
        failure,
    })
}

/// The line of the history of `payout`: what the address gets, and what
/// it costs on top, `lock_fee` the hub's; nothing once it failed, its
/// notes back once its lock expires.
fn activity(payout: &Payout, lock_fee: u64) -> Activity {
    let fee = payout
        .budget
        .saturating_add(payout.fee)
        .saturating_add(lock_fee);
    let (state, detail, fee) = match &payout.state {
        PayoutState::Done => (State::Done, payout.address.clone(), fee),
        PayoutState::Failed(why) => (State::Failed, why.clone(), 0),
        _ => (State::Pending, payout.address.clone(), fee),
    };
    Activity {
        id: payout.hash.clone(),
        kind: Kind::Payout,
        state,
        amount: payout.amount,
        fee,
        detail,
        at: Some(payout.created_at),
    }
}
