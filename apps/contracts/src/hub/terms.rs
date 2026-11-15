//! The hub's terms for contracts, on each network it offers them on.

use anyhow::{bail, ensure, Result};
use licash::base::{ChainTerms, Fees, GraphTerms, Limits, Recipe};
use licash::chain::Checkpoint;
use licash::note::Keyset;
use licash::spend::ChainProfile;
use licash_contract::opening::{Bounds, Quote};
use licash_contract::{INSTANCES, KEPT};

/// The most blocks the hub's collateral is locked for.
pub const LOCK: u32 = 1_000;

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

/// The most notes a contract may protect, in sat: of a 31-bit graph, the
/// widest the graph's construction takes (its units counted in a `u32`).
pub const MAX_CAP: u32 = (1 << 31) - 1;

/// The fewest bits of the graph's payout: as wide as the fixed terms'
/// graph, whatever the cap.
const MIN_BITS: u8 = 21;

/// The caps and durations a hub lets holders choose besides its fixed
/// terms ([`CAP`] sat over [`LOCK`] blocks), as its operator sets them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    /// The most a contract protects, in sat: [`CAP`] to [`MAX_CAP`].
    pub max_cap: u32,
    /// The most blocks one lasts: [`LOCK`] at least.
    pub max_blocks: u32,
    /// A contract's cap at most this many times the coin its holder
    /// deposits, at least once: no small coin locks much of the hub's.
    pub per_coin: u32,
    /// Millionths of its cap a contract costs for each year it lasts,
    /// over the fixed price ([`licash_api::rate`]).
    pub ppm_year: u32,
}

/// The hub's timing on a network: the windows, delays and depths of its
/// quote, and its bounds; the terms holders may choose, if any.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Blocks the holder has to answer the hub's challenge.
    holder_answer: u16,
    /// Blocks each party's own outputs wait to be spent.
    to_self_delay: u16,
    /// Blocks deep F, a request and a publication are taken as final.
    depth: u32,
    /// Blocks it has to respond to her request, at least.
    response_window: u16,
    /// The fewest blocks a contract chosen lasts: her deadlines leave
    /// time to recover in after a quote a few blocks old.
    min_blocks: u32,
    /// The terms holders may choose, if any.
    range: Option<Range>,
}

impl Timing {
    /// The hub's timing on `profile`: on regtest, a test's few blocks; on
    /// signet, the least its floors allow ([`Quote::check`]), a day to
    /// answer and to wait, 3 blocks deep, and 12 blocks at least to
    /// respond to a request, for the publication to confirm 3 deep in
    /// them. None elsewhere yet. Holders choose their terms within
    /// `range`, if any, the fixed terms among them.
    pub fn of(profile: ChainProfile, range: Option<Range>) -> Result<Self> {
        let (holder_answer, to_self_delay, depth, response_window, min_blocks) = match profile {
            ChainProfile::Core30DefaultRegtest => (6, 6, 1, 2, 200),
            // Three days: her deadlines 180 blocks before the close.
            ChainProfile::Core30Signet => (144, 144, 3, 12, 432),
            _ => bail!("the hub offers no contracts on {profile} yet"),
        };
        if let Some(r) = range {
            ensure!(
                (CAP..=MAX_CAP).contains(&r.max_cap),
                "a contract's cap of {} sat at most: not {CAP} to {MAX_CAP}",
                r.max_cap
            );
            ensure!(
                r.max_blocks >= LOCK,
                "contracts of {} blocks at most: not {LOCK} at least",
                r.max_blocks
            );
            ensure!(r.per_coin >= 1, "a contract's cap of no times her coin");
        }
        Ok(Self {
            holder_answer,
            to_self_delay,
            depth,
            response_window,
            min_blocks,
            range,
        })
    }

    /// The terms holders may choose, if any, as the hub's info says them.
    pub fn range(&self) -> Option<licash_api::ContractRange> {
        self.range.map(|r| licash_api::ContractRange {
            min_cap: CAP,
            max_cap: r.max_cap,
            min_blocks: self.min_blocks,
            max_blocks: r.max_blocks,
            per_coin: r.per_coin,
            base: RESERVE_ALICE + OPENING_FEE,
            ppm_year: r.ppm_year,
        })
    }

    /// Refused unless a holder may ask for `cap` sat over `blocks`: the
    /// fixed terms, or terms within the hub's range.
    pub fn allows(&self, cap: u32, blocks: u32) -> Result<()> {
        if (cap, blocks) == (CAP, LOCK) {
            return Ok(());
        }
        let Some(range) = self.range() else {
            bail!("the hub offers contracts of {CAP} sat over {LOCK} blocks only");
        };
        ensure!(
            (range.min_cap..=range.max_cap).contains(&cap),
            "a cap of {cap} sat: not {} to {}",
            range.min_cap,
            range.max_cap
        );
        ensure!(
            (range.min_blocks..=range.max_blocks).contains(&blocks),
            "{blocks} blocks: not {} to {}",
            range.min_blocks,
            range.max_blocks
        );
        Ok(())
    }

