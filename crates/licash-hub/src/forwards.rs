//! Payments the hub makes outside its ledger, such as over Lightning, for
//! a lock that pays the hub only: the preimage the payment buys settles
//! the lock.
//!
//! A forward is recorded before the payment starts, in the transaction
//! that checks its lock, and resolved once the paying node knows it paid
//! or failed, or the lock is settled by its preimage. While it is pending
//! its lock is not refunded. One live forward per hash: a payment's
//! preimage settles one lock only. A failed forward refunds its lock at
//! once, as the hub gives up a claim it never paid for: an activation of
//! the hash after that owes no one.
//!
//! A forward by [`LOCAL`] is paid inside the hub: the payee is a receipt
//! of the hub's own gateway, which backs its own lock under the hash with
//! the forward's lock rather than an HTLC. No node pays it: the preimage
//! the payee reveals to claim the own lock pays it.

use licash::spend::{lock_hash, ProposalId};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    locks::{settled_of, tip},
    notify, now, sql, Error, Hub, Notice, Refusal,
};

/// The node of a forward paid inside the hub, to a receipt of its own
/// gateway: no Lightning node's key.
pub const LOCAL: &str = "local";

/// A payment the hub makes outside its ledger for a lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forward {
    pub lock: ProposalId,
    pub hash: [u8; 32],
    /// What to pay, such as a Lightning invoice.
    pub request: String,
    /// The node paying it, as it names itself.
    pub node: String,
    /// What the lock pays the hub.
    pub amount: u64,
    /// The lock's expiry: the payment must be resolved well before it.
    pub expiry: u32,
    pub state: Forwarded,
    /// Unix seconds.
    pub created_at: i64,
}

/// Where a forward stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Forwarded {
    Pending,
    /// Paid, for this preimage: its lock pays the hub.
    Paid([u8; 32]),
    /// Failed, and why: its lock is refunded.
    Failed(String),
}

/// Forwards.
impl Hub {
    /// Record the payment of `request`, of `hash`, by `node`, for lock
    /// `id`, before it starts: the lock must be under `hash`, pay the hub
    /// only, at least `least`, and not be settled or expired. The same
    /// request for the same lock gets the same forward while it is
    /// pending; a forward paid or failed is not made again.
    pub fn forward(
        &self,
        id: &ProposalId,
        hash: &[u8; 32],
        request: &str,
        node: &str,
        least: u64,
    ) -> Result<Forward, Error> {
        let new = self.new_forward(id, hash, request, node, least)?;
        let forward = self.store.write(|tx| record(tx, &new))?;
        tracing::info!(proposal = %hex::encode(id.as_bytes()), amount = new.amount, "forwarding");
        Ok(forward)
    }

    /// Refuse the payment of `request`, of `hash`, by `node`, for lock
    /// `id`, for `reason`: recorded as [`Hub::forward`] records it,
    /// whatever the lock pays, but failed in the same transaction, which
    /// refunds the lock. Never pending, it is never one to start.
    pub fn forward_refused(
        &self,
        id: &ProposalId,
        hash: &[u8; 32],
        request: &str,
        node: &str,
        reason: &str,
    ) -> Result<(), Error> {
        let new = self.new_forward(id, hash, request, node, 0)?;
        self.refund_failed(id, reason, new.amount, Some(&new))?;
        tracing::warn!(proposal = %hex::encode(id.as_bytes()), reason, "forward refused");
        Ok(())
    }

