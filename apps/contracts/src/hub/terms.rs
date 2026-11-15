//! The hub's terms for contracts.

use licash::base::{ChainTerms, Fees, GraphTerms, Limits, Recipe};
use licash::chain::Checkpoint;
use licash::note::Keyset;
use licash_contract::opening::{Bounds, Quote};
use licash_contract::{INSTANCES, KEPT};

/// How far beyond its quote a holder may ask: its collateral locked 1 000
/// blocks at most, 2 blocks at least to respond to her request.
pub const BOUNDS: Bounds = Bounds {
    lock: 1_000,
    response_window: 2,
};

/// The most notes a contract protects, in sat.
pub const CAP: u32 = 4_096;

/// The holder's reserve, from her deposit.
pub const RESERVE_ALICE: u64 = 20_000;

/// The hub's fee for an opening, from her deposit.
pub const OPENING_FEE: u64 = 3_000;

/// What each of F's five publication outputs holds, of the hub's coins.
pub const PUBLICATION_SATS: u64 = 20_000;

/// The most F's and the graph's transactions pay, in sat per 1 000 vB.
const MAX_RATE_SAT_PER_KVB: u64 = 10_000;

/// The most F's and the graph's transactions pay, in sat/vB.
pub fn quote_ceiling() -> f64 {
    MAX_RATE_SAT_PER_KVB as f64 / 1_000.0
}

/// The hub's quote on `keyset` at `checkpoint`: a 21-bit graph of 1-sat
/// units protecting up to 4 096 sat, its challenge and the holder's answer
/// 12 and 6 blocks, her reserve 20 000 sat and its fee 3 000.
pub fn quote(keyset: Keyset, checkpoint: Checkpoint) -> Quote {
    Quote {
        keyset,
        cap: CAP,
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: 0,
            reserve_alice: RESERVE_ALICE,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 2,
            response_window: 0,
            assert_height: 0,
            hub_challenge: 12,
            holder_answer: 6,
        },
        recipe: Recipe {
            graph_version: 1,
            holder_program: [0; 32],
            counter_program: [0; 32],
            full_instances: INSTANCES as u16,
            full_kept: KEPT as u16,
            corrected_instances: INSTANCES as u16,
            corrected_kept: KEPT as u16,
        },
        chain: ChainTerms {
            checkpoint,
            funding_depth: 1,
            request_depth: 1,
            publication_depth: 1,
            lookback: 6,
            observation_margin: 1,
            max_headers: 1_000,
        },
        limits: Limits {
            max_transaction_bytes: 100_000,
            max_publication_bytes: 64_000,
            max_authorizations: 128,
            max_source_inputs: 128,
        },
        fees: Fees {
            sweep_fee_max: 1_000,
            external_fee_max: 2_000,
            max_rate_sat_per_kvb: MAX_RATE_SAT_PER_KVB,
            opening_fee_quote: OPENING_FEE,
        },
    }
}
