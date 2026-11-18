//! Her exit from contracts ending, once she consented: the phone's own
//! keeper. Of each live contract nearing the last block its claim request
//! may confirm in ([`safety::Safety::request_by`]), less the exit margin
//! she consented with: nothing while the other contracts at its hub
//! recover her notes there; else those notes paid out to her address,
//! then the contract folded; forced closed if they are not paid out by a
//! day before that block.
//!
//! Her consent is a row of `contracts.sqlite`: none, never given, revoked
//! or restored from a backup, and nothing is done, only told. Its exit
//! margin is the one she consented with, not a preference read each time.
//! Each payout's ticket is kept before it is paid; the rest is read from
//! the wallet, the folds and the recoveries as they stand, each round.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::FeeRate;
use licash::spend::ChainProfile;
use licash_wallet::{PayoutQuote, PayoutState, Transport, Wallet};

use super::store::Store;
use super::{recovery, safety, Chain, Contract};

/// Notes at a hub worth no exit: a payout of them costs about as much,
/// and a forced close more.
pub const FLOOR: u64 = 1_000;

/// Blocks allowed for a timeout to confirm, as the apps plan with.
pub const TIMEOUT_SLACK: u32 = 6;

/// Blocks a day on `network`: Mutinynet's every 30 seconds.
pub fn blocks_a_day(network: ChainProfile) -> u32 {
    match network {
        ChainProfile::Mutinynet => 2_880,
        _ => 144,
    }
}

/// Her consent to exit contracts ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consent {
    /// Where her notes are paid out to, an address on Bitcoin.
    pub address: String,
    /// Blocks kept for a forced close to play out in.
    pub exit_margin: u32,
    pub given_at: i64,
}

/// Her consent, if given and not revoked: none where no contract was
/// ever opened.
pub fn consent(dir: &Path) -> Result<Option<Consent>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(None);
    };
    Ok(store
        .exit_consent()?
        .map(|(address, exit_margin, given_at)| Consent {
            address,
            exit_margin,
            given_at,
        }))
}

/// Consent to exit contracts ending, paying her notes out to `address` on
/// the wallet's network, with `exit_margin` blocks: in place of any given
/// before. Refused where no contract was ever opened.
pub fn consent_to<T: Transport>(
    wallet: &Wallet<T>,
    dir: &Path,
    address: &str,
    exit_margin: u32,
) -> Result<Consent> {
    let store = Store::existing(dir)?.context("no contracts to exit")?;
    let network = licash_onchain::network(wallet.network());
    let address = bitcoin::Address::from_str(address.trim())
        .map_err(|e| anyhow!("not an address on Bitcoin: {e}"))?
        .require_network(network)
        .map_err(|_| anyhow!("an address not on {}", wallet.network()))?
        .to_string();
    ensure!(exit_margin > 0, "an exit margin of no blocks");
    store.consent_to_exit(&address, exit_margin)?;
    consent(dir)?.context("consent not kept")
}

/// Keep the locks `wallet` makes at a hub claimable on Bitcoin by her
/// contracts there in `dir` ([`Wallet::guard_locks`]).
pub fn guard_locks<T: Transport>(wallet: &Wallet<T>, dir: &Path) {
    let (seed, network) = (wallet.contract_secrets().seed, wallet.network());
    let dir = dir.to_path_buf();
    wallet.guard_locks(Arc::new(move |urls: &[String], tip: u32| {
        lock_horizon(&seed, network, &dir, urls, tip).map_err(|e| format!("{e:#}"))
    }));
}

