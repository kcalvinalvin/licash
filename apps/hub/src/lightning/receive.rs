//! Receipts: the hub's own notes, locked to a payee for a held invoice
//! paid, settled with the preimage that claims them.

use std::{
    collections::HashSet,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use licash::spend::{ProposalId, TimedTerms};
use licash_api::{Code, Hex, Problem, Receive, ReceiveState, ReceiveStatus, Receiving};
use licash_hub::{Error, Refusal, Settled};
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{node_down, now, refused, Gateway, HeldState};
use crate::{
    peer::{self, Recorded},
    Failure,
};

const VERSION: i64 = 1;

const SCHEMA: &str = "
-- Notes asked for over Lightning: recorded before their invoice exists,
-- locked once it is paid, and settled or canceled with their lock.
CREATE TABLE receipts (
    hash BLOB PRIMARY KEY,
    -- The receipt asked for, as JSON.
    body TEXT NOT NULL,
    amount INTEGER NOT NULL,
    msat INTEGER NOT NULL,
    request TEXT,
    lock BLOB,
    state TEXT NOT NULL CHECK (state IN ('open', 'locked', 'settled', 'canceled')),
    created_at INTEGER NOT NULL,
    done_at INTEGER
) STRICT;
CREATE INDEX live_receipts ON receipts (state) WHERE state IN ('open', 'locked');
";

/// The gateway's receipts, in a SQLite file of their own.
pub(super) struct Store(Mutex<Connection>);

/// A receipt as recorded.
#[derive(Clone, Debug)]
pub(super) struct Receipt {
    hash: [u8; 32],
    body: String,
    amount: u64,
    msat: u64,
    request: Option<String>,
    lock: Option<ProposalId>,
    state: ReceiveState,
    created_at: u64,
}

impl Store {
    /// The receipts at `path`, a new file or one this gateway wrote.
    pub(super) fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let tables: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            anyhow::ensure!(
                tables == 0,
                "{} is not the gateway's receipts",
                path.display()
            );
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else {
            anyhow::ensure!(
                version == VERSION,
                "receipts schema {version}, this gateway reads {VERSION}"
            );
        }
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, hash: &[u8; 32]) -> Result<Option<Receipt>, Failure> {
        let conn = self.conn();
        let receipt = conn
            .query_row(&format!("{RECEIPTS} WHERE hash = ?1"), [hash], receipt)
            .optional();
        receipt.map_err(store_down)
    }

    /// The receipts open or locked, oldest first.
    fn live(&self) -> Result<Vec<Receipt>, Failure> {
        let conn = self.conn();
        let live = conn
            .prepare(&format!(
                "{RECEIPTS} WHERE state IN ('open', 'locked') ORDER BY created_at, rowid"
            ))
            .and_then(|mut stmt| stmt.query_map([], receipt)?.collect());
        live.map_err(store_down)
    }

    /// Record a receipt unless one under its hash is.
    fn insert(&self, hash: &[u8; 32], body: &str, amount: u64, msat: u64) -> Result<(), Failure> {
        self.conn()
            .execute(
                "INSERT INTO receipts (hash, body, amount, msat, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'open', ?5) ON CONFLICT DO NOTHING",
                params![hash, body, sql(amount), sql(msat), sql(now())],
            )
            .map(drop)
            .map_err(store_down)
    }

    fn update(
        &self,
        hash: &[u8; 32],
        set: &str,
        value: &dyn rusqlite::ToSql,
    ) -> Result<(), Failure> {
        self.conn()
            .execute(
                &format!("UPDATE receipts SET {set} = ?2 WHERE hash = ?1"),
                params![hash, value],
            )
            .map(drop)
            .map_err(store_down)
    }

    fn finish(&self, hash: &[u8; 32], state: &str) -> Result<(), Failure> {
        self.conn()
            .execute(
                "UPDATE receipts SET state = ?2, done_at = ?3 WHERE hash = ?1",
                params![hash, state, sql(now())],
            )
            .map(drop)
            .map_err(store_down)
    }
}

const RECEIPTS: &str =
    "SELECT hash, body, amount, msat, request, lock, state, created_at FROM receipts";

fn receipt(r: &Row<'_>) -> rusqlite::Result<Receipt> {
    let state = match r.get::<_, String>(6)?.as_str() {
        "open" => ReceiveState::Open,
        "locked" => ReceiveState::Locked,
        "settled" => ReceiveState::Settled,
        _ => ReceiveState::Canceled,
    };
    Ok(Receipt {
        hash: r.get(0)?,
        body: r.get(1)?,
        amount: r.get::<_, i64>(2)? as u64,
        msat: r.get::<_, i64>(3)? as u64,
        request: r.get(4)?,
        lock: r.get::<_, Option<[u8; 32]>>(5)?.map(ProposalId::from_bytes),
        state,
        created_at: r.get::<_, i64>(7)? as u64,
    })
}

