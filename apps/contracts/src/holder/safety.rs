//! Holder-owned planning, not a promise that a hub's answer expires. The
//! timeout must win promptly on chain for the planned exit time to hold.

use anyhow::{Context as _, Result};
use licash::spend::ChainProfile;
use licash_contract::opening::{Context, Holder};
use licash_wallet::{Transport, Wallet};

use super::Store;

/// The holder's exit margin (blocks kept for a forced close to play out
/// before her last request height) and timeout-confirmation allowance,
/// with the signed challenge window: the most blocks she may stay offline
/// once the hub asserts. No policy is persisted here and calculating it
/// never starts a recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Safety {
    pub assert_height: u32,
    pub response_window: u32,
    pub exit_margin: u32,
    pub timeout_slack: u32,
    pub challenge_window: u32,
    /// Latest planned R confirmation, not the time to start broadcasting.
    /// None if the margin does not fit before the assertion deadline.
    pub request_by: Option<u32>,
}

impl Safety {
    pub fn new(
        assert_height: u32,
        response_window: u32,
        exit_margin: u32,
        timeout_slack: u32,
        challenge_window: u32,
    ) -> Self {
        let request_by = response_window
            .checked_add(exit_margin)
            .and_then(|n| n.checked_add(timeout_slack))
            .and_then(|n| assert_height.checked_sub(n));
        Self {
            assert_height,
            response_window,
            exit_margin,
            timeout_slack,
            challenge_window,
            request_by,
        }
    }

    /// Planning is feasible at the boundary, overdue one block later.
    pub fn overdue(self, tip: u32) -> bool {
        self.request_by.is_none_or(|by| tip > by)
    }
}

/// Read the holder's own signed terms, never the hub's current policy.
/// Existing state is unchanged, including a recovery already under way.
pub fn of<T: Transport>(
    wallet: &Wallet<T>,
    dir: &std::path::Path,
    id: &[u8; 32],
    tip: u32,
    exit_margin: u32,
    timeout_slack: u32,
) -> Result<Safety> {
    let seed = wallet.contract_secrets().seed;
    let terms = (exit_margin, timeout_slack);
    of_seed(&seed, wallet.network(), dir, id, tip, terms)
}

/// As [`of`], by the wallet's contract seed and network: its exit margin
/// and timeout slack in `planned`.
pub(super) fn of_seed(
    seed: &[u8; 32],
    network: ChainProfile,
    dir: &std::path::Path,
    id: &[u8; 32],
    tip: u32,
    (exit_margin, timeout_slack): (u32, u32),
) -> Result<Safety> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let (state, _) = store.state(id)?;
    let ctx = Context {
        tip,
        trust: crate::trust(network)?,
    };
    let holder = Holder::load(&state, seed, &ctx)?;
    let opening = holder.signed()?;
    let graph = &opening.base.parts().graph;
    Ok(Safety::new(
        graph.request_height,
        u32::from(graph.response_window),
        exit_margin,
        timeout_slack,
        u32::from(graph.challenge_window),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_holder_chooses_the_margin_and_the_equal_boundary_fits() {
        let s = Safety::new(10_000, 12, 2_016, 6, 144);
        assert_eq!(s.request_by, Some(7_966));
        assert!(!s.overdue(7_966));
        assert!(s.overdue(7_967));
        assert_eq!(
            Safety::new(10_000, 12, 1_008, 6, 144).request_by,
            Some(8_974)
        );
    }

    #[test]
    fn short_and_overflowing_terms_are_not_feasible() {
        assert_eq!(Safety::new(100, 12, 2_016, 6, 144).request_by, None);
        assert!(Safety::new(100, 12, 2_016, 6, 144).overdue(0));
        assert_eq!(Safety::new(u32::MAX, 12, u32::MAX, 6, 144).request_by, None);
    }
}