/// The block by which a lock at the hub of `urls` must have expired at
/// depth, at `tip`, for the forced close of one of her contracts there in
/// `dir` to claim its refund: the latest of those her exit plans ([`drive`],
/// a close takes only the refunds owed as it starts). Planned with her
/// consent's margin, or none without it: she forces it herself then, as
/// late as she may. None if no contract protects notes there.
fn lock_horizon(
    seed: &[u8; 32],
    network: ChainProfile,
    dir: &Path,
    urls: &[String],
    tip: u32,
) -> Result<Option<u32>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(None);
    };
    let recovering: HashSet<[u8; 32]> = store.recoveries()?.iter().map(|r| r.id).collect();
    let mut horizon = None;
    for c in
        (store.list()?.iter()).filter(|c| c.at(urls) && c.live(tip) && !recovering.contains(&c.id))
    {
        horizon = horizon.max(Some(planned_force_at(seed, network, dir, &c.id, tip)?));
    }
    Ok(horizon)
}

/// The block from which her exit forces contract `id` in `dir` closed, as
/// planned at `tip` with her consent's margin, or none without it.
pub(super) fn planned_force_at(
    seed: &[u8; 32],
    network: ChainProfile,
    dir: &Path,
    id: &[u8; 32],
    tip: u32,
) -> Result<u32> {
    let margin = consent(dir)?.map_or(0, |c| c.exit_margin);
    let s = safety::of_seed(seed, network, dir, id, tip, (margin, TIMEOUT_SLACK))?;
    Ok(deadlines(s.request_by.unwrap_or(0), blocks_a_day(network)).1)
}

/// Revoke her consent: no step more is taken. Those under way go on.
pub fn revoke(dir: &Path) -> Result<()> {
    match Store::existing(dir)? {
        Some(store) => store.revoke_exit_consent(),
        None => Ok(()),
    }
}

/// Finds a provider to pay out notes: all those at the hub at `hub`, to
/// `address`, within the fees it judges usual.
pub trait CashOut {
    fn quote_all(&self, hub: &str, address: &str) -> impl Future<Output = Result<PayoutQuote>>;
}

/// What the exit does of a contract now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Not near its end yet.
    Waiting,
    /// The other contracts at its hub recover her notes there, or too few
    /// are left to exit for: it ends as it would.
    Covered,
    /// Her notes there are at risk, and she did not consent.
    NeedsConsent,
    /// Its exit margin never fitted it: she is told, nothing is done.
    NeverFitted,
    /// Her notes there paid out.
    CashingOut,
    /// A payout from its hub under way: waited for.
    Paying,
    /// Paid out, closed early with its hub: her reserve back, and any
    /// notes left (a fold claims them).
    Folding,
    /// Not paid out in time: forced closed.
    ForceClosing,
}

/// A contract's exit, as it stands this round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub id: [u8; 32],
    pub hub: String,
    pub step: Step,
    /// From this block on, her notes at its hub are paid out.
    pub cash_at: u32,
    /// From this block on, it is forced closed if they are not.
    pub force_at: u32,
    /// Her notes at its hub the others there do not recover.
    pub uncovered: u64,
    /// The locks owed to her that it alone claims on Bitcoin
    /// ([`super::activation`]), and the notes she funded it with that her
    /// wallet was never issued: what only its forced close recovers.
    pub owed: u64,
    /// Why its step failed this round, if it did.
    pub error: Option<String>,
}

/// What a contract's step turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub tip: u32,
    pub cash_at: u32,
    pub force_at: u32,
    /// The block she started opening it at.
    pub since: u32,
    pub consented: bool,
    pub uncovered: u64,
    /// The locks owed to her it claims, and the notes she funded it with
    /// herself that her wallet was never issued: what no payout recovers.
    pub owed: u64,
    /// Her notes at its hub held by payments and locks under way.
    pub pending: u64,
    /// A payout from its hub under way, hers or the exit's.
    pub paying: bool,
    /// A payout the exit made for it, done.
    pub cashed: bool,
}