fn sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn store_down(e: rusqlite::Error) -> Failure {
    tracing::error!(error = %e, "the receipts cannot be read or written");
    Failure(Problem::new(
        Code::Unavailable,
        "the gateway cannot serve now",
    ))
}

impl Gateway {
    /// Whether a receipt is under `hash`, which no other lock of the hub's
    /// own notes may take.
    pub fn holds(&self, hash: &[u8; 32]) -> Result<bool, String> {
        Ok(self.recorded(hash)?.is_some())
    }

    /// Which of `hashes` the receipts file at `path` holds, if there is one.
    pub fn holding(path: &Path, hashes: &[[u8; 32]]) -> rusqlite::Result<HashSet<[u8; 32]>> {
        peer::recorded_in(path, "SELECT 1 FROM receipts WHERE hash = ?1", hashes)
    }

    /// The receipt under `hash`, if any, and the lock it backs once
    /// locked.
    pub fn recorded(&self, hash: &[u8; 32]) -> Result<Recorded, String> {
        match self.receipts.get(hash) {
            Ok(receipt) => Ok(receipt.map(|receipt| receipt.lock)),
            Err(Failure(problem)) => Err(problem.message),
        }
    }

    /// The invoice whose payment locks the hub's notes to the outputs of
    /// `body`: the same again for the same receipt.
    pub async fn receive(&self, body: &Receive) -> Result<Receiving, Failure> {
        let outputs = body.outputs()?;
        let (keyset, ephemeral, hash) = body.terms()?;
        if self.hub.keyset(&keyset).is_none() {
            return Err(Failure(Problem::new(Code::UnknownKeyset, "unknown keyset")));
        }
        let amount = outputs
            .iter()
            .try_fold(0u64, |sum, o| sum.checked_add(o.denomination()))
            .ok_or_else(|| Failure(Problem::invalid("the outputs overflow")))?;
        let c = &self.config;
        if outputs.is_empty() || amount < c.min_receive || amount > c.max_receive {
            return Err(refused("outside the amounts the gateway receives"));
        }
        let msat = amount
            .checked_add(c.receive.due(amount))
            .and_then(|sat| sat.checked_mul(1000))
            .ok_or_else(|| Failure(Problem::invalid("the amount overflows")))?;
        let canonical = Receive::new(&keyset, &ephemeral, &outputs, &hash);
        let canonical = serde_json::to_string(&canonical).expect("a receipt serializes");
        let turn = self.peer.turn().await;
        if self.receipts.get(&hash)?.is_none() {
            if self.peer.recorded(&hash)?.is_some() {
                return Err(refused("the hub holds this hash for another payment"));
            }
            if self
                .on_hub(move |hub| hub.own_lock_of(&hash))
                .await?
                .is_some()
            {
                return Err(refused("the hub locked notes under this hash before"));
            }
            self.receipts.insert(&hash, &canonical, amount, msat)?;
        }
        drop(turn);
        let receipt = self.receipts.get(&hash)?.ok_or_else(store_gone)?;
        if receipt.body != canonical {
            return Err(refused("another receipt under this hash"));
        }
        if let Some(request) = receipt.request {
            return Ok(Receiving { request });
        }
        let cltv = c.receive_cltv(self.hub.info().policy.timing.min_depth);
        let held = self
            .node
            .hold(hash, receipt.msat, cltv, c.invoice_expiry)
            .await;
        let request = match held {
            Ok(request) => request,
            // Made by an attempt that did not hear back.
            Err(e) => match self.node.invoice(hash).await {
                Ok(Some(held)) => held.request,
                _ => return Err(node_down(e)),
            },
        };
        self.receipts.update(&hash, "request", &request)?;
        tracing::info!(hash = %hex::encode(hash), amount, msat, "invoice issued");
        Ok(Receiving { request })
    }

    /// Where the receipt under `hash` stands.
    pub fn receive_status(&self, hash: &[u8; 32]) -> Result<ReceiveStatus, Failure> {
        let receipt = self.receipts.get(hash)?.ok_or_else(|| {
            Failure(Problem::new(
                Code::UnknownLock,
                "no receipt under this hash",
            ))
        })?;
        Ok(ReceiveStatus {
            state: receipt.state,
            lock: receipt.lock.map(|id| Hex(id.as_bytes().to_vec())),
        })
    }

    /// Lock the receipts paid, and settle or cancel those decided.
    pub(super) async fn follow_receipts(&self) -> Result<(), Failure> {
        for receipt in self.receipts.live()? {
            let followed = match receipt.state {
                ReceiveState::Open => self.accept(&receipt).await,
                _ => self.decide(&receipt).await,
            };
            if let Err(e) = followed {
                let hash = hex::encode(receipt.hash);
                tracing::warn!(%hash, error = %e.0.message, "cannot follow a receipt");
            }
        }
        Ok(())
    }
}