    /// Refused unless a contract of `cap` sat may be paid from a coin of
    /// `coin` sat: of the fixed terms, any coin paying its price does.
    pub fn covers(&self, cap: u32, coin: u64) -> Result<()> {
        let per_coin = self.range.map_or(1, |r| r.per_coin);
        let most = coin.saturating_mul(u64::from(per_coin));
        ensure!(
            u64::from(cap) <= most,
            "a cap of {cap} sat on a coin of {coin}: {per_coin} times it at most"
        );
        Ok(())
    }

    /// What the hub's opening fee is for `cap` sat over `blocks`: the
    /// fixed one, and its rate on the cap by the year.
    fn fee(&self, cap: u32, blocks: u32) -> u64 {
        let ppm = self.range.map_or(0, |r| r.ppm_year);
        OPENING_FEE.saturating_add(licash_api::rate(ppm, cap, blocks))
    }

    /// What opening a contract of `cap` sat over `blocks` costs its
    /// holder, from her deposit: her reserve and the hub's fee.
    pub fn price(&self, cap: u32, blocks: u32) -> u64 {
        RESERVE_ALICE.saturating_add(self.fee(cap, blocks))
    }

    /// How far beyond `quote` a holder may ask: the close its headers
    /// reach at most, and the hub's least response window.
    pub fn bounds(&self, quote: &Quote) -> Bounds {
        Bounds {
            lock: quote.chain.max_headers,
            response_window: self.response_window,
        }
    }

    /// The hub's fixed quote on `keyset` at `checkpoint`, of [`CAP`] sat
    /// over [`LOCK`] blocks: the one asked for without terms.
    pub fn quote(&self, keyset: Keyset, checkpoint: Checkpoint) -> Quote {
        self.quote_for(keyset, checkpoint, CAP, LOCK)
    }

