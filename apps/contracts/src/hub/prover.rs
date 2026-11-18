//! The hub's prover of its statement: `licash-r0` (`LICASH_HP_PROVER`
//! names another), one proof at a time, on the GPU if the container has
//! one, else on the CPU (r0/host).
//!
//! The hub commits to a proof when it responds to a claim: its assertion
//! must confirm before her `respond_timeout`, `w_a` blocks after the
//! response. So it responds only to what its prover can still prove in
//! time ([`Prover::admit`]): a claim it withholds a response from is paid
//! what she asks by her timeout, as a response it can't assert in time
//! would be, less the fees, and without delaying the proofs it did commit
//! to. Contested at once by many holders, it spends its prover on the
//! largest stakes.
//!
//! Its speed is measured, not assumed: at startup the prover is probed
//! (its device, its Groth16 setup, its memory) and, once per device,
//! benchmarked (`licash-r0 bench`, kept in `hp-proofs/bench.json`). Until
//! then, or if anything is missing, the hub is not ready to prove, and
//! responds to no claim: its operator is alerted.
//!
//! Each proof's work is in `hp-proofs/<id>/`: its input, frozen once it
//! first holds (the journal depends on Base alone, so a proof of it holds
//! for the contract whatever blocks followed), the prover's own work,
//! carried on across restarts, its log, and `proof.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use licash::spend::ChainProfile;

/// Cycles the statement takes for each header of an input, and for each
/// other byte (its filing, evidence and transactions): measured with
/// `licash-r0 hp-exec` on the statement's test inputs, 15 headers and
/// 33.6 kB (67.7 M cycles), then 3,000 headers more (151.6 M).
pub(super) const HEADER_CYCLES: u64 = 28_000;
pub(super) const BYTE_CYCLES: u64 = 2_000;

/// What the benchmark may be off by on a real proof.
const MARGIN: f64 = 1.3;

/// Not seen in this many polls, a contract's proof is no longer needed:
/// its assertion confirmed, or her timeout paid her. A poll that failed
/// for a contract is no reason to drop its proof.
const FORGET_POLLS: u64 = 10;

/// After a prover failure, or a failed probe, how long until it is tried
/// again.
const RETRY: Duration = Duration::from_secs(60);
const REPROBE: Duration = Duration::from_secs(15 * 60);

use super::Alert;

/// The prover's speed, as benchmarked: the cycles a segment holds, the
/// seconds a segment takes (proved, lifted and joined), and those of the
/// wrap every proof ends with.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Speed {
    pub device: String,
    pub segment_cycles: u64,
    pub segment: f64,
    pub tail: f64,
}

impl Speed {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            device: v["device"].as_str()?.to_owned(),
            segment_cycles: v["segment_cycles"].as_u64().filter(|c| *c > 0)?,
            segment: v["segment"].as_f64()?,
            tail: v["tail"].as_f64()?,
        })
    }

    /// The seconds a proof of `cycles` takes, with the margin.
    pub fn seconds(&self, cycles: u64) -> f64 {
        let segments = cycles.div_ceil(self.segment_cycles) as f64;
        MARGIN * (segments * self.segment + self.tail)
    }
}

/// Whether the hub may respond to a claim ([`Prover::admit`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Admission {
    Admit,
    /// Not yet: the prover is being probed, or the claim was first seen on
    /// this poll.
    Wait,
    /// Not: why.
    Withhold(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Readiness {
    Ready(Speed),
    NotReady(String),
}

fn not_ready(why: &str) -> String {
    format!(
        "the hub's prover is not ready, so it responds to no contested claim (her timeout \
         pays her what she claims): {why}"
    )
}

/// The cycles of a statement's input of `headers` headers and `other`
/// bytes besides.
pub(super) fn cycles(headers: u64, other: u64) -> u64 {
    HEADER_CYCLES * headers + BYTE_CYCLES * other
}

/// The cycles of the statement's input `bytes` (`HubInput::to_bytes`):
/// Base, then the exit's headers.
pub(super) fn input_cycles(bytes: &[u8]) -> u64 {
    let word = |at: usize| {
        bytes.get(at..at + 4).map_or(0, |w| {
            u32::from_be_bytes(w.try_into().unwrap_or_default()) as usize
        })
    };
    let headers = word(4 + word(0)) as u64;
    let other = (bytes.len() as u64).saturating_sub(80 * headers);
    cycles(headers, other)
}

/// The target time between blocks of `profile`, in seconds.
pub(super) fn spacing(profile: ChainProfile) -> f64 {
    match profile {
        ChainProfile::Mutinynet => 30.0,
        _ => 600.0,
    }
}

/// A proof to plan: the last tip at which it must exist (its assertion
/// broadcast then confirms in time), the height from which it can be made
/// (its response and disclosure buried), and its seconds left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Work {
    pub deadline: u32,
    pub release: u32,
    pub seconds: f64,
}

