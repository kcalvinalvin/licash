//! Proving a RISC Zero program where memory is short and work is cut off:
//! on a phone.
//!
//! A [`Job`] executes the program once, keeping its segments on disk, then
//! proves them one [`Job::step`] at a time, joining each into one succinct
//! receipt kept on disk too. A job cut off, or an app killed, resumes from
//! the last segment proven. Once all are, [`Job::receipt`] is the verified
//! succinct receipt, for an untrusted wrapper to compress to Groth16.
//!
//! In its directory a job keeps `job.json`, written once the program was
//! executed, `segments/` (those yet to prove) and `joined.bin`, the receipt
//! of the segments proven so far, replaced whole after each.
use std::{
    fs,
    io::{ErrorKind, Write},
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};

mod risc0;

/// The layout of a job on disk.
const VERSION: u32 = 1;
const META: &str = "job.json";
const JOINED: &str = "joined.bin";
const SEGMENTS: &str = "segments";

/// The segment sizes a job takes, as powers of two of cycles. Larger ones
/// prove faster but need more memory: about 1.4 GB at 16, 2.4 GB at 18.
pub const PO2: RangeInclusive<u32> = 13..=20;

/// A program executed, its segments proven in turn.
pub struct Job {
    dir: PathBuf,
    meta: Meta,
    proven: u32,
    backend: Box<dyn Backend>,
}

/// How far a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub proven: u32,
    pub segments: u32,
    /// The program's user cycles.
    pub cycles: u64,
}

/// What executing the program said: recorded once it was executed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    version: u32,
    /// In hex, as are the others.
    image_id: String,
    journal: String,
    po2: u32,
    segments: u32,
    cycles: u64,
}

/// The receipt of the first `proven` segments.
#[derive(Serialize, Deserialize)]
struct Joined {
    proven: u32,
    receipt: Vec<u8>,
}

/// What a job needs of a zkVM, receipts as opaque bytes: RISC Zero's, or
/// a stand-in testing a job's bookkeeping.
trait Backend: Send + Sync {
    /// Execute `program` on `input` in segments of 2^`po2` cycles, handing
    /// each to `segment` in turn.
    fn execute(
        &self,
        program: &[u8],
        input: &[u8],
        po2: u32,
        segment: &mut (dyn FnMut(Vec<u8>) -> Result<()> + Send),
    ) -> Result<Executed>;
    /// The succinct receipt of `segment`.
    fn prove(&self, segment: &[u8]) -> Result<Vec<u8>>;
    /// The succinct receipt of `left` then `right`.
    fn join(&self, left: &[u8], right: &[u8]) -> Result<Vec<u8>>;
    /// The receipt of the whole program, of `joined` every segment, once
    /// verified to say `journal` for `image_id`.
    fn receipt(&self, image_id: &[u8], journal: &[u8], joined: &[u8]) -> Result<Vec<u8>>;
}

struct Executed {
    image_id: Vec<u8>,
    journal: Vec<u8>,
    cycles: u64,
}

impl Job {
    /// Execute `program` on `input` in `dir`, in segments of 2^`po2`
    /// cycles, to prove them. Refused if a job is there already: proving
    /// takes long, and what was proven is never thrown away but by
    /// [`Job::discard`]. Segments of an execution cut off are made again.
    pub fn start(dir: &Path, program: &[u8], input: &[u8], po2: u32) -> Result<Self> {
        Self::start_with(Box::new(risc0::Risc0), dir, program, input, po2)
    }

    /// The job in `dir`, as far as it got: none if no program was executed
    /// to the end there.
    pub fn open(dir: &Path) -> Result<Option<Self>> {
        Self::open_with(Box::new(risc0::Risc0), dir)
    }

