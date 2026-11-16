//! licash-r0: packs, executes and proves the RISC Zero guests (`r0/native-guest`
//! is the holder statement, `r0/native-counter-guest` the Hub's counter).
//!
//!   licash-r0 pack <elf> <out.bin>          pack an ELF with the zkOS kernel; prints the image id
//!   licash-r0 native-exec|native-counter-exec <input.bin>
//!                                           execute an input; prints journal and cycles
//!   licash-r0 native-prove|native-counter-prove <input.bin> <out.json>
//!                                           prove, wrap in Groth16, verify and export
//!   licash-r0 native-prove-succinct|native-counter-prove-succinct <input.bin> [work-dir]
//!                                           prove the STARK only, as a phone
//!                                           can, a segment at a time, carried on across
//!                                           runs in work-dir (<input.bin>.proof): writes
//!                                           work-dir/receipt
//!   licash-r0 native-wrap <receipt> <out.json>
//!                                           wrap such a receipt in Groth16 (x86, Docker),
//!                                           verify and export
//!
//! Packed guests are read from `r0/guests/` (built by r0/build-guest.sh).

mod native;
mod resumable;
mod seal;

use std::fs::{read, write, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use risc0_zkvm::{compute_image_id, default_prover, ExecutorEnv, ProverOpts, Receipt};

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

fn env_for(input: &[u8]) -> Result<ExecutorEnv<'static>> {
    let mut env = ExecutorEnv::builder();
    if let Some(po2) = segment_limit(std::env::var("LICASH_SEGMENT_PO2").ok().as_deref())? {
        env.segment_limit_po2(po2);
    }
    env.write(&(input.len() as u32))?.write_slice(&words(input));
    env.build()
}

/// Segment size for measurements; it does not change what is proven.
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

/// Allocate private, exclusive output handles before expensive proving. Never
/// reopen a predictable temporary path after a proof has been generated.
struct ProofArtifacts {
    directory: PathBuf,
    receipt: File,
    groth16: Option<File>,
    name: String,
}