/// Which of `jobs` the one prover makes in time, made earliest deadline
/// first (then by id) from `tip`, a block every `spacing` seconds: a job
/// that would finish late is left out, and takes no time from those after.
pub(super) fn plan(jobs: &[([u8; 32], Work)], tip: u32, spacing: f64) -> HashMap<[u8; 32], bool> {
    let mut order: Vec<_> = jobs.iter().collect();
    order.sort_by_key(|(id, w)| (w.deadline, *id));
    let mut t = 0.0;
    let mut fits = HashMap::new();
    for (id, w) in order {
        let start = f64::max(t, f64::from(w.release.saturating_sub(tip)) * spacing);
        let end = start + w.seconds;
        let by = f64::from((w.deadline + 1).saturating_sub(tip)) * spacing;
        let fit = w.deadline >= tip && end <= by;
        if fit {
            t = end;
        }
        fits.insert(*id, fit);
    }
    fits
}

/// A proof the hub committed to: its response is on chain.
#[derive(Clone, Debug)]
struct Job {
    program: [u8; 32],
    deadline: u32,
    release: u32,
    cycles: u64,
    /// Its input, once frozen: until then it can't be made.
    frozen: bool,
    proven: bool,
    seen: u64,
    /// While failing: when to try again.
    retry: Option<Instant>,
}

/// A claim the hub would respond to, if its prover has room: what
/// responding saves it, and its proof's cycles.
#[derive(Clone, Debug)]
struct Candidate {
    stake: u64,
    cycles: u64,
    first: u64,
    seen: u64,
    deadline: u32,
    release: u32,
}

#[derive(Default)]
struct State {
    readiness: Option<Readiness>,
    jobs: HashMap<[u8; 32], Job>,
    candidates: HashMap<[u8; 32], Candidate>,
    /// The job being proved, since when.
    running: Option<([u8; 32], Instant)>,
    /// Set to stop the running job: no longer needed.
    cancel: bool,
    /// Polls of every contract completed.
    polls: u64,
    tip: u32,
}

struct Inner {
    dir: PathBuf,
    command: String,
    program: [u8; 32],
    spacing: f64,
    state: Mutex<State>,
    wake: Condvar,
    alert: OnceLock<Arc<Alert>>,
}

/// The hub's one prover: see the module.
pub(super) struct Prover(Arc<Inner>);

impl Prover {
    /// The prover of `data`'s contracts, which open under `program` on a
    /// chain of a block every `spacing` seconds: probed and benchmarked,
    /// then proving, on a thread of its own.
    pub fn start(data: &Path, program: [u8; 32], spacing: f64) -> Self {
        let command = std::env::var("LICASH_HP_PROVER").unwrap_or_else(|_| "licash-r0".into());
        Self::start_with(data, command, program, spacing)
    }

    fn start_with(data: &Path, command: String, program: [u8; 32], spacing: f64) -> Self {
        let inner = Arc::new(Inner {
            dir: data.join("hp-proofs"),
            command,
            program,
            spacing,
            state: Mutex::default(),
            wake: Condvar::new(),
            alert: OnceLock::new(),
        });
        let worker = inner.clone();
        std::thread::Builder::new()
            .name("hub prover".into())
            .spawn(move || worker.run())
            .expect("a thread for the prover");
        Self(inner)
    }

    /// Tell the operator through `f` from now on: the prover's not ready,
    /// fails, or has no room.
    pub fn alert_with(&self, f: Arc<Alert>) {
        let _ = self.0.alert.set(f);
        // Found not ready before: told now.
        let readiness = self.0.state().readiness.clone();
        if let Some(Readiness::NotReady(why)) = readiness {
            self.0.alert(&not_ready(&why));
        }
    }

    pub fn alert(&self, message: &str) {
        self.0.alert(message);
    }

    /// How many proofs of the smallest contract's statement (the
    /// benchmark's input, 15 headers) the prover makes in `blocks`: what
    /// bounds the exits it can contest at once. None until it is ready.
    pub fn defendable(&self, blocks: u32) -> Option<u64> {
        let Some(Readiness::Ready(speed)) = &self.0.state().readiness else {
            return None;
        };
        let seconds = speed.seconds(cycles(15, 33_600));
        Some((f64::from(blocks) * self.0.spacing / seconds) as u64)
    }