    fn start_with(
        backend: Box<dyn Backend>,
        dir: &Path,
        program: &[u8],
        input: &[u8],
        po2: u32,
    ) -> Result<Self> {
        ensure!(
            PO2.contains(&po2),
            "segments of 2^{po2} cycles: not in {PO2:?}"
        );
        ensure!(
            !dir.join(META).exists(),
            "a job is in {} already",
            dir.display()
        );
        // What a job discarded or cut off executing left.
        remove(&dir.join(JOINED))?;
        let segments = dir.join(SEGMENTS);
        if segments.exists() {
            fs::remove_dir_all(&segments)?;
        }
        fs::create_dir_all(&segments)?;
        let mut count = 0u32;
        let executed = backend.execute(program, input, po2, &mut |bytes| {
            write(&segment(&segments, count), &bytes)?;
            count += 1;
            Ok(())
        })?;
        ensure!(count > 0, "the program ran no segment");
        let meta = Meta {
            version: VERSION,
            image_id: hex::encode(executed.image_id),
            journal: hex::encode(executed.journal),
            po2,
            segments: count,
            cycles: executed.cycles,
        };
        write(&dir.join(META), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(Self {
            dir: dir.to_owned(),
            meta,
            proven: 0,
            backend,
        })
    }

    fn open_with(backend: Box<dyn Backend>, dir: &Path) -> Result<Option<Self>> {
        let Some(bytes) = read(&dir.join(META))? else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let Versioned { version } = serde_json::from_slice(&bytes).context("reading a job")?;
        ensure!(
            version == VERSION,
            "a job of layout {version}, not {VERSION}"
        );
        let meta: Meta = serde_json::from_slice(&bytes).context("reading a job")?;
        let mut job = Self {
            dir: dir.to_owned(),
            meta,
            proven: 0,
            backend,
        };
        job.proven = job.joined()?.map_or(0, |j| j.proven);
        ensure!(
            job.proven <= job.meta.segments,
            "a job of {} segments, {} proven",
            job.meta.segments,
            job.proven
        );
        // Those proven before the job was cut off, before being removed.
        for i in 0..job.proven {
            remove(&segment(&dir.join(SEGMENTS), i))?;
        }
        Ok(Some(job))
    }

    pub fn status(&self) -> Status {
        Status {
            proven: self.proven,
            segments: self.meta.segments,
            cycles: self.meta.cycles,
        }
    }

    /// What the program committed to, as its receipt says once proven.
    pub fn journal(&self) -> Vec<u8> {
        hex::decode(&self.meta.journal).unwrap_or_default()
    }

    /// The image id of the program, in hex.
    pub fn image_id(&self) -> &str {
        &self.meta.image_id
    }

    /// Prove the next segment and join it to those before, on disk before
    /// returning; refused once all are. A step failing changes nothing.
    pub fn step(&mut self) -> Result<()> {
        ensure!(
            self.proven < self.meta.segments,
            "all {} segments are proven",
            self.meta.segments
        );
        let path = segment(&self.dir.join(SEGMENTS), self.proven);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let lifted = self.backend.prove(&bytes)?;
        drop(bytes);
        let receipt = match self.joined()? {
            None => lifted,
            Some(before) => self.backend.join(&before.receipt, &lifted)?,
        };
        let proven = self.proven + 1;
        let joined = Joined { proven, receipt };
        write(&self.dir.join(JOINED), &bincode::serialize(&joined)?)?;
        self.proven = proven;
        remove(&path)
    }

    /// The program's succinct receipt, serialized, once every segment is
    /// proven: verified to say the journal for the image id.
    pub fn receipt(&self) -> Result<Vec<u8>> {
        ensure!(
            self.proven == self.meta.segments,
            "{} of {} segments proven",
            self.proven,
            self.meta.segments
        );
        let joined = self.joined()?.context("no segment proven")?;
        let image_id = hex::decode(&self.meta.image_id)?;
        self.backend
            .receipt(&image_id, &self.journal(), &joined.receipt)
    }

    /// Remove the job, what it proved included: what else is in its
    /// directory stays.
    pub fn discard(self) -> Result<()> {
        // First what makes it a job, so that one cut off is none.
        remove(&self.dir.join(META))?;
        remove(&self.dir.join(JOINED))?;
        match fs::remove_dir_all(self.dir.join(SEGMENTS)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The receipt of the segments proven so far, if any.
    fn joined(&self) -> Result<Option<Joined>> {
        let Some(bytes) = read(&self.dir.join(JOINED))? else {
            return Ok(None);
        };
        let joined: Joined = bincode::deserialize(&bytes).context("reading the receipt joined")?;
        Ok(Some(joined))
    }
}

fn segment(dir: &Path, i: u32) -> PathBuf {
    dir.join(format!("{i}.bin"))
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("reading {}", path.display()))),
    }
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Write `bytes` to `path` whole or not at all, and durably: through a
/// file beside it, synced, then renamed over it.
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let partial = path.with_extension("partial");
    let mut file = fs::File::create(&partial)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&partial, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    };

    use super::*;

    /// A stand-in zkVM: segments are `po2` bytes of the input each, a
    /// segment's receipt is it in brackets, joining concatenates.
    #[derive(Clone, Default)]
    struct Fake {
        proved: Arc<AtomicU32>,
        /// The step whose proof fails, once.
        failing: Arc<Mutex<Option<u32>>>,
    }

    impl Backend for Fake {
        fn execute(
            &self,
            program: &[u8],
            input: &[u8],
            po2: u32,
            segment: &mut (dyn FnMut(Vec<u8>) -> Result<()> + Send),
        ) -> Result<Executed> {
            for chunk in input.chunks(po2 as usize) {
                segment(chunk.to_vec())?;
            }
            Ok(Executed {
                image_id: program.to_vec(),
                journal: (input.len() as u32).to_le_bytes().to_vec(),
                cycles: 1000 * input.len() as u64,
            })
        }

        fn prove(&self, segment: &[u8]) -> Result<Vec<u8>> {
            let n = self.proved.load(Ordering::SeqCst);
            let mut failing = self.failing.lock().unwrap();
            if *failing == Some(n) {
                *failing = None;
                anyhow::bail!("out of memory");
            }
            self.proved.fetch_add(1, Ordering::SeqCst);
            Ok([b"(", segment, b")"].concat())
        }

        fn join(&self, left: &[u8], right: &[u8]) -> Result<Vec<u8>> {
            Ok([left, right].concat())
        }

        fn receipt(&self, image_id: &[u8], journal: &[u8], joined: &[u8]) -> Result<Vec<u8>> {
            Ok([image_id, b"|", journal, b"|", joined].concat())
        }
    }

    const PROGRAM: &[u8] = b"program";
    /// 40 bytes: four segments of 13, 13, 13 and 1.
    const INPUT: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCD";

    fn proven() -> Vec<u8> {
        let joined: Vec<u8> = INPUT
            .chunks(13)
            .flat_map(|c| [b"(", c, b")"].concat())
            .collect();
        [PROGRAM, b"|", &40u32.to_le_bytes(), b"|", &joined].concat()
    }

    fn start(fake: &Fake, dir: &Path) -> Job {
        Job::start_with(Box::new(fake.clone()), dir, PROGRAM, INPUT, 13).unwrap()
    }

    fn open(fake: &Fake, dir: &Path) -> Option<Job> {
        Job::open_with(Box::new(fake.clone()), dir).unwrap()
    }

    fn left(dir: &Path) -> usize {
        fs::read_dir(dir.join(SEGMENTS)).unwrap().count()
    }

    #[test]
    fn a_job_proves_its_segments_in_turn_and_removes_each_once_proven() {
        let (dir, fake) = (tempfile::tempdir().unwrap(), Fake::default());
        let mut job = start(&fake, dir.path());
        let status = Status {
            proven: 0,
            segments: 4,
            cycles: 40_000,
        };
        assert_eq!(job.status(), status);
        assert_eq!(job.journal(), 40u32.to_le_bytes());
        assert_eq!(job.image_id(), hex::encode(PROGRAM));
        assert!(job.receipt().unwrap_err().to_string().contains("0 of 4"));
        for proven in 1..=4 {
            job.step().unwrap();
            assert_eq!(job.status().proven, proven);
            assert_eq!(left(dir.path()), 4 - proven as usize);
        }
        assert_eq!(job.receipt().unwrap(), proven());
        assert!(job.step().unwrap_err().to_string().contains("all 4"));
        assert_eq!(fake.proved.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_job_cut_off_resumes_after_the_last_segment_proven() {
        let (dir, fake) = (tempfile::tempdir().unwrap(), Fake::default());
        let mut job = start(&fake, dir.path());
        job.step().unwrap();
        job.step().unwrap();
        drop(job);
        let mut job = open(&fake, dir.path()).expect("a job");
        assert_eq!((job.status().proven, job.status().segments), (2, 4));
        job.step().unwrap();
        // Cut off after its receipt was recorded, before its segment was
        // removed: not proven again.
        let kept = dir.path().join(SEGMENTS).join("3.bin");
        let last = fs::read(&kept).unwrap();
        job.step().unwrap();
        fs::write(&kept, &last).unwrap();
        drop(job);
        let job = open(&fake, dir.path()).expect("a job");
        assert_eq!(job.status().proven, 4);
        assert_eq!(left(dir.path()), 0);
        assert_eq!(job.receipt().unwrap(), proven());
        assert_eq!(fake.proved.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_step_failing_changes_nothing() {
        let (dir, fake) = (tempfile::tempdir().unwrap(), Fake::default());
        let mut job = start(&fake, dir.path());
        job.step().unwrap();
        *fake.failing.lock().unwrap() = Some(1);
        assert!(job.step().unwrap_err().to_string().contains("memory"));
        assert_eq!(job.status().proven, 1);
        assert_eq!(open(&fake, dir.path()).unwrap().status().proven, 1);
        while job.status().proven < 4 {
            job.step().unwrap();
        }
        assert_eq!(job.receipt().unwrap(), proven());
    }

    #[test]
    fn a_job_is_never_started_over_one_there_and_a_cut_off_execution_is_none() {
        let (dir, fake) = (tempfile::tempdir().unwrap(), Fake::default());
        assert!(open(&fake, dir.path()).is_none());
        for po2 in [12, 21] {
            let refused = Job::start_with(Box::new(fake.clone()), dir.path(), PROGRAM, INPUT, po2);
            assert!(refused.is_err(), "{po2}");
        }
        // An execution cut off: segments, no record of it. Started over.
        fs::create_dir_all(dir.path().join(SEGMENTS)).unwrap();
        fs::write(dir.path().join(SEGMENTS).join("7.bin"), b"stale").unwrap();
        fs::write(dir.path().join(JOINED), b"stale").unwrap();
        assert!(open(&fake, dir.path()).is_none());
        let mut job = start(&fake, dir.path());
        assert_eq!(left(dir.path()), 4);
        job.step().unwrap();
        let again = Job::start_with(Box::new(fake.clone()), dir.path(), PROGRAM, INPUT, 13);
        assert!(again.err().unwrap().to_string().contains("already"));
        assert_eq!(open(&fake, dir.path()).unwrap().status().proven, 1);

        // A layout it does not know: refused, and left as it is.
        let meta = fs::read(dir.path().join(META)).unwrap();
        let newer = String::from_utf8(meta.clone())
            .unwrap()
            .replace("\"version\": 1", "\"version\": 2");
        fs::write(dir.path().join(META), &newer).unwrap();
        let refused = Job::open_with(Box::new(fake.clone()), dir.path());
        assert!(refused.err().unwrap().to_string().contains("layout 2"));
        assert_eq!(fs::read(dir.path().join(META)).unwrap(), newer.as_bytes());
        fs::write(dir.path().join(META), &meta).unwrap();

        // Discarded: what it kept goes, what else is there stays.
        fs::write(dir.path().join("other"), b"mine").unwrap();
        job.discard().unwrap();
        let mut there: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        there.sort();
        assert_eq!(there, ["other"]);
        assert!(open(&fake, dir.path()).is_none());
    }
}
