//! Common note shaping, exposed to the app without exposing plans or
//! input reservations. A manual execution has an explicit total fee cap.

use super::Wallet;
use crate::{LicashError, NoteOptimization};

#[uniffi::export]
impl Wallet {
    /// Whether periodic sync should continue for note work, even while
    /// privacy rest or the rolling daily budget prevents a remint now.
    pub fn note_maintenance_needed(&self) -> Result<bool, LicashError> {
        self.agree_fees();
        Ok(self.wallet.note_maintenance_needed()?)
    }

    /// A possible explicit optimization at `hub`, using advertised fees
    /// as last heard: no work started. Only unlinked notes can be used.
    pub fn optimization_quote(&self, hub: String) -> Result<Option<NoteOptimization>, LicashError> {
        self.agree_fees();
        Ok(self
            .wallet
            .optimization_quote(&hub)?
            .map(|q| shown(q, None)))
    }

    /// Optimize within `max_fee` sat, bypassing automatic rest and the
    /// daily budget, but never contract ceilings. None if no improvement
    /// is available or another optimization is pending.
    pub fn optimize_notes(
        &self,
        hub: String,
        max_fee: u64,
    ) -> Result<Option<NoteOptimization>, LicashError> {
        self.agree_fees();
        let optimized = self
            .runtime
            .block_on(self.wallet.optimize_notes(&hub, max_fee))?;
        Ok(optimized.map(|done| shown(done.quote, Some(done.id))))
    }

    /// Run after the app's regular sync round. Errors are returned to be
    /// logged, not to turn a completed payment or receipt into a failure.
    pub fn maintain_notes(&self) -> Result<u32, LicashError> {
        let _timed = crate::log::timed("maintain_notes");
        self.agree_fees();
        let completed = self.runtime.block_on(self.wallet.maintain_notes())?;
        Ok(u32::try_from(completed).unwrap_or(u32::MAX))
    }
}

fn shown(quote: licash_wallet::OptimizationQuote, id: Option<String>) -> NoteOptimization {
    NoteOptimization {
        id,
        hub: quote.hub,
        value: quote.value,
        fee: quote.fee,
        notes_before: quote.notes_before as u64,
        notes_after: quote.notes_after as u64,
    }
}
