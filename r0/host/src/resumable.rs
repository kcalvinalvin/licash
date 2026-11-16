//! A succinct proof carried on across runs, a segment at a time. Each
//! segment, proved and lifted, is kept in a work directory, and so is the
//! join of those before it: a run stopped anywhere (killed, out of memory,
//! the phone restarted) carries on where it stopped when run again. The
//! segments wait on disk, not in memory, one at a time in memory proved.
//!
//! The same proof as [`risc0_zkvm::Prover::prove_with_opts`] with
//! [`risc0_zkvm::ProverOpts::succinct`]: lift each segment's receipt, join
//! them in order. For a guest that needs no assumptions only.

use std::fs::{create_dir_all, read, read_to_string, remove_file, rename, write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use risc0_zkvm::sha::{Impl, Sha256};
use risc0_zkvm::{
    compute_image_id, get_prover_server, ExecutorImpl, InnerReceipt, ProverOpts, Receipt,
    ReceiptClaim, Segment, SegmentRef, SuccinctReceipt, VerifierContext,
};

type Lifted = SuccinctReceipt<ReceiptClaim>;

/// A segment written to the work directory, read back to be proved.
struct OnDisk(PathBuf);

impl SegmentRef for OnDisk {
    fn resolve(&self) -> Result<Segment> {
        let bytes = read(&self.0).with_context(|| format!("{}", self.0.display()))?;
        Ok(bincode::deserialize(&bytes)?)
    }
}

/// What the execution the work directory proves made, kept once all its
/// segments are: their number, the journal and the user cycles. The
/// guest's execution is not the same each run (the random bytes it is
/// handed differ), so the segments of one only are joined: never
/// executed again once kept.
struct Executed {
    segments: usize,
    journal: Vec<u8>,
    user_cycles: u64,
}

/// `bytes` to `path` whole or not at all.
fn keep(path: &Path, bytes: &[u8]) -> Result<()> {
    let part = path.with_extension("part");
    write(&part, bytes)?;
    std::fs::File::open(&part)?.sync_all()?;
    rename(&part, path)?;
    Ok(())
}

/// What the work directory proves: refused if it was another proof's.
fn claim_dir(dir: &Path, image: &str, input: &[u8], po2: Option<u32>) -> Result<()> {
    create_dir_all(dir)?;
    let what = format!(
        "image {image}\ninput {}\npo2 {}\n",
        hex::encode(Impl::hash_bytes(input).as_bytes()),
        po2.map_or("default".into(), |p| p.to_string())
    );
    let meta = dir.join("proving");
    match read_to_string(&meta) {
        Ok(had) => ensure!(
            had == what,
            "{} holds another proof's work (its program, input or segment size): \
             use another directory",
            dir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => keep(&meta, what.as_bytes())?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn lifted_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("lifted-{i:05}"))
}

fn segment_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("segment-{i:05}"))
}

/// The work of `dir` but what says what it proves: that of an execution
/// cut short, or of another.
fn forget(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let work = ["segment-", "lifted-", "joined", "executed", "receipt"];
        if work.iter().any(|w| name.starts_with(w)) {
            remove_file(&path)?;
        }
    }
    Ok(())
}

/// Execute `bin` on `input` in `dir`, every segment written there, then
/// what it made: refused unless its journal is `expected`.
fn execute(bin: &[u8], name: &str, input: &[u8], expected: &[u8], dir: &Path) -> Result<Executed> {
    let start = Instant::now();
    forget(dir)?;
    let session = ExecutorImpl::from_elf(super::env_for(input)?, bin)?.run_with_callback(
        |segment| -> Result<Box<dyn SegmentRef>> {
            let path = segment_path(dir, segment.index as usize);
            keep(&path, &bincode::serialize(&segment)?)?;
            Ok(Box::new(OnDisk(path)))
        },
    )?;
    let journal = session.journal.clone().unwrap_or_default().bytes;
    ensure!(journal == expected, "journal");
    ensure!(
        session.assumptions.is_empty(),
        "{name} needs assumptions, which this prover does not resolve"
    );
    let executed = Executed {
        segments: session.segments.len(),
        journal,
        user_cycles: session.user_cycles,
    };
    let kept = (executed.segments, &executed.journal, executed.user_cycles);
    keep(&dir.join("executed"), &bincode::serialize(&kept)?)?;
    println!(
        "{name}: executed in {:.1} s, {} user cycles, {} segments; work in {}",
        start.elapsed().as_secs_f64(),
        executed.user_cycles,
        executed.segments,
        dir.display()
    );
    Ok(executed)
}