    /// The hub's quote on `keyset` at `checkpoint` of `cap` sat over
    /// `blocks`: a graph of 1-sat units as wide as the cap needs, 21 bits
    /// at least, its headers reaching the close, its challenge 12 blocks,
    /// her reserve 20 000 sat and its fee 3 000 and its rate. Of [`CAP`]
    /// and [`LOCK`], the fixed quote, as it always was.
    pub fn quote_for(
        &self,
        keyset: Keyset,
        checkpoint: Checkpoint,
        cap: u32,
        blocks: u32,
    ) -> Quote {
        let bits = (u32::BITS - cap.leading_zeros()) as u8;
        Quote {
            keyset,
            cap,
            graph: GraphTerms {
                id: 7,
                unit_sats: 1,
                bits: bits.max(MIN_BITS),
                low_bits: 9,
                deposit_units: 500_000,
                close_height: 0,
                reserve_alice: RESERVE_ALICE,
                reserve_hub: 100_000,
                to_self_delay: self.to_self_delay,
                commit_fee: 1_000,
                presign_fee: 1_000,
                dust: 330,
                delta: 6,
                delta_prime: 6,
                deadline_margin: 2,
                response_window: 0,
                assert_height: 0,
                hub_challenge: 12,
                holder_answer: self.holder_answer,
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
                funding_depth: self.depth,
                request_depth: self.depth,
                publication_depth: self.depth,
                lookback: 6,
                observation_margin: 1,
                max_headers: blocks,
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
                opening_fee_quote: self.fee(cap, blocks),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use licash_contract::opening::Hub;
    use licash_contract::wire::Writer;
    use sha2::{Digest, Sha256};

    /// A hub's opening at Funded, of a mock opening on regtest: its keyset
    /// and checkpoint.
    const FUNDED: &[u8] =
        include_bytes!("../../../../crates/licash-contract/tests/golden/hub-funded.bin");

    fn digest(q: &Quote) -> String {
        let mut w = Writer(Vec::new());
        w.quote(q);
        hex::encode(Sha256::digest(&w.0))
    }

    /// The quote every app asking without terms of its own opens on, as
    /// apps already installed do: byte for byte as the hub has always
    /// quoted it, on each network.
    #[test]
    fn the_fixed_quote_is_as_it_always_was() {
        let parts = Hub::peek(FUNDED).unwrap().base.parts().clone();
        let (keyset, checkpoint) = (parts.keyset, parts.chain.checkpoint);
        // Whether the hub lets holders choose or not, at 1% a year.
        for range in [None, Some(RANGE)] {
            let quoted = |profile| {
                let timing = Timing::of(profile, range).unwrap();
                let quote = timing.quote(keyset.clone(), checkpoint.clone());
                (digest(&quote), timing.bounds(&quote))
            };
            let (regtest, bounds) = quoted(ChainProfile::Core30DefaultRegtest);
            assert_eq!(
                regtest,
                "cb47276d1d570cde9b7605f0047bcd968d3abfb5e06c8722e12535a9ff01cc4a"
            );
            assert_eq!((bounds.lock, bounds.response_window), (1_000, 2));
            let (signet, bounds) = quoted(ChainProfile::Core30Signet);
            assert_eq!(
                signet,
                "1f25979c47ec3c8f192b4ed2bd008423e50e68a821483fdf3c2074dd8fd0c34a"
            );
            assert_eq!((bounds.lock, bounds.response_window), (1_000, 12));
        }
    }

    /// Up to 100 M sat over half a year, twice her coin, at 1% a year.
    const RANGE: Range = Range {
        max_cap: 100_000_000,
        max_blocks: 26_280,
        per_coin: 2,
        ppm_year: 10_000,
    };

    #[test]
    fn a_quote_chosen_is_as_wide_as_its_cap_and_as_long_as_its_blocks() {
        let parts = Hub::peek(FUNDED).unwrap().base.parts().clone();
        let (keyset, checkpoint) = (parts.keyset.clone(), parts.chain.checkpoint.clone());
        let timing = Timing::of(ChainProfile::Core30Signet, Some(RANGE)).unwrap();
        let fixed = timing.quote(keyset.clone(), checkpoint.clone());
        let quote = |cap, blocks| timing.quote_for(keyset.clone(), checkpoint.clone(), cap, blocks);

        // The user's example: 15 M sat for half a year, 75 000 sat a half
        // year over the fixed fee, of a 24-bit graph its headers reaching
        // the close.
        let chosen = quote(15_000_000, 26_280);
        assert_eq!(chosen.cap, 15_000_000);
        assert_eq!(chosen.graph.bits, 24);
        assert_eq!(chosen.chain.max_headers, 26_280);
        assert_eq!(chosen.fees.opening_fee_quote, 78_000);
        assert_eq!(timing.bounds(&chosen).lock, 26_280);
        // But for those, as the fixed quote is.
        let mut same = chosen.clone();
        same.cap = fixed.cap;
        same.graph.bits = fixed.graph.bits;
        same.chain.max_headers = fixed.chain.max_headers;
        same.fees.opening_fee_quote = fixed.fees.opening_fee_quote;
        assert_eq!(digest(&same), digest(&fixed));

        // 21 bits at least; 2^n sat needs n + 1 of them; 31 at most.
        let bits = |cap| quote(cap, 1_000).graph.bits;
        assert_eq!(
            (bits(4_096), bits(2_097_151), bits(2_097_152)),
            (21, 21, 22)
        );
        assert_eq!(
            (bits(16_777_215), bits(16_777_216), bits(MAX_CAP)),
            (24, 25, 31)
        );
        let base = |q: &Quote| {
            licash::base::Base::new(licash::base::Parts {
                cap: q.cap,
                graph: GraphTerms {
                    bits: q.graph.bits,
                    ..parts.graph.clone()
                },
                ..parts.clone()
            })
        };
        assert!(base(&quote(MAX_CAP, 1_000)).is_ok());
    }

    #[test]
    fn holders_choose_within_the_hubs_range_and_their_coin() {
        let fixed = Timing::of(ChainProfile::Core30Signet, None).unwrap();
        assert!(fixed.range().is_none());
        assert!(fixed.allows(CAP, LOCK).is_ok());
        let error = fixed.allows(CAP, LOCK - 1).unwrap_err();
        assert!(error.to_string().contains("only"), "{error}");
        // Of the fixed terms, any coin paying their price.
        assert!(fixed.covers(CAP, 23_000).is_ok());

        let timing = Timing::of(ChainProfile::Core30Signet, Some(RANGE)).unwrap();
        let range = timing.range().unwrap();
        assert_eq!((range.min_cap, range.max_cap), (CAP, 100_000_000));
        assert_eq!((range.min_blocks, range.max_blocks), (432, 26_280));
        assert_eq!(
            (range.per_coin, range.base, range.ppm_year),
            (2, 23_000, 10_000)
        );
        assert!(timing.allows(CAP, LOCK).is_ok());
        assert!(timing.allows(100_000_000, 26_280).is_ok());
        assert!(timing.allows(CAP, 432).is_ok());
        for (cap, blocks) in [
            (CAP - 1, LOCK),
            (100_000_001, LOCK),
            (CAP, 431),
            (CAP, 26_281),
        ] {
            assert!(timing.allows(cap, blocks).is_err(), "{cap} {blocks}");
        }
        assert!(timing.covers(15_000_000, 7_500_000).is_ok());
        assert!(timing.covers(15_000_001, 7_500_000).is_err());

        // Out of bounds, refused as the hub starts.
        let wider = |r: Range| Timing::of(ChainProfile::Core30Signet, Some(r)).is_err();
        assert!(wider(Range {
            max_cap: MAX_CAP + 1,
            ..RANGE
        }));
        assert!(wider(Range {
            max_cap: CAP - 1,
            ..RANGE
        }));
        assert!(wider(Range {
            max_blocks: LOCK - 1,
            ..RANGE
        }));
        assert!(wider(Range {
            per_coin: 0,
            ..RANGE
        }));
    }
}
