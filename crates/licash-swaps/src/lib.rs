//! A Licash swap provider: it swaps its notes at one hub for a client's
//! notes at another ([`licash_api::swaps`]), with a timed lock at each under
//! the client's hash, from its own [`Wallet`].
//!
//! A client locks the amount and the provider's fee to the provider at hub
//! `from`, then asks for the swap. The provider checks that lock as its hub
//! holds it, records the swap and locks the amount to the client at hub
//! `to`, expiring [`Config::margin`] blocks earlier at least. Once the
//! client claimed that lock, revealing the preimage, the provider claims
//! the client's lock with it. Unclaimed, its own lock is refunded once
//! expired: either way the provider holds what it held, or more, unless it
//! claims too late ([`Stage::Lost`]).
//!
//! The provider swaps between the hubs its wallet added. Each step is
//! recorded before the next is asked for, so a provider stopped at any
//! point takes its swaps on with [`Provider::step`]. What its operator is
//! told of is kept in order: [`Provider::events`].

mod store;

use std::{
    collections::HashSet,
    path::Path,
    sync::{Mutex, PoisonError},
};

use licash::{address::Address, spend::ProposalId};
use licash_api::{
    swaps::{self, Hub, Request, State},
    Code, Fee, Problem,
};
use licash_wallet::{LockState, Transport, Wallet};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use store::{now, Store};

/// Blocks a provider's lock leaves at least before it expires: for the
/// client to claim it, and for the provider to follow it.
pub const MIN_ROOM: u32 = 6;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The swaps file is missing, exists already or does not match.
    #[error("{0}")]
    Setup(String),
    #[error("swaps database: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Wallet(#[from] licash_wallet::Error),
}

/// What a provider swaps, and on what terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Its fee on the amount swapped, which the client pays on top.
    pub fee: Fee,
    /// The least and the most it swaps at once.
    pub min: u64,
    pub max: u64,
    /// Its lock expires this many blocks before the client's, at least:
    /// its time to claim the client's lock once the client claimed its own.
    pub margin: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fee: Fee { base: 0, ppm: 2000 },
            min: 1_000,
            max: 1_000_000,
            margin: 12,
        }
    }
}

/// Where a swap stands at the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Taken: the provider is locking.
    Accepted,
    /// Its lock is certified: the client claims it.
    Locked,
    /// The client claimed it: the provider claims the client's lock.
    Paid,
    Done,
    /// The client never claimed: the provider's lock was refunded.
    Refunded,
    /// The provider did not lock.
    Failed,
    /// The provider paid, but did not claim the client's lock in time.
    Lost,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Locked => "locked",
            Self::Paid => "paid",
            Self::Done => "done",
            Self::Refunded => "refunded",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    fn parse(name: &str) -> rusqlite::Result<Self> {
        Ok(match name {
            "accepted" => Self::Accepted,
            "locked" => Self::Locked,
            "paid" => Self::Paid,
            "done" => Self::Done,
            "refunded" => Self::Refunded,
            "failed" => Self::Failed,
            "lost" => Self::Lost,
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }

    /// As the client is told: a swap lost to the provider is paid to the
    /// client.
    fn state(self) -> State {
        match self {
            Self::Accepted => State::Accepted,
            Self::Locked => State::Locked,
            Self::Paid => State::Paid,
            Self::Done | Self::Lost => State::Done,
            Self::Refunded => State::Refunded,
            Self::Failed => State::Failed,
        }
    }

    fn open(self) -> bool {
        matches!(self, Self::Accepted | Self::Locked | Self::Paid)
    }
}

/// A swap as the provider recorded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Record {
    /// The client's lock at `from`, in hex.
    pub id: String,
    pub hash: String,
    pub from: String,
    pub to: String,
    pub amount: u64,
    pub fee: u64,
    /// What the client's lock pays the provider.
    pub paid: u64,
    /// The provider's lock at `to`, once recorded, and its expiry.
    pub lock: Option<String>,
    pub expiry: u32,
    pub stage: Stage,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What the operator is told of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Notice {
    /// A swap of `amount` from hub `from` to hub `to` was taken, for `fee`.
    Accepted {
        id: String,
        amount: u64,
        fee: u64,
        from: String,
        to: String,
    },
    /// Both locks paid: the provider earned `fee`.
    Done { id: String, amount: u64, fee: u64 },
    /// The client never claimed: the provider's lock was refunded.
    Refunded { id: String, amount: u64 },
    /// The provider did not lock, and why.
    Failed { id: String, reason: String },
    /// The provider paid `amount` at hub `to` but did not claim the
    /// client's lock: lost.
    Lost {
        id: String,
        amount: u64,
        reason: String,
    },
    /// Something the operator should look at.
    Alert { message: String },
}

impl Notice {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Accepted { .. } => "accepted",
            Self::Done { .. } => "done",
            Self::Refunded { .. } => "refunded",
            Self::Failed { .. } => "failed",
            Self::Lost { .. } => "lost",
            Self::Alert { .. } => "alert",
        }
    }
}