/// The step of a contract seen as `v`.
pub fn plan(v: View) -> Step {
    if v.tip < v.cash_at {
        return Step::Waiting;
    }
    let at_risk = v.uncovered.saturating_add(v.owed) > FLOOR;
    if !at_risk {
        // Paid out by the exit, and nothing more on its way back: folded.
        return match (v.cashed, v.consented) {
            (true, true) if v.pending == 0 && !v.paying => Step::Folding,
            (true, true) => Step::Paying,
            _ => Step::Covered,
        };
    }
    if v.force_at <= v.since {
        return Step::NeverFitted;
    }
    if !v.consented {
        return Step::NeedsConsent;
    }
    if v.tip >= v.force_at {
        return Step::ForceClosing;
    }
    if v.paying {
        return Step::Paying;
    }
    // Only what a forced close claims is at risk: not folded, issued yet
    // or forced closed in time.
    if v.uncovered <= FLOOR {
        return Step::Waiting;
    }
    Step::CashingOut
}

/// The blocks of a contract whose request must confirm by `request_by`:
/// paid out three days before, forced closed one day before.
fn deadlines(request_by: u32, day: u32) -> (u32, u32) {
    (
        request_by.saturating_sub(3 * day),
        request_by.saturating_sub(day),
    )
}

/// Take each live contract's exit a step on, as [`plan`] says, at `tip`
/// of `chain`: one step a hub a round, as each changes her notes there.
/// `exit_margin` plans those she did not consent to exit, to tell her;
/// her consent's own margin plans the rest. `rate` is the fee rate of a
/// forced close.
pub async fn drive<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    cash_out: &impl CashOut,
    exit_margin: u32,
    rate: FeeRate,
) -> Result<Vec<Exit>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(vec![]);
    };
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    // Nothing reads as covered at no height.
    ensure!(tip > 0, "no chain height");
    let consent = consent(dir)?;
    let margin = consent.as_ref().map_or(exit_margin, |c| c.exit_margin);
    let day = blocks_a_day(wallet.network());
    let recovering: HashSet<[u8; 32]> = store.recoveries()?.iter().map(|r| r.id).collect();
    let live: Vec<Contract> = (store.list()?.into_iter())
        .filter(|c| c.live(tip) && !recovering.contains(&c.id))
        .collect();
    let mut due = HashMap::new();
    for c in &live {
        let s = safety::of(wallet, dir, &c.id, tip, margin, TIMEOUT_SLACK)?;
        due.insert(c.id, deadlines(s.request_by.unwrap_or(0), day));
    }
    let balance = wallet.balance()?;
    let payouts = wallet.payouts()?;
    let states: HashMap<&str, &PayoutState> = payouts
        .iter()
        .map(|p| (p.hash.as_str(), &p.state))
        .collect();
    let stake = super::activation::at_stake(wallet, &store, dir, tip)?;
    let mut acted: HashSet<String> = HashSet::new();
    let mut exits = Vec::new();
    for c in &live {
        let (cash_at, force_at) = due[&c.id];
        let urls = wallet.hub_urls(&c.hub)?;
        let held = |pick: fn(&licash_wallet::Balance) -> u64| -> u64 {
            (balance.iter())
                .filter(|b| urls.contains(&b.hub))
                .map(pick)
                .sum()
        };
        let others: Vec<Contract> = (live.iter())
            .filter(|o| o.id != c.id && o.at(&urls) && due[&o.id].1 > tip)
            .cloned()
            .collect();
        let covered: u64 = super::coverage(wallet, dir, &others, tip)?.values().sum();
        let paying = (payouts.iter()).any(|p| {
            urls.contains(&p.from) && matches!(p.state, PayoutState::Open | PayoutState::Claimed)
        });
        let cashed = (store.exit_payouts(&c.id)?.iter())
            .any(|t| states.get(t.as_str()) == Some(&&PayoutState::Done));
        let view =
            View {
                tip,
                cash_at,
                force_at,
                since: c.since,
                consented: consent.is_some(),
                uncovered: held(|b| b.available).saturating_sub(covered),
                owed: (stake.get(&c.id).copied().unwrap_or(0))
                    .saturating_add(if c.funded_received { 0 } else { c.funded }),
                pending: held(|b| b.pending),
                paying,
                cashed,
            };
        let step = plan(view);
        let mut exit = Exit {
            id: c.id,
            hub: c.hub.clone(),
            step,
            cash_at,
            force_at,
            uncovered: view.uncovered,
            owed: view.owed,
            error: None,
        };
        let acts = matches!(step, Step::CashingOut | Step::Folding | Step::ForceClosing);
        // One step a hub a round: the next reads her notes as it left them.
        if acts && !acted.insert(urls.first().cloned().unwrap_or_else(|| c.hub.clone())) {
            exit.error = Some("waits for the step taken at its hub this round".into());
            exits.push(exit);
            continue;
        }
        if let Some(consent) = consent.as_ref().filter(|_| acts) {
            let done = match step {
                Step::CashingOut => {
                    pay_out(wallet, &store, cash_out, consent, c, &urls, force_at, tip).await
                }
                Step::Folding => super::close(wallet, chain, dir, &c.id).await.map(|_| ()),
                _ => recovery::force_close(wallet, chain, dir, &c.id, rate)
                    .await
                    .map(|_| ()),
            };
            if let Err(e) = done {
                tracing::warn!(id = %crate::hex_id(&c.id), ?step, "exit: {e:#}");
                exit.error = Some(format!("{e:#}"));
            } else {
                tracing::info!(id = %crate::hex_id(&c.id), ?step, "exit");
            }
        }
        exits.push(exit);
    }
    Ok(exits)
}

