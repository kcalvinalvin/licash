//! licash-r0: packs, executes and proves the Hub's statement guest
//! (`r0/hp-guest`, `licash_hp::check`).
//!
//!   licash-r0 pack <elf> <out.bin>         pack an ELF with the zkOS kernel; prints the image id
//!   licash-r0 id <program.bin>             a packed program's image id
//!   licash-r0 hp-exec <input.bin> [program]
//!                                          execute an input; prints journal and cycles
//!   licash-r0 hp-prove <input.bin> <work-dir> [program]
//!                                          prove, wrap in Groth16, verify and export to
//!                                          work-dir/proof.json, carried on across runs
//!   licash-r0 probe                        what this machine proves with, as JSON
//!   licash-r0 bench <out.json> [program]   how fast it proves, as JSON
//!
//! A program is a packed guest's path, or an image id kept in the programs
//! directory ([`probe::share`]); by default the checkout's `r0/guests/hp.bin`
//! (built by r0/build-guest.sh).
//!
//! The export is JSON: the image id, the journal and the 256-byte Groth16
//! seal (`lichen_babe::risc0::proof_from_seal`'s input), all hex.
//!
//! Built with the `cuda` feature (as `licash-r0-cuda`), it proves on the
//! GPU. The CPU build proves through such a build beside it when it finds a
//! usable GPU, and on the CPU otherwise: one command, whichever the device.

mod bench;
mod groth16;
mod hp;
mod probe;
mod resumable;

use std::fs::{read, write};

use anyhow::{bail, ensure, Context, Result};
use risc0_zkvm::{compute_image_id, ExecutorEnv, Receipt};

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect()
}

/// The guest's environment for `input`, in segments of 2^`po2` cycles (or
/// the zkVM's default). The segment size does not change what is proven.
fn env_for(input: &[u8], po2: Option<u32>) -> Result<ExecutorEnv<'static>> {
    let mut env = ExecutorEnv::builder();
    if let Some(po2) = po2 {
        env.segment_limit_po2(po2);
    }
    // What a profile build of the guest prints (`r0/build-guest.sh hp-profile`).
    env.stdout(std::io::stdout());
    env.write(&(input.len() as u32))?.write_slice(&words(input));
    env.build()
}

/// `LICASH_SEGMENT_PO2`, a segment size for measurements, if set.
fn segment_limit(value: Option<&str>) -> Result<Option<u32>> {
    let Some(value) = value else { return Ok(None) };
    let po2 = value
        .parse::<u32>()
        .context("LICASH_SEGMENT_PO2 must be an integer")?;
    ensure!(
        (13..=20).contains(&po2),
        "LICASH_SEGMENT_PO2 must be 13..20"
    );
    Ok(Some(po2))
}

fn segment_override() -> Result<Option<u32>> {
    segment_limit(std::env::var("LICASH_SEGMENT_PO2").ok().as_deref())
}

/// The export of a Groth16 `receipt` of `bin`, verified, its journal
/// `expected`. The consumer must still compare the journal to its own
/// opening and verify the proof.
fn export(bin: &[u8], receipt: &Receipt, expected: &[u8]) -> Result<Vec<u8>> {
    let id = compute_image_id(bin)?;
    receipt.verify(id)?;
    ensure!(receipt.journal.bytes == expected, "journal");
    let groth16 = receipt
        .inner
        .groth16()
        .context("a Groth16 receipt is required")?;
    ensure!(groth16.seal.len() == 256, "a Groth16 seal is 256 bytes");
    let data = serde_json::json!({
        "image_id": id.to_string(),
        "journal": hex::encode(&receipt.journal.bytes),
        "seal": hex::encode(&groth16.seal),
    });
    Ok(serde_json::to_vec_pretty(&data)?)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["pack", elf, out] => {
            let bin =
                risc0_binfmt::ProgramBinary::new(&read(elf)?, risc0_zkos_v1compat::V1COMPAT_ELF)
                    .encode();
            write(out, &bin)?;
            println!("{}", compute_image_id(&bin)?);
            Ok(())
        }
        // A packed program's image id: those kept in r0/programs are
        // named by theirs.
        ["id", bin] => {
            println!("{}", compute_image_id(&read(bin)?)?);
            Ok(())
        }
        ["hp-exec", input, program @ ..] if program.len() <= 1 => {
            hp::exec(input, program.first().copied())
        }
        ["hp-prove", input, work, program @ ..] if program.len() <= 1 => {
            hp::prove(input, work.as_ref(), program.first().copied())
        }
        // The CUDA build's part of hp-prove: the succinct receipt and its
        // identity_p254 seal, in segments of 2^po2.
        ["hp-stark", input, work, po2, program @ ..] if program.len() <= 1 => {
            hp::stark_command(input, work.as_ref(), po2, program.first().copied())
        }
        ["bench", out, program @ ..] if program.len() <= 1 => {
            bench::bench(out.as_ref(), program.first().copied())
        }
        // The CUDA build's part of bench.
        ["bench-stark", dir, po2, program @ ..] if program.len() <= 1 => {
            let po2 = segment_limit(Some(po2))?.context("a segment size")?;
            bench::stark(dir.as_ref(), po2, program.first().copied()).map(drop)
        }
        ["probe"] => {
            println!("{}", serde_json::to_string(&probe::probe())?);
            Ok(())
        }
        // The CUDA build's own check of its GPU, for the CPU build's probe.
        ["probe-gpu"] => probe::gpu(),
        _ => bail!("usage: licash-r0 pack <elf> <out.bin> | id <program.bin> | hp-exec <input.bin> [program] | hp-prove <input.bin> <work-dir> [program] | probe | bench <out.json> [program]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmentation_is_optional_and_bounded() {
        assert_eq!(segment_limit(None).unwrap(), None);
        for po2 in ["13", "17", "18", "20"] {
            assert_eq!(
                segment_limit(Some(po2)).unwrap(),
                Some(po2.parse().unwrap())
            );
        }
        for value in ["", "12", "21", "invalid", "-1"] {
            assert!(segment_limit(Some(value)).is_err());
        }
    }
}
