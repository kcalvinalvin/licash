//! The protocol on Bitcoin Core with real proofs: both programs run
//! in the RISC Zero zkVM, their receipts are wrapped in Groth16, and the
//! contract's instances are garbled under the real verifying key.
//!
//! A holder claims 3072 from a stale backup. The Hub proves its counter in
//! the zkVM and posts it. The holder's Full statement is refused, she proves
//! the corrected one, which verifies the Hub's seal inside the zkVM, and
//! asserts it. The Hub challenges anyway, her proof opens a kept instance,
//! and she is paid her unspent 2048.
//!
//! Proving takes about five hours on four CPU threads and 10 GB of memory.
//! Build the guests and the host first (`r0/build-guest.sh native-counter`,
//! `r0/build-guest.sh native-claim`), then run
//!
//! ```sh
//! LICASH_NATIVE_GUESTS=$PWD/r0/guests LICASH_R0=$PWD/r0/host/target/release/licash-r0 \
//!   regtest/run.sh --test native_real_proof -- --ignored --nocapture
//! ```

use std::{path::PathBuf, process::Command};

use anyhow::{ensure, Context, Result};
use licash::{disclosure::Disclosures, guest::CounterInput, statement::counter_k};
use licash_regtest::{
    contract::{Opened, Stage},
    graph::Babe,
    native::{journal, Authority},
    native_fixture::{fixture_under, Terms},
    proof_from_points, verified_points,
};
use lngap_btc::regtest::Regtest;

/// Prove `input` with `licash-r0 <command>` and return the Groth16 points,
/// accepted only for `image` and `journal`.
fn prove(
    command: &str,
    name: &str,
    input: &[u8],
    image: &[u8; 32],
    journal: &[u8],
) -> Result<Vec<u8>> {
    let host = std::env::var_os("LICASH_R0")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../r0/host/target/release/licash-r0")
        });
    let dir = tempfile::tempdir()?.keep();
    let (bin, out) = (
        dir.join(format!("{name}.bin")),
        dir.join(format!("{name}.json")),
    );
    std::fs::write(&bin, input)?;
    println!("{command} {} (hours)", bin.display());
    let status = Command::new(&host)
        .arg(command)
        .args([&bin, &out])
        .status()
        .with_context(|| format!("{}: build r0/host", host.display()))?;
    ensure!(status.success(), "{command} failed");
    verified_points(&std::fs::read(&out)?, image, journal)
}

#[test]
#[ignore = "proves both native programs for real: about five hours, run explicitly"]
fn stale_native_claim_is_countered_and_paid_under_real_proofs() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let authority = Authority::real()?;
    let programs = authority.programs;
    let f = fixture_under(&rt, "native-real", Terms::default(), authority)?;
    let (rt, c) = (f.rt, &f.c);
    let (base, d) = (&f.n.base, &f.n.d);
    let q = f.request()?;

    // The Hub's counter: the holder's own spend of her 1024 note.
    let input = f.counter_input(&q, &f.paid_away()?, None)?;
    let counter = CounterInput::from_bytes(&input)?.run()?;
    ensure!(counter.mask == 0b10 && counter.owed == 2048);
    let seal = prove(
        "native-counter-prove",
        "counter",
        &input,
        &programs.counter,
        &counter.to_bytes(),
    )?;
    let k = counter_k(&counter, &seal)?;
    let response = f.respond_sealed(&q, &counter, &Disclosures::empty(), k)?;

    // The holder: Full is refused, the corrected statement holds.
    let mut verify =
        |seal: &[u8], public: &[u8]| licash_groth16::verify(&programs.counter, seal, public);
    let full = f.statement(&q, 0, response.clone(), f.witness())?;
    let refused = full.run(&mut verify).unwrap_err();
    ensure!(refused.to_string().contains("countered"), "{refused}");
    let corrected = f.statement(&q, 1, response, f.witness())?;
    let input = corrected.to_bytes()?;
    let (j, paid) = corrected.run(&mut verify)?;
    ensure!(j == journal(base, d, 1) && paid == 2048);
    let points = prove(
        "native-prove",
        "holder",
        &input,
        &programs.holder,
        &j.to_bytes(),
    )?;
    let proof = proof_from_points(&points)?;

    let hub_corr = Opened::responded_corrected(&rt.get_tx(&c.txid("respond_counter"))?)?;
    let asserted = c.assert_corrected(rt, &proof, &hub_corr)?;
    let (bits, read) = c.asserted(&asserted)?;
    // The Hub challenges a true assertion and loses.
    let challenged = c.challenge(rt, &asserted)?;
    let recovered = c
        .answer(rt, &challenged, &bits, &read)?
        .context("a real proof opens a kept instance")?;
    ensure!(recovered.output == c.set["wrongly_challenged_corrected_0"].tx.output);
    let mine = Opened::asserted_b_corr(&asserted)?;
    f.collect(
        &recovered,
        Stage::Corrected,
        2048,
        &Babe::request_wire(&mine),
    )
}
