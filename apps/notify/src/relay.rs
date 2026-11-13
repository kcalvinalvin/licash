//! Delivery of an outbox to targets, each from its own durable place.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{Message, Priority, Target};

/// An outbox event as a notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub seq: u64,
    pub kind: String,
    pub message: Message,
}

/// A service's events, in order of increasing `seq`. Its calls may block.
pub trait Outbox: Send + Sync + 'static {
    /// The items after `after`, oldest first, at most `limit`.
    fn after(&self, after: u64, limit: u32) -> anyhow::Result<Vec<Item>>;
    /// The last item's `seq`, or 0.
    fn last(&self) -> anyhow::Result<u64>;
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How often an idle relay looks for new events.
    pub poll: Duration,
    /// The first delay before sending a failed notification again, doubled
    /// up to `max_retry`.
    pub retry: Duration,
    pub max_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(2),
            retry: Duration::from_secs(2),
            max_retry: Duration::from_secs(300),
        }
    }
}

const PAGE: u32 = 64;
const ALERT_TRIES: u32 = 3;

/// Delivers an outbox to each target in order, from where that target
/// stopped. A target new to the cursor file starts at the outbox's end: it
/// is told of what happens from then on, not of the history before it. A
/// notification is sent again until its target takes it, and may arrive
/// twice if the relay stops between sending it and recording that.
#[derive(Clone)]
pub struct Relay {
    outbox: Arc<dyn Outbox>,
    targets: Arc<[Target]>,
    kinds: Option<Arc<BTreeSet<String>>>,
    new_after: Option<u64>,
    cursors: Arc<Cursors>,
    client: reqwest::Client,
    timing: Timing,
}

