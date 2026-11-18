//! Fee rates for a transaction that must confirm before a deadline, the
//! block after which its party loses more than its fee: a deposit's claim,
//! a payout's refund, the cancel of a payout's coin not mined. Such a
//! transaction is made again at each block it is not mined in, at the rate
//! [`rate`] gives for the blocks left, and broadcast in place of the one
//! before when it pays more ([`replaces`]): a rate fixed once, the mempool's
//! floor rising past it, would leave it unmined past its deadline.

use bitcoin::{Amount, FeeRate};

/// Blocks left from which the rate rises above the estimate.
const RISE_FROM: u32 = 12;
/// Blocks fewer for each doubling.
const STEP: u32 = 3;
/// The most it rises to, as a multiple of the estimate.
const MOST: u64 = 16;

/// The blocks to ask the chain's estimate for, `left` blocks before the
/// deadline: half of them, 1 to 144.
pub fn target(left: u32) -> u16 {
    (left / 2).clamp(1, 144) as u16
}

/// The rate to pay at `left` blocks before the deadline, the chain
/// estimating `estimate` for [`target`] of them, `floor` at least (and
/// without an estimate): the estimate while 12 blocks or more are left,
/// doubled for each 3 fewer, 16 times it at most.
pub fn rate(estimate: Option<FeeRate>, floor: FeeRate, left: u32) -> FeeRate {
    let base = estimate.unwrap_or(floor).max(floor);
    let steps = RISE_FROM.saturating_sub(left).div_ceil(STEP);
    let factor = (1u64 << steps.min(8)).min(MOST);
    FeeRate::from_sat_per_kwu(base.to_sat_per_kwu().saturating_mul(factor))
}

/// Whether a transaction of `vsize` virtual bytes paying `fee` may take the
/// place of one paying `old` in the mempool: it pays more by the relay's
/// increment, 1 sat/vB of its own size, at least (BIP125).
pub fn replaces(fee: Amount, vsize: u64, old: Amount) -> bool {
    old.checked_add(Amount::from_sat(vsize))
        .is_some_and(|least| fee >= least)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: FeeRate = FeeRate::from_sat_per_vb_u32(2);
    const ONE: FeeRate = FeeRate::from_sat_per_vb_u32(1);

    fn vb(rate: FeeRate) -> u64 {
        rate.to_sat_per_vb_floor()
    }

    #[test]
    fn the_rate_is_the_estimate_until_twelve_blocks_are_left() {
        assert_eq!(rate(Some(TWO), ONE, 100), TWO);
        assert_eq!(rate(Some(TWO), ONE, 12), TWO);
        // No estimate, or one under the floor: the floor.
        assert_eq!(rate(None, ONE, 100), ONE);
        assert_eq!(rate(Some(ONE), TWO, 100), TWO);
    }

    #[test]
    fn the_rate_doubles_for_each_three_blocks_fewer_sixteen_times_at_most() {
        let at = |left| vb(rate(Some(TWO), ONE, left));
        assert_eq!(
            [at(11), at(9), at(8), at(6), at(5), at(3)],
            [4, 4, 8, 8, 16, 16]
        );
        assert_eq!([at(2), at(0)], [32, 32]);
    }

    #[test]
    fn the_estimate_asked_for_is_of_half_the_blocks_left() {
        assert_eq!(
            [target(0), target(3), target(36), target(1_000)],
            [1, 1, 18, 144]
        );
    }

    #[test]
    fn a_replacement_pays_one_sat_a_vbyte_of_its_own_more_at_least() {
        let sat = Amount::from_sat;
        assert!(!replaces(sat(1_100), 150, sat(1_000)));
        assert!(replaces(sat(1_150), 150, sat(1_000)));
        assert!(!replaces(sat(1_000), 0, sat(1_001)));
    }
}