/// What the execution kept in `dir` made, if one was and each of its
/// segments is there, lifted or to prove.
fn executed(dir: &Path) -> Result<Option<Executed>> {
    let executed = match read(dir.join("executed")) {
        Ok(bytes) => {
            let (segments, journal, user_cycles) = bincode::deserialize(&bytes)?;
            Executed {
                segments,
                journal,
                user_cycles,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let whole = (0..executed.segments)
        .all(|i| lifted_path(dir, i).exists() || segment_path(dir, i).exists());
    Ok(whole.then_some(executed))
}

fn rss() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
            .unwrap_or_else(|| "?".into())
    };
    format!("rss {} kB, peak {} kB", field("VmRSS:"), field("VmHWM:"))
}

/// Prove `bin` on `input`, its journal `expected`, carrying on the work kept
/// in `dir`: the verified succinct receipt, also written to `dir/receipt`.
pub fn prove(bin: &[u8], name: &str, input: &[u8], expected: &[u8], dir: &Path) -> Result<Receipt> {
    let po2 = super::segment_limit(std::env::var("LICASH_SEGMENT_PO2").ok().as_deref())?;
    let id = compute_image_id(bin)?;
    claim_dir(dir, &id.to_string(), input, po2)?;
    let start = Instant::now();
    let executed = match executed(dir)? {
        Some(executed) => {
            let lifted = (0..executed.segments).filter(|i| lifted_path(dir, *i).exists());
            println!(
                "{name}: carried on, {} of {} segments lifted already; work in {}",
                lifted.count(),
                executed.segments,
                dir.display()
            );
            executed
        }
        None => execute(bin, name, input, expected, dir)?,
    };
    ensure!(executed.journal == expected, "journal");
    let n = executed.segments;

    let prover = get_prover_server(&ProverOpts::succinct())?;
    let ctx = VerifierContext::default();
    let mut proved = 0;
    for i in 0..n {
        let lifted = lifted_path(dir, i);
        if lifted.exists() {
            continue;
        }
        let t = Instant::now();
        let segment = OnDisk(segment_path(dir, i)).resolve()?;
        let receipt = prover.prove_segment(&ctx, &segment)?;
        drop(segment);
        let up = prover.lift(&receipt)?;
        keep(&lifted, &bincode::serialize(&up)?)?;
        let _ = remove_file(segment_path(dir, i));
        proved += 1;
        println!(
            "{name}: segment {}/{n} proved and lifted in {:.1} s ({})",
            i + 1,
            t.elapsed().as_secs_f64(),
            rss()
        );
    }

    // Joined in order, the running join kept after each.
    let joined_path = dir.join("joined");
    let load = |path: &Path| -> Result<Lifted> { Ok(bincode::deserialize(&read(path)?)?) };
    let (mut through, mut acc): (usize, Lifted) = match read(&joined_path) {
        Ok(bytes) => bincode::deserialize(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (0, load(&lifted_path(dir, 0))?),
        Err(e) => return Err(e.into()),
    };
    for i in through + 1..n {
        let t = Instant::now();
        acc = prover.join(&acc, &load(&lifted_path(dir, i))?)?;
        through = i;
        keep(&joined_path, &bincode::serialize(&(through, &acc))?)?;
        println!(
            "{name}: joined {}/{n} in {:.1} s ({})",
            i + 1,
            t.elapsed().as_secs_f64(),
            rss()
        );
    }

    let receipt = Receipt::new(InnerReceipt::Succinct(acc), executed.journal);
    receipt.verify(id).context("the joined receipt")?;
    let out = dir.join("receipt");
    keep(&out, &bincode::serialize(&receipt)?)?;
    println!(
        "{name}: succinct receipt, {proved} of {n} segments proved this run, in {:.1} s ({}); \
         written {}",
        start.elapsed().as_secs_f64(),
        rss(),
        out.display()
    );
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_work_directory_is_one_proofs() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        claim_dir(&work, "program", b"input", Some(18)).unwrap();
        // The same proof carries on.
        claim_dir(&work, "program", b"input", Some(18)).unwrap();
        for (image, input, po2) in [
            ("program", &b"input"[..], Some(19)),
            ("program", b"input", None),
            ("program", b"other", Some(18)),
            ("other", b"input", Some(18)),
        ] {
            let e = claim_dir(&work, image, input, po2).unwrap_err();
            assert!(format!("{e}").contains("another proof's work"), "{e}");
        }
    }

    #[test]
    fn an_execution_is_carried_on_only_whole() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        assert!(executed(work).unwrap().is_none());
        let kept = (3usize, vec![1u8, 2], 99u64);
        keep(&work.join("executed"), &bincode::serialize(&kept).unwrap()).unwrap();
        keep(&lifted_path(work, 0), b"lifted").unwrap();
        keep(&segment_path(work, 2), b"segment").unwrap();
        // Segment 1 neither lifted nor kept: executed again.
        assert!(executed(work).unwrap().is_none());
        keep(&segment_path(work, 1), b"segment").unwrap();
        let carried = executed(work).unwrap().unwrap();
        assert_eq!(
            (carried.segments, carried.journal, carried.user_cycles),
            (3, vec![1, 2], 99)
        );
        // Forgotten but what it proves.
        keep(&work.join("proving"), b"image").unwrap();
        keep(&work.join("joined"), b"joined").unwrap();
        forget(work).unwrap();
        let left: Vec<_> = std::fs::read_dir(work)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["proving"]);
    }

    #[test]
    fn kept_files_are_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifted-00000");
        keep(&path, b"one").unwrap();
        keep(&path, b"two").unwrap();
        assert_eq!(read(&path).unwrap(), b"two");
        assert!(!path.with_extension("part").exists());
    }
}