impl Gateway {
    /// Lock the hub's notes for `receipt` once its invoice is paid, if
    /// its HTLCs outlast the lock, its refund and the gateway's downtime
    /// by the node's hold delta.
    async fn accept(&self, receipt: &Receipt) -> Result<(), Failure> {
        let hash = receipt.hash;
        let name = hex::encode(hash);
        let Some(held) = self.node.invoice(hash).await.map_err(node_down)? else {
            // Never made, or lost by the node: given up once expired.
            let expired = receipt.created_at + self.config.invoice_expiry + self.config.grace;
            if now() > expired {
                self.finish(receipt, "canceled")?;
            }
            return Ok(());
        };
        match held.state {
            HeldState::Open => return Ok(()),
            HeldState::Canceled => return self.finish(receipt, "canceled"),
            HeldState::Settled => {
                let message = format!("The invoice of receipt {name} was settled unlocked.");
                self.alert(format!("settled {name}"), message).await;
                return Ok(());
            }
            HeldState::Accepted => {}
        }
        let body: Receive = serde_json::from_str(&receipt.body).map_err(|_| store_gone())?;
        let ((keyset, ephemeral, _), outputs) = (body.terms()?, body.outputs()?);
        let Some(least_expiry) = held.min_expiry else {
            return Ok(());
        };
        let info = self.node.info().await.map_err(node_down)?;
        let c = &self.config;
        let depth = self.hub.info().policy.timing.min_depth;
        // The latest a lock may expire, its HTLCs outlasting it, its refund
        // and the gateway's downtime by the node's hold delta.
        let latest = [c.hold_delta, depth, c.downtime, 1]
            .iter()
            .try_fold(least_expiry, |at, b| at.checked_sub(*b));
        // Until the lock under the hash is recorded.
        let turn = self.peer.turn().await;
        // A lock made before the gateway stopped, or for a deposit under the
        // same hash, which backs it once the deposits recorded it: one
        // payment per lock.
        if let Some(lock) = self.on_hub(move |hub| hub.own_lock_of(&hash)).await? {
            if self.peer.recorded(&hash)? == Some(Some(lock.proposal.id())) {
                drop(turn);
                let message = format!(
                    "The hash of receipt {name} is locked for a deposit: its invoice is canceled."
                );
                self.alert(format!("taken {name}"), message).await;
                return self.cancel(receipt).await;
            }
            let expiry = lock
                .proposal
                .lock()
                .ok_or_else(store_gone)?
                .terms()
                .expiry();
            if lock.proposal.outputs() != outputs.as_slice()
                || latest.is_none_or(|latest| expiry > latest)
            {
                let message = format!(
                    "The hash of receipt {name} is locked to other notes, or for longer than its \
                     HTLCs last."
                );
                self.alert(format!("taken {name}"), message).await;
                return Ok(());
            }
            self.receipts
                .update(&hash, "lock", &lock.proposal.id().as_bytes())?;
            return self.finish(receipt, "locked");
        }
        let (scanned, checkpoint) = self
            .on_hub(|hub| Ok((hub.scanned()?, hub.checkpoint())))
            .await?;
        let (Some((tip, _)), Some(checkpoint)) = (scanned, checkpoint) else {
            return Ok(());
        };
        let expiry = tip.max(info.height).saturating_add(c.receive_blocks);
        if latest.is_none_or(|latest| expiry > latest) {
            drop(turn);
            tracing::warn!(hash = %name, least_expiry, expiry, "HTLCs expire too soon");
            return self.cancel(receipt).await;
        }
        let terms = TimedTerms::new(
            checkpoint.profile(),
            checkpoint.commitment(),
            expiry,
            depth,
            hash,
        )
        .map_err(|e| Failure(Problem::invalid(e.to_string())))?;
        let locked = self
            .on_hub(move |hub| hub.lock_own(&keyset, ephemeral, outputs, terms))
            .await;
        match locked {
            Ok(lock) => {
                let id = lock.proposal.id();
                self.receipts.update(&hash, "lock", &id.as_bytes())?;
                self.finish(receipt, "locked")?;
                tracing::info!(hash = %name, lock = %hex::encode(id.as_bytes()), expiry, "receipt locked");
                Ok(())
            }
            // Retried: the hub catches up, or adopts its lock above.
            Err(Error::Refused(Refusal::Unsynced | Refusal::NoChain | Refusal::Exists)) => Ok(()),
            Err(Error::Refused(refusal)) => {
                drop(turn);
                tracing::warn!(hash = %name, %refusal, "the hub refuses to lock a receipt");
                self.cancel(receipt).await
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Settle the invoice of locked `receipt` once its lock paid, or
    /// cancel it once its lock is refunded, which the gateway asks for
    /// once the lock expired at depth.
    async fn decide(&self, receipt: &Receipt) -> Result<(), Failure> {
        let id = receipt.lock.ok_or_else(store_gone)?;
        let name = hex::encode(receipt.hash);
        let lock = self.on_hub(move |hub| hub.locked(&id)).await?;
        let terms = lock.proposal.lock().ok_or_else(store_gone)?.terms().clone();
        match (lock.settled, lock.preimage) {
            (Some(Settled::Refund), _) => self.cancel(receipt).await,
            (Some(Settled::Pay), Some(preimage)) => self.settle_invoice(receipt, preimage).await,
            // Activated on Bitcoin: the lock pays.
            (None, Some(preimage)) => {
                self.on_hub(move |hub| hub.settle(&id, &preimage)).await?;
                self.settle_invoice(receipt, preimage).await
            }
            (None, None) => {
                let tip = self
                    .on_hub(|hub| hub.scanned())
                    .await?
                    .map_or(0, |(h, _)| h);
                let decided = terms.expiry().saturating_add(terms.depth());
                if tip < decided {
                    return Ok(());
                }
                match self.on_hub(move |hub| hub.refund(&id)).await {
                    Ok(_) => self.cancel(receipt).await,
                    Err(Error::Refused(Refusal::Unexpired | Refusal::Activated)) => Ok(()),
                    Err(e) => {
                        if tip >= decided.saturating_add(self.config.downtime) {
                            let message = format!(
                                "The lock of receipt {name} is unrefunded past block {decided}: \
                                 its invoice may be canceled before."
                            );
                            self.alert(format!("unrefunded {name}"), message).await;
                        }
                        Err(e.into())
                    }
                }
            }
            (Some(Settled::Pay), None) => Err(store_gone()),
        }
    }

    async fn settle_invoice(&self, receipt: &Receipt, preimage: [u8; 32]) -> Result<(), Failure> {
        let settled = self.node.settle(preimage).await;
        let state = match settled {
            Ok(()) => HeldState::Settled,
            Err(e) => match self.node.invoice(receipt.hash).await {
                Ok(Some(held)) => held.state,
                _ => return Err(node_down(e)),
            },
        };
        match state {
            HeldState::Settled => {
                tracing::info!(hash = %hex::encode(receipt.hash), "receipt settled");
                self.finish(receipt, "settled")
            }
            HeldState::Canceled => {
                let name = hex::encode(receipt.hash);
                let message = format!(
                    "The invoice of receipt {name} was canceled after its lock paid: the hub \
                     lost {}.",
                    licash_notify::sat(receipt.amount)
                );
                self.alert(format!("lost {name}"), message).await;
                self.finish(receipt, "canceled")
            }
            _ => Ok(()),
        }
    }

    async fn cancel(&self, receipt: &Receipt) -> Result<(), Failure> {
        if let Err(e) = self.node.cancel(receipt.hash).await {
            let held = self.node.invoice(receipt.hash).await.map_err(node_down)?;
            if held.is_none_or(|held| held.state != HeldState::Canceled) {
                return Err(node_down(e));
            }
        }
        tracing::info!(hash = %hex::encode(receipt.hash), "receipt canceled");
        self.finish(receipt, "canceled")
    }

    fn finish(&self, receipt: &Receipt, state: &str) -> Result<(), Failure> {
        if state == "locked" {
            return self.receipts.update(&receipt.hash, "state", &state);
        }
        self.receipts.finish(&receipt.hash, state)
    }
}

/// The receipts do not read as written.
fn store_gone() -> Failure {
    tracing::error!("a receipt does not read as written");
    Failure(Problem::new(
        Code::Unavailable,
        "the gateway cannot serve now",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_receipts_file_is_new_or_the_gateways() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lightning.sqlite");
        let store = Store::open(&path).unwrap();
        store.insert(&[1; 32], "{}", 5, 6000).unwrap();
        store.insert(&[1; 32], "{\"other\":1}", 7, 8000).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let receipt = store.get(&[1; 32]).unwrap().unwrap();
        assert_eq!(
            (receipt.body.as_str(), receipt.amount, receipt.msat),
            ("{}", 5, 6000)
        );
        assert_eq!(receipt.state, ReceiveState::Open);
        assert_eq!(store.live().unwrap().len(), 1);
        store.finish(&[1; 32], "canceled").unwrap();
        assert!(store.live().unwrap().is_empty());
        drop(store);

        let other = dir.path().join("other.sqlite");
        let conn = Connection::open(&other).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
        drop(conn);
        assert!(Store::open(&other).is_err());
        let newer = dir.path().join("newer.sqlite");
        let conn = Connection::open(&newer).unwrap();
        conn.pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        drop(conn);
        assert!(Store::open(&newer).is_err());
    }
}
