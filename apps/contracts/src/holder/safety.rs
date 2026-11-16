//! Holder-owned planning, not a promise that a hub's answer expires. The
//! timeout must win promptly on chain for the planned proving time to hold.

use anyhow::{Context as _, Result};
use licash_contract::opening::{Context, Holder};
use licash_wallet::{Transport, Wallet};

use super::Store;

/// Local proof time and timeout-confirmation allowance, in blocks. No
/// policy is persisted here and calculating it never starts a recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Safety {
    pub assert_height: u32,
    pub response_window: u32,
    pub proof_margin: u32,
    pub timeout_slack: u32,
    /// Latest planned R confirmation, not the time to start broadcasting.
    /// None if the margin does not fit before the assertion deadline.
    pub request_by: Option<u32>,
}

impl Safety {
    pub fn new(
        assert_height: u32,
        response_window: u32,
        proof_margin: u32,
        timeout_slack: u32,
    ) -> Self {
        let request_by = response_window
            .checked_add(proof_margin)
            .and_then(|n| n.checked_add(timeout_slack))
            .and_then(|n| assert_height.checked_sub(n));
        Self {
            assert_height,
            response_window,
            proof_margin,
            timeout_slack,
            request_by,
        }
    }

    /// Planning is feasible at the boundary, overdue one block later.
    pub fn overdue(self, tip: u32) -> bool {
        self.request_by.is_none_or(|by| tip > by)
    }

    /// Actual blocks left to assert once the hub's answer confirms.
    pub fn proving_blocks(self, answered_at: u32) -> u32 {
        self.assert_height.saturating_sub(answered_at)
    }
}

/// Read the holder's own signed terms, never the hub's current policy.
/// Existing state is unchanged, including a recovery already under way.
pub fn of<T: Transport>(
    wallet: &Wallet<T>,
    dir: &std::path::Path,
    id: &[u8; 32],
    tip: u32,
    proof_margin: u32,
    timeout_slack: u32,
) -> Result<Safety> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let (state, _) = store.state(id)?;
    let secrets = wallet.contract_secrets();
    let ctx = Context {
        tip,
        trust: crate::trust(wallet.network())?,
    };
    let holder = Holder::load(&state, secrets.seed, secrets.spend, &ctx)?;
    let opening = holder.opening()?;
    let graph = &opening.base.parts().graph;
    Ok(Safety::new(
        graph.assert_height,
        u32::from(graph.response_window),
        proof_margin,
        timeout_slack,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_holder_chooses_the_margin_and_the_equal_boundary_fits() {
        let s = Safety::new(10_000, 12, 2_016, 6);
        assert_eq!(s.request_by, Some(7_966));
        assert!(!s.overdue(7_966));
        assert!(s.overdue(7_967));
        assert_eq!(Safety::new(10_000, 12, 1_008, 6).request_by, Some(8_974));
    }

    #[test]
    fn short_and_overflowing_terms_are_not_feasible() {
        assert_eq!(Safety::new(100, 12, 2_016, 6).request_by, None);
        assert!(Safety::new(100, 12, 2_016, 6).overdue(0));
        assert_eq!(Safety::new(u32::MAX, 12, u32::MAX, 6).request_by, None);
    }

    #[test]
    fn actual_proving_time_shrinks_when_the_hub_answers_late() {
        let s = Safety::new(10_000, 12, 2_016, 6);
        assert_eq!(s.proving_blocks(7_984), 2_016);
        assert_eq!(s.proving_blocks(9_999), 1);
        assert_eq!(s.proving_blocks(10_001), 0);
    }
}