/// A [`Notice`] in the provider's outbox, in order of `seq`: its kind and
/// its JSON as stored, which a provider of another version may have
/// written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// Unix seconds.
    pub at: u64,
    pub kind: String,
    pub data: serde_json::Value,
}

impl Event {
    /// The notice, if this version knows it.
    pub fn notice(&self) -> Option<Notice> {
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// Counts over the swaps, by stage, and the fees earned.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub open: u64,
    pub done: u64,
    pub refunded: u64,
    pub failed: u64,
    pub lost: u64,
    /// The fees of the swaps done.
    pub earned: u64,
    /// What the swaps lost cost.
    pub lost_amount: u64,
    /// The last event's `seq`.
    pub events: u64,
}

/// A swap as stored, to take it on.
struct Row {
    id: [u8; 32],
    hash: [u8; 32],
    from: String,
    to: String,
    address: Vec<u8>,
    amount: u64,
    fee: u64,
    paid: u64,
    expiry: u32,
    lock: Option<[u8; 32]>,
    preimage: Option<[u8; 32]>,
    stage: Stage,
    reason: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl Row {
    fn record(&self) -> Record {
        Record {
            id: hex::encode(self.id),
            hash: hex::encode(self.hash),
            from: self.from.clone(),
            to: self.to.clone(),
            amount: self.amount,
            fee: self.fee,
            paid: self.paid,
            lock: self.lock.map(hex::encode),
            expiry: self.expiry,
            stage: self.stage,
            reason: self.reason.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn swap(&self) -> swaps::Swap {
        swaps::Swap {
            id: licash_api::Hex(self.id.to_vec()),
            state: self.stage.state(),
            amount: self.amount,
            lock: self.lock.map(|l| licash_api::Hex(l.to_vec())),
            expiry: self.expiry,
            reason: self.reason.clone(),
        }
    }
}

const LAST_EVENT: &str = "SELECT coalesce(max(seq), 0) FROM events";

const ROWS: &str = "SELECT id, hash, from_url, to_url, address, amount, fee, paid, expiry,
    lock, preimage, state, reason, created_at, updated_at FROM swaps";

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        hash: r.get(1)?,
        from: r.get(2)?,
        to: r.get(3)?,
        address: r.get(4)?,
        amount: r.get::<_, i64>(5)? as u64,
        fee: r.get::<_, i64>(6)? as u64,
        paid: r.get::<_, i64>(7)? as u64,
        expiry: r.get(8)?,
        lock: r.get(9)?,
        preimage: r.get(10)?,
        stage: Stage::parse(&r.get::<_, String>(11)?)?,
        reason: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

/// A swap in hand, so that no other task takes it on meanwhile.
struct Hold<'a> {
    busy: &'a Mutex<HashSet<[u8; 32]>>,
    id: [u8; 32],
}

impl<'a> Hold<'a> {
    fn take(busy: &'a Mutex<HashSet<[u8; 32]>>, id: [u8; 32]) -> Option<Self> {
        let mut held = busy.lock().unwrap_or_else(PoisonError::into_inner);
        // Not `then_some`: a hold made only to be dropped would lock again,
        // and release the other task's.
        held.insert(id).then(|| Self { busy, id })
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let mut held = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        held.remove(&self.id);
    }
}

fn declined(message: impl Into<String>) -> Problem {
    Problem::new(Code::Declined, message)
}

/// A problem of the provider's own, for the client to retry.
fn unavailable(e: impl std::fmt::Display) -> Problem {
    Problem::new(Code::Unavailable, e.to_string())
}

/// A client's request refused, as a wallet error says it.
fn refused(e: licash_wallet::Error) -> Problem {
    match e {
        licash_wallet::Error::Refused { hub, problem } => {
            declined(format!("hub {hub} refused: {}", problem.message))
        }
        licash_wallet::Error::PaysNothing => declined("the lock pays the provider nothing"),
        licash_wallet::Error::UnknownHub(hub) => declined(format!("no swaps at hub {hub}")),
        licash_wallet::Error::Invalid(why) => Problem::invalid(why),
        e => unavailable(e),
    }
}

pub struct Provider<T: Transport> {
    wallet: Wallet<T>,
    store: Store,
    config: Config,
    /// Swaps in hand, by the client's lock.
    busy: Mutex<HashSet<[u8; 32]>>,
    /// Held while one request is checked against what the provider holds
    /// and recorded.
    taking: Mutex<()>,
}

/// Setup and what the provider says of itself.
impl<T: Transport> Provider<T> {
    /// A new swaps file at `path`, for the provider of `wallet`.
    pub fn create(path: &Path, wallet: Wallet<T>, config: Config) -> Result<Self, Error> {
        let mut nostr = [0; 32];
        OsRng.fill_bytes(&mut nostr);
        Ok(Self::with(Store::create(path, &nostr)?, wallet, config))
    }

    /// The swaps file at `path`.
    pub fn open(path: &Path, wallet: Wallet<T>, config: Config) -> Result<Self, Error> {
        Ok(Self::with(Store::open(path)?, wallet, config))
    }

    fn with(store: Store, wallet: Wallet<T>, config: Config) -> Self {
        Self {
            wallet,
            store,
            config,
            busy: Mutex::default(),
            taking: Mutex::default(),
        }
    }

    pub fn wallet(&self) -> &Wallet<T> {
        &self.wallet
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The secret key the provider announces itself under on Nostr.
    pub fn nostr_secret(&self) -> Result<[u8; 32], Error> {
        self.store
            .read(|c| Ok(c.query_row("SELECT nostr FROM meta", [], |r| r.get(0))?))
    }

    /// What the provider swaps now, as its hubs last said of themselves:
    /// at each hub that follows a chain, the most it can lock there.
    pub fn info(&self) -> Result<swaps::Info, Error> {
        let balance = self.wallet.balance()?;
        let mut hubs = Vec::new();
        for (url, info) in self.wallet.hubs()? {
            if info.locks.is_none() {
                continue;
            }
            let available = balance
                .iter()
                .find(|b| b.hub == url)
                .map_or(0, |b| b.available);
            let free = available
                .saturating_sub(self.reserved(&url)?)
                .saturating_sub(info.fees.lock.due(available));
            hubs.push(Hub {
                address: info.address,
                max: free.min(self.config.max),
            });
        }
        Ok(swaps::Info {
            address: self.wallet.address(),
            fee: self.config.fee,
            min: self.config.min,
            max: self.config.max,
            margin: self.config.margin,
            hubs,
            nostr: None,
        })
    }

    /// What the swaps accepted and not locked yet will lock at `url`.
    fn reserved(&self, url: &str) -> Result<u64, Error> {
        self.store.read(|c| {
            Ok(c.query_row(
                "SELECT coalesce(sum(amount), 0) FROM swaps
                 WHERE to_url = ?1 AND state = 'accepted'",
                [url],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }
}

/// Swaps taken.
impl<T: Transport> Provider<T> {
    /// Take the swap `request` asks for, or answer where it stands if taken
    /// already. Declined unless the client's lock, as its hub holds it,
    /// pays the provider the amount and its fee, unsettled, and leaves the
    /// provider room to lock earlier at a hub where it holds enough.
    pub async fn take(&self, request: &Request) -> Result<swaps::Swap, Problem> {
        let id = request.lock()?;
        let key = *id.as_bytes();
        if let Some(row) = self.row(&key).map_err(unavailable)? {
            return self.again(&row, request);
        }
        let config = &self.config;
        if request.amount < config.min || request.amount > config.max {
            return Err(declined(format!(
                "swaps of {} to {}",
                config.min, config.max
            )));
        }
        let hubs = self.wallet.hubs().map_err(unavailable)?;
        let at = |address: &Address| hubs.iter().find(|(_, info)| info.address == *address);
        let (Some((from, _)), Some((to, _))) = (at(&request.from), at(&request.to)) else {
            return Err(declined("no swaps between these hubs"));
        };
        if from == to {
            return Err(declined("a swap is between two hubs"));
        }
        if at(&request.address).is_some() {
            return Err(Problem::invalid("a hub's address is not a client's"));
        }
        let Some(_hold) = Hold::take(&self.busy, key) else {
            return Err(unavailable("the swap is under way"));
        };
        let incoming = self
            .wallet
            .incoming(from, &hex::encode(key))
            .await
            .map_err(refused)?;
        let fee = config.fee.due(request.amount);
        let due = request.amount.saturating_add(fee);
        if incoming.amount < due {
            return Err(declined(format!(
                "the lock pays {}, the swap {due}",
                incoming.amount
            )));
        }
        if incoming.settled.is_some() || incoming.preimage.is_some() {
            return Err(declined("the lock is settled"));
        }
        let to_info = self.wallet.add_hub(to).await.map_err(refused)?;
        let Some(locks) = to_info.locks.as_ref() else {
            return Err(declined(format!("hub {to} follows no chain")));
        };
        let latest = locks
            .latest_expiry(locks.min_depth.max(1))
            .map_err(unavailable)?;
        let expiry = incoming.expiry.saturating_sub(config.margin).min(latest);
        let room = locks.height.max(incoming.height).saturating_add(MIN_ROOM);
        if expiry < room {
            return Err(declined(format!(
                "the lock expires at {}: too soon to lock {} blocks earlier",
                incoming.expiry, config.margin
            )));
        }
        let terms = Terms {
            from,
            to,
            hash: incoming.hash,
            fee,
            paid: incoming.amount,
            expiry_from: incoming.expiry,
            expiry,
        };
        self.record(request, &key, &terms, &to_info)?;
        tracing::info!(id = %hex::encode(key), %from, %to, amount = request.amount, fee, expiry, "swap accepted");
        self.lock(&key).await.map_err(unavailable)?;
        Ok(self
            .row(&key)
            .map_err(unavailable)?
            .expect("recorded")
            .swap())
    }

    /// The swap `row` asked for again: where it stands, if it is the same.
    fn again(&self, row: &Row, request: &Request) -> Result<swaps::Swap, Problem> {
        let hubs = self.wallet.hubs().map_err(unavailable)?;
        let address = |url: &str| hubs.iter().find(|(u, _)| u == url).map(|(_, i)| i.address);
        let same = address(&row.from) == Some(request.from)
            && address(&row.to) == Some(request.to)
            && row.address == request.address.to_bytes()
            && row.amount == request.amount;
        if !same {
            return Err(declined("another swap of this lock"));
        }
        Ok(row.swap())
    }
}

/// What the provider checked of a client's lock, and the expiry of its own.
struct Terms<'a> {
    from: &'a str,
    to: &'a str,
    hash: [u8; 32],
    fee: u64,
    paid: u64,
    expiry_from: u32,
    expiry: u32,
}

/// Recording and locking.
impl<T: Transport> Provider<T> {
    /// Record the swap `request` asks for on `terms`, if the provider holds
    /// enough at hub `to` beside the swaps it has yet to lock there.
    fn record(
        &self,
        request: &Request,
        id: &[u8; 32],
        terms: &Terms<'_>,
        to_info: &licash_api::Info,
    ) -> Result<(), Problem> {
        let _taking = self.taking.lock().unwrap_or_else(PoisonError::into_inner);
        let balance = self.wallet.balance().map_err(unavailable)?;
        let available = balance
            .iter()
            .find(|b| b.hub == terms.to)
            .map_or(0, |b| b.available);
        let reserved = self.reserved(terms.to).map_err(unavailable)?;
        let needed = reserved
            .saturating_add(request.amount)
            .saturating_add(to_info.fees.lock.due(available));
        if available < needed {
            return Err(declined(format!(
                "the provider holds too little at hub {} now",
                terms.to
            )));
        }
        let notice = Notice::Accepted {
            id: hex::encode(id),
            amount: request.amount,
            fee: terms.fee,
            from: terms.from.to_owned(),
            to: terms.to.to_owned(),
        };
        let recorded = self.store.write(|tx| {
            let at = now();
            let inserted = tx.execute(
                "INSERT INTO swaps (id, hash, from_url, to_url, address, amount, fee, paid,
                    expiry_from, expiry, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'accepted', ?11, ?11)
                 ON CONFLICT DO NOTHING",
                params![
                    id,
                    terms.hash,
                    terms.from,
                    terms.to,
                    request.address.to_bytes(),
                    request.amount as i64,
                    terms.fee as i64,
                    terms.paid as i64,
                    terms.expiry_from,
                    terms.expiry,
                    at
                ],
            )?;
            if inserted == 1 {
                emit(tx, &notice)?;
            }
            Ok(inserted == 1)
        });
        match recorded {
            Ok(true) => Ok(()),
            Ok(false) => Err(declined("a swap under this hash was taken before")),
            Err(e) => Err(unavailable(e)),
        }
    }

    /// Lock the amount of the swap `id` to its client, as recorded: give
    /// it up if nothing was locked.
    async fn lock(&self, id: &[u8; 32]) -> Result<(), Error> {
        let row = self.row(id)?.expect("recorded");
        let address = Address::from_bytes(&row.address).map_err(licash_wallet::Error::from)?;
        let mut tries = 0;
        let locked = loop {
            let locked = self
                .wallet
                .lock(&row.to, &address, row.amount, row.hash, Some(row.expiry))
                .await;
            // Another payment of the wallet, such as an unlinking, took a
            // note it chose: nothing was locked.
            match locked {
                Err(licash_wallet::Error::NoteTaken) if tries < 3 => tries += 1,
                locked => break locked,
            }
        };
        match locked {
            Ok(lock) => {
                let lock_id = lock_id(&lock.id)?;
                self.set_lock(id, &lock_id)?;
                if lock.state == LockState::Locked {
                    self.advance(id, Stage::Accepted, Stage::Locked, None, None)?;
                }
            }
            // Recorded, its outcome unknown: taken on by `step`.
            Err(licash_wallet::Error::Pending { id: lock, reason }) => {
                tracing::warn!(id = %hex::encode(id), %lock, reason, "lock pending");
                self.set_lock(id, &lock_id(&lock)?)?;
            }
            Err(e) => {
                self.fail(id, &row, &format!("not locked: {e}"))?;
            }
        }
        Ok(())
    }

    fn set_lock(&self, id: &[u8; 32], lock: &[u8; 32]) -> Result<(), Error> {
        self.store.write(|tx| {
            tx.execute(
                "UPDATE swaps SET lock = ?2, updated_at = ?3 WHERE id = ?1 AND lock IS NULL",
                params![id, lock, now()],
            )?;
            Ok(())
        })
    }

    /// Move the swap `id` from stage `from` to `to`, recording `preimage`
    /// and `notice` with it. False if it had moved meanwhile.
    fn advance(
        &self,
        id: &[u8; 32],
        from: Stage,
        to: Stage,
        preimage: Option<&[u8; 32]>,
        notice: Option<(&Notice, &str)>,
    ) -> Result<bool, Error> {
        self.store.write(|tx| {
            let moved = tx.execute(
                "UPDATE swaps SET state = ?3, preimage = coalesce(?4, preimage),
                    reason = coalesce(?5, reason), updated_at = ?6
                 WHERE id = ?1 AND state = ?2",
                params![
                    id,
                    from.name(),
                    to.name(),
                    preimage,
                    notice.map(|(_, reason)| reason).filter(|r| !r.is_empty()),
                    now()
                ],
            )?;
            if moved == 1 {
                if let Some((notice, _)) = notice {
                    emit(tx, notice)?;
                }
            }
            Ok(moved == 1)
        })
    }

    /// Give the swap `row` up: the provider did not lock.
    fn fail(&self, id: &[u8; 32], row: &Row, reason: &str) -> Result<(), Error> {
        tracing::warn!(id = %hex::encode(id), reason, "swap failed");
        let notice = Notice::Failed {
            id: hex::encode(id),
            reason: reason.to_owned(),
        };
        self.advance(id, row.stage, Stage::Failed, None, Some((&notice, reason)))?;
        Ok(())
    }
}

fn lock_id(text: &str) -> Result<[u8; 32], Error> {
    let mut id = [0; 32];
    hex::decode_to_slice(text, &mut id)
        .map_err(|_| licash_wallet::Error::Invalid(format!("not a lock id: {text}")))?;
    Ok(id)
}

fn emit(tx: &rusqlite::Transaction<'_>, notice: &Notice) -> Result<(), Error> {
    let json = serde_json::to_string(notice).expect("a notice serializes");
    tx.execute(
        "INSERT INTO events (at, kind, json) VALUES (?1, ?2, ?3)",
        params![now(), notice.kind(), json],
    )?;
    Ok(())
}

/// Swaps taken on.
impl<T: Transport> Provider<T> {
    /// Take every swap under way on: follow the provider's locks, and
    /// claim the clients' once they revealed their preimages. Its hubs are
    /// asked again what they say of themselves, and its wallet's other
    /// payments taken on. The swaps that could not move now, by id and why.
    pub async fn step(&self) -> Result<Vec<(String, String)>, Error> {
        for (url, _) in self.wallet.hubs()? {
            if let Err(e) = self.wallet.add_hub(&url).await {
                tracing::warn!(%url, "hub unavailable: {e}");
            }
        }
        let resumed = self.wallet.resume().await?;
        for (id, reason) in &resumed.pending {
            tracing::debug!(%id, reason, "payment pending");
        }
        if let Err(e) = self.wallet.unlink().await {
            tracing::warn!("notes stay linked for now: {e}");
        }
        let open: Vec<[u8; 32]> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM swaps WHERE state IN ('accepted', 'locked', 'paid')
                 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut pending = Vec::new();
        for id in open {
            let Some(_hold) = Hold::take(&self.busy, id) else {
                continue;
            };
            if let Err(e) = self.step_one(&id).await {
                tracing::warn!(id = %hex::encode(id), "swap pending: {e}");
                pending.push((hex::encode(id), e.to_string()));
            }
        }
        Ok(pending)
    }

    async fn step_one(&self, id: &[u8; 32]) -> Result<(), Error> {
        let Some(row) = self.row(id)? else {
            return Ok(());
        };
        match row.stage {
            Stage::Accepted | Stage::Locked => self.follow(id, &row).await,
            Stage::Paid => self.claim(id, &row).await,
            _ => Ok(()),
        }
    }

    /// Follow the provider's lock of the swap `row` to its settlement.
    async fn follow(&self, id: &[u8; 32], row: &Row) -> Result<(), Error> {
        let lock = match row.lock {
            Some(lock) => lock,
            // Stopped after the wallet recorded the lock, or before.
            None => {
                let locks = self.wallet.locks()?;
                let found = locks.iter().find(|l| l.hub == row.to && l.hash == row.hash);
                let Some(found) = found else {
                    return self.fail(id, row, "stopped before locking");
                };
                let lock = lock_id(&found.id)?;
                self.set_lock(id, &lock)?;
                lock
            }
        };
        let state = match self.wallet.follow_lock(&hex::encode(lock)).await {
            Ok(lock) => lock.state,
            Err(licash_wallet::Error::Abandoned { reason, .. }) => LockState::Abandoned(reason),
            Err(licash_wallet::Error::Refused { problem, .. }) => {
                LockState::Abandoned(problem.to_string())
            }
            Err(e) => return Err(e.into()),
        };
        match state {
            LockState::Locked if row.stage == Stage::Accepted => {
                self.advance(id, Stage::Accepted, Stage::Locked, None, None)?;
                tracing::info!(id = %hex::encode(id), lock = %hex::encode(lock), "locked");
            }
            LockState::Paid(preimage) => {
                self.advance(id, row.stage, Stage::Paid, Some(&preimage), None)?;
                let row = self.row(id)?.expect("recorded");
                return self.claim(id, &row).await;
            }
            LockState::Refunded => {
                tracing::info!(id = %hex::encode(id), "refunded: the client never claimed");
                let notice = Notice::Refunded {
                    id: hex::encode(id),
                    amount: row.amount,
                };
                self.advance(id, row.stage, Stage::Refunded, None, Some((&notice, "")))?;
            }
            LockState::Abandoned(reason) => {
                return self.fail(id, row, &format!("its hub refused the lock: {reason}"));
            }
            _ => {}
        }
        Ok(())
    }

    /// Claim the client's lock of the swap `row` with the preimage it
    /// revealed.
    async fn claim(&self, id: &[u8; 32], row: &Row) -> Result<(), Error> {
        let preimage = row.preimage.expect("paid on it");
        let key = hex::encode(id);
        match self.wallet.claim(&row.from, &key, &preimage).await {
            Ok(_) | Err(licash_wallet::Error::AlreadyReceived) => {
                tracing::info!(id = %key, amount = row.amount, fee = row.fee, "swap done");
                let notice = Notice::Done {
                    id: key,
                    amount: row.amount,
                    fee: row.fee,
                };
                self.advance(id, Stage::Paid, Stage::Done, None, Some((&notice, "")))?;
            }
            Err(e @ (licash_wallet::Error::Invalid(_) | licash_wallet::Error::PaysNothing)) => {
                let reason = format!("the client's lock was not claimed: {e}");
                tracing::error!(id = %key, amount = row.amount, reason, "swap lost");
                let notice = Notice::Lost {
                    id: key,
                    amount: row.amount,
                    reason: reason.clone(),
                };
                self.advance(id, Stage::Paid, Stage::Lost, None, Some((&notice, &reason)))?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

/// What the provider recorded.
impl<T: Transport> Provider<T> {
    fn row(&self, id: &[u8; 32]) -> Result<Option<Row>, Error> {
        self.store.read(|c| {
            Ok(c.query_row(&format!("{ROWS} WHERE id = ?1"), [id], row_of)
                .optional()?)
        })
    }

    /// The swap of the client's lock `id`, as its client is told.
    pub fn swap(&self, id: &ProposalId) -> Result<Option<swaps::Swap>, Error> {
        Ok(self.row(id.as_bytes())?.map(|row| row.swap()))
    }

    /// The swaps, newest first, at most `limit`.
    pub fn records(&self, limit: u32) -> Result<Vec<Record>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(&format!(
                "{ROWS} ORDER BY created_at DESC, rowid DESC LIMIT ?1"
            ))?;
            let rows = stmt.query_map([limit], row_of)?;
            rows.map(|row| Ok(row?.record())).collect()
        })
    }

    pub fn stats(&self) -> Result<Stats, Error> {
        self.store.read(|c| {
            let mut stats = Stats {
                events: c.query_row(LAST_EVENT, [], |r| r.get::<_, i64>(0))? as u64,
                ..Stats::default()
            };
            let mut stmt = c.prepare(
                "SELECT state, count(*), sum(fee), sum(amount) FROM swaps GROUP BY state",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    Stage::parse(&r.get::<_, String>(0)?)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?;
            for row in rows {
                let (stage, count, fees, amount) = row?;
                match stage {
                    _ if stage.open() => stats.open += count,
                    Stage::Done => {
                        stats.done += count;
                        stats.earned += fees;
                    }
                    Stage::Refunded => stats.refunded += count,
                    Stage::Failed => stats.failed += count,
                    Stage::Lost => {
                        stats.lost += count;
                        stats.lost_amount += amount;
                    }
                    _ => {}
                }
            }
            Ok(stats)
        })
    }

    /// The events after `after`, oldest first, at most `limit`.
    pub fn events(&self, after: u64, limit: u32) -> Result<Vec<Event>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT seq, at, kind, json FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![after as i64, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            rows.map(|row| {
                let (seq, at, kind, json) = row?;
                Ok(Event {
                    seq: seq as u64,
                    at: at as u64,
                    kind,
                    data: serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect()
        })
    }

    /// The last event's `seq`, or 0.
    pub fn last_event(&self) -> Result<u64, Error> {
        self.store
            .read(|c| Ok(c.query_row(LAST_EVENT, [], |r| r.get::<_, i64>(0))? as u64))
    }

    /// Tell the operator `message`.
    pub fn alert(&self, message: &str) -> Result<(), Error> {
        let notice = Notice::Alert {
            message: message.to_owned(),
        };
        self.store.write(|tx| emit(tx, &notice))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swap_in_hand_is_held_until_dropped() {
        let busy = Mutex::new(HashSet::new());
        let first = Hold::take(&busy, [1; 32]).unwrap();
        assert!(Hold::take(&busy, [1; 32]).is_none());
        assert!(Hold::take(&busy, [1; 32]).is_none());
        let other = Hold::take(&busy, [2; 32]).unwrap();
        drop(first);
        assert!(Hold::take(&busy, [1; 32]).is_some());
        drop(other);
        assert!(busy.lock().unwrap().is_empty());
    }
}