    /// The work directory of `id`'s proof.
    pub fn dir(&self, id: &[u8; 32]) -> PathBuf {
        self.0.dir.join(crate::hex_id(id))
    }

    /// The input of `id`'s proof, frozen as `input` unless it was before:
    /// its bytes.
    pub fn freeze(&self, id: &[u8; 32], input: &[u8]) -> Result<Vec<u8>> {
        let dir = self.dir(id);
        let path = dir.join("input.bin");
        if let Ok(frozen) = std::fs::read(&path) {
            return Ok(frozen);
        }
        std::fs::create_dir_all(&dir)?;
        let part = dir.join("input.part");
        std::fs::write(&part, input)?;
        std::fs::File::open(&part)?.sync_all()?;
        std::fs::rename(&part, &path)?;
        Ok(input.to_vec())
    }

    /// The frozen input of `id`'s proof, if it is.
    pub fn frozen(&self, id: &[u8; 32]) -> Option<Vec<u8>> {
        std::fs::read(self.dir(id).join("input.bin")).ok()
    }

    /// The seal of `id`'s proof, once made: its `proof.json`, set aside
    /// (and made again) if it isn't of `program`.
    pub fn proof(&self, id: &[u8; 32], program: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let out = self.dir(id).join("proof.json");
        let bytes = match std::fs::read(&out) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let read = || -> Result<Vec<u8>> {
            let json: serde_json::Value = serde_json::from_slice(&bytes)?;
            let image = json["image_id"].as_str().context("no image id")?;
            anyhow::ensure!(image == hex::encode(program), "a proof of program {image}");
            Ok(hex::decode(json["seal"].as_str().context("no seal")?)?)
        };
        match read() {
            Ok(seal) => Ok(Some(seal)),
            Err(e) => {
                self.reject(id, &format!("{e:#}"))?;
                Ok(None)
            }
        }
    }

    /// Set aside `id`'s `proof.json`, which does not verify, to make it
    /// again.
    pub fn reject(&self, id: &[u8; 32], why: &str) -> Result<()> {
        let dir = self.dir(id);
        std::fs::rename(dir.join("proof.json"), dir.join("proof.rejected"))?;
        self.alert(&format!(
            "contract {}: the prover's proof was rejected ({why}): proving again",
            crate::hex_id(id)
        ));
        if let Some(job) = self.0.state().jobs.get_mut(id) {
            job.proven = false;
        }
        self.0.wake.notify_all();
        Ok(())
    }

    /// The hub responded to `id`'s claim: its proof is to be made before
    /// `deadline`, from `release`, of `cycles` (its frozen input's, once it
    /// is). Called on each poll while it is needed: whether the plan makes
    /// it in time (always, once made or while the prover is not ready).
    #[allow(clippy::too_many_arguments)]
    pub fn commit(
        &self,
        id: &[u8; 32],
        program: [u8; 32],
        deadline: u32,
        release: u32,
        cycles: u64,
        frozen: bool,
        proven: bool,
        tip: u32,
    ) -> bool {
        let mut s = self.0.state();
        s.tip = s.tip.max(tip);
        let seen = s.polls;
        s.candidates.remove(id);
        let job = s.jobs.entry(*id).or_insert(Job {
            program,
            deadline,
            release,
            cycles,
            frozen,
            proven,
            seen,
            retry: None,
        });
        job.deadline = deadline;
        job.release = release;
        job.cycles = cycles;
        job.frozen = frozen;
        job.proven = proven;
        job.seen = seen;
        let fits = match &s.readiness {
            Some(Readiness::Ready(speed)) if !proven => {
                plan(&s.committed(speed), s.tip, self.0.spacing)[id]
            }
            _ => true,
        };
        drop(s);
        self.0.wake.notify_all();
        fits
    }