    /// The forward of `request` for lock `id` to record, if the lock is
    /// under `hash`, pays the hub only, and at least `least`.
    fn new_forward<'a>(
        &self,
        id: &ProposalId,
        hash: &[u8; 32],
        request: &'a str,
        node: &'a str,
        least: u64,
    ) -> Result<New<'a>, Error> {
        let lock = self.locked(id)?;
        if lock.own {
            return Err(Refusal::Forward("the hub's own lock").into());
        }
        let outputs = lock.proposal.outputs();
        let charge = self.lock_charge(&lock.proposal)?;
        if charge.count(licash::claim::Branch::Pay) != outputs.len() {
            return Err(Refusal::Forward("the lock pays others than the hub").into());
        }
        if charge.pay < least {
            return Err(Refusal::Fee {
                due: least,
                paid: charge.pay,
            }
            .into());
        }
        let terms = lock.proposal.lock().expect("a lock").terms().clone();
        if terms.hash() != hash {
            return Err(Refusal::Forward("the payment is not of the lock's hash").into());
        }
        Ok(New {
            id: *id,
            hash: *hash,
            expiry: terms.expiry(),
            request,
            node,
            amount: charge.pay,
            synced: self.is_synced(),
        })
    }

    /// The forward for lock `id` paid, buying `preimage`: the lock pays the
    /// hub. Paid after it was told failed, the hub lost the payment, and
    /// alerts its operator.
    pub fn forward_paid(&self, id: &ProposalId, preimage: &[u8; 32]) -> Result<(), Error> {
        let forward = self.forward_of(id)?.ok_or(Refusal::UnknownLock)?;
        if lock_hash(preimage) != forward.hash {
            return Err(Refusal::Preimage.into());
        }
        match forward.state {
            Forwarded::Pending => {}
            Forwarded::Paid(_) => return Ok(()),
            Forwarded::Failed(_) => {
                let proposal = hex::encode(id.as_bytes());
                self.alert(&format!(
                    "a payment told failed was made: the lock {proposal} of {} was refunded",
                    forward.amount
                ))?;
                return Err(Refusal::Forward("the payment failed before").into());
            }
        }
        // Settling the lock resolves its forward.
        self.settle(id, preimage)?;
        tracing::info!(proposal = %hex::encode(id.as_bytes()), "forward paid");
        Ok(())
    }

    /// The forward for lock `id` failed for `reason`, and can no longer
    /// pay: the lock is refunded at once. Refused as [`Refusal::Settled`]
    /// once the lock was paid, which resolved the forward.
    pub fn forward_failed(&self, id: &ProposalId, reason: &str) -> Result<(), Error> {
        let forward = self.forward_of(id)?.ok_or(Refusal::UnknownLock)?;
        match forward.state {
            Forwarded::Pending => {}
            Forwarded::Failed(_) => return Ok(()),
            Forwarded::Paid(_) => return Err(Refusal::Settled.into()),
        }
        self.refund_failed(id, reason, forward.amount, None)?;
        tracing::warn!(proposal = %hex::encode(id.as_bytes()), reason, "forward failed");
        Ok(())
    }

    /// The forward for lock `id`, if any.
    pub fn forward_of(&self, id: &ProposalId) -> Result<Option<Forward>, Error> {
        self.store.read(|c| forward_of(c, id))
    }

    /// The forward of `hash` pending or paid, if any: one at most.
    pub fn live_forward(&self, hash: &[u8; 32]) -> Result<Option<Forward>, Error> {
        self.store.read(|c| {
            Ok(c.query_row(
                &format!("{FORWARDS} WHERE hash = ?1 AND state IN ('pending', 'paid')"),
                [hash],
                forward,
            )
            .optional()?)
        })
    }

    /// The forwards pending, oldest first.
    pub fn forwards(&self) -> Result<Vec<Forward>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(&format!(
                "{FORWARDS} WHERE state = 'pending' ORDER BY created_at, rowid"
            ))?;
            let rows = stmt.query_map([], forward)?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

/// A forward checked against its lock, to record.
pub(crate) struct New<'a> {
    id: ProposalId,
    hash: [u8; 32],
    expiry: u32,
    request: &'a str,
    node: &'a str,
    amount: u64,
    /// Whether the hub followed the chain when checked.
    synced: bool,
}

