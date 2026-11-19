//! The chain program (`r0/chain-guest`) on a batch of the hub's header
//! chain (`licash_chain::Batch::to_bytes`): the succinct receipt of the batch,
//! kept as `<work-dir>/receipt`, with the receipt of the batch before
//! resolved (`--assume`), each segment kept as it is proven, so a stopped
//! batch carries on where it stopped (`crate::resumable`). Each command also
//! runs `licash_chain::extend` natively, and a guest that disagrees with it is
//! an error.

use std::fs::{create_dir_all, read};
use std::path::Path;

use anyhow::{Context, Result};
use licash_chain::{extend, Batch};

use crate::hp::{on_device, receipts, succinct};
use crate::probe;

const NAME: &str = "chain";

/// The journal the library computes for `bytes`, or its refusal.
fn expected(bytes: &[u8]) -> Result<Vec<u8>> {
    let batch = Batch::from_bytes(bytes)?;
    // The zkVM verifies the receipt the batch resumes; here, the receipt's
    // own claim, which the host proved before handing it over.
    Ok(extend(&batch, &|_, _| {})?.to_bytes())
}

/// Execute the batch at `path` (resuming the batch whose receipt `assume`
/// names, if it does): its journal and cycles, as `hp-exec` says them of a
/// statement. A guest that disagrees with the library is an error.
pub fn exec(path: &str, program: Option<&str>, assume: &[String]) -> Result<()> {
    let bytes = read(path)?;
    let bin = probe::program(program)?;
    let expected = expected(&bytes)?;
    let env = super::env_with(&bytes, super::segment_override()?, &receipts(assume)?)?;
    let t = std::time::Instant::now();
    let run = risc0_zkvm::default_executor().execute(env, &bin)?;
    anyhow::ensure!(
        run.journal.bytes == expected,
        "the guest's journal differs from the library's"
    );
    let cycles: u64 = run.segments.iter().map(|g| g.cycles as u64).sum();
    let headers = Batch::from_bytes(&bytes)?.headers.len();
    println!(
        "{NAME}: {headers} headers, {cycles} cycles, {} segments, {} ms",
        run.segments.len(),
        t.elapsed().as_millis()
    );
    Ok(())
}

/// Prove the batch at `path` in `work`, on the GPU through the CUDA build if
/// one is usable, else on the CPU.
pub fn prove(path: &str, work: &Path, program: Option<&str>, assume: &[String]) -> Result<()> {
    let bytes = read(path)?;
    let bin = probe::program(program)?;
    let expected = expected(&bytes)?;
    create_dir_all(work)?;
    let receipts = receipts(assume)?;
    if work.join("receipt").exists() {
        println!("{NAME}: proven already: {}", work.join("receipt").display());
        return Ok(());
    }
    on_device("chain-stark", path, work, program, assume, |po2| {
        succinct(NAME, &bin, &bytes, &receipts, &expected, work, po2).map(drop)
    })
    .context("the chain's batch")
}

/// `chain-stark`, the CUDA build's part of [`prove`].
pub fn stark_command(
    path: &str,
    work: &Path,
    po2: &str,
    program: Option<&str>,
    assume: &[String],
) -> Result<()> {
    let bytes = read(path)?;
    let bin = probe::program(program)?;
    let expected = expected(&bytes)?;
    let po2 = super::segment_limit(Some(po2))?.context("a segment size")?;
    succinct(NAME, &bin, &bytes, &receipts(assume)?, &expected, work, po2).map(drop)
}

/// A receipt that only says the batch at `path` proved its journal, to
/// execute a guest that verifies it (`hp-exec --assume`) and count its
/// cycles, which the executor does by the claim alone. It proves nothing: no
/// prover resolves it ([`crate::resumable`] refuses a receipt that is not
/// succinct) and a verifier refuses it.
pub fn fake(path: &str, out: &str) -> Result<()> {
    use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};
    let batch = Batch::from_bytes(&read(path)?)?;
    let journal = extend(&batch, &|_, _| {})?.to_bytes();
    let claim = ReceiptClaim::ok(
        risc0_zkvm::sha::Digest::from_bytes(batch.chain),
        journal.clone(),
    );
    let receipt = Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal);
    crate::resumable::keep(out.as_ref(), &bincode::serialize(&receipt)?)?;
    println!("{NAME}: a receipt that proves nothing, for hp-exec: {out}");
    Ok(())
}
