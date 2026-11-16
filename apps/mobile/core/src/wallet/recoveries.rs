//! Contracts forced closed when their hub went silent: the notes at the
//! hub, up to the contract's cap, claimed on Bitcoin, paid back on Bitcoin
//! once the hub's window to answer passed. Started only by the holder's
//! explicit choice, carried on with the other contracts as the chain
//! moves. A hub that answers instead leaves it to the holder's proof of
//! her claim, made elsewhere from the statement the wallet exports
//! ([`Wallet::recovery_statement`]) and brought back
//! ([`Wallet::import_recovery_proof`]).

use std::sync::{Arc, PoisonError};

use bitcoin::FeeRate;
use licash_contracts::holder::recovery::{self, Stage};

use super::Wallet;
use crate::{chain::Chain, ChainSource, LicashError};

/// Its fees' rate, in sat/vB: the wallet knows no estimates, and on signet
/// blocks have room. Neither P nor the timeout's fee coin is bumped later.
const RECOVERY_RATE: u64 = 2;

fn rate() -> FeeRate {
    FeeRate::from_sat_per_vb(RECOVERY_RATE).expect("a rate")
}

/// A contract being forced closed, as the wallet last saw it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Recovery {
    /// The contract's id, in hex.
    pub id: String,
    pub stage: RecoveryStage,
    /// Past giving it up: the notes it claimed are gone from the balance
    /// for good, to come back on Bitcoin.
    pub irrevocable: bool,
    /// What the notes it claimed come to, in sat, and how many they are.
    pub claimed: u64,
    pub notes: u32,
    /// Its request on Bitcoin and the transaction publishing its claim,
    /// once signed, their ids in hex.
    pub request: Option<String>,
    pub publication: Option<String>,
    /// The transaction paying the claim back once the hub let its window
    /// pass, and the one sweeping it to the wallet, once built.
    pub timeout: Option<String>,
    pub sweep: Option<String>,
    /// The block its next step waits for, if it waits for one.
    pub next_at: Option<u32>,
    /// What stopped its last step, if anything did.
    pub error: Option<String>,
    /// The hub's answer to its request, its id in hex, and the last block
    /// the proof may be posted in, once known.
    #[uniffi(default = None)]
    pub response: Option<String>,
    #[uniffi(default = None)]
    pub deadline: Option<u32>,
    /// Whether its proof is kept.
    #[uniffi(default = false)]
    pub proved: bool,
    /// The proof posted, and the payout after it, their ids in hex.
    #[uniffi(default = None)]
    pub assertion: Option<String>,
    #[uniffi(default = None)]
    pub payout: Option<String>,
    /// Corrected principal, distinct from the original claim and net sweep.
    #[uniffi(default = None)]
    pub corrected: Option<u64>,
    #[uniffi(default = None)]
    pub received: Option<u64>,
}

/// Where a forced close stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryStage {
    /// The notes claimed: it may still be given up, until its request may
    /// be in the next block.
    Claimed,
    /// Its request signed, on its way into a block.
    Requesting,
    /// Its request in a block: the hub's window to answer running.
    Requested,
    /// The window passed unanswered: the payback on its way, then its
    /// delay running.
    TimingOut,
    /// The payback swept to the wallet, until in a block.
    Sweeping,
    /// Its value back in the wallet on Bitcoin.
    Done,
    /// A legacy counter awaiting reclassification on the next advance.
    Answered,
    /// The contract's collateral went elsewhere.
    Lost,
    /// The hub answered: the statement to prove being built from the
    /// chain, then the proof needed by [`Recovery::deadline`].
    Proving,
    /// The proof posted on Bitcoin, until in a block.
    Asserting,
    /// The proof in a block: the hub's chance to challenge it running,
    /// then the payout on its way.
    Asserted,
    /// The hub challenged the proof: the answer on its way.
    Challenged,
    /// The payout after the proof in a block: its delay running.
    Paid,
}

fn stage(s: Stage) -> RecoveryStage {
    match s {
        Stage::Claiming | Stage::Claimed | Stage::GivingUp => RecoveryStage::Claimed,
        Stage::Irrevocable | Stage::Signing | Stage::Published => RecoveryStage::Requesting,
        Stage::Requested => RecoveryStage::Requested,
        Stage::TimingOut | Stage::TimedOut => RecoveryStage::TimingOut,
        Stage::Sweeping => RecoveryStage::Sweeping,
        Stage::Done => RecoveryStage::Done,
        Stage::Countered => RecoveryStage::Answered,
        Stage::Answered | Stage::Proving => RecoveryStage::Proving,
        Stage::Asserting => RecoveryStage::Asserting,
        Stage::Asserted | Stage::Succeeding => RecoveryStage::Asserted,
        Stage::Answering => RecoveryStage::Challenged,
        Stage::Paid => RecoveryStage::Paid,
        Stage::Lost => RecoveryStage::Lost,
    }
}

fn shown(r: &recovery::Recovery) -> Recovery {
    let txid = |t: Option<bitcoin::Txid>| t.map(|t| t.to_string());
    Recovery {
        id: hex::encode(r.id),
        stage: stage(r.stage),
        irrevocable: r.irrevocable,
        claimed: r.claimed,
        notes: u32::try_from(r.notes).unwrap_or(u32::MAX),
        request: txid(r.request),
        publication: txid(r.publication),
        timeout: txid(r.timeout),
        sweep: txid(r.sweep),
        next_at: r.next_at,
        error: r.error.clone(),
        response: txid(r.response),
        deadline: r.deadline,
        proved: r.proved,
        assertion: txid(r.assertion),
        payout: txid(r.payout),
        corrected: r.corrected,
        received: r.received,
    }
}

