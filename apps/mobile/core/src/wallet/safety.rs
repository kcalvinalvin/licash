//! A local recovery margin read from the holder's signed terms. This is
//! planning only: no payment, close, pin or new persisted state is made.

use std::sync::atomic::Ordering;

use super::Wallet;
use crate::LicashError;

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ContractSafety {
    pub request_by: Option<u32>,
    pub assert_height: u32,
    pub response_window: u32,
    pub exit_margin: u32,
    pub timeout_slack: u32,
    /// Blocks she has to challenge the hub's assertion in: during a forced
    /// close, the wallet must come online at least this often.
    pub challenge_window: u32,
}

#[uniffi::export]
impl Wallet {
    /// Latest planned R confirmation with a holder-chosen exit margin.
    /// The hub can still answer late until the timeout wins on chain.
    pub fn contract_safety(
        &self,
        id: String,
        exit_margin: u32,
        timeout_slack: u32,
    ) -> Result<ContractSafety, LicashError> {
        let id = licash_contracts::parse_id(&id).map_err(|e| LicashError::Invalid {
            reason: e.to_string(),
        })?;
        let s = licash_contracts::holder::safety::of(
            &self.wallet,
            self.wallet.path(),
            &id,
            self.tip.load(Ordering::Relaxed),
            exit_margin,
            timeout_slack,
        )
        .map_err(|e| LicashError::Unavailable {
            reason: format!("{e:#}"),
        })?;
        Ok(ContractSafety {
            request_by: s.request_by,
            assert_height: s.assert_height,
            response_window: s.response_window,
            exit_margin: s.exit_margin,
            timeout_slack: s.timeout_slack,
            challenge_window: s.challenge_window,
        })
    }
}