    /// Whether the hub may respond to `id`'s claim, which saves it `stake`
    /// sat if proved, and whose proof of `cycles` it would make before
    /// `deadline`, from `release`: its prover is ready, the claim was seen
    /// on a poll before this one (all the proofs committed to are known
    /// then, and its stake against the others'), and its proof fits with
    /// those committed to and those of larger stakes, none of which it
    /// would make late. Why not, if not.
    pub fn admit(
        &self,
        id: &[u8; 32],
        stake: u64,
        cycles: u64,
        deadline: u32,
        release: u32,
        tip: u32,
    ) -> Admission {
        let mut s = self.0.state();
        s.tip = s.tip.max(tip);
        let polls = s.polls;
        let first = s.candidates.get(id).map_or(polls, |c| c.first);
        s.candidates.insert(
            *id,
            Candidate {
                stake,
                cycles,
                first,
                seen: polls,
                deadline,
                release,
            },
        );
        let speed = match &s.readiness {
            Some(Readiness::Ready(speed)) => speed.clone(),
            Some(Readiness::NotReady(why)) => {
                return Admission::Withhold(format!("the prover is not ready: {why}"))
            }
            _ => return Admission::Wait,
        };
        if first == polls {
            return Admission::Wait;
        }
        let mut before = s.committed(&speed);
        let larger = s.candidates.iter().filter(|(other, o)| {
            *other != id && o.seen + 1 >= polls && (o.stake, *id) > (stake, **other)
        });
        let larger: Vec<_> = larger
            .map(|(other, o)| {
                let work = Work {
                    deadline: o.deadline,
                    release: o.release,
                    seconds: speed.seconds(o.cycles),
                };
                (*other, work)
            })
            .collect();
        before.extend(larger);
        let fitted = plan(&before, s.tip, self.0.spacing);
        let mut with = before;
        let work = Work {
            deadline,
            release,
            seconds: speed.seconds(cycles),
        };
        with.push((*id, work));
        let fits = plan(&with, s.tip, self.0.spacing);
        if !fits[id] {
            return Admission::Withhold(format!(
                "its proof (about {:.0} min) would not be made in time",
                work.seconds / 60.0
            ));
        }
        if fitted.iter().any(|(other, fit)| *fit && !fits[other]) {
            return Admission::Withhold(
                "its proof would delay one committed to, or of a larger stake".into(),
            );
        }
        Admission::Admit
    }

    /// A poll of every contract is over: forget the proofs and claims no
    /// longer seen, and stop proving one of them.
    pub fn polled(&self) {
        let mut s = self.0.state();
        s.polls += 1;
        let polls = s.polls;
        let live = |seen: u64| seen + FORGET_POLLS >= polls;
        s.jobs.retain(|_, j| live(j.seen));
        s.candidates.retain(|_, c| live(c.seen));
        if let Some((id, _)) = s.running {
            if !s.jobs.contains_key(&id) {
                s.cancel = true;
            }
        }
    }
}

