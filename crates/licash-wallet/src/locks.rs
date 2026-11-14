//! Timed locks: notes that pay a payee on the preimage of a hash, or return
//! to the wallet once the lock expired unpaid.
//!
//! A lock is recorded before its hub hears of it, as a send is. Its hub
//! certifies it, then settles one branch: the pay branch once someone
//! reveals the preimage, the refund branch once the lock expired at depth
//! unactivated. The wallet keeps its notes of the branch settled, its
//! change or its refund. A lock its hub refused before certifying it was
//! never recorded there, so it is given up and its notes freed.
//!
//! The wallet reveals the preimage of a lock that pays it only well before
//! the lock expires, while its hub must still pay it.

use licash::{
    address::{Address, Ephemeral},
    authorization::Authorization,
    claim::Branch,
    note::{Keyset, Note},
    spend::{lock_hash, Proposal, ProposalId, TimedLock, TimedTerms},
};
use licash_api::{Code, Issued, Locked, Refund, Settle, Settled};
use rusqlite::{params, OptionalExtension, Row};

use crate::{
    coins, hub_url, insert_note, invalid, now, payment_id, sql, unavailable, Error, Received, Step,
    Transport, Wallet,
};

/// Blocks before its expiry after which the wallet no longer claims a
/// lock: its hub pays a claim only before the lock expires.
pub const CLAIM_MARGIN: u32 = 3;

/// A lock made, and where it stands.
#[derive(Clone, Debug)]
pub struct Lock {
    pub id: String,
    pub hub: String,
    pub payee: Address,
    pub hash: [u8; 32],
    /// Paid to the payee.
    pub amount: u64,
    /// Paid to the hub on top, in the pay branch.
    pub fee: u64,
    /// The height from which, at its depth, the lock is refunded unpaid.
    pub expiry: u32,
    pub state: LockState,
    /// The Lightning invoice the lock pays through its hub, if any.
    pub invoice: Option<String>,
    /// Why its hub failed or refused to pay the invoice: the lock is
    /// refunded.
    pub failure: Option<String>,
    pub created_at: i64,
}

/// A lock at a hub that pays this wallet, as its hub holds it: certified,
/// so that its hub owes one branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    pub id: String,
    pub hub: String,
    /// What its pay branch pays this wallet.
    pub amount: u64,
    pub hash: [u8; 32],
    pub expiry: u32,
    /// The height its hub scanned to.
    pub height: u32,
    /// The branch its hub issued, if any.
    pub settled: Option<Settled>,
    /// Known once it was paid or activated on Bitcoin.
    pub preimage: Option<[u8; 32]>,
}

/// Where a lock the wallet made stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockState {
    /// Not certified yet: its hub may refuse it, which frees its notes.
    Requested,
    /// Certified: it pays on the preimage, or is refunded once expired.
    Locked,
    /// Paid on this preimage, and its change kept.
    Paid([u8; 32]),
    /// Refunded, and the refund kept.
    Refunded,
    /// Given up, its notes freed, and why.
    Abandoned(String),
}

/// Notes chosen of one keyset.
type Chosen = (Keyset, Vec<[u8; 33]>);

/// The branch a hub issued, its issuances and the preimage it paid on.
type Settlement = (Branch, Issued, Option<[u8; 32]>);

/// Timed locks.
impl<T: Transport> Wallet<T> {
    /// Lock `amount` to `payee` through the hub at `url`, paid on the
    /// preimage of `hash` until `expiry`, by default the latest its hub
    /// admits. The pay branch pays the hub its lock fee and returns the
    /// change; the refund branch pays it its refund fee and returns the
    /// rest. A lock to the hub itself pays the hub only, which can then
    /// settle it at any time: it spends notes worth `amount` exactly, which
    /// the wallet makes with a payment to itself when it holds none.
    pub async fn lock(
        &self,
        url: &str,
        payee: &Address,
        amount: u64,
        hash: [u8; 32],
        expiry: Option<u32>,
    ) -> Result<Lock, Error> {
        self.lock_paying(url, payee, amount, hash, expiry, None)
            .await
    }

