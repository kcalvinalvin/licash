//! Contracts wider than the fixture's 21 bits, as a hub quotes caps its
//! holders choose (up to 2^31 - 1 sat in 31 bits, apps/contracts' hub
//! terms), on Bitcoin Core under the mock proof authority, with genuine
//! garbled instances: opened and funded, the hub silent, the request timed
//! out, and the payout's bit outputs (one a bit, from the low bits up)
//! swept by both parties, every transaction relayed by Core's policy.
//!
//! The holder claims every bit but the top one, so her sweep is about the
//! widest there is, the hub's of the top bit alone. (A low bit swept alone
//! is worth less than its fee: 512 sat at bit 9.) What each sweep weighs is
//! printed: against the 1,000-sat sweep fee ceiling the quote agrees, a
//! measure, not checked.

use anyhow::{ensure, Result};
use licash::note::IssuerKeys;
use licash_regtest::{
    contract::Stage,
    graph::Babe,
    native::Width,
    native_fixture::{fixture_wide, Fixture, Terms},
};
use lngap_btc::{bitcoin::Transaction, regtest::Regtest};

/// The low bits the payout rounds away (the hub's terms: 9).
const LOW_BITS: u8 = 9;

/// A contract of `bits` and the most cap in them; the holder's notes
/// of every power of two from 2^LOW_BITS up to, not including, the cap's
/// top bit.
fn silent_hub_pays_a_wide_claim(bits: u8) -> Result<()> {
    let rt = Regtest::start_standard()?;
    let cap = u32::try_from((1u64 << bits) - 1)?;
    let width = Width::of(cap);
    ensure!(width.bits == bits, "{cap} sat in {} bits", width.bits);
    let denominations: Vec<u64> = (LOW_BITS..bits - 1).map(|i| 1u64 << i).collect();
    let terms = Terms {
        denominations,
        issuer: Some(IssuerKeys::random(u32::from(bits) - 2)?),
        ..Terms::default()
    };
    let f = fixture_wide(&rt, &format!("native-wide-{bits}"), terms, width)?;
    let graph = &f.c.babe.spec.terms;
    ensure!(
        graph.bits == u32::from(bits),
        "a graph of {} bits",
        graph.bits
    );
    let q = f.request()?;
    let amount = q.request.amount;
    let all = u32::try_from(((1u64 << (bits - 1)) - 1) & !((1u64 << LOW_BITS) - 1))?;
    ensure!(amount == all, "claimed {amount}, not {all}");
    let timeout = f.c.timeout(f.rt, "request_timeout")?;
    ensure!(timeout.output == f.c.set["request_timeout"].tx.output);
    println!(
        "{bits} bits: the payout of {} outputs, {} vB",
        timeout.output.len(),
        timeout.vsize()
    );
    collect(&f, &timeout, amount, &Babe::request_wire(&q.alice_b))
}

/// The payout's bits of `amount` collected, as the fixture's `collect`
/// does: the hub's clear bits at once, the holder's set bits after the
/// delay. Her sweep's fee is measured, not bounded: what she takes is her
/// bits less it.
fn collect(f: &Fixture, payout: &Transaction, amount: u32, wire: &[Vec<u8>]) -> Result<()> {
    let (rt, c) = (f.rt, &f.c);
    let terms = &c.babe.spec.terms;
    let (set, clear): (Vec<u32>, Vec<u32>) =
        (terms.low_bits..terms.bits).partition(|i| (amount >> i) & 1 == 1);
    let sweep = c.bit_spend(rt, payout, Stage::Full, "hub_bit", &clear, wire)?;
    c.post(rt, &sweep)?;
    let take = c.bit_spend(rt, payout, Stage::Full, "alice_bit", &set, wire)?;
    let early = rt.test_accept(&take).unwrap_err();
    ensure!(early.contains("non-BIP68-final"), "{early}");
    rt.mine(u64::from(c.babe.spec.params.delta))?;
    let taken = c.post(rt, &take)?;
    let bits: u64 = take
        .input
        .iter()
        .map(|i| {
            payout.output[i.previous_output.vout as usize]
                .value
                .to_sat()
        })
        .sum();
    let low = (1u32 << terms.low_bits) - 1;
    ensure!(
        bits == u64::from(amount & !low),
        "her bits are worth {bits}"
    );
    let value: u64 = taken.output.iter().map(|o| o.value.to_sat()).sum();
    let vsize = take.vsize() as u64;
    ensure!(
        value + 2 * (vsize + 10) == bits,
        "she takes {value} of {bits}"
    );
    println!(
        "{} bits: the hub sweeps {} bits in {} vB, she sweeps {} in {vsize} vB \
         ({vsize} sat at 1 sat/vB, against the 1,000 sat agreed)",
        terms.bits,
        clear.len(),
        sweep.vsize(),
        set.len(),
    );
    Ok(())
}

#[test]
fn silent_hub_pays_a_24_bit_claim_on_bitcoin() -> Result<()> {
    silent_hub_pays_a_wide_claim(24)
}

#[test]
fn silent_hub_pays_a_31_bit_claim_on_bitcoin() -> Result<()> {
    silent_hub_pays_a_wide_claim(31)
}
