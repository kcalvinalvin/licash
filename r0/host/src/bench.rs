//! How fast this machine proves: the hub's measure of its prover, to plan
//! the proofs it commits to (`licash-r0 bench <out.json> [program]`). It
//! proves the first two segments of a kept input, joins them, and wraps
//! the join as a whole proof is: its seconds per segment, and those of the
//! wrap every proof ends with. Minutes on a CPU, about one on a GPU, then
//! the Groth16 wrap on the CPU either way.

use std::fs::{create_dir_all, read};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use risc0_zkvm::{
    get_prover_server, ExecutorImpl, ProverOpts, Segment, SegmentRef, VerifierContext,
};
use serde_json::{json, Value};

use crate::probe;
use crate::resumable::keep;

/// A segment the measure does not prove.
struct Unused;

impl SegmentRef for Unused {
    fn resolve(&self) -> Result<Segment> {
        bail!("not kept")
    }
}

/// The kept input: the statement's tests' answered claim
/// (`export_inputs`, r0/README.md).
fn input() -> Result<Vec<u8>> {
    let installed = probe::share().join("bench.bin");
    let path = match installed.exists() {
        true => installed,
        false => Path::new(env!("CARGO_MANIFEST_DIR")).join("bench.bin"),
    };
    read(&path).with_context(|| format!("no input to measure with at {}", path.display()))
}

/// Where to measure: a directory beside `out`, removed once done. Not in
/// the system's temporary directory: the Groth16 wrap's witness takes
/// hundreds of MB, more than a hardened container's /tmp holds, and cut
/// short there it fails the wrap.
fn work_beside(out: &Path) -> Result<tempfile::TempDir> {
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    Ok(tempfile::Builder::new().prefix(".bench-").tempdir_in(dir)?)
}

/// The orchestrating build's measure, written to `out`: through the CUDA
/// build when a GPU is usable, as [`crate::hp::prove`] proves.
pub fn bench(out: &Path, program: Option<&str>) -> Result<()> {
    let work = work_beside(out)?;
    let dir = work.path();
    let (device, stark) = match probe::cuda_build().filter(|_| probe::gpu_usable()) {
        Some(cuda) => {
            let mut measured = None;
            for po2 in super::hp::GPU_PO2 {
                let status = Command::new(&cuda)
                    .arg("bench-stark")
                    .arg(dir)
                    .arg(po2.to_string())
                    .args(program)
                    .status()
                    .context("run the CUDA prover")?;
                if status.success() {
                    measured = Some(serde_json::from_slice(&read(dir.join("stark.json"))?)?);
                    break;
                }
                println!("bench: the GPU failed at segments of 2^{po2}: {status}");
            }
            ("cuda", measured.context("the GPU prover failed")?)
        }
        None => ("cpu", stark(dir, super::hp::CPU_PO2, program)?),
    };
    let t = Instant::now();
    crate::groth16::wrap(
        &probe::groth16_dir(),
        &read(dir.join("p254"))?,
        &dir.join("groth16"),
    )?;
    let wrap = t.elapsed().as_secs_f64();
    let p254 = stark["p254"].as_f64().context("p254")?;
    let report = json!({
        "device": device,
        "po2": stark["po2"],
        "segment_cycles": stark["segment_cycles"],
        "segment": stark["segment"],
        "tail": p254 + wrap,
    });
    println!("bench: {report}");
    keep(out, &serde_json::to_vec(&report)?)?;
    Ok(())
}

/// This build's part: two segments of 2^`po2` proved, lifted and joined,
/// the join's `identity_p254` seal kept as `dir/p254`. Its report, also
/// kept as `dir/stark.json`: the user cycles a segment holds (as
/// `hp-exec` counts them), the seconds a segment takes (proved, lifted and
/// joined), those of `identity_p254`.
pub fn stark(dir: &Path, po2: u32, program: Option<&str>) -> Result<Value> {
    create_dir_all(dir)?;
    let bin = probe::program(program)?;
    let bytes = input()?;
    let kept: Arc<Mutex<Vec<Segment>>> = Arc::default();
    let first = kept.clone();
    let session = ExecutorImpl::from_elf(super::env_for(&bytes, Some(po2))?, &bin)?
        .run_with_callback(move |segment| -> Result<Box<dyn SegmentRef>> {
            let mut first = first
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if first.len() < 2 {
                first.push(segment);
            }
            Ok(Box::new(Unused))
        })?;
    let n = session.segments.len() as u64;
    let segment_cycles = session.user_cycles / n.max(1);
    drop(session);
    let segments = std::mem::take(
        &mut *kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    ensure!(
        segments.len() == 2,
        "the measure's input runs in one segment"
    );
    let prover = get_prover_server(&ProverOpts::succinct())?;
    let ctx = VerifierContext::default();
    let t = Instant::now();
    let mut lifted = Vec::new();
    for segment in &segments {
        lifted.push(prover.lift(&prover.prove_segment(&ctx, segment)?)?);
    }
    let joined = prover.join(&lifted[0], &lifted[1])?;
    let segment = t.elapsed().as_secs_f64() / 2.0;
    let t = Instant::now();
    let ident = prover.identity_p254(&joined)?;
    let p254 = t.elapsed().as_secs_f64();
    keep(&dir.join("p254"), &ident.get_seal_bytes())?;
    let report = json!({
        "po2": po2,
        "segment_cycles": segment_cycles,
        "segment": segment,
        "p254": p254,
    });
    keep(&dir.join("stark.json"), &serde_json::to_vec(&report)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_measures_beside_where_it_keeps_its_measure() {
        let kept = tempfile::tempdir().unwrap();
        let out = kept.path().join("bench.json");
        let work = work_beside(&out).unwrap();
        assert_eq!(work.path().parent(), Some(kept.path()));
        let path = work.path().to_path_buf();
        drop(work);
        assert!(!path.exists(), "left behind");
    }
}