fn create_artifact(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl ProofArtifacts {
    fn create(name: &str, groth16: bool) -> Result<Self> {
        ensure!(
            !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "proof artifact name must be a simple workload label"
        );
        let mut builder = tempfile::Builder::new();
        builder.prefix("licash-proof-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir()?;
        let receipt = create_artifact(&directory.path().join(format!("{name}.receipt")))?;
        let groth16 = if groth16 {
            Some(create_artifact(
                &directory.path().join(format!("{name}.groth16")),
            )?)
        } else {
            None
        };
        Ok(Self {
            directory: directory.keep(),
            receipt,
            groth16,
            name: name.into(),
        })
    }

    fn path(&self, extension: &str) -> PathBuf {
        self.directory.join(format!("{}.{extension}", self.name))
    }
}

fn prove_bytes(
    bin: &[u8],
    name: &str,
    bytes: &[u8],
    expected: &[u8],
    groth16: bool,
) -> Result<Receipt> {
    let mut artifacts = ProofArtifacts::create(name, groth16)?;
    println!("proof artifacts: {}", artifacts.directory.display());
    let id = compute_image_id(bin)?;
    let t = Instant::now();
    let info = default_prover().prove_with_opts(env_for(bytes)?, bin, &ProverOpts::succinct())?;
    let succinct_s = t.elapsed().as_secs_f64();
    let receipt: Receipt = info.receipt;
    receipt.verify(id)?;
    ensure!(receipt.journal.bytes == expected, "journal");
    println!(
        "{name}: succinct receipt in {succinct_s:.1} s, {} user cycles, {} segments, journal D {}.., receipt {} bytes",
        info.stats.user_cycles,
        info.stats.segments,
        hex::encode(&receipt.journal.bytes[..4]),
        bincode::serialize(&receipt)?.len()
    );
    bincode::serialize_into(&mut artifacts.receipt, &receipt)?;
    artifacts.receipt.sync_all()?;
    println!("written {}", artifacts.path("receipt").display());
    if groth16 {
        let t = Instant::now();
        let wrapped = default_prover().compress(&ProverOpts::groth16(), &receipt)?;
        let wrap_s = t.elapsed().as_secs_f64();
        wrapped.verify(id)?;
        let seal = wrapped.inner.groth16().map(|g| g.seal.len()).unwrap_or(0);
        println!("{name}: groth16 wrap in {wrap_s:.1} s, seal {seal} bytes");
        let file = artifacts
            .groth16
            .as_mut()
            .context("unallocated Groth16 output")?;
        bincode::serialize_into(&mut *file, &wrapped)?;
        file.sync_all()?;
        println!("written {}", artifacts.path("groth16").display());
        return Ok(wrapped);
    }
    Ok(receipt)
}

/// Export only a verified receipt of the current guest. The consumer must
/// still compare the journal to its own opening and verify the proof.
fn export_receipt(bin: &[u8], receipt: &Receipt, out: &str, journal_len: usize) -> Result<()> {
    let id = compute_image_id(bin)?;
    receipt.verify(id)?;
    ensure!(receipt.journal.bytes.len() == journal_len, "journal length");
    let groth16 = receipt
        .inner
        .groth16()
        .context("a Groth16 receipt is required")?;
    let proof = seal::compress(&groth16.seal)?;
    let data = serde_json::json!({
        "image_id": id.to_string(),
        "journal": hex::encode(&receipt.journal.bytes),
        "proof": hex::encode(proof),
    });
    write(out, serde_json::to_vec_pretty(&data)?)?;
    println!("verified and exported {out}");
    Ok(())
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
        ["native-exec", path] => native::exec(native::Program::Holder, path),
        ["native-counter-exec", path] => native::exec(native::Program::Counter, path),
        ["native-prove", path, out] => native::prove(native::Program::Holder, path, out),
        ["native-counter-prove", path, out] => native::prove(native::Program::Counter, path, out),
        ["native-prove-succinct", path, dir @ ..] if dir.len() <= 1 => {
            native::prove_succinct(native::Program::Holder, path, dir.first().copied())
        }
        ["native-counter-prove-succinct", path, dir @ ..] if dir.len() <= 1 => {
            native::prove_succinct(native::Program::Counter, path, dir.first().copied())
        }
        ["native-wrap", receipt, out] => native::wrap(native::Program::Holder, receipt, out),
        _ => bail!("usage: licash-r0 pack <elf> <out.bin> | id <program.bin> | native-exec|native-counter-exec <input.bin> | native-prove|native-counter-prove <input.bin> <out.json> | native-prove-succinct|native-counter-prove-succinct <input.bin> [work-dir] | native-wrap <receipt> <out.json>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn proof_artifact_files_are_exclusive_and_do_not_truncate_existing_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.receipt");
        write(&path, b"keep this data").unwrap();
        assert_eq!(
            create_artifact(&path).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(read(path).unwrap(), b"keep this data");
    }

    #[test]
    fn concurrent_workload_labels_get_distinct_private_artifacts() {
        let mut first = ProofArtifacts::create("counter-file", true).unwrap();
        let second = ProofArtifacts::create("counter-file", true).unwrap();
        assert_ne!(first.directory, second.directory);
        first.receipt.write_all(b"first receipt").unwrap();
        assert_eq!(read(first.path("receipt")).unwrap(), b"first receipt");
        assert!(read(second.path("receipt")).unwrap().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first.directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                first.receipt.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(first.directory).unwrap();
        std::fs::remove_dir_all(second.directory).unwrap();
    }

    #[test]
    fn artifact_labels_cannot_select_external_paths() {
        for name in ["", "../target", "/absolute", "a/b"] {
            assert!(ProofArtifacts::create(name, true).is_err());
        }
    }
}
