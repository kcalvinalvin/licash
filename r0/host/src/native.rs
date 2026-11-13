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

    fn guest(self) -> Result<Vec<u8>> {
        let path = format!(
            "{}/../guests/{}.bin",
            env!("CARGO_MANIFEST_DIR"),
            self.name()
        );
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