    /// [`Wallet::lock`], paying `invoice` through the hub once certified.
    pub(crate) async fn lock_paying(
        &self,
        url: &str,
        payee: &Address,
        amount: u64,
        hash: [u8; 32],
        expiry: Option<u32>,
        invoice: Option<&str>,
    ) -> Result<Lock, Error> {
        let url = hub_url(url)?;
        if amount == 0 {
            return Err(Error::Invalid("nothing to lock".into()));
        }
        let (hub, info) = self.refresh(&url).await?;
        let locks = info.locks.as_ref().ok_or_else(|| {
            Error::Invalid(format!("hub {url} follows no chain: it takes no locks"))
        })?;
        let checkpoint = locks.checkpoint().map_err(|p| unavailable(&url, p))?;
        let depth = locks.min_depth.max(1);
        let latest = locks
            .latest_expiry(depth)
            .map_err(|p| unavailable(&url, p))?;
        let expiry = expiry.unwrap_or(latest);
        if expiry <= locks.height || expiry > latest {
            return Err(Error::Invalid(format!(
                "expiry {expiry} outside what hub {url} admits, {} to {latest}",
                locks.height + 1
            )));
        }
        let to_hub = *payee == info.address;
        let (keyset, inputs, total, fee) = if to_hub {
            let (keyset, inputs) = match self.exact(hub, amount)? {
                Some(exact) => exact,
                None => {
                    tracing::info!(%url, amount, "making exact notes to lock");
                    self.send(&url, &self.address(), amount).await?;
                    self.exact(hub, amount)?.ok_or(Error::Insufficient {
                        needed: amount,
                        available: 0,
                    })?
                }
            };
            (keyset, inputs, amount, 0)
        } else {
            let (keyset, selection) = self.select(hub, amount, &info.fees.lock)?;
            (keyset, selection.inputs, selection.total, selection.fee)
        };
        let refund_fee = info.fees.refund.due(total);
        if refund_fee > total {
            return Err(Error::Insufficient {
                needed: refund_fee,
                available: total,
            });
        }
        let notes = self.notes_of(&keyset, &inputs)?;
        let me = self.address();
        let ephemeral = Ephemeral::random();
        let pay = [
            (payee, amount),
            (&info.address, fee),
            (&me, total - amount - fee),
        ];
        let refund = [(&info.address, refund_fee), (&me, total - refund_fee)];
        let terms = TimedTerms::new(
            checkpoint.profile(),
            checkpoint.commitment(),
            expiry,
            depth,
            hash,
        )?;
        let refund = crate::outputs(&keyset, &ephemeral, Branch::Refund, &refund)?;
        let pay = crate::outputs(&keyset, &ephemeral, Branch::Pay, &pay)?;
        let timed = TimedLock::new(terms, refund)?;
        let proposal = Proposal::new(&keyset, &notes, ephemeral.public(), pay, Some(timed))?;
        let id = proposal.id();
        let signed = self.sign_as_owner(&keyset, proposal, &notes, Authorization::Plain)?;
        let request =
            serde_json::to_string(&licash_api::Lock::new(&signed, &notes)).map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO locks
                    (id, hub, payee, hash, expiry, amount, fee, request, invoice, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id.as_bytes(),
                    hub,
                    payee.to_bytes(),
                    hash,
                    expiry,
                    sql(amount),
                    sql(fee),
                    request,
                    invoice,
                    now()
                ],
            )?;
            crate::take(tx, &id, &inputs)
        })?;
        tracing::info!(id = %hex::encode(id.as_bytes()), %url, amount, fee, expiry, "locking");
        self.advance_lock(&id).await
    }

    /// Notes of one keyset of `hub` worth `amount` exactly, the largest
    /// first: exact whenever any are, as each denomination divides the
    /// larger ones.
    pub(crate) fn exact(&self, hub: i64, amount: u64) -> Result<Option<Chosen>, Error> {
        for keyset in self.keysets(hub)? {
            let held = self.store.read(|c| {
                let mut stmt = c.prepare(
                    "SELECT denom, y FROM notes WHERE keyset = ?1
                     AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
                )?;
                let rows = stmt.query_map([keyset.id()], |r| {
                    Ok((r.get::<_, i64>(0)? as u64, r.get::<_, [u8; 33]>(1)?))
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?;
            if let Some(inputs) = coins::exact(&held, amount) {
                return Ok(Some((keyset, inputs)));
            }
        }
        Ok(None)
    }

    /// Take lock `id` on: have its hub certify it, then keep the wallet's
    /// notes of the branch its hub settled, asking for the refund once the
    /// lock expired at depth. Where it stands: a lock waiting on its payee
    /// or its expiry is [`LockState::Locked`].
    pub async fn follow_lock(&self, id: &str) -> Result<Lock, Error> {
        self.advance_lock(&payment_id(id)?).await
    }

    pub(crate) async fn advance_lock(&self, id: &ProposalId) -> Result<Lock, Error> {
        let pending = |e: Error| match e {
            Error::Refused { .. } | Error::Pending { .. } | Error::Abandoned { .. } => e,
            e => Error::Pending {
                id: hex::encode(id.as_bytes()),
                reason: e.to_string(),
            },
        };
        self.advance_lock_from(id).await.map_err(pending)?;
        self.lock_record(id)
    }

    async fn advance_lock_from(&self, id: &ProposalId) -> Result<(), Error> {
        type Row = (
            i64,
            String,
            String,
            bool,
            bool,
            Option<String>,
            Option<String>,
        );
        let row: Row = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT l.hub, r.url, l.request, l.certificate IS NOT NULL,
                    l.done_at IS NOT NULL, l.abandoned,
                    CASE WHEN l.failure IS NULL THEN l.invoice END
                 FROM locks l JOIN reach r ON r.hub = l.hub WHERE l.id = ?1",
                [id.as_bytes()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?)
        })?;
        let (hub, url, request, certified, done, abandoned, invoice) = row;
        if let Some(reason) = abandoned {
            let id = hex::encode(id.as_bytes());
            return Err(Error::Abandoned { id, reason });
        }
        if done {
            return Ok(());
        }
        let keysets = self.keysets(hub)?;
        let lookup = |id: &[u8; 32]| keysets.iter().find(|k| k.id() == id);
        let request: licash_api::Lock = serde_json::from_str(&request).map_err(invalid)?;
        let (signed, notes) = request.decode(lookup).map_err(invalid)?;
        let keyset = lookup(signed.proposal().keyset_id())
            .ok_or_else(|| Error::Invalid("a lock under a keyset its hub no longer has".into()))?;
        if !certified {
            let locked = match self.post::<Locked>(&url, licash_api::LOCK, &request).await {
                Ok(locked) => locked,
                Err(Error::Refused { problem, .. }) => {
                    return Err(self.refused(&url, id, problem, Step::Lock).await?);
                }
                Err(e) => return Err(e),
            };
            let certificate = locked
                .decode(keyset, &signed, &notes)
                .map_err(|p| unavailable(&url, p))?;
            self.store.write(|tx| {
                tx.execute(
                    "UPDATE locks SET certificate = ?2 WHERE id = ?1 AND certificate IS NULL",
                    params![id.as_bytes(), certificate.to_bytes()],
                )?;
                Ok(())
            })?;
            tracing::info!(id = %hex::encode(id.as_bytes()), %url, "locked");
        }
        if let Some(invoice) = invoice {
            self.ask_payment(&url, id, &invoice).await?;
        }
        let terms = signed.proposal().lock().expect("a lock").terms().clone();
        let state: licash_api::LockState = self.get(&url, &licash_api::lock_state(id)).await?;
        let view = state.decode(lookup).map_err(|p| unavailable(&url, p))?;
        if view.proposal.id() != *id {
            return Err(unavailable(&url, "it answered with another lock"));
        }
        let settled = match (view.settled, view.preimage) {
            (Some(Settled::Refund), _) => self.refund_lock(&url, id).await?,
            (settled, Some(preimage)) => {
                let settle = Settle::new(id, &preimage);
                match self.post::<Issued>(&url, licash_api::SETTLE, &settle).await {
                    Ok(issued) => Some((Branch::Pay, issued, Some(preimage))),
                    // Revealed too late, past its expiry unactivated: the
                    // lock is refunded once expired at depth.
                    Err(Error::Refused { problem, .. })
                        if settled.is_none() && problem.code == Code::LockExpired =>
                    {
                        self.refund_deep(&url, id, &terms).await?
                    }
                    Err(Error::Refused { problem, .. }) => return Err(unavailable(&url, problem)),
                    Err(e) => return Err(e),
                }
            }
            (Some(Settled::Pay), None) => {
                return Err(unavailable(&url, "a lock paid without its preimage"));
            }
            (None, None) => self.refund_deep(&url, id, &terms).await?,
        };
        let Some((branch, issued, preimage)) = settled else {
            return Ok(());
        };
        let issued = issued.decode(keyset).map_err(|p| unavailable(&url, p))?;
        let proposal = signed.proposal();
        let ours = self.opened_branch(&url, keyset, proposal, branch, &issued)?;
        let ephemeral = proposal.ephemeral();
        let settled = match branch {
            Branch::Pay => "pay",
            Branch::Refund => "refund",
        };
        self.store.write(|tx| {
            for (r, note) in &ours {
                insert_note(tx, keyset, note, id, &ephemeral, r.branch, r.position)?;
            }
            tx.execute(
                "UPDATE locks SET settled = ?2, preimage = ?3, done_at = ?4
                 WHERE id = ?1 AND done_at IS NULL",
                params![id.as_bytes(), settled, preimage, now()],
            )?;
            Ok(())
        })?;
        tracing::info!(id = %hex::encode(id.as_bytes()), %url, settled, "lock settled");
        Ok(())
    }

    /// The refund of lock `id`, of `terms`, once its hub scanned past its
    /// expiry at depth.
    async fn refund_deep(
        &self,
        url: &str,
        id: &ProposalId,
        terms: &TimedTerms,
    ) -> Result<Option<Settlement>, Error> {
        let (_, info) = self.refresh(url).await?;
        let height = info.locks.map_or(0, |l| l.height);
        if height >= terms.expiry().saturating_add(terms.depth()) {
            self.refund_lock(url, id).await
        } else {
            Ok(None)
        }
    }

    /// The refund of lock `id`: none while the hub does not owe it yet, or
    /// owes the pay branch instead.
    async fn refund_lock(&self, url: &str, id: &ProposalId) -> Result<Option<Settlement>, Error> {
        match self
            .post::<Issued>(url, licash_api::REFUND, &Refund::new(id))
            .await
        {
            Ok(issued) => Ok(Some((Branch::Refund, issued, None))),
            Err(Error::Refused { problem, .. })
                if matches!(problem.code, Code::Unexpired | Code::Activated) =>
            {
                Ok(None)
            }
            // A refusal here gives nothing up: the hub owes one branch.
            Err(Error::Refused { problem, .. }) => Err(unavailable(url, problem)),
            Err(e) => Err(e),
        }
    }

    /// The wallet's notes of `branch` of `proposal`, opened from the hub's
    /// `issued` for that branch.
    fn opened_branch(
        &self,
        url: &str,
        keyset: &Keyset,
        proposal: &Proposal,
        branch: Branch,
        issued: &[licash::note::MintResponse],
    ) -> Result<Vec<(licash::address::Received, Note)>, Error> {
        self.owner
            .scan_key()
            .receive(proposal)?
            .into_iter()
            .filter(|r| r.branch == branch)
            .map(|r| {
                let note = issued
                    .get(r.position as usize)
                    .ok_or_else(|| unavailable(url, "it issued fewer outputs than locked"))?
                    .open(keyset, &r.derivation)
                    .map_err(|e| unavailable(url, format!("its issuance: {e}")))?;
                Ok((r, note))
            })
            .collect()
    }

    /// The lock `id` at the hub at `url`, as its hub holds it: what its
    /// pay branch pays this wallet, under which hash, until when, and where
    /// it stands. [`Error::PaysNothing`] unless it pays this wallet.
    pub async fn incoming(&self, url: &str, id: &str) -> Result<Incoming, Error> {
        let url = hub_url(url)?;
        let id = payment_id(id)?;
        let (info, _, view) = self.lock_view(&url, &id).await?;
        let proposal = &view.proposal;
        let outputs = proposal.outputs();
        let amount: u64 = self
            .owner
            .scan_key()
            .receive(proposal)?
            .iter()
            .filter(|r| r.branch == Branch::Pay)
            .map(|r| outputs[r.position as usize].denomination())
            .sum();
        if amount == 0 {
            return Err(Error::PaysNothing);
        }
        let terms = proposal.lock().expect("a lock").terms();
        Ok(Incoming {
            id: hex::encode(id.as_bytes()),
            hub: url,
            amount,
            hash: *terms.hash(),
            expiry: terms.expiry(),
            height: info.locks.map_or(0, |l| l.height),
            settled: view.settled,
            preimage: view.preimage,
        })
    }

    /// The lock `id` as the hub at `url` holds it, decoded and checked, with
    /// its keyset and what the hub says of itself.
    async fn lock_view(
        &self,
        url: &str,
        id: &ProposalId,
    ) -> Result<(licash_api::Info, Keyset, licash_api::LockView), Error> {
        let (hub, info) = self.refresh(url).await?;
        let keysets = self.keysets(hub)?;
        let lookup = |id: &[u8; 32]| keysets.iter().find(|k| k.id() == id);
        let state: licash_api::LockState = self.get(url, &licash_api::lock_state(id)).await?;
        let view = state.decode(lookup).map_err(|p| unavailable(url, p))?;
        if view.proposal.id() != *id {
            return Err(unavailable(url, "it answered with another lock"));
        }
        let keyset = lookup(view.proposal.keyset_id()).expect("decoded under it");
        Ok((info, keyset.clone(), view))
    }

    /// Take the notes the lock `id` at the hub at `url` pays this wallet,
    /// revealing `preimage`, the preimage of its hash. Refused without
    /// revealing it within [`CLAIM_MARGIN`] blocks of the lock's expiry, as
    /// the hub could then refund the lock and keep what the preimage buys.
    pub async fn claim(&self, url: &str, id: &str, preimage: &[u8; 32]) -> Result<Received, Error> {
        let url = hub_url(url)?;
        let id = payment_id(id)?;
        let (info, keyset, view) = self.lock_view(&url, &id).await?;
        let keyset = &keyset;
        let proposal = &view.proposal;
        let terms = proposal.lock().expect("a lock").terms();
        if lock_hash(preimage) != *terms.hash() {
            return Err(Error::Invalid("not the preimage of the lock's hash".into()));
        }
        let ours = self.owner.scan_key().receive(proposal)?;
        if !ours.iter().any(|r| r.branch == Branch::Pay) {
            return Err(Error::PaysNothing);
        }
        match view.settled {
            Some(Settled::Refund) => {
                return Err(Error::Invalid("the lock was refunded".into()));
            }
            // Revealed already.
            Some(Settled::Pay) => {}
            None => {
                let height = info.locks.map_or(u32::MAX, |l| l.height);
                if height.saturating_add(CLAIM_MARGIN) >= terms.expiry() {
                    return Err(Error::Invalid(format!(
                        "too late to claim: the lock expires at {}, its hub is at {height}",
                        terms.expiry()
                    )));
                }
            }
        }
        let issued: Issued = self
            .post(&url, licash_api::SETTLE, &Settle::new(&id, preimage))
            .await?;
        let issued = issued.decode(keyset).map_err(|p| unavailable(&url, p))?;
        let ours = self.opened_branch(&url, keyset, proposal, Branch::Pay, &issued)?;
        self.keep(keyset, url, proposal, &ours)
    }

    /// The locks made, newest first.
    pub fn locks(&self) -> Result<Vec<Lock>, Error> {
        self.store.read(|c| {
            let mut stmt =
                c.prepare(&format!("{LOCKS} ORDER BY l.created_at DESC, l.rowid DESC"))?;
            let rows = stmt.query_map([], lock_of)?;
            rows.map(|row| row?).collect()
        })
    }

    fn lock_record(&self, id: &ProposalId) -> Result<Lock, Error> {
        self.store.read(|c| {
            c.query_row(
                &format!("{LOCKS} WHERE l.id = ?1"),
                [id.as_bytes()],
                lock_of,
            )
            .optional()?
            .ok_or_else(|| Error::UnknownPayment(hex::encode(id.as_bytes())))?
        })
    }
}

const LOCKS: &str = "SELECT l.id, r.url, l.payee, l.hash, l.amount, l.fee, l.expiry,
    l.certificate IS NOT NULL, l.settled, l.preimage, l.done_at IS NOT NULL, l.abandoned,
    l.created_at, l.invoice, l.failure
    FROM locks l JOIN reach r ON r.hub = l.hub";

fn lock_of(r: &Row<'_>) -> rusqlite::Result<Result<Lock, Error>> {
    let id: [u8; 32] = r.get(0)?;
    let payee: Vec<u8> = r.get(2)?;
    let (amount, fee): (i64, i64) = (r.get(4)?, r.get(5)?);
    let certified: bool = r.get(7)?;
    let settled: Option<String> = r.get(8)?;
    let preimage: Option<[u8; 32]> = r.get(9)?;
    let done: bool = r.get(10)?;
    let abandoned: Option<String> = r.get(11)?;
    let state = match (abandoned, done, settled.as_deref(), preimage) {
        (Some(reason), ..) => LockState::Abandoned(reason),
        (None, true, Some("pay"), Some(preimage)) => LockState::Paid(preimage),
        (None, true, Some("refund"), _) => LockState::Refunded,
        _ if certified => LockState::Locked,
        _ => LockState::Requested,
    };
    let Ok(payee) = Address::from_bytes(&payee) else {
        return Ok(Err(Error::Invalid("a lock to no address".into())));
    };
    Ok(Ok(Lock {
        id: hex::encode(id),
        hub: r.get(1)?,
        payee,
        hash: r.get(3)?,
        amount: amount as u64,
        fee: fee as u64,
        expiry: r.get(6)?,
        state,
        invoice: r.get(13)?,
        failure: r.get(14)?,
        created_at: r.get(12)?,
    }))
}