fn contract_id(id: &str) -> Result<[u8; 32], LicashError> {
    let mut out = [0; 32];
    hex::decode_to_slice(id.trim(), &mut out)
        .map_err(|_| LicashError::invalid(format!("{id} is no contract id")))?;
    Ok(out)
}

fn failed(e: anyhow::Error) -> LicashError {
    LicashError::from_any(e, |reason| LicashError::Failed { reason })
}

/// Forced closes.
#[uniffi::export]
impl Wallet {
    /// Force contract `id` (in hex) closed, its hub silent: the notes at
    /// its hub, up to its cap, leave the balance and are claimed on
    /// Bitcoin through `chain`, carried on as far as the chain lets it.
    /// Irrevocable once its request may be in the next block. Refused for
    /// a contract not protecting yet, or any longer.
    pub fn force_close_contract(
        &self,
        chain: Arc<dyn ChainSource>,
        id: String,
    ) -> Result<Recovery, LicashError> {
        let id = contract_id(&id)?;
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let chain = Chain(chain, self.wallet.network());
        let forced = recovery::force_close(&self.wallet, &chain, &self.dir, &id, rate());
        let forced = self.runtime.block_on(forced).map_err(failed)?;
        tracing::warn!(id = %hex::encode(id), stage = forced.stage.as_str(), "contract forced closed");
        Ok(shown(&forced))
    }

    /// The contracts forced closed, the oldest first, as last seen.
    pub fn recoveries(&self) -> Result<Vec<Recovery>, LicashError> {
        let all = recovery::recoveries(&self.dir).map_err(failed)?;
        Ok(all.iter().map(shown).collect())
    }

    /// The statement to prove of contract `id` (in hex), once the hub
    /// answered and it is built (`RecoveryStage::Proving`): to prove
    /// on a computer (`licash-r0 native-prove-succinct`, then
    /// `native-wrap`). Private: it shows the notes claimed.
    pub fn recovery_statement(&self, id: String) -> Result<Vec<u8>, LicashError> {
        let id = contract_id(&id)?;
        recovery::statement(&self.dir, &id).map_err(failed)
    }

    /// The program contract `id`'s statement is proved with, its image id
    /// in hex: the app proves it with `<image id>.bin` among those it
    /// carries (`r0/programs`). Refused before the statement is built.
    pub fn recovery_program(&self, id: String) -> Result<String, LicashError> {
        let id = contract_id(&id)?;
        let statement = recovery::statement(&self.dir, &id).map_err(failed)?;
        let input = licash::statement::StatementInput::from_bytes(&statement)
            .map_err(|e| failed(anyhow::anyhow!("its statement: {e}")))?;
        Ok(hex::encode(input.base.parts().recipe.holder_program))
    }

    /// Bring back the proof of contract `id`'s claim, `export` as
    /// `licash-r0 native-wrap` wrote it: checked against the contract,
    /// kept, and posted on Bitcoin through `chain` at once. The same proof
    /// again is no error; another is refused once one is kept.
    pub fn import_recovery_proof(
        &self,
        chain: Arc<dyn ChainSource>,
        id: String,
        export: Vec<u8>,
    ) -> Result<Recovery, LicashError> {
        let id = contract_id(&id)?;
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let chain = Chain(chain, self.wallet.network());
        let imported =
            recovery::import_proof(&self.wallet, &chain, &self.dir, &id, &export, rate());
        let imported = self.runtime.block_on(imported).map_err(failed)?;
        tracing::warn!(id = %hex::encode(id), stage = imported.stage.as_str(), "a contract's proof brought");
        Ok(shown(&imported))
    }

    /// Give up forcing contract `id` closed, its notes back in the
    /// balance: only while `RecoveryStage::Claimed`.
    pub fn give_up_recovery(&self, id: String) -> Result<(), LicashError> {
        let id = contract_id(&id)?;
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        recovery::give_up(&self.wallet, &self.dir, &id).map_err(failed)
    }
}

impl Wallet {
    /// Carry every forced close on as far as `chain` lets it, the wallet
    /// contracting: one that cannot go on holds up nothing else.
    pub(super) fn advance_recoveries(&self, chain: &Chain) {
        let advanced = recovery::advance(&self.wallet, chain, &self.dir, rate());
        if let Err(e) = self.runtime.block_on(advanced) {
            tracing::warn!("forced closes not carried on: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's stages of a forced close the hub answered: its proof
    /// awaited, posted, challenged, paid; a counter, which this app does
    /// not answer, as answered, at an end for it.
    #[test]
    fn a_disputed_close_shows_its_own_stages_and_a_counter_as_answered() {
        let cases = [
            (Stage::Answered, RecoveryStage::Proving),
            (Stage::Proving, RecoveryStage::Proving),
            (Stage::Asserting, RecoveryStage::Asserting),
            (Stage::Asserted, RecoveryStage::Asserted),
            (Stage::Succeeding, RecoveryStage::Asserted),
            (Stage::Answering, RecoveryStage::Challenged),
            (Stage::Paid, RecoveryStage::Paid),
            (Stage::Countered, RecoveryStage::Answered),
            (Stage::TimedOut, RecoveryStage::TimingOut),
        ];
        for (s, shown) in cases {
            assert_eq!(stage(s), shown, "{}", s.as_str());
        }
    }
}