/// Record `new` pending, in `tx`: the forward already recorded for its
/// lock if pending and of the same request; refused if the lock is settled
/// or expired, or another forward of its hash is live.
pub(crate) fn record(tx: &rusqlite::Transaction<'_>, new: &New<'_>) -> Result<Forward, Error> {
    let id = &new.id;
    if let Some(forward) = forward_of(tx, id)? {
        return match forward.state {
            _ if forward.request != new.request => {
                Err(Refusal::Forward("the lock pays another payment").into())
            }
            Forwarded::Pending => Ok(forward),
            Forwarded::Paid(_) => Err(Refusal::Settled.into()),
            Forwarded::Failed(_) => Err(Refusal::Forward("the payment failed").into()),
        };
    }
    if settled_of(tx, id)?.is_some() {
        return Err(Refusal::Settled.into());
    }
    let (top, _) = tip(tx)?.ok_or(Refusal::NoChain)?;
    if top >= new.expiry {
        return Err(Refusal::LockExpired.into());
    }
    if !new.synced {
        return Err(Refusal::Unsynced.into());
    }
    let live: bool = tx
        .query_row(
            "SELECT 1 FROM forwards WHERE hash = ?1 AND state IN ('pending', 'paid')",
            [new.hash],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if live {
        return Err(Refusal::Forward("a payment of this hash is under way or made").into());
    }
    tx.execute(
        "INSERT INTO forwards (lock, hash, request, node, amount, expiry, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
        params![
            id.as_bytes(),
            new.hash,
            new.request,
            new.node,
            sql(new.amount),
            new.expiry,
            now()
        ],
    )?;
    forward_of(tx, id)?.ok_or(Error::Halted)
}

/// Whether a forward for lock `id` is pending.
pub(crate) fn pending(c: &Connection, id: &ProposalId) -> Result<bool, Error> {
    Ok(c.query_row(
        "SELECT 1 FROM forwards WHERE lock = ?1 AND state = 'pending'",
        [id.as_bytes()],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

/// Resolve the pending forward for lock `id`, if any, as paid for
/// `preimage`, in the transaction that settles the lock.
pub(crate) fn paid(
    tx: &rusqlite::Transaction<'_>,
    id: &ProposalId,
    preimage: &[u8; 32],
) -> Result<(), Error> {
    let amount: Option<i64> = tx
        .query_row(
            "UPDATE forwards SET state = 'paid', preimage = ?2, resolved_at = ?3
             WHERE lock = ?1 AND state = 'pending' RETURNING amount",
            params![id.as_bytes(), preimage, now()],
            |r| r.get(0),
        )
        .optional()?;
    match amount {
        Some(amount) => notify(
            tx,
            &Notice::Forwarded {
                proposal: hex::encode(id.as_bytes()),
                amount: amount as u64,
                failed: None,
            },
        ),
        None => Ok(()),
    }
}

/// Resolve the pending forward for lock `id` as failed for `reason`, in
/// the transaction that refunds the lock.
pub(crate) fn fail(
    tx: &rusqlite::Transaction<'_>,
    id: &ProposalId,
    reason: &str,
    amount: u64,
) -> Result<(), Error> {
    let updated = tx.execute(
        "UPDATE forwards SET state = 'failed', reason = ?2, resolved_at = ?3
         WHERE lock = ?1 AND state = 'pending'",
        params![id.as_bytes(), reason, now()],
    )?;
    if updated != 1 {
        return Err(Refusal::Forward("no payment under way for the lock").into());
    }
    notify(
        tx,
        &Notice::Forwarded {
            proposal: hex::encode(id.as_bytes()),
            amount,
            failed: Some(reason.to_owned()),
        },
    )
}

const FORWARDS: &str = "SELECT lock, hash, request, node, amount, expiry, state, preimage,
    reason, created_at FROM forwards";

fn forward_of(c: &Connection, id: &ProposalId) -> Result<Option<Forward>, Error> {
    Ok(c.query_row(
        &format!("{FORWARDS} WHERE lock = ?1"),
        [id.as_bytes()],
        forward,
    )
    .optional()?)
}

fn forward(r: &Row<'_>) -> rusqlite::Result<Forward> {
    let state: String = r.get(6)?;
    let preimage: Option<[u8; 32]> = r.get(7)?;
    let state = match (state.as_str(), preimage) {
        ("paid", Some(preimage)) => Forwarded::Paid(preimage),
        ("failed", _) => Forwarded::Failed(r.get::<_, Option<String>>(8)?.unwrap_or_default()),
        _ => Forwarded::Pending,
    };
    Ok(Forward {
        lock: ProposalId::from_bytes(r.get(0)?),
        hash: r.get(1)?,
        request: r.get(2)?,
        node: r.get(3)?,
        amount: r.get::<_, i64>(4)? as u64,
        expiry: r.get(5)?,
        state,
        created_at: r.get(9)?,
    })
}
