//! Funded recovery allowance arithmetic.
//!
//! Denominations and b count satoshis. The pristine bit-output template
//! rounds principal down; recovery must not silently round the debt down too.
//! This stateless check authenticates neither a fee quote nor a terminal output.

use crate::Error;

pub const UNIT_SATS: u64 = 1;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// Minimum surplus over the holder's own reserve on EVERY holder-paying
/// terminal. The gate must derive each actual reserve output from the exact
/// presigned graph, not accept it as a peer assertion or a negotiated Fmax.
///
/// In-contract floors/anchors have already reduced that output. Sweep fees
/// cover collecting winning bits AND the reserve; external fees cover reaching
/// the terminal (P, fee children, terminal fee coins and their preparation),
/// without counting those same costs twice. Opening/funding fees are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryBudget {
    low_bits: u8,
    sweep_fee_max: u64,
    external_fee_max: u64,
    minimum_surplus: u64,
}

impl RecoveryBudget {
    /// Fee ceilings are agreed satoshi amounts, not estimates measured here.
    /// The complete opening must bind their quote/feerate ceiling and verify
    /// graph dimensions, money ranges, dust, funding, delays and all required
    /// fee resources. This arithmetic alone does not validate a Bitcoin amount.
    pub fn new(
        unit_sats: u64,
        low_bits: u8,
        sweep_fee_max: u64,
        external_fee_max: u64,
    ) -> Result<Self, Error> {
        if unit_sats != UNIT_SATS {
            return Err(bad("native payout unit must be one satoshi"));
        }
        // A claim has at most 32 amount bits; at least one bit must remain.
        if low_bits >= 32 {
            return Err(bad("native payout low bits exceed amount profile"));
        }
        let rounding_max = (1u64 << low_bits) - 1;
        let minimum_surplus = rounding_max
            .checked_add(sweep_fee_max)
            .and_then(|sum| sum.checked_add(external_fee_max))
            .ok_or_else(|| bad("native recovery allowance overflow"))?;
        Ok(Self {
            low_bits,
            sweep_fee_max,
            external_fee_max,
            minimum_surplus,
        })
    }

    pub fn low_bits(&self) -> u8 {
        self.low_bits
    }
    pub fn sweep_fee_max(&self) -> u64 {
        self.sweep_fee_max
    }
    pub fn external_fee_max(&self) -> u64 {
        self.external_fee_max
    }
    pub fn minimum_surplus(&self) -> u64 {
        self.minimum_surplus
    }

    /// Check the actual post-floor reserve, not gross initial hub collateral.
    /// If C/X stay within their ceilings, collecting all winning outputs and
    /// this reserve returns at least own_reserve + b, net of those recovery fees.
    /// It does not execute or confirm collection, or promise fees stay bounded.
    pub fn verify_terminal_reserve(
        &self,
        own_reserve: u64,
        terminal_reserve: u64,
    ) -> Result<(), Error> {
        let surplus = terminal_reserve
            .checked_sub(own_reserve)
            .ok_or_else(|| bad("terminal cannot return holder reserve"))?;
        if surplus < self.minimum_surplus {
            return Err(bad("terminal recovery allowance is underfunded"));
        }
        Ok(())
    }
}
