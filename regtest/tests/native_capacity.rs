//! A claim at the item limit, on Bitcoin Core: 32 notes in one claim body,
//! paid in full, and the same claim from a stale backup countered by many
//! separate spends, plain and granted. With `LICASH_NATIVE_INPUTS` these are
//! the largest guest inputs the suite exports (`native-cap-*`).

use std::collections::BTreeSet;

use anyhow::{ensure, Result};
use licash::{
    authorization::{Authorization, PreparedGrant, SignedProposal, Unlock},
    note::HoldRoot,
    recovery::{counter_disclosures, Evidence},
    spend::{Output, Proposal},
};
use licash_regtest::native_fixture::{derive, export, fixture_with, owner, point, Fixture, Terms};
use lngap_btc::regtest::Regtest;

const ITEMS: usize = 32;

/// 32 notes of 128: a claim of the whole cap.
fn full<'a>(rt: &'a Regtest, label: &str) -> Result<Fixture<'a>> {
    let terms = Terms {
        denominations: vec![128; ITEMS],
        ..Terms::default()
    };
    fixture_with(rt, label, terms)
}

#[test]
fn honest_claim_of_32_notes_is_paid_in_full_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = full(&rt, "native-cap-honest")?;
    let q = f.request()?;
    ensure!(q.request.amount == 4096);
    let response = f.respond_plain(&q)?;
    f.paid_in_full(&q, response, f.witness())
}

/// The holder spent 16 of the 32 notes, one spend each, after her backup.
/// The Hub cites all 16 and she is paid the other 2048.
#[test]
fn stale_claim_of_32_notes_is_countered_spend_by_spend_and_paid_the_rest() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = full(&rt, "native-cap-stale")?;
    let q = f.request()?;

    // Cost of the largest counter: every item voided by its own spend.
    let all = (0..ITEMS).map(|i| f.spent(i)).collect::<Result<Vec<_>>>()?;
    let bytes = f.counter_input(&q, &all, None)?;
    export("native-cap-all-counter.bin", &bytes)?;
    let voided = f.counter(&q, &all, None)?;
    ensure!(voided.mask == u32::MAX && voided.owed == 0);

    let evidence = &all[ITEMS / 2..];
    let counter = f.counter(&q, evidence, None)?;
    ensure!(counter.mask == 0xffff_0000 && counter.owed == 2048);
    let response = f.respond_counter(&q, &counter, &counter_disclosures(evidence)?)?;
    f.paid_corrected(&q, response, f.witness(), 2048)
}

/// The holder's payment of note `i` to another wallet in four outputs, under
/// a grant that the Hub released: the signed spend and its unlock.
fn granted(f: &Fixture, i: usize) -> Result<(Evidence, Unlock)> {
    let notes = &f.notes[i..=i];
    let payee = HoldRoot::from_seed(&[9; 32])?;
    let outputs = [64, 32, 16, 16]
        .iter()
        .enumerate()
        .map(|(j, d)| Output::from_derivation(&derive(41, &payee, *d, (4 * i + j) as u32)))
        .collect::<Result<Vec<_>, _>>()?;
    let proposal = Proposal::new(f.keyset(), notes, point(43), outputs, None)?;
    let prepared = PreparedGrant::new(&f.issuer, &proposal)?;
    let signed = SignedProposal::sign(
        f.keyset(),
        proposal,
        notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &[owner(f.root, i as u32)],
    )?;
    let unlock = prepared.release(f.keyset(), notes, &signed)?;
    let spend = Evidence::Spend {
        signed,
        notes: notes.to_vec(),
        unlock: Some(unlock),
    };
    Ok((spend, unlock))
}

/// The 16 spends were payments under grants. The Hub can cite a granted
/// spend only with its unlock, so its response publishes all 16, and the
/// payees can open their notes from the chain. A counter of all 32 would
/// fill every disclosure slot.
#[test]
fn stale_claim_countered_by_granted_spends_publishes_every_unlock() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = full(&rt, "native-cap-granted")?;
    let q = f.request()?;
    let (all, unlocks): (Vec<_>, Vec<_>) = (0..ITEMS)
        .map(|i| granted(&f, i))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let bytes = f.counter_input(&q, &all, None)?;
    export("native-cap-granted-all-counter.bin", &bytes)?;
    let voided = f.counter(&q, &all, None)?;
    ensure!(voided.mask == u32::MAX && voided.owed == 0);
    let slots: BTreeSet<_> = counter_disclosures(&all)?.values.into_iter().collect();
    ensure!(slots == unlocks.iter().copied().collect());

    let evidence = &all[ITEMS / 2..];
    let counter = f.counter(&q, evidence, None)?;
    ensure!(counter.mask == 0xffff_0000 && counter.owed == 2048);
    let response = f.respond_counter(&q, &counter, &counter_disclosures(evidence)?)?;
    let posted: BTreeSet<_> = f.published()?.into_iter().collect();
    ensure!(unlocks[ITEMS / 2..].iter().all(|t| posted.contains(t)));
    ensure!(posted.len() == ITEMS / 2 + 1, "the other slots are empty");
    f.paid_corrected(&q, response, f.witness(), 2048)
}
