//! Her exit from contracts ending, once she consents: each round, the
//! contracts nearing their end whose notes no other contract at their hub
//! recovers have those notes paid out to the address she gave, are folded
//! once paid out, or are forced closed a day before their claim request
//! must confirm ([`licash_contracts::holder::exit`]). Without her consent
//! nothing is done; the app is told, to tell her.

use std::sync::{Arc, PoisonError};

use bitcoin::FeeRate;
use licash_contracts::holder::exit::{self, CashOut, Step};

use super::Wallet;
use crate::{chain::Chain, ChainSource, LicashError};

/// The fee rate of a forced close the exit makes, in sat/vB, as for one
/// she makes herself.
const RECOVERY_RATE: u64 = 2;

/// Her consent to exit contracts ending.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ExitConsent {
    /// Where her notes are paid out to, on Bitcoin.
    pub address: String,
    /// The blocks kept for a forced close to play out in, as she gave.
    pub exit_margin: u32,
    /// When she gave it, in seconds since the epoch.
    pub given_at: i64,
}

/// What the exit does of a contract now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ExitStep {
    /// Not near its end yet.
    Waiting,
    /// Her notes at its hub recovered by its others there, or too few.
    Covered,
    /// Her notes at its hub at risk, and no consent to exit: tell her.
    NeedsConsent,
    /// The exit margin never fitted the contract: tell her.
    NeverFitted,
    /// Her notes at its hub being paid out to her address.
    CashingOut,
    /// A payout from its hub under way: waited for.
    Paying,
    /// Paid out, closed early with its hub.
    Folding,
    /// Not paid out in time: forced closed.
    ForceClosing,
}

/// A contract's exit, as it stood this round.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ContractExit {
    /// The contract's id, in hex.
    pub id: String,
    pub hub: String,
    pub step: ExitStep,
    /// From this block on, her notes at its hub are paid out.
    pub cash_at: u32,
    /// From this block on, it is forced closed if they are not.
    pub force_at: u32,
    /// Her notes at its hub the others there do not recover.
    pub uncovered: u64,
    /// Why its step failed this round, if it did.
    pub error: Option<String>,
    /// The locks owed to her only its forced close claims on Bitcoin,
    /// which no payout recovers.
    #[uniffi(default = 0)]
    pub owed: u64,
}

fn step(s: Step) -> ExitStep {
    match s {
        Step::Waiting => ExitStep::Waiting,
        Step::Covered => ExitStep::Covered,
        Step::NeedsConsent => ExitStep::NeedsConsent,
        Step::NeverFitted => ExitStep::NeverFitted,
        Step::CashingOut => ExitStep::CashingOut,
        Step::Paying => ExitStep::Paying,
        Step::Folding => ExitStep::Folding,
        Step::ForceClosing => ExitStep::ForceClosing,
    }
}

fn shown(c: exit::Consent) -> ExitConsent {
    ExitConsent {
        address: c.address,
        exit_margin: c.exit_margin,
        given_at: c.given_at,
    }
}

fn failed(e: anyhow::Error) -> LicashError {
    LicashError::from_any(e, |reason| LicashError::Failed { reason })
}

/// Exits from contracts ending.
#[uniffi::export]
impl Wallet {
    /// Her consent to exit contracts ending, if given and not revoked.
    pub fn exit_consent(&self) -> Result<Option<ExitConsent>, LicashError> {
        Ok(exit::consent(&self.dir).map_err(failed)?.map(shown))
    }

    /// Consent to exit contracts ending, her notes paid out to `address`
    /// on the wallet's network, planned with `exit_margin` blocks: in
    /// place of any given before. Refused before a first contract opens.
    pub fn consent_to_exit(
        &self,
        address: String,
        exit_margin: u32,
    ) -> Result<ExitConsent, LicashError> {
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let given = exit::consent_to(&self.wallet, &self.dir, &address, exit_margin)
            .map_err(|e| LicashError::invalid(format!("{e:#}")))?;
        tracing::info!(exit_margin, "consented to exit contracts ending");
        Ok(shown(given))
    }

    /// Revoke her consent to exit: no step more is taken, those under way
    /// go on.
    pub fn revoke_exit_consent(&self) -> Result<(), LicashError> {
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        exit::revoke(&self.dir).map_err(failed)?;
        tracing::info!("consent to exit contracts revoked");
        Ok(())
    }

    /// Take the exit from each live contract a step on through `chain`, as
    /// her consent says: `exit_margin` plans those she did not consent to
    /// exit, to tell her. Skipped while a contract is being opened or
    /// closed. Each contract's exit as it stood.
    pub fn drive_exits(
        &self,
        chain: Arc<dyn ChainSource>,
        exit_margin: u32,
    ) -> Result<Vec<ContractExit>, LicashError> {
        let Ok(_contracting) = self.contracting.try_lock() else {
            return Ok(vec![]);
        };
        let chain = Chain(chain, self.wallet.network());
        let rate = FeeRate::from_sat_per_vb(RECOVERY_RATE).expect("a rate");
        let cash_out = Discovered(self);
        let driven = exit::drive(
            &self.wallet,
            &chain,
            &self.dir,
            &cash_out,
            exit_margin,
            rate,
        );
        let exits = self.runtime.block_on(driven).map_err(failed)?;
        Ok(exits
            .into_iter()
            .map(|e| ContractExit {
                id: hex::encode(e.id),
                hub: e.hub,
                step: step(e.step),
                cash_at: e.cash_at,
                force_at: e.force_at,
                uncovered: e.uncovered,
                error: e.error,
                owed: e.owed,
            })
            .collect())
    }
}

/// The providers announced on the relays the wallet's hubs name, as for a
/// payout she makes herself.
struct Discovered<'a>(&'a Wallet);

impl CashOut for Discovered<'_> {
    async fn quote_all(
        &self,
        hub: &str,
        address: &str,
    ) -> anyhow::Result<licash_wallet::PayoutQuote> {
        let wallet = self.0;
        // Discovery blocks on the wallet's runtime, whose future this is.
        tokio::task::block_in_place(|| {
            let offers = wallet.payout_offers(address.to_owned(), None, Some(hub.to_owned()))?;
            let cheapest = (offers.into_iter())
                .filter(|o| o.fee <= o.amount / 100 + exit::FLOOR)
                .min_by_key(|o| o.fee)
                .ok_or_else(|| LicashError::Unavailable {
                    reason: "no provider pays out within 1% and 1000 sat".into(),
                })?;
            wallet.ticketed(&cheapest)
        })
        .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    /// Discovery blocking on the wallet's runtime from within a future it
    /// runs, as a provider is found for the exit.
    #[test]
    fn the_wallets_runtime_blocks_on_discovery_inside_its_own_future() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let inner = async {
            tokio::task::yield_now().await;
            7
        };
        let outer = async { tokio::task::block_in_place(|| runtime.block_on(inner)) };
        assert_eq!(runtime.block_on(outer), 7);
    }
}
