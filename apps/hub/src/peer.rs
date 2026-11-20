//! The hub's Lightning receipts, deposits and swap issuer share one turn:
//! a hash backs one own lock of the hub's
//! notes ([`licash_hub::Hub::lock_own`]), by one payment. The wallet that
//! asks for a receipt or a deposit picks its hash, so it may ask both
//! under one, and pay both: each gateway refuses a hash its peer holds,
//! and leaves a lock under it to the payment its peer recorded it for.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use licash::spend::ProposalId;
use licash_api::{Code, Problem};
use licash_store::Kind;
use rusqlite::OptionalExtension;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{deposit, lightning, Failure};

/// What a gateway recorded under a hash: a payment, if any, and the own
/// lock it backs once locked.
pub type Recorded = Option<Option<ProposalId>>;

/// What a gateway asks of its peer, if it has one ([`pair`]).
#[derive(Default)]
pub struct Peer(OnceLock<Paired>);

/// What the peer recorded under a hash.
type Lookup = dyn Fn(&[u8; 32]) -> Result<Recorded, String> + Send + Sync;

struct Paired {
    /// Held by either gateway from checking a hash with the peer to
    /// recording what it holds under it: a payment, or the lock it backs.
    turn: Arc<Mutex<()>>,
    recorded: Box<Lookup>,
    swaps: Option<PathBuf>,
}

impl Peer {
    pub(crate) fn holds_swap(&self, hash: &[u8; 32]) -> Result<bool, Failure> {
        let Some(path) = self.0.get().and_then(|p| p.swaps.as_ref()) else {
            return Ok(false);
        };
        licash_swaps::holds_hash(path, hash).map_err(|e| unreadable(e.to_string()))
    }

    pub(crate) fn swap_owns(&self, hash: &[u8; 32], lock: &ProposalId) -> Result<bool, Failure> {
        let Some(path) = self.0.get().and_then(|p| p.swaps.as_ref()) else {
            return Ok(false);
        };
        licash_swaps::owns_hash_lock(path, hash, lock).map_err(|e| unreadable(e.to_string()))
    }
    /// Wait for the turn to check a hash with the peer and record what
    /// the gateway holds under it, until dropped: none needed unpaired.
    pub(crate) async fn turn(&self) -> Option<OwnedMutexGuard<()>> {
        let paired = self.0.get()?;
        Some(paired.turn.clone().lock_owned().await)
    }

    /// What the peer recorded under `hash`: nothing unpaired.
    pub(crate) fn recorded(&self, hash: &[u8; 32]) -> Result<Recorded, Failure> {
        let Some(paired) = self.0.get() else {
            return Ok(None);
        };
        (paired.recorded)(hash).map_err(unreadable)
    }
}

fn unreadable(error: String) -> Failure {
    tracing::error!(%error, "the hub's other payment service cannot be read");
    Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
}

/// What the component `component` of the hub's database at `db` recorded
/// under `hash` in its `table`, if the gateway ever ran there: read only,
/// beside the gateway running or without it.
fn recorded_file(
    db: Option<&Path>,
    component: &str,
    table: &str,
    hash: &[u8; 32],
) -> Result<Recorded, String> {
    let Some(db) = db else { return Ok(None) };
    let Some(conn) = reader(db, component).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let row: Option<Option<[u8; 32]>> = conn
        .query_row(
            &format!("SELECT lock FROM {table} WHERE hash = ?1"),
            [hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(row.map(|lock| lock.map(ProposalId::from_bytes)))
}

/// A read only connection to the hub's database at `db`, if it has one and
/// the gateway of `component` made its tables in it.
pub fn reader(db: &Path, component: &str) -> anyhow::Result<Option<rusqlite::Connection>> {
    let Some(conn) = licash_store::inspect(db, Kind::Hub)? else {
        return Ok(None);
    };
    // The layout of each gateway's tables this hub reads.
    let reads = match component {
        deposit::COMPONENT => deposit::VERSION,
        _ => 1,
    };
    match licash_store::component_version(&conn, component)? {
        Some(v) if v == reads => Ok(Some(conn)),
        Some(v) => anyhow::bail!("{component} of version {v}, this hub reads {reads}"),
        None => Ok(None),
    }
}

/// Which of `hashes` `query`, of a hash, finds among the tables of the
/// gateway `component` in the hub's database at `db`, if the gateway ever
/// ran there: read only, beside it running or without it.
pub(crate) fn recorded_in(
    db: &Path,
    component: &str,
    query: &str,
    hashes: &[[u8; 32]],
) -> anyhow::Result<HashSet<[u8; 32]>> {
    let mut found = HashSet::new();
    let Some(conn) = reader(db, component)? else {
        return Ok(found);
    };
    let mut stmt = conn.prepare(query)?;
    for hash in hashes {
        if stmt.exists([hash])? {
            found.insert(*hash);
        }
    }
    Ok(found)
}

/// Make the hub's Lightning gateway and its deposits peers, once both
/// are open.
pub fn pair(receipts: &Arc<lightning::Gateway>, deposits: &Arc<deposit::Gateway>) {
    link(
        Some(receipts),
        Some(deposits),
        Arc::new(Mutex::new(())),
        None,
    );
}

/// Coordinate all enabled payment services, including a lone gateway and
/// an issuer. Disabled services still reserve their recorded hashes.
pub fn coordinate(
    receipts: Option<&Arc<lightning::Gateway>>,
    deposits: Option<&Arc<deposit::Gateway>>,
    data: &Path,
    turn: Arc<Mutex<()>>,
) {
    link(receipts, deposits, turn, Some(data));
}

fn link(
    receipts: Option<&Arc<lightning::Gateway>>,
    deposits: Option<&Arc<deposit::Gateway>>,
    turn: Arc<Mutex<()>>,
    data: Option<&Path>,
) {
    // The swap provider's tables are in its wallet's file.
    let swaps = data.map(|d| d.join("swaps/wallet.sqlite"));
    if let Some(receipts) = receipts {
        let other = deposits.map(Arc::downgrade);
        let file = data.map(licash_contracts::hub_db);
        let recorded = move |hash: &[u8; 32]| match &other {
            Some(other) => other
                .upgrade()
                .ok_or_else(|| "the deposits stopped".to_owned())?
                .recorded(hash),
            None => recorded_file(file.as_deref(), "deposits", "deposits", hash),
        };
        let paired = Paired {
            turn: turn.clone(),
            recorded: Box::new(recorded),
            swaps: swaps.clone(),
        };
        assert!(receipts.peer.0.set(paired).is_ok(), "paired once");
    }
    if let Some(deposits) = deposits {
        let other = receipts.map(Arc::downgrade);
        let file = data.map(licash_contracts::hub_db);
        let recorded = move |hash: &[u8; 32]| match &other {
            Some(other) => other
                .upgrade()
                .ok_or_else(|| "the Lightning gateway stopped".to_owned())?
                .recorded(hash),
            None => recorded_file(file.as_deref(), "lightning", "lightning_receipts", hash),
        };
        let paired = Paired {
            turn,
            recorded: Box::new(recorded),
            swaps,
        };
        assert!(deposits.peer.0.set(paired).is_ok(), "paired once");
    }
}