impl State {
    /// The proofs committed to and not made, with their seconds left.
    fn committed(&self, speed: &Speed) -> Vec<([u8; 32], Work)> {
        self.jobs
            .iter()
            .filter(|(_, j)| !j.proven)
            .map(|(id, j)| {
                let mut seconds = speed.seconds(j.cycles);
                if let Some((running, since)) = self.running {
                    if running == *id {
                        seconds = (seconds - since.elapsed().as_secs_f64()).max(0.0);
                    }
                }
                let work = Work {
                    deadline: j.deadline,
                    release: j.release,
                    seconds,
                };
                (*id, work)
            })
            .collect()
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn alert(&self, message: &str) {
        match self.alert.get() {
            Some(alert) => alert(message),
            None => tracing::error!("alert: {message}"),
        }
    }

    fn set_readiness(&self, readiness: Readiness) {
        let before = self.state().readiness.replace(readiness.clone());
        match &readiness {
            Readiness::Ready(speed) => tracing::info!(
                device = %speed.device,
                segment_cycles = speed.segment_cycles,
                segment_s = speed.segment,
                tail_s = speed.tail,
                "the hub's prover is ready"
            ),
            Readiness::NotReady(why) if before != Some(readiness.clone()) => {
                self.alert(&not_ready(why))
            }
            _ => {}
        }
        self.wake.notify_all();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.stdin(Stdio::null());
        cmd
    }

    /// Probe, then benchmark unless measured on this device before.
    fn ready(&self) -> Result<Speed> {
        let out = self
            .command()
            .arg("probe")
            .output()
            .with_context(|| format!("no prover: {} doesn't run", self.command))?;
        if !out.status.success() {
            bail!(
                "{} probe: {}",
                self.command,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let probe: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        let mut problems: Vec<String> = probe["problems"]
            .as_array()
            .map(|p| {
                p.iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let program = hex::encode(self.program);
        let programs = probe["programs"].as_array();
        if !programs.is_some_and(|p| p.iter().any(|p| p.as_str() == Some(&program))) {
            problems.push(format!(
                "the program contracts open under, {program}, is not installed"
            ));
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        let device = probe["device"].as_str().unwrap_or("cpu").to_owned();
        if device == "cpu" {
            tracing::warn!("the hub's prover has no GPU: it proves on the CPU, hours a proof");
        }
        std::fs::create_dir_all(&self.dir)?;
        let kept = self.dir.join("bench.json");
        let measured = std::fs::read(&kept)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .and_then(|v| Speed::from_json(&v));
        if let Some(speed) = measured.filter(|s| s.device == device) {
            return Ok(speed);
        }
        tracing::warn!(%device, "benchmarking the hub's prover: minutes");
        let log = std::fs::File::create(self.dir.join("bench.log"))?;
        let status = self
            .command()
            .arg("bench")
            .arg(&kept)
            .arg(&program)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()?;
        if !status.success() {
            bail!("its benchmark failed ({status}): see hp-proofs/bench.log");
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&kept)?)?;
        Speed::from_json(&v).context("its benchmark's report")
    }

    fn run(self: Arc<Self>) {
        loop {
            match self.ready() {
                Ok(speed) => {
                    self.set_readiness(Readiness::Ready(speed));
                    self.prove_all();
                }
                Err(e) => {
                    self.set_readiness(Readiness::NotReady(format!("{e:#}")));
                    std::thread::sleep(REPROBE);
                }
            }
        }
    }

    /// The next proof to make: the first that fits the plan, else the
    /// earliest still live. None if there is none to make now.
    fn next(&self, s: &State) -> Option<([u8; 32], [u8; 32])> {
        let Some(Readiness::Ready(speed)) = &s.readiness else {
            return None;
        };
        let now = Instant::now();
        let runnable = |j: &Job| {
            j.frozen && !j.proven && j.deadline >= s.tip && j.retry.is_none_or(|r| r <= now)
        };
        let jobs: Vec<_> = s
            .committed(speed)
            .into_iter()
            .filter(|(id, _)| runnable(&s.jobs[id]))
            .collect();
        let fits = plan(&jobs, s.tip, self.spacing);
        let mut order: Vec<_> = jobs
            .iter()
            .map(|(id, w)| (!fits[id], w.deadline, *id))
            .collect();
        order.sort();
        let (_, _, id) = order.first()?;
        Some((*id, s.jobs[id].program))
    }

    /// Make the proofs committed to, one at a time, for as long as the
    /// hub runs.
    fn prove_all(&self) {
        loop {
            let (id, program) = {
                let mut s = self.state();
                loop {
                    if let Some(next) = self.next(&s) {
                        break next;
                    }
                    s = self
                        .wake
                        .wait_timeout(s, RETRY)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            };
            let ok = self.prove(&id, &program);
            let mut s = self.state();
            s.running = None;
            s.cancel = false;
            if let Some(job) = s.jobs.get_mut(&id) {
                match ok {
                    Ok(true) => {
                        job.proven = true;
                        job.retry = None;
                    }
                    // Stopped: no longer needed.
                    Ok(false) => {}
                    Err(e) => {
                        let first = job.retry.is_none();
                        job.retry = Some(Instant::now() + RETRY);
                        drop(s);
                        let message = format!(
                            "contract {}: the prover failed, trying again (see hp-proofs/{}/prover.log): {e:#}",
                            crate::hex_id(&id),
                            crate::hex_id(&id)
                        );
                        match first {
                            true => self.alert(&message),
                            false => tracing::error!("{message}"),
                        }
                    }
                }
            }
        }
    }

    /// Prove `id`'s frozen input with `program`: whether it was proved, or
    /// stopped as no longer needed.
    fn prove(&self, id: &[u8; 32], program: &[u8; 32]) -> Result<bool> {
        let dir = self.dir.join(crate::hex_id(id));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("prover.log"))?;
        let mut child = self
            .command()
            .arg("hp-prove")
            .arg(dir.join("input.bin"))
            .arg(&dir)
            .arg(hex::encode(program))
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("run {}", self.command))?;
        let started = Instant::now();
        self.state().running = Some((*id, started));
        tracing::warn!(id = %crate::hex_id(id), "proving the hub's statement");
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("{status}");
                }
                anyhow::ensure!(dir.join("proof.json").exists(), "it left no proof");
                tracing::info!(
                    id = %crate::hex_id(id),
                    seconds = started.elapsed().as_secs(),
                    "the hub's statement proved"
                );
                return Ok(true);
            }
            if self.state().cancel {
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!(id = %crate::hex_id(id), "a proof no longer needed: stopped");
                return Ok(false);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(deadline: u32, release: u32, seconds: f64) -> Work {
        Work {
            deadline,
            release,
            seconds,
        }
    }

    #[test]
    fn the_plan_makes_proofs_earliest_deadline_first_and_leaves_out_those_late() {
        // A block every 600 s from tip 100.
        let jobs = [
            ([1; 32], work(110, 100, 3_000.0)),
            ([2; 32], work(105, 100, 3_000.0)),
            // Late whatever: left out, taking no time from the next.
            ([3; 32], work(106, 100, 9_000.0)),
            ([4; 32], work(112, 100, 1_200.0)),
        ];
        let fits = plan(&jobs, 100, 600.0);
        // [2] by 3,000 s of 3,600; [1] by 6,000 of 6,600; [4] by 7,200 of 7,800.
        assert_eq!(
            [
                fits[&[1; 32]],
                fits[&[2; 32]],
                fits[&[3; 32]],
                fits[&[4; 32]]
            ],
            [true, true, false, true]
        );
        // One can't start before its release; one past its deadline is late.
        let fits = plan(
            &[
                ([5; 32], work(110, 106, 3_100.0)),
                ([6; 32], work(99, 90, 1.0)),
            ],
            100,
            600.0,
        );
        assert_eq!((fits[&[5; 32]], fits[&[6; 32]]), (false, false));
        let fits = plan(&[([5; 32], work(110, 105, 3_000.0))], 100, 600.0);
        assert!(fits[&[5; 32]]);
    }

    #[test]
    fn a_proofs_seconds_are_its_segments_and_the_wrap_with_the_margin() {
        let speed = Speed {
            device: "cuda".into(),
            segment_cycles: 250_000,
            segment: 3.0,
            tail: 100.0,
        };
        assert_eq!(speed.seconds(500_001), MARGIN * (3.0 * 3.0 + 100.0));
        // The statement's test input: 67.7 M cycles measured.
        let measured: f64 = 67_652_388.0;
        let estimated = cycles(15, 34_780 - 1_200) as f64;
        assert!((estimated / measured - 1.0).abs() < 0.01, "{estimated}");
        // With 3,000 headers more: 151.6 M.
        let estimated = cycles(3_015, 274_776 - 3_015 * 80) as f64;
        assert!(
            (estimated / 151_639_658.0 - 1.0).abs() < 0.01,
            "{estimated}"
        );
    }

    #[test]
    fn an_inputs_cycles_count_its_headers() {
        let mut bytes = Vec::new();
        bytes.extend(3u32.to_be_bytes());
        bytes.extend([7; 3]);
        bytes.extend(2u32.to_be_bytes());
        bytes.extend([0; 160]);
        bytes.extend([1; 10]);
        let other = bytes.len() as u64 - 160;
        assert_eq!(input_cycles(&bytes), cycles(2, other));
        assert_eq!(input_cycles(&[]), 0);
    }

    fn ready(dir: &Path, speed: Speed) -> Prover {
        let inner = Arc::new(Inner {
            dir: dir.to_owned(),
            command: "false".into(),
            program: [0; 32],
            spacing: 600.0,
            state: Mutex::default(),
            wake: Condvar::new(),
            alert: OnceLock::new(),
        });
        inner.state().readiness = Some(Readiness::Ready(speed));
        Prover(inner)
    }

    /// A proof of 1 M cycles takes 1,300 s with the margin; one of 2 M,
    /// 2,600. From tip 100, a deadline of 110 leaves 6,600 s.
    #[test]
    fn admission_keeps_the_proofs_committed_to_and_prefers_larger_stakes() {
        let dir = tempfile::tempdir().unwrap();
        let speed = Speed {
            device: "cuda".into(),
            segment_cycles: 1_000_000,
            segment: 1_000.0,
            tail: 0.0,
        };
        let p = ready(dir.path(), speed);
        // Not before a poll saw it.
        assert_eq!(
            p.admit(&[1; 32], 10, 2_000_000, 110, 100, 100),
            Admission::Wait
        );
        p.polled();
        assert_eq!(
            p.admit(&[1; 32], 10, 2_000_000, 110, 100, 100),
            Admission::Admit
        );
        p.commit(&[1; 32], [0; 32], 110, 100, 2_000_000, true, false, 100);
        // Two more claims, one of a larger stake: room for one of them.
        let _ = p.admit(&[2; 32], 5, 2_000_000, 110, 100, 100);
        let _ = p.admit(&[3; 32], 50, 2_000_000, 110, 100, 100);
        p.polled();
        let small = p.admit(&[2; 32], 5, 2_000_000, 110, 100, 100);
        assert!(
            matches!(&small, Admission::Withhold(w) if w.contains("larger stake")),
            "{small:?}"
        );
        assert_eq!(
            p.admit(&[3; 32], 50, 2_000_000, 110, 100, 100),
            Admission::Admit
        );
        p.commit(&[3; 32], [0; 32], 110, 100, 2_000_000, false, false, 100);
        // Seen later, it would not be in time now.
        let late = p.admit(&[2; 32], 5, 2_000_000, 111, 101, 100);
        assert!(
            matches!(&late, Admission::Withhold(w) if w.contains("in time")),
            "{late:?}"
        );
        // A proof made frees its room.
        p.commit(&[1; 32], [0; 32], 110, 100, 2_000_000, true, true, 100);
        assert_eq!(
            p.admit(&[2; 32], 5, 2_000_000, 111, 101, 100),
            Admission::Admit
        );
    }

    #[test]
    fn the_exits_defendable_are_the_smallest_proofs_a_window_holds() {
        let dir = tempfile::tempdir().unwrap();
        // About 1,000 s with the margin.
        let segment = 1_000.0 / MARGIN / 335.0;
        let speed = Speed {
            device: "cuda".into(),
            segment_cycles: cycles(15, 33_600).div_ceil(335),
            segment,
            tail: 0.0,
        };
        let p = ready(dir.path(), speed);
        // Signet's 39 blocks: 23,400 s.
        assert_eq!(p.defendable(39), Some(23));
        p.0.state().readiness = Some(Readiness::NotReady("no GPU".into()));
        assert_eq!(p.defendable(39), None);
    }

    #[test]
    fn a_prover_not_ready_admits_nothing_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let p = ready(
            dir.path(),
            Speed {
                device: "cpu".into(),
                segment_cycles: 1,
                segment: 1.0,
                tail: 1.0,
            },
        );
        p.0.state().readiness = Some(Readiness::NotReady("no GPU".into()));
        p.polled();
        let e = p.admit(&[1; 32], 1, 1, 110, 100, 100);
        assert!(
            matches!(&e, Admission::Withhold(w) if w.contains("no GPU")),
            "{e:?}"
        );
    }

    #[test]
    fn contracts_no_longer_seen_are_forgotten_and_their_proof_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let speed = Speed {
            device: "cuda".into(),
            segment_cycles: 1,
            segment: 1.0,
            tail: 1.0,
        };
        let p = ready(dir.path(), speed);
        p.commit(&[1; 32], [0; 32], 110, 100, 1, true, false, 100);
        p.0.state().running = Some(([1; 32], Instant::now()));
        for _ in 0..FORGET_POLLS {
            p.polled();
        }
        assert!(!p.0.state().cancel);
        p.polled();
        let s = p.0.state();
        assert!(s.jobs.is_empty() && s.cancel);
    }

    #[test]
    fn an_input_is_frozen_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = ready(
            dir.path(),
            Speed {
                device: "cpu".into(),
                segment_cycles: 1,
                segment: 1.0,
                tail: 1.0,
            },
        );
        assert_eq!(p.frozen(&[1; 32]), None);
        assert_eq!(p.freeze(&[1; 32], b"first").unwrap(), b"first");
        assert_eq!(p.freeze(&[1; 32], b"later").unwrap(), b"first");
        assert_eq!(p.frozen(&[1; 32]).as_deref(), Some(&b"first"[..]));
    }

    #[test]
    fn a_proof_of_another_program_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let p = ready(
            dir.path(),
            Speed {
                device: "cpu".into(),
                segment_cycles: 1,
                segment: 1.0,
                tail: 1.0,
            },
        );
        let d = p.dir(&[1; 32]);
        std::fs::create_dir_all(&d).unwrap();
        let json = |image: &str| format!(r#"{{"image_id":"{image}","seal":"0102"}}"#);
        std::fs::write(d.join("proof.json"), json(&hex::encode([0; 32]))).unwrap();
        assert_eq!(p.proof(&[1; 32], &[0; 32]).unwrap(), Some(vec![1, 2]));
        assert_eq!(p.proof(&[1; 32], &[9; 32]).unwrap(), None);
        assert!(d.join("proof.rejected").exists() && !d.join("proof.json").exists());
    }

    /// A prover of `script`'s, in `dir`, for program [7; 32].
    fn fake(dir: &Path, script: &str) -> Prover {
        let path = dir.join("prover.sh");
        std::fs::write(&path, format!("#!/bin/sh\nset -e\n{script}")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Prover::start_with(dir, path.display().to_string(), [7; 32], 600.0)
    }

    fn until(what: &str, f: impl Fn() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn readiness(p: &Prover) -> Option<Readiness> {
        p.0.state().readiness.clone()
    }

    /// Probed, benchmarked once, then each proof committed to made once its
    /// input is frozen, by `hp-prove <input> <dir> <program>`.
    #[test]
    fn a_ready_prover_is_benchmarked_once_and_makes_the_proofs_committed_to() {
        let dir = tempfile::tempdir().unwrap();
        let id = hex::encode([7u8; 32]);
        let script = format!(
            r#"echo "$@" >> "$(dirname "$0")/calls"
case "$1" in
probe) echo '{{"device":"cuda","programs":["{id}"],"problems":[]}}' ;;
bench) echo '{{"device":"cuda","segment_cycles":1000000,"segment":1.0,"tail":60.0}}' > "$2" ;;
hp-prove) echo "{{\"image_id\":\"$4\",\"seal\":\"0a0b\"}}" > "$3/proof.json" ;;
esac
"#
        );
        let p = fake(dir.path(), &script);
        until("ready", || {
            matches!(readiness(&p), Some(Readiness::Ready(_)))
        });
        let Some(Readiness::Ready(speed)) = readiness(&p) else {
            unreachable!()
        };
        assert_eq!(
            (speed.device.as_str(), speed.segment_cycles),
            ("cuda", 1_000_000)
        );
        // Committed to, not frozen: not proved.
        assert!(p.commit(&[1; 32], [7; 32], 110, 100, 1_000, false, false, 100));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(p.proof(&[1; 32], &[7; 32]).unwrap(), None);
        p.freeze(&[1; 32], b"input").unwrap();
        p.commit(&[1; 32], [7; 32], 110, 100, 1_000, true, false, 100);
        until("proved", || p.dir(&[1; 32]).join("proof.json").exists());
        until("its seal", || {
            p.proof(&[1; 32], &[7; 32]).unwrap() == Some(vec![10, 11])
        });
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        let input = p.dir(&[1; 32]).join("input.bin");
        let lines: Vec<_> = calls.lines().collect();
        assert_eq!(lines[0], "probe");
        assert!(
            lines[1].starts_with("bench ") && lines[1].ends_with(&id),
            "{calls}"
        );
        assert_eq!(
            lines[2],
            format!(
                "hp-prove {} {} {id}",
                input.display(),
                p.dir(&[1; 32]).display()
            )
        );
        // Measured on this device before: not benchmarked again.
        drop(p);
        std::fs::remove_file(dir.path().join("calls")).unwrap();
        let p = fake(dir.path(), &script);
        until("ready again", || {
            matches!(readiness(&p), Some(Readiness::Ready(_)))
        });
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(calls, "probe\n");
    }

