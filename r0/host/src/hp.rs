//! The Hub's statement guest (`r0/hp-guest`) on exported inputs
//! (`licash_hp::HubInput::to_bytes`): execute, or prove and export the
//! Groth16 seal the holder's garbling opens to. Each command also runs
//! `licash_hp::check` natively, and a guest that disagrees with it is an
//! error.

use std::fs::read;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use licash_hp::HubInput;
use risc0_zkvm::default_executor;

const NAME: &str = "hp";

/// The packed guest, from `LICASH_R0_GUESTS` if set (a host moved to
/// another machine), else from this checkout's.
fn guest() -> Result<Vec<u8>> {
    let dir = std::env::var("LICASH_R0_GUESTS")
        .unwrap_or_else(|_| format!("{}/../guests", env!("CARGO_MANIFEST_DIR")));
    let path = format!("{dir}/{NAME}.bin");
    read(&path).with_context(|| format!("no packed guest at {path}: run r0/build-guest.sh {NAME}"))
}

/// The journal the library computes for this input, or its refusal.
fn expected(bytes: &[u8]) -> Result<Vec<u8>> {
    let input = HubInput::from_bytes(bytes)?;
    Ok(licash_hp::check(&input)?.to_vec())
}

pub fn exec(path: &str) -> Result<()> {
    let bytes = read(path)?;
    let bin = guest()?;
    let expected = expected(&bytes);
    let t = Instant::now();
    let run = default_executor().execute(super::env_for(&bytes)?, &bin);
    let ms = t.elapsed().as_millis();
    match (run, expected) {
        (Ok(s), Ok(j)) => {
            ensure!(
                s.journal.bytes == j,
                "the guest's journal differs from the library's"
            );
            let cycles: u64 = s.segments.iter().map(|g| g.cycles as u64).sum();
            println!(
                "{NAME}: journal {}, {} input bytes, {cycles} cycles, {} segments, {ms} ms",
                hex::encode(&j),
                bytes.len(),
                s.segments.len()
            );
        }
        (Err(_), Err(e)) => println!("{NAME}: no proof, as the library refuses: {e}"),
        (Ok(_), Err(e)) => bail!("the guest proved what the library refuses: {e}"),
        (Err(e), Ok(_)) => bail!("the guest refused what the library proves: {e}"),
    }
    Ok(())
}

pub fn prove(path: &str, out: &str) -> Result<()> {
    let bytes = read(path)?;
    let bin = guest()?;
    let expected = expected(&bytes)?;
    let receipt = super::prove_bytes(&bin, NAME, &bytes, &expected, true)?;
    super::export_receipt(&bin, &receipt, out, expected.len())
}

/// The succinct (STARK) receipt only (Groth16 needs an x86 machine,
/// [`wrap`]), carried on across runs in `dir`: run again after any stop,
/// it goes on where it stopped.
pub fn prove_succinct(path: &str, dir: Option<&str>) -> Result<()> {
    let bytes = read(path)?;
    let bin = guest()?;
    let expected = expected(&bytes)?;
    let dir = dir.map_or_else(|| format!("{path}.proof"), str::to_owned);
    super::resumable::prove(&bin, NAME, &bytes, &expected, dir.as_ref())?;
    Ok(())
}

/// A succinct receipt of the guest at `receipt`, wrapped in Groth16,
/// verified and exported to `out`.
pub fn wrap(receipt: &str, out: &str) -> Result<()> {
    let bin = guest()?;
    let succinct: risc0_zkvm::Receipt = bincode::deserialize(&read(receipt)?)?;
    let id = risc0_zkvm::compute_image_id(&bin)?;
    succinct.verify(id).context("not a receipt of this guest")?;
    let journal = succinct.journal.bytes.len();
    let t = Instant::now();
    let opts = risc0_zkvm::ProverOpts::groth16();
    let wrapped = risc0_zkvm::default_prover().compress(&opts, &succinct)?;
    println!("{NAME}: groth16 wrap in {:.1} s", t.elapsed().as_secs_f64());
    super::export_receipt(&bin, &wrapped, out, journal)
}
