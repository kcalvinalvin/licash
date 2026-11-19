//! A succinct proof carried on across runs, a segment at a time. Each
//! segment, proved and lifted, is kept in a work directory, and so is the
//! join of those before it: a run stopped anywhere (killed, out of memory,
//! the phone restarted) carries on where it stopped when run again. The
//! segments wait on disk, not in memory, one at a time in memory proved.
//!
//! The same proof as [`risc0_zkvm::Prover::prove_with_opts`] with
//! [`risc0_zkvm::ProverOpts::succinct`]: lift each segment's receipt, join
//! them in order, then resolve each assumption the guest made (a receipt of
//! another program it verified, `env::verify`) with the succinct receipt the
//! host gave for it, as `composite_to_succinct` does.

use std::fs::{create_dir_all, read, read_to_string, remove_file, rename, write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use risc0_zkvm::sha::{Digestible, Impl, Sha256};
use risc0_zkvm::{
    compute_image_id, get_prover_server, AssumptionReceipt, ExecutorImpl, InnerAssumptionReceipt,
    InnerReceipt, MaybePruned, Output, ProverOpts, Receipt, ReceiptClaim, Segment, SegmentRef,
    SuccinctReceipt, VerifierContext,
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

/// The assumptions the guest made, with the receipts that prove them,
/// kept beside `executed` (written before it, so an `executed` whose
/// assumptions are missing is never carried on). A work directory
/// without the file is one of a guest with none, as every one was before
/// this prover resolved any.
fn assumptions_path(dir: &Path) -> PathBuf {
    dir.join("assumptions")
}

type Made = Vec<(risc0_zkvm::Assumption, AssumptionReceipt)>;

fn kept_assumptions(dir: &Path) -> Result<Made> {
    match read(assumptions_path(dir)) {
        Ok(bytes) => Ok(bincode::deserialize(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// The succinct receipt that proves `assumption`, as `resolve` takes it. A
/// proof that is not succinct is refused: it is the only kind the
/// recursion resolves.
fn proving(
    assumption: &AssumptionReceipt,
) -> Result<risc0_zkvm::SuccinctReceipt<risc0_zkvm::Unknown>> {
    match assumption {
        AssumptionReceipt::Proven(InnerAssumptionReceipt::Succinct(s)) => Ok(s.clone()),
        AssumptionReceipt::Proven(_) => {
            bail!("an assumption is proven by a succinct receipt alone")
        }
        AssumptionReceipt::Unresolved(a) => {
            bail!("no receipt was given for the assumption {:?}", a.claim)
        }
    }
}

/// `bytes` to `path` whole or not at all.
pub fn keep(path: &Path, bytes: &[u8]) -> Result<()> {
    let part = path.with_extension("part");
    write(&part, bytes)?;
    std::fs::File::open(&part)?.sync_all()?;
    rename(&part, path)?;
    Ok(())
}

/// What the work directory proves: refused if it was another proof's.
fn claim_dir(
    dir: &Path,
    image: &str,
    input: &[u8],
    assumptions: &[Receipt],
    po2: Option<u32>,
) -> Result<()> {
    create_dir_all(dir)?;
    let mut what = format!(
        "image {image}\ninput {}\npo2 {}\n",
        hex::encode(Impl::hash_bytes(input).as_bytes()),
        po2.map_or("default".into(), |p| p.to_string())
    );
    // The receipts it assumes are part of what it proves; none adds no line,
    // so a directory of a proof without any is as it was.
    for receipt in assumptions {
        what.push_str(&format!(
            "assumes {}\n",
            hex::encode(Impl::hash_bytes(&receipt.journal.bytes).as_bytes())
        ));
    }
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
        let work = [
            "segment-",
            "lifted-",
            "joined",
            "resolved",
            "assumptions",
            "executed",
            "receipt",
        ];
        if work.iter().any(|w| name.starts_with(w)) {
            remove_file(&path)?;
        }
    }
    Ok(())
}

/// Execute `bin` on `input` in `dir`, every segment written there, then
/// what it made: refused unless its journal is `expected`.
fn execute(
    bin: &[u8],
    name: &str,
    input: &[u8],
    assumptions: &[Receipt],
    expected: &[u8],
    dir: &Path,
    po2: Option<u32>,
) -> Result<Executed> {
    let start = Instant::now();
    forget(dir)?;
    let session = ExecutorImpl::from_elf(super::env_with(input, po2, assumptions)?, bin)?
        .run_with_callback(|segment| -> Result<Box<dyn SegmentRef>> {
            let path = segment_path(dir, segment.index as usize);
            keep(&path, &bincode::serialize(&segment)?)?;
            Ok(Box::new(OnDisk(path)))
        })?;
    let journal = session.journal.clone().unwrap_or_default().bytes;
    ensure!(journal == expected, "journal");
    ensure!(
        session.mmr_assumptions.is_empty(),
        "{name} makes assumptions of a kind this prover does not resolve"
    );
    // Every assumption must have been proven by the host: one left
    // unresolved would stay in the receipt's claim.
    for (_, receipt) in &session.assumptions {
        proving(receipt)?;
    }
    keep(
        &assumptions_path(dir),
        &bincode::serialize(&session.assumptions)?,
    )?;
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

/// Prove `bin` on `input`, its journal `expected`, in segments of 2^`po2`
/// cycles, carrying on the work kept in `dir`: the verified succinct
/// receipt, also written to `dir/receipt`.
pub fn prove(
    bin: &[u8],
    name: &str,
    input: &[u8],
    expected: &[u8],
    dir: &Path,
    po2: Option<u32>,
) -> Result<Receipt> {
    prove_with(bin, name, input, &[], expected, dir, po2)
}

/// [`prove`] of a guest that verifies the receipts `assumptions` (of other
/// programs, each the succinct receipt of an unconditional proof): the
/// proof is unconditional once each is resolved, before it is wrapped.
pub fn prove_with(
    bin: &[u8],
    name: &str,
    input: &[u8],
    assumptions: &[Receipt],
    expected: &[u8],
    dir: &Path,
    po2: Option<u32>,
) -> Result<Receipt> {
    let id = compute_image_id(bin)?;
    claim_dir(dir, &id.to_string(), input, assumptions, po2)?;
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
        None => execute(bin, name, input, assumptions, expected, dir, po2)?,
    };
    ensure!(executed.journal == expected, "journal");
    let n = executed.segments;
    let made = kept_assumptions(dir)?;

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
        let mut receipt = prover.prove_segment(&ctx, &segment)?;
        drop(segment);
        // The last segment's claim carries the journal's digest and the
        // assumptions the guest made, which the recursion resolves one by
        // one: as `prove_session` merges them in, before the lift.
        if i + 1 == n && !made.is_empty() {
            let output = Output {
                journal: MaybePruned::Pruned(executed.journal.digest()),
                assumptions: made
                    .iter()
                    .map(|(assumption, _)| assumption.clone())
                    .collect::<Vec<_>>()
                    .into(),
            };
            // `merge_with` is the zkVM's own, and private to it: the output
            // replaces the segment's pruned one only if it is the same
            // output, by digest.
            let output: MaybePruned<Option<Output>> = Some(output).into();
            ensure!(
                output.digest() == receipt.claim.output.digest(),
                "the final segment's output is not the journal and assumptions the execution made"
            );
            receipt.claim.output = output;
        }
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

    // Each assumption resolved in the order the guest made them, the
    // running result kept after each: a succinct receipt per assumption, one
    // recursion proof each.
    let resolved_path = dir.join("resolved");
    let (mut done, mut acc): (usize, Lifted) = match read(&resolved_path) {
        Ok(bytes) => bincode::deserialize(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (0, acc),
        Err(e) => return Err(e.into()),
    };
    while done < made.len() {
        let t = Instant::now();
        acc = prover.resolve(&acc, &proving(&made[done].1)?)?;
        done += 1;
        keep(&resolved_path, &bincode::serialize(&(done, &acc))?)?;
        println!(
            "{name}: assumption {done}/{} resolved in {:.1} s ({})",
            made.len(),
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
        claim_dir(&work, "program", b"input", &[], Some(18)).unwrap();
        // The same proof carries on.
        claim_dir(&work, "program", b"input", &[], Some(18)).unwrap();
        for (image, input, po2) in [
            ("program", &b"input"[..], Some(19)),
            ("program", b"input", None),
            ("program", b"other", Some(18)),
            ("other", b"input", Some(18)),
        ] {
            let e = claim_dir(&work, image, input, &[], po2).unwrap_err();
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

    fn assumed(journal: &[u8]) -> Receipt {
        let claim = ReceiptClaim::ok(risc0_zkvm::sha::Digest::ZERO, journal.to_vec());
        Receipt::new(
            InnerReceipt::Fake(risc0_zkvm::FakeReceipt::new(claim)),
            journal.to_vec(),
        )
    }

    #[test]
    fn a_work_directory_is_the_proof_of_what_it_assumes() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let (a, b) = (assumed(b"one"), assumed(b"two"));
        claim_dir(
            &work,
            "program",
            b"input",
            std::slice::from_ref(&a),
            Some(18),
        )
        .unwrap();
        claim_dir(
            &work,
            "program",
            b"input",
            std::slice::from_ref(&a),
            Some(18),
        )
        .unwrap();
        // another receipt assumed, or none, or more, is another proof
        for other in [vec![b.clone()], vec![], vec![a.clone(), b]] {
            let e = claim_dir(&work, "program", b"input", &other, Some(18)).unwrap_err();
            assert!(format!("{e}").contains("another proof's work"), "{e}");
        }
    }

    #[test]
    fn a_directory_without_assumptions_is_of_a_guest_that_made_none() {
        let dir = tempfile::tempdir().unwrap();
        // work kept before this prover resolved any carries on as it did
        assert!(kept_assumptions(dir.path()).unwrap().is_empty());
        keep(
            &assumptions_path(dir.path()),
            &bincode::serialize(&Made::new()).unwrap(),
        )
        .unwrap();
        assert!(kept_assumptions(dir.path()).unwrap().is_empty());
        // and the assumptions and their resolution are the work forgotten with the rest
        keep(&dir.path().join("resolved"), b"resolved").unwrap();
        keep(&dir.path().join("proving"), b"image").unwrap();
        forget(dir.path()).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["proving"]);
    }

    #[test]
    fn an_assumption_is_resolved_by_a_succinct_receipt_alone() {
        use risc0_zkvm::Assumption;
        let claim = Assumption {
            claim: risc0_zkvm::sha::Digest::ZERO,
            control_root: risc0_zkvm::sha::Digest::ZERO,
        };
        let e = proving(&AssumptionReceipt::Unresolved(claim)).unwrap_err();
        assert!(format!("{e}").contains("no receipt was given"), "{e}");
        let claim = MaybePruned::<risc0_zkvm::Unknown>::Pruned(risc0_zkvm::sha::Digest::ZERO);
        let fake = InnerAssumptionReceipt::Fake(risc0_zkvm::FakeReceipt::new(claim));
        let e = proving(&AssumptionReceipt::Proven(fake)).unwrap_err();
        assert!(format!("{e}").contains("succinct receipt alone"), "{e}");
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