impl Relay {
    /// A relay of `outbox` to `targets` that keeps their places in the
    /// file `cursors`.
    pub fn new(
        outbox: Arc<dyn Outbox>,
        targets: Vec<Target>,
        cursors: &Path,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            outbox,
            targets: targets.into(),
            kinds: None,
            new_after: None,
            cursors: Arc::new(Cursors::load(cursors)?),
            client: crate::client()?,
            timing: Timing::default(),
        })
    }

    /// Notify only of events of `kinds`; the others are passed over.
    pub fn only(mut self, kinds: impl IntoIterator<Item = String>) -> Self {
        self.kinds = Some(Arc::new(kinds.into_iter().collect()));
        self
    }

    /// Start targets new to the cursor file after `seq`, not at the
    /// outbox's end: a service that writes an event as it starts passes the
    /// `seq` from before, so that new targets hear of the start too.
    pub fn new_targets_after(mut self, seq: u64) -> Self {
        self.new_after = Some(seq);
        self
    }

    pub fn timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Deliver to every target, for as long as the future is polled.
    pub async fn run(self) {
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..self.targets.len() {
            let relay = self.clone();
            tasks.spawn(async move { relay.deliver(&relay.targets[index]).await });
        }
        while let Some(done) = tasks.join_next().await {
            if let Err(e) = done {
                tracing::error!(error = %e, "a notification task failed");
            }
        }
    }

    /// Send `message` to every target now, trying each a few times: for
    /// what the outbox cannot hold. Nothing is recorded.
    pub async fn alert(&self, message: &Message) {
        let mut tasks = tokio::task::JoinSet::new();
        for target in self.targets.iter() {
            let (target, client, message) = (target.clone(), self.client.clone(), message.clone());
            let timing = self.timing;
            tasks.spawn(async move {
                let mut delay = timing.retry;
                for attempt in 1..=ALERT_TRIES {
                    match target.send(&client, &message).await {
                        Ok(()) => return,
                        Err(e) => {
                            tracing::warn!(%target, attempt, error = %format!("{e:#}"), "alert not sent")
                        }
                    }
                    if attempt == ALERT_TRIES {
                        break;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(timing.max_retry);
                }
                tracing::error!(%target, title = %message.title, "alert lost");
            });
        }
        tasks.join_all().await;
    }

    async fn deliver(&self, target: &Target) {
        let id = target.id();
        let mut after = loop {
            match self.start(id).await {
                Ok(after) => break after,
                Err(e) => {
                    tracing::warn!(%target, error = %format!("{e:#}"), "notifications not started");
                    tokio::time::sleep(self.timing.poll).await;
                }
            }
        };
        tracing::info!(%target, after, "notifying");
        loop {
            let page = match self.read(move |o| o.after(after, PAGE)).await {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "reading the events");
                    tokio::time::sleep(self.timing.poll).await;
                    continue;
                }
            };
            if page.is_empty() {
                if let Ok(last) = self.read(|o| o.last()).await {
                    if last < after {
                        let lost = Message::new(
                            Priority::High,
                            "Events went back",
                            format!(
                                "The event log ends at {last}, but notifications reached {after}: \
                                 the database was replaced by an older copy. Notifying from {last}."
                            ),
                        );
                        self.send(target, &lost).await;
                        after = last;
                        self.record(target, id, after).await;
                    }
                }
                tokio::time::sleep(self.timing.poll).await;
                continue;
            }
            // Events passed over are recorded with the next one sent, or
            // at the page's end.
            let mut sent = false;
            for item in page {
                after = item.seq;
                if self.kinds.as_ref().is_none_or(|k| k.contains(&item.kind)) {
                    self.send(target, &item.message).await;
                    self.record(target, id, after).await;
                    sent = true;
                } else {
                    sent = false;
                }
            }
            if !sent {
                self.record(target, id, after).await;
            }
        }
    }

    /// Where `id` resumes: its recorded place, or else the outbox's end
    /// (or [`Self::new_targets_after`]), which is recorded first.
    async fn start(&self, id: &str) -> anyhow::Result<u64> {
        if let Some(after) = self.cursors.get(id) {
            return Ok(after);
        }
        let end = self.read(|o| o.last()).await?;
        let last = self.new_after.map_or(end, |after| after.min(end));
        let (cursors, id) = (self.cursors.clone(), id.to_owned());
        tokio::task::spawn_blocking(move || cursors.set(&id, last)).await??;
        Ok(last)
    }

    /// Send until the target takes it.
    async fn send(&self, target: &Target, message: &Message) {
        let mut delay = self.timing.retry;
        let mut attempt = 1u32;
        while let Err(e) = target.send(&self.client, message).await {
            tracing::warn!(%target, attempt, ?delay, error = %format!("{e:#}"), "notification not sent, retrying");
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(self.timing.max_retry);
            attempt += 1;
        }
        tracing::debug!(%target, title = %message.title, "notified");
    }

    async fn record(&self, target: &Target, id: &str, after: u64) {
        let (cursors, id) = (self.cursors.clone(), id.to_owned());
        let recorded = tokio::task::spawn_blocking(move || cursors.set(&id, after)).await;
        if let Err(e) = recorded.map_err(anyhow::Error::from).and_then(|r| r) {
            // Kept in memory: a restart repeats these notifications.
            tracing::warn!(%target, error = %format!("{e:#}"), "notification place not recorded");
        }
    }

    async fn read<T: Send + 'static>(
        &self,
        op: impl FnOnce(&dyn Outbox) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let outbox = self.outbox.clone();
        tokio::task::spawn_blocking(move || op(&*outbox)).await?
    }
}

/// Each target's place, by its id, in a file replaced whole on each change.
struct Cursors {
    path: PathBuf,
    seqs: Mutex<BTreeMap<String, u64>>,
}

#[derive(Serialize, Deserialize)]
struct CursorFile {
    version: u32,
    targets: BTreeMap<String, u64>,
}

const VERSION: u32 = 1;

impl Cursors {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let seqs = match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            read => {
                let read = read.with_context(|| format!("read {}", path.display()))?;
                let file: CursorFile = serde_json::from_slice(&read).with_context(|| {
                    format!(
                        "{} is damaged: delete it to notify from now on",
                        path.display()
                    )
                })?;
                anyhow::ensure!(
                    file.version == VERSION,
                    "{} is of version {}, this one reads {VERSION}",
                    path.display(),
                    file.version
                );
                file.targets
            }
        };
        Ok(Self {
            path: path.to_owned(),
            seqs: Mutex::new(seqs),
        })
    }

    fn get(&self, id: &str) -> Option<u64> {
        self.seqs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .copied()
    }

    fn set(&self, id: &str, seq: u64) -> anyhow::Result<()> {
        let mut seqs = self.seqs.lock().unwrap_or_else(PoisonError::into_inner);
        seqs.insert(id.to_owned(), seq);
        let file = CursorFile {
            version: VERSION,
            targets: seqs.clone(),
        };
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut out = std::fs::File::create(&tmp)?;
        out.write_all(&serde_json::to_vec(&file)?)?;
        out.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}
