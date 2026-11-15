//! The guests on exported inputs (the regtest lifecycle
//! writes them under `LICASH_NATIVE_INPUTS`): execute, or prove and export a
//! Groth16 receipt. Each command also runs the same library code natively,
//! and a guest that disagrees with it is an error.

use std::fs::read;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use licash::guest::CounterInput;
use licash::statement::StatementInput;
use risc0_zkvm::default_executor;

#[derive(Clone, Copy)]
pub enum Program {
    Holder,
    Counter,
}

impl Program {
    fn name(self) -> &'static str {
        match self {
            Self::Holder => "native-claim",
            Self::Counter => "native-counter",
        }
    }

    /// The packed guest, from `LICASH_R0_GUESTS` if set (a host moved
    /// to another machine, a phone say), else from this checkout's.
    fn guest(self) -> Result<Vec<u8>> {
        let dir = std::env::var("LICASH_R0_GUESTS")
            .unwrap_or_else(|_| format!("{}/../guests", env!("CARGO_MANIFEST_DIR")));
        let path = format!("{dir}/{}.bin", self.name());
        read(&path).with_context(|| {
            format!(
                "no packed guest at {path}: run r0/build-guest.sh {}",
                self.name()
            )
        })
    }

    /// The journal the library computes for this input, or its refusal.
    fn expected(self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Counter => CounterInput::from_bytes(bytes)?.run()?.to_bytes().to_vec(),
            Self::Holder => {
                let input = StatementInput::from_bytes(bytes)?;
                let program = input.base.parts().recipe.counter_program;
                input
                    .run(&mut |seal, public| licash_groth16::verify(&program, seal, public))?
                    .0
                    .to_bytes()
            }
        })
    }
}

pub fn exec(program: Program, path: &str) -> Result<()> {
    let bytes = read(path)?;
    let bin = program.guest()?;
    let expected = program.expected(&bytes);
    let t = Instant::now();
    let run = default_executor().execute(super::env_for(&bytes)?, &bin);
    let ms = t.elapsed().as_millis();
    let name = program.name();
    match (run, expected) {
        (Ok(s), Ok(j)) => {
            ensure!(
                s.journal.bytes == j,
                "the guest's journal differs from the library's"
            );
            let cycles: u64 = s.segments.iter().map(|g| g.cycles as u64).sum();
            println!(
                "{name}: journal {}, {} input bytes, {cycles} cycles, {} segments, {ms} ms",
                hex::encode(&j),
                bytes.len(),
                s.segments.len()
            );
        }
        (Err(_), Err(e)) => println!("{name}: no proof, as the library refuses: {e}"),
        (Ok(_), Err(e)) => bail!("the guest proved what the library refuses: {e}"),
        (Err(e), Ok(_)) => bail!("the guest refused what the library proves: {e}"),
    }
    Ok(())
}

pub fn prove(program: Program, path: &str, out: &str) -> Result<()> {
    let bytes = read(path)?;
    let bin = program.guest()?;
    let expected = program.expected(&bytes)?;
    let receipt = super::prove_bytes(&bin, program.name(), &bytes, &expected, true)?;
    super::export_receipt(&bin, &receipt, out, expected.len())
}

/// The succinct (STARK) receipt only, written for [`wrap`] elsewhere: the
/// part of a proof a phone can make, Groth16 needing an x86 machine.
pub fn prove_succinct(program: Program, path: &str) -> Result<()> {
    let bytes = read(path)?;
    let bin = program.guest()?;
    let expected = program.expected(&bytes)?;
    super::prove_bytes(&bin, program.name(), &bytes, &expected, false)?;
    Ok(())
}

/// A succinct receipt of the guest at `receipt`, wrapped in Groth16,
/// verified and exported to `out`.
pub fn wrap(program: Program, receipt: &str, out: &str) -> Result<()> {
    let bin = program.guest()?;
    let succinct: risc0_zkvm::Receipt = bincode::deserialize(&read(receipt)?)?;
    let id = risc0_zkvm::compute_image_id(&bin)?;
    succinct.verify(id).context("not a receipt of this guest")?;
    let journal = succinct.journal.bytes.len();
    let t = Instant::now();
    let opts = risc0_zkvm::ProverOpts::groth16();
    let wrapped = risc0_zkvm::default_prover().compress(&opts, &succinct)?;
    println!(
        "{}: groth16 wrap in {:.1} s",
        program.name(),
        t.elapsed().as_secs_f64()
    );
    super::export_receipt(&bin, &wrapped, out, journal)
}