    /// A probe's problems, or a program not installed, and it is not ready:
    /// told once, and claims withheld.
    #[test]
    fn a_prover_with_problems_is_not_ready_and_alerts() {
        let dir = tempfile::tempdir().unwrap();
        let p = fake(
            dir.path(),
            r#"echo '{"device":"cpu","programs":[],"problems":["memory: 2 GB"]}'"#,
        );
        until("probed", || readiness(&p).is_some());
        let Some(Readiness::NotReady(why)) = readiness(&p) else {
            panic!("ready")
        };
        assert!(
            why.contains("memory: 2 GB") && why.contains("is not installed"),
            "{why}"
        );
        let told: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = told.clone();
        p.alert_with(Arc::new(move |m| kept.lock().unwrap().push(m.to_owned())));
        assert!(told.lock().unwrap()[0].contains("responds to no contested claim"));
        p.polled();
        let e = p.admit(&[1; 32], 1, 1, 110, 100, 100);
        assert!(
            matches!(&e, Admission::Withhold(w) if w.contains("memory")),
            "{e:?}"
        );
    }

    /// A failing prover is alerted once, and tried again.
    #[test]
    fn a_prover_that_fails_is_alerted() {
        let dir = tempfile::tempdir().unwrap();
        let id = hex::encode([7u8; 32]);
        let p = fake(
            dir.path(),
            &format!(
                r#"case "$1" in
probe) echo '{{"device":"cuda","programs":["{id}"],"problems":[]}}' ;;
bench) echo '{{"device":"cuda","segment_cycles":1,"segment":1.0,"tail":1.0}}' > "$2" ;;
*) exit 3 ;;
esac
"#
            ),
        );
        let told: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = told.clone();
        p.alert_with(Arc::new(move |m| kept.lock().unwrap().push(m.to_owned())));
        until("ready", || {
            matches!(readiness(&p), Some(Readiness::Ready(_)))
        });
        p.freeze(&[1; 32], b"input").unwrap();
        p.commit(&[1; 32], [7; 32], 110, 100, 1, true, false, 100);
        until("alerted", || !told.lock().unwrap().is_empty());
        assert!(told.lock().unwrap()[0].contains("the prover failed"));
        assert_eq!(p.proof(&[1; 32], &[7; 32]).unwrap(), None);
    }
}