/// Pay her notes at `c`'s hub out to her consented address, the lock
/// expiring by `force_at` so that a failed payout gives them back before
/// a forced close, within 1% and 1 000 sat: its ticket kept first.
#[allow(clippy::too_many_arguments)]
async fn pay_out<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    cash_out: &impl CashOut,
    consent: &Consent,
    c: &Contract,
    urls: &[String],
    force_at: u32,
    tip: u32,
) -> Result<()> {
    let quote = cash_out.quote_all(&c.hub, &consent.address).await?;
    ensure!(
        urls.contains(&quote.from),
        "quoted from {}, not {}",
        quote.from,
        c.hub
    );
    ensure!(
        quote.address == consent.address,
        "quoted to {}, not {}",
        quote.address,
        consent.address
    );
    let cap = quote.amount / 100 + FLOOR;
    ensure!(
        quote.max_fee <= cap,
        "quoted at up to {} sat in fees, more than {cap}",
        quote.max_fee
    );
    ensure!(
        quote.expiry <= force_at,
        "its lock would expire at block {}, past {force_at}, when it is forced closed",
        quote.expiry
    );
    store.exit_payout(&quote.ticket, &c.id, &quote.from, tip)?;
    wallet.payout(&quote).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            tip: 1_000,
            cash_at: 900,
            force_at: 1_188,
            since: 10,
            consented: true,
            uncovered: 50_000,
            owed: 0,
            pending: 0,
            paying: false,
            cashed: false,
        }
    }

    #[test]
    fn notes_she_funded_never_issued_are_waited_for_then_forced_closed() {
        let v = View {
            uncovered: 0,
            owed: 65_535,
            ..view()
        };
        // No payout reaches them, nor does a fold give them up.
        assert_eq!(plan(v), Step::Waiting);
        assert_eq!(plan(View { cashed: true, ..v }), Step::Waiting);
        assert_eq!(plan(View { tip: 1_188, ..v }), Step::ForceClosing);
        let unconsented = View {
            consented: false,
            ..v
        };
        assert_eq!(plan(unconsented), Step::NeedsConsent);
        // Her other notes there are paid out meanwhile.
        assert_eq!(
            plan(View {
                uncovered: 50_000,
                ..v
            }),
            Step::CashingOut
        );
    }

    #[test]
    fn nothing_is_done_before_a_contract_nears_its_end() {
        let v = View { tip: 899, ..view() };
        assert_eq!(plan(v), Step::Waiting);
        let v = View {
            consented: false,
            ..v
        };
        assert_eq!(plan(v), Step::Waiting);
    }

    #[test]
    fn notes_the_others_recover_or_too_few_need_no_exit() {
        for uncovered in [0, FLOOR] {
            let v = View {
                uncovered,
                ..view()
            };
            assert_eq!(plan(v), Step::Covered);
            assert_eq!(plan(View { tip: 1_188, ..v }), Step::Covered);
            assert_eq!(
                plan(View {
                    consented: false,
                    ..v
                }),
                Step::Covered
            );
        }
    }

    #[test]
    fn without_consent_she_is_told_and_nothing_is_done() {
        let v = View {
            consented: false,
            ..view()
        };
        assert_eq!(plan(v), Step::NeedsConsent);
        assert_eq!(plan(View { tip: 1_188, ..v }), Step::NeedsConsent);
        // Even once paid out by an exit she consented to before.
        let v = View {
            uncovered: 0,
            cashed: true,
            ..v
        };
        assert_eq!(plan(v), Step::Covered);
    }

    #[test]
    fn notes_at_risk_are_paid_out_then_forced_closed_if_not() {
        assert_eq!(plan(view()), Step::CashingOut);
        let paying = View {
            paying: true,
            ..view()
        };
        assert_eq!(plan(paying), Step::Paying);
        // From force_at, whatever is under way.
        assert_eq!(
            plan(View {
                tip: 1_188,
                ..view()
            }),
            Step::ForceClosing
        );
        assert_eq!(
            plan(View {
                tip: 1_188,
                ..paying
            }),
            Step::ForceClosing
        );
    }

    #[test]
    fn paid_out_it_folds_once_nothing_may_come_back() {
        let v = View {
            uncovered: 0,
            cashed: true,
            ..view()
        };
        assert_eq!(plan(v), Step::Folding);
        // A lock under way could return its notes: not folded yet.
        assert_eq!(plan(View { pending: 1, ..v }), Step::Paying);
        assert_eq!(plan(View { paying: true, ..v }), Step::Paying);
        // Past force_at too: nothing at risk is forced closed.
        assert_eq!(plan(View { tip: 1_188, ..v }), Step::Folding);
    }

    #[test]
    fn locks_owed_on_bitcoin_keep_it_from_folding_until_forced_closed() {
        let owed = View {
            uncovered: 0,
            owed: FLOOR + 1,
            ..view()
        };
        // No payout recovers them: it waits for its forced close.
        assert_eq!(plan(owed), Step::Waiting);
        assert_eq!(plan(View { tip: 1_188, ..owed }), Step::ForceClosing);
        // Paid out of her notes, it does not fold.
        let cashed = View {
            cashed: true,
            ..owed
        };
        assert_eq!(plan(cashed), Step::Waiting);
        assert_eq!(
            plan(View {
                tip: 1_188,
                ..cashed
            }),
            Step::ForceClosing
        );
        // Beside notes at risk, those are paid out first.
        assert_eq!(
            plan(View {
                uncovered: 50_000,
                ..owed
            }),
            Step::CashingOut
        );
        // With notes too few on their own, together at risk.
        let both = View {
            uncovered: FLOOR,
            owed: 1,
            ..view()
        };
        assert_eq!(plan(both), Step::Waiting);
        assert_eq!(plan(View { tip: 1_188, ..both }), Step::ForceClosing);
        assert_eq!(
            plan(View {
                consented: false,
                ..owed
            }),
            Step::NeedsConsent
        );
    }

    #[test]
    fn a_margin_that_never_fitted_is_only_told() {
        let v = View {
            since: 1_188,
            ..view()
        };
        assert_eq!(plan(v), Step::NeverFitted);
        assert_eq!(plan(View { tip: 1_200, ..v }), Step::NeverFitted);
    }

    #[test]
    fn a_contract_is_paid_out_three_days_and_forced_closed_a_day_before_its_request() {
        assert_eq!(deadlines(10_000, 144), (9_568, 9_856));
        assert_eq!(deadlines(100, 144), (0, 0));
    }
}
