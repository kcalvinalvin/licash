//! Notes claimed on Bitcoin: taken by a contract's force close out of every
//! payment, as a payment takes the notes it spends, and what authorizes
//! them in the claim.
//!
//! A claim's notes are taken under the contract's id: no send, lock,
//! renewal or unlink selects them from then on, and the balance counts
//! them no more (the contract's recovery shows them). They are given back
//! only while the recovery can still be given up, before the holder signs
//! her request on Bitcoin; after, they stay taken for good.
//!
//! Which notes each contract claims is decided for all the contracts at a
//! hub together ([`ClaimSlot`]), so that what a wallet shows protected is
//! what its recoveries would claim: the most recovered in all, then the
//! least face value spent beyond the caps, then the contracts asserting
//! first served first, then the fewest notes. A note above what is left
//! of a cap may be claimed for that much only, its excess lost, when
//! nothing else recovers as much.
//!
//! A recovery claims the refund of each of the wallet's locks at its hub
//! that the hub certified and never settled, once it expired at depth, and
//! the pay branch of each lock paying the wallet that the hub did not
//! issue, once activated on Bitcoin at depth ([`LockClaim`]): first, within
//! its cap, before its share of the notes. Taken as the notes are, under
//! its id, given back with them.

use std::cmp::Reverse;
use std::collections::{hash_map::Entry, BTreeSet, HashMap};

use licash::{
    address::Ephemeral,
    chain::{Activation, Checkpoint},
    claim::{ActivationRecord, Branch, Item, Source},
    crypto::{Point, ScalarBytes, Signature},
    note::{Derivation, Keyset, Message, Note, Ownership},
    recovery::{ActivationWitness, SourceWitness},
    source::Certificate,
    spend::{Output, Proposal, ProposalId},
};
use rusqlite::{params, OptionalExtension};

use crate::{held, invalid, Error, Held, Transport, Wallet};

/// A branch of a lock, claimed on Bitcoin, as pending items of the claim,
/// each authorized by its hold key, with the hub's certificate of the lock
/// showing it admitted it:
/// - the refund of a lock this wallet made, its hub settling neither
///   branch, once it expired at depth (filed earlier, it would be owed
///   nothing and given up): its signed body is the witness;
/// - the pay branch of a lock paying it, its hub not issuing it, once
///   activated on Bitcoin ([`OwedLock`]): its payer's owner signatures are
///   the witness, its activation filed with it.
#[derive(Clone)]
pub struct LockClaim {
    pub id: ProposalId,
    pub branch: Branch,
    pub items: Vec<Item>,
    pub source: Source,
    pub witness: SourceWitness,
    /// Each item's message and hold key.
    pub keys: Vec<(Message, ScalarBytes)>,
    /// What its items are worth.
    pub value: u64,
    /// Its activation, of a pay branch.
    pub activation: Option<(ActivationRecord, ActivationWitness)>,
}

/// A lock paying the wallet whose preimage it revealed to its hub, which
/// has not issued its pay branch: provable on Bitcoin by an activation
/// before its expiry ([`Wallet::record_activation`]).
#[derive(Clone, Debug)]
pub struct OwedLock {
    pub id: ProposalId,
    /// Its hub's URL.
    pub hub: String,
    pub keyset: [u8; 32],
    pub hash: [u8; 32],
    pub preimage: [u8; 32],
    pub expiry: u32,
    pub depth: u32,
    /// The commitment of the checkpoint its terms name, and the checkpoint
    /// once known.
    pub checkpoint: [u8; 32],
    pub anchor: Option<Checkpoint>,
    /// Its activation's block, once recorded.
    pub activated_at: Option<u32>,
    /// Whether a recovery claims it.
    pub claimed: bool,
}

/// A note a contract's recovery claims, as the wallet keeps it: the note,
/// and what derives its keys again (its proposal's ephemeral key, its
/// branch and its position there).
#[derive(Clone, Debug)]
pub struct ClaimNote {
    pub note: Note,
    pub ephemeral: [u8; 33],
    pub branch: Branch,
    pub position: u32,
}

impl ClaimNote {
    /// The note's bytes and its coordinates, for the recovery to keep a
    /// copy of what it claimed.
    pub fn to_bytes(&self) -> Vec<u8> {
        let note = self.note.to_bytes();
        let mut bytes = Vec::with_capacity(4 + note.len() + 33 + 1 + 4);
        bytes.extend((note.len() as u32).to_be_bytes());
        bytes.extend(note);
        bytes.extend(self.ephemeral);
        bytes.push(match self.branch {
            Branch::Pay => 0,
            Branch::Refund => 1,
        });
        bytes.extend(self.position.to_be_bytes());
        bytes
    }

    /// [`ClaimNote::to_bytes`] read back, of notes of `keyset`.
    pub fn from_bytes(keyset: &Keyset, bytes: &[u8]) -> Result<Self, Error> {
        let bad = || Error::Invalid("a claimed note's bytes".into());
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or_else(bad)?;
        let len = u32::from_be_bytes(*len) as usize;
        if rest.len() != len + 33 + 1 + 4 {
            return Err(bad());
        }
        let (note, rest) = rest.split_at(len);
        let (ephemeral, rest) = rest.split_first_chunk::<33>().ok_or_else(bad)?;
        let (branch, position) = rest.split_first().ok_or_else(bad)?;
        Ok(Self {
            note: Note::from_bytes(keyset, note)?,
            ephemeral: *ephemeral,
            branch: match branch {
                0 => Branch::Pay,
                1 => Branch::Refund,
                _ => return Err(bad()),
            },
            position: u32::from_be_bytes(position.try_into().map_err(|_| bad())?),
        })
    }

    fn held(&self) -> Held {
        Held {
            note: self.note.clone(),
            ephemeral: Point(self.ephemeral),
            branch: self.branch,
            position: self.position,
        }
    }
}

/// A contract whose recovery may claim the wallet's notes: its id, the
/// keyset (of its hub) whose notes it claims, the most it recovers, and
/// the most notes its claim holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimSlot {
    pub id: [u8; 32],
    pub keyset: [u8; 32],
    pub cap: u64,
    pub max: usize,
}

impl ClaimSlot {
    /// What each of `slots` (those to be served first on a tie first)
    /// recovers of `notes`, each its keyset's id and its denomination, as
    /// [`Wallet::recoverable`] shares them out.
    pub fn recoverable(slots: &[ClaimSlot], notes: &[([u8; 32], u64)]) -> Vec<u64> {
        let plan = allocate(notes, slots);
        slots
            .iter()
            .zip(&plan)
            .map(|(slot, picked)| recovered(slot, notes, picked))
            .collect()
    }
}

/// What `slot` recovers of the `picked` of `notes`: their face value, its
/// cap at most.
fn recovered(slot: &ClaimSlot, notes: &[([u8; 32], u64)], picked: &[usize]) -> u64 {
    let sum = picked
        .iter()
        .map(|&i| notes[i].1)
        .fold(0, u64::saturating_add);
    sum.min(slot.cap)
}

/// A note she funds a contract with herself ([`Wallet::funded_outputs`]):
/// what her claim names it by should its hub never issue it her.
pub struct FundedNote {
    /// Its output's position among those she funded with.
    pub index: u32,
    pub derivation: Derivation,
    /// Its hold key, which authorizes it in a claim.
    pub hold_key: ScalarBytes,
}

impl<T: Transport> Wallet<T> {
    /// Outputs worth `amount` under `keyset`, paying this wallet, for her
    /// to fund a contract with: the ephemeral key they are derived with,
    /// which their issuance carries, and them, the largest first.
    pub fn funded_outputs(
        &self,
        keyset: &Keyset,
        amount: u64,
    ) -> Result<(Point, Vec<Output>), Error> {
        let ephemeral = Ephemeral::random();
        let outputs = crate::pay_outputs(keyset, &ephemeral, &self.address(), amount)?;
        Ok((ephemeral.public(), outputs))
    }

    /// The notes of `outputs`, as [`Wallet::funded_outputs`] made them
    /// with `ephemeral`: refused for one not of this wallet's keys.
    pub fn funded_notes(
        &self,
        ephemeral: &Point,
        outputs: &[Output],
    ) -> Result<Vec<FundedNote>, Error> {
        let scan = self.owner.scan_key();
        let mine = |e: licash::Error| Error::Invalid(format!("a funded output: {e}"));
        (outputs.iter().enumerate())
            .map(|(i, output)| {
                let derivation = scan
                    .derive(ephemeral, output.denomination(), Branch::Pay, i)
                    .map_err(mine)?;
                if Output::from_derivation(&derivation).map_err(mine)? != *output {
                    return Err(Error::Invalid(
                        "a funded output not of this wallet's keys".into(),
                    ));
                }
                let hold_key = derivation.hold_key(self.owner.hold_root()).map_err(mine)?;
                Ok(FundedNote {
                    index: u32::try_from(i)
                        .map_err(|_| Error::Invalid("too many outputs".into()))?,
                    derivation,
                    hold_key,
                })
            })
            .collect()
    }

    /// Whether this wallet holds the note of each of `outputs`, as
    /// [`Wallet::funded_outputs`] made them with `ephemeral`: received
    /// once, held for good, spent or not.
    pub fn funded_held(&self, ephemeral: &Point, outputs: &[Output]) -> Result<Vec<bool>, Error> {
        let held: Vec<(u32, u64)> = self.store.read(|c| {
            let mut stmt =
                c.prepare("SELECT position, denom FROM notes WHERE ephemeral = ?1 AND branch = 0")?;
            let rows = stmt.query_map([ephemeral.0.as_slice()], |r| {
                Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            Ok::<_, Error>(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        Ok((outputs.iter().enumerate())
            .map(|(i, o)| held.contains(&(i as u32, o.denomination())))
            .collect())
    }

    /// What each of `slots`, the contracts at a hub with the one asserting
    /// first first, would recover of the wallet's notes at `tip`, by id:
    /// what [`Wallet::claim_with_locks`] would take for it, each in turn,
    /// of notes, its cap at most. The lock branches it takes first leave
    /// it less of its cap and count for its notes: those `pays` keeps for
    /// it of the pay branches owed (by its id), and the refunds owed. A
    /// contract whose recovery took its notes already recovers none of
    /// those left, nor keeps any from the others.
    pub fn recoverable(
        &self,
        slots: &[ClaimSlot],
        tip: u32,
        pays: Option<&SlotPays<'_>>,
    ) -> Result<HashMap<[u8; 32], u64>, Error> {
        let mut keysets = HashMap::new();
        for slot in slots {
            // A keyset the wallet never held: no notes nor locks under it.
            if let (Entry::Vacant(e), Ok(keyset)) =
                (keysets.entry(slot.keyset), self.keyset_of(&slot.keyset))
            {
                e.insert(keyset);
            }
        }
        self.store.read(|c| {
            let (open, _, notes) = claimable(c, slots)?;
            let mut taken = BTreeSet::new();
            let mut left = Vec::with_capacity(open.len());
            for slot in &open {
                let Some(keyset) = keysets.get(&slot.keyset) else {
                    left.push(*slot);
                    continue;
                };
                let keep = |l: &LockClaim| pays.is_some_and(|p| p(&slot.id, l));
                let keep: Option<&dyn Fn(&LockClaim) -> bool> = pays.is_some().then_some(&keep);
                let mut due = self.due_locks(c, keyset, tip, keep)?;
                due.retain(|l| !taken.contains(&l.id));
                let (cap, max, locks) = fit(slot, due);
                taken.extend(locks.into_iter().map(|l| l.id));
                left.push(ClaimSlot { cap, max, ..*slot });
            }
            let amounts = ClaimSlot::recoverable(&left, &notes);
            let mut all: HashMap<_, _> = slots.iter().map(|s| (s.id, 0)).collect();
            all.extend(open.iter().map(|s| s.id).zip(amounts));
            Ok(all)
        })
    }

    /// Take for the recovery of contract `id`, one of `slots` (as
    /// [`Wallet::recoverable`] takes them), the wallet's spendable notes
    /// its share is of. All or none: none if a payment took one meanwhile.
    /// None at all if the recovery took notes already:
    /// [`Wallet::claimed_notes`] are its own. What it recovers is their
    /// face value, its cap at most.
    pub fn claim_notes(&self, id: &[u8; 32], slots: &[ClaimSlot]) -> Result<Vec<ClaimNote>, Error> {
        let keyset = self.keyset_of(&slot_of(id, slots)?.keyset)?;
        self.store.write(|t| {
            if took(t, id)? {
                return Err(Error::Invalid(
                    "this recovery took its notes already".into(),
                ));
            }
            take_notes(t, &keyset, id, slots)
        })
    }

    /// [`Wallet::claim_notes`], after the branches of locks under its
    /// slot's keyset owed by `tip` ([`LockClaim`]), the largest first, as
    /// many as fit its cap and its count: its share of the notes is of what
    /// they leave of both. All in one, or none.
    ///
    /// The refunds of the wallet's locks that expired at depth unsettled;
    /// and, given `pays`, the pay branches of the locks paying it, unpaid,
    /// whose activation is at depth by `tip`, of those `pays` keeps: an
    /// activation the claim's chain would not show voids it all, so the
    /// caller checks each against it.
    pub fn claim_with_locks(
        &self,
        id: &[u8; 32],
        slots: &[ClaimSlot],
        tip: u32,
        pays: Option<&dyn Fn(&LockClaim) -> bool>,
    ) -> Result<(Vec<ClaimNote>, Vec<LockClaim>), Error> {
        let slot = *slot_of(id, slots)?;
        let keyset = self.keyset_of(&slot.keyset)?;
        self.store.write(|t| {
            if took(t, id)? {
                return Err(Error::Invalid(
                    "this recovery took its notes already".into(),
                ));
            }
            let due = self.due_locks(t, &keyset, tip, pays)?;
            let (cap, max, locks) = fit(&slot, due);
            let left: Vec<_> = (slots.iter())
                .map(|s| match s.id == *id {
                    true => ClaimSlot { cap, max, ..*s },
                    false => *s,
                })
                .collect();
            // Before the locks are marked, which close its slot.
            let notes = match cap > 0 && max > 0 {
                true => take_notes(t, &keyset, id, &left)?,
                false => vec![],
            };
            for lock in &locks {
                let table = match lock.branch {
                    Branch::Refund => "locks",
                    Branch::Pay => "owed_locks",
                };
                let taken = t.execute(
                    &format!(
                        "UPDATE {table} SET claimed_by = ?1 WHERE id = ?2 AND claimed_by IS NULL"
                    ),
                    params![id, lock.id.as_bytes()],
                )?;
                if taken != 1 {
                    return Err(Error::NoteTaken);
                }
            }
            Ok((notes, locks))
        })
    }

    /// The lock branches under `keyset` owed by `tip`, unclaimed, the
    /// largest first, as [`Wallet::claim_with_locks`] reads them.
    fn due_locks(
        &self,
        c: &rusqlite::Connection,
        keyset: &Keyset,
        tip: u32,
        pays: Option<&dyn Fn(&LockClaim) -> bool>,
    ) -> Result<Vec<LockClaim>, Error> {
        let mut stmt = c.prepare(
            "SELECT id, request, certificate, expiry FROM locks
             WHERE certificate IS NOT NULL AND id NOT IN (SELECT id FROM lock_settlements)
                AND abandoned IS NULL AND claimed_by IS NULL AND expiry <= ?1",
        )?;
        let rows = stmt
            .query_map([tip], |r| {
                Ok((
                    r.get::<_, [u8; 32]>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut due = Vec::new();
        for (lock, request, certificate) in rows {
            // One the wallet can't read back is left to its hub.
            match self.lock_refund(keyset, &request, &certificate) {
                Ok(Some((refund, expired))) if expired <= tip => due.push(refund),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    lock = %hex::encode(lock), "a lock's refund not claimable: {e}"
                ),
            }
        }
        if let Some(keep) = pays {
            for (lock, row) in owed_rows(c, OWED_DUE, params![keyset.id(), tip])? {
                match self.lock_pay(keyset, &row) {
                    Ok(Some(pay)) if keep(&pay) => due.push(pay),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        lock = %hex::encode(lock), "a lock's pay not claimable: {e}"
                    ),
                }
            }
        }
        due.sort_by_key(|r| (Reverse(r.value), *r.id.as_bytes()));
        Ok(due)
    }

    /// The lock branches the recovery of contract `id` took
    /// ([`Wallet::claim_with_locks`]), of the keyset of id `keyset`, by id.
    pub fn claimed_locks(&self, id: &[u8; 32], keyset: &[u8; 32]) -> Result<Vec<LockClaim>, Error> {
        let keyset = self.keyset_of(keyset)?;
        let (rows, owed): (Vec<(String, Vec<u8>)>, _) = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT request, certificate FROM locks WHERE claimed_by = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let rows = rows.collect::<Result<_, _>>()?;
            Ok((rows, owed_rows(c, OWED_CLAIMED, params![id])?))
        })?;
        let mut locks = Vec::with_capacity(rows.len() + owed.len());
        for (request, certificate) in rows {
            if let Some((refund, _)) = self.lock_refund(&keyset, &request, &certificate)? {
                locks.push(refund);
            }
        }
        for (_, row) in &owed {
            if let Some(pay) = self.lock_pay(&keyset, row)? {
                locks.push(pay);
            }
        }
        locks.sort_by_key(|l| *l.id.as_bytes());
        Ok(locks)
    }

    /// The locks paying the wallet, unpaid by their hub, whose preimage it
    /// revealed ([`OwedLock`]), by expiry.
    pub fn owed_locks(&self) -> Result<Vec<OwedLock>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT o.id, r.url, o.keyset, o.proposal, o.preimage, o.expiry, o.depth,
                    o.anchor, a.height, o.claimed_by IS NOT NULL
                 FROM owed_locks o JOIN reach r ON r.hub = o.hub
                 LEFT JOIN owed_lock_activations a ON a.id = o.id
                 WHERE NOT EXISTS (SELECT 1 FROM notes n WHERE n.proposal = o.id AND n.branch = 0)
                 ORDER BY o.expiry, o.id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, [u8; 32]>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, [u8; 32]>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, [u8; 32]>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, u32>(6)?,
                    r.get::<_, Option<Vec<u8>>>(7)?,
                    r.get::<_, Option<u32>>(8)?,
                    r.get::<_, bool>(9)?,
                ))
            })?;
            let mut owed = Vec::new();
            for row in rows {
                let (
                    id,
                    hub,
                    keyset,
                    proposal,
                    preimage,
                    expiry,
                    depth,
                    anchor,
                    activated_at,
                    claimed,
                ) = row?;
                let proposal = Proposal::from_commitment_bytes(&proposal)?;
                let terms = (proposal.lock())
                    .ok_or_else(|| Error::Invalid("a lock of no terms".into()))?
                    .terms();
                owed.push(OwedLock {
                    id: ProposalId::from_bytes(id),
                    hub,
                    keyset,
                    hash: *terms.hash(),
                    preimage,
                    expiry,
                    depth,
                    checkpoint: *terms.checkpoint(),
                    anchor: anchor.map(|a| Checkpoint::from_bytes(&a)).transpose()?,
                    activated_at,
                    claimed,
                });
            }
            Ok(owed)
        })
    }

    /// The pay branch of the owed lock `id` as a recovery under `keyset`
    /// would claim it, without taking it: none if under another keyset,
    /// paying the wallet nothing or not owed.
    pub fn owed_claim(
        &self,
        id: &ProposalId,
        keyset: &[u8; 32],
    ) -> Result<Option<LockClaim>, Error> {
        let keyset = self.keyset_of(keyset)?;
        let rows = self
            .store
            .read(|c| owed_rows(c, "WHERE o.id = ?1", params![id.as_bytes()]))?;
        match rows.first() {
            Some((_, row)) => self.lock_pay(&keyset, row),
            None => Ok(None),
        }
    }

    /// Record the owed lock `id`'s activation on Bitcoin and the checkpoint
    /// its terms name, which it is anchored to: none forgets it, as when
    /// its block left the chain. Refused once a recovery claims it.
    pub fn record_activation(
        &self,
        id: &ProposalId,
        activation: Option<(&Activation, &Checkpoint)>,
    ) -> Result<(), Error> {
        let (json, at, anchor) = match activation {
            Some((a, anchor)) => (
                Some(serde_json::to_string(a).map_err(invalid)?),
                Some(a.reference.height),
                Some(anchor.to_bytes()),
            ),
            None => (None, None, None),
        };
        let changed = self.store.write(|t| {
            let changed = t.execute(
                "UPDATE owed_locks SET anchor = COALESCE(?2, anchor)
                 WHERE id = ?1 AND claimed_by IS NULL",
                params![id.as_bytes(), anchor],
            )?;
            if changed == 1 {
                match (json, at) {
                    (Some(json), Some(at)) => t.execute(
                        "INSERT INTO owed_lock_activations (id, activation, height)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT (id) DO UPDATE
                         SET activation = excluded.activation, height = excluded.height",
                        params![id.as_bytes(), json, at],
                    )?,
                    _ => t.execute(
                        "DELETE FROM owed_lock_activations WHERE id = ?1",
                        [id.as_bytes()],
                    )?,
                };
            }
            Ok(changed)
        })?;
        if changed != 1 {
            return Err(Error::Invalid(format!(
                "lock {}: no owed lock to record an activation of",
                hex::encode(id.as_bytes())
            )));
        }
        Ok(())
    }

    /// The pay branch of the owed lock of `row`, under `keyset`: none if
    /// under another keyset or paying the wallet nothing.
    fn lock_pay(&self, keyset: &Keyset, row: &OwedRow) -> Result<Option<LockClaim>, Error> {
        if row.keyset != *keyset.id() {
            return Ok(None);
        }
        let proposal = Proposal::from_commitment_bytes(&row.proposal)?;
        let certificate = Certificate::from_bytes(keyset, &row.certificate)?;
        let (signatures, rest) = row.signatures.as_chunks::<64>();
        if !rest.is_empty() {
            return Err(Error::Invalid(
                "an owed lock's signatures are cut short".into(),
            ));
        }
        let signatures = signatures.iter().map(|s| Signature(*s)).collect();
        let (mut items, mut keys, mut value) = (Vec::new(), Vec::new(), 0u64);
        let received = self.owner.scan_key().receive(&proposal)?;
        for r in received.iter().filter(|r| r.branch == Branch::Pay) {
            let message = r.derivation.message().clone();
            keys.push((
                message.clone(),
                r.derivation.hold_key(self.owner.hold_root())?,
            ));
            items.push(Item::Pending {
                message,
                source: proposal.id(),
                branch: Branch::Pay,
                index: r.position,
                blinding: *r.derivation.blinding(),
                preimage: Some(row.preimage),
            });
            let output = (proposal.outputs().get(r.position as usize))
                .ok_or_else(|| Error::Invalid("a lock's pay output out of range".into()))?;
            value = value.saturating_add(output.denomination());
        }
        if items.is_empty() {
            return Ok(None);
        }
        let activation = match (&row.activation, &row.anchor) {
            (Some(json), Some(anchor)) => {
                let a: Activation = serde_json::from_str(json).map_err(invalid)?;
                Some((
                    ActivationRecord {
                        source: proposal.id(),
                        reference: a.reference,
                    },
                    ActivationWitness {
                        inclusion: a.inclusion,
                        anchor: Some(Checkpoint::from_bytes(anchor)?),
                    },
                ))
            }
            _ => None,
        };
        Ok(Some(LockClaim {
            id: proposal.id(),
            branch: Branch::Pay,
            items,
            source: Source::Admitted(certificate),
            witness: SourceWitness::Certified {
                proposal,
                signatures,
            },
            keys,
            value,
            activation,
        }))
    }

    /// The refund of the lock of `request`, certified by `certificate`,
    /// under `keyset`, and the height at which it is owed, its expiry at
    /// depth: none if under another keyset or paying the wallet nothing.
    fn lock_refund(
        &self,
        keyset: &Keyset,
        request: &str,
        certificate: &[u8],
    ) -> Result<Option<(LockClaim, u32)>, Error> {
        let request: licash_api::Lock = serde_json::from_str(request).map_err(invalid)?;
        if request.keyset.0 != keyset.id() {
            return Ok(None);
        }
        let lookup = |id: &[u8; 32]| (id == keyset.id()).then_some(keyset);
        let (signed, notes) = request.decode(lookup).map_err(invalid)?;
        let certificate = Certificate::from_bytes(keyset, certificate)?;
        let proposal = signed.proposal();
        let lock = (proposal.lock()).ok_or_else(|| Error::Invalid("a lock of no terms".into()))?;
        let terms = lock.terms();
        let owed = (terms.expiry())
            .checked_add(terms.depth())
            .ok_or_else(|| Error::Invalid("a lock's expiry at depth overflows".into()))?;
        let (mut items, mut keys, mut value) = (Vec::new(), Vec::new(), 0u64);
        for (index, output) in lock.refund().iter().enumerate() {
            let derivation = self.owner.scan_key().derive(
                &proposal.ephemeral(),
                output.denomination(),
                Branch::Refund,
                index,
            )?;
            // The hub's refund fee, say: not the wallet's to claim.
            if Output::from_derivation(&derivation)? != *output {
                continue;
            }
            let message = derivation.message().clone();
            keys.push((
                message.clone(),
                derivation.hold_key(self.owner.hold_root())?,
            ));
            items.push(Item::Pending {
                message,
                source: proposal.id(),
                branch: Branch::Refund,
                index: u32::try_from(index).map_err(invalid)?,
                blinding: *derivation.blinding(),
                preimage: None,
            });
            value = value.saturating_add(output.denomination());
        }
        if items.is_empty() {
            return Ok(None);
        }
        let refund = LockClaim {
            id: proposal.id(),
            branch: Branch::Refund,
            items,
            source: Source::Admitted(certificate),
            witness: SourceWitness::Signed { signed, notes },
            keys,
            value,
            activation: None,
        };
        Ok(Some((refund, owed)))
    }

    /// The notes the recovery of contract `id` took, of the keyset of id
    /// `keyset`, the largest first.
    pub fn claimed_notes(&self, id: &[u8; 32], keyset: &[u8; 32]) -> Result<Vec<ClaimNote>, Error> {
        let keyset = self.keyset_of(keyset)?;
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT y FROM notes WHERE spent_by = ?1 AND keyset = ?2 ORDER BY denom DESC, y",
            )?;
            let ys = stmt
                .query_map(params![id, keyset.id()], |r| r.get::<_, [u8; 33]>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ys.iter().map(|y| claim_note(c, &keyset, y)).collect()
        })
    }

    /// Give back the notes the recovery of contract `id` took, for it was
    /// given up before its request was signed: how many.
    /// The lock refunds it took are given back too.
    pub fn unclaim_notes(&self, id: &[u8; 32]) -> Result<usize, Error> {
        let (freed, locks) = self.store.write(|t| {
            let freed = t.execute(
                "UPDATE notes SET spent_by = NULL, spent_kind = NULL WHERE spent_by = ?1",
                [id],
            )?;
            let locks = t.execute(
                "UPDATE locks SET claimed_by = NULL WHERE claimed_by = ?1",
                [id],
            )? + t.execute(
                "UPDATE owed_locks SET claimed_by = NULL WHERE claimed_by = ?1",
                [id],
            )?;
            Ok((freed, locks))
        })?;
        tracing::info!(id = %hex::encode(id), freed, locks, "claimed notes given back");
        Ok(freed)
    }

    /// Each of `notes`' message and its hold key, which authorizes it in a
    /// claim: of this wallet's keys, refused for a note not derived from
    /// them.
    pub fn hold_keys(&self, notes: &[ClaimNote]) -> Result<Vec<(Message, ScalarBytes)>, Error> {
        notes
            .iter()
            .map(|n| {
                let derivation = self.derivation(&n.held())?;
                if derivation.message() != n.note.message() {
                    return Err(Error::Invalid("a note not of this wallet's keys".into()));
                }
                let key = derivation.hold_key(self.owner.hold_root())?;
                Ok((n.note.message().clone(), key))
            })
            .collect()
    }

    /// Each of `notes`' ownership, which her proof of a claim shows the
    /// note is hers by (its spending key derived from her spending root):
    /// of this wallet's keys, refused for a note not derived from them.
    /// Not a spending key, but private: it links the note to her.
    pub fn ownerships(&self, notes: &[ClaimNote]) -> Result<Vec<Ownership>, Error> {
        notes
            .iter()
            .map(|n| {
                let derivation = self.derivation(&n.held())?;
                if derivation.message() != n.note.message() {
                    return Err(Error::Invalid("a note not of this wallet's keys".into()));
                }
                Ok(derivation.ownership())
            })
            .collect()
    }

    /// The wallet's keyset of id `id`.
    pub(crate) fn keyset_of(&self, id: &[u8; 32]) -> Result<Keyset, Error> {
        let bytes: Option<Vec<u8>> = self.store.read(|c| {
            Ok(
                c.query_row("SELECT bytes FROM keysets WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })?;
        let bytes = bytes.ok_or_else(|| {
            Error::Invalid(format!(
                "keyset {}: not one of the wallet's",
                hex::encode(id)
            ))
        })?;
        Ok(Keyset::from_bytes(&bytes)?)
    }
}

/// Whether the contract of an id claims a pay branch owed.
pub type SlotPays<'a> = dyn Fn(&[u8; 32], &LockClaim) -> bool + 'a;

/// Of `due`, the largest first, those `slot` claims, each as fits what is
/// left of its cap and its count, and what is left of both.
fn fit(slot: &ClaimSlot, due: Vec<LockClaim>) -> (u64, usize, Vec<LockClaim>) {
    let (mut cap, mut max) = (slot.cap, slot.max);
    let mut locks = Vec::new();
    for lock in due {
        if lock.value <= cap && lock.keys.len() <= max {
            cap -= lock.value;
            max -= lock.keys.len();
            locks.push(lock);
        }
    }
    (cap, max, locks)
}

fn slot_of<'s>(id: &[u8; 32], slots: &'s [ClaimSlot]) -> Result<&'s ClaimSlot, Error> {
    (slots.iter().find(|s| s.id == *id))
        .ok_or_else(|| Error::Invalid("a recovery claiming in no slot".into()))
}

/// Whether the recovery of contract `id` took notes or lock branches.
fn took(c: &rusqlite::Connection, id: &[u8; 32]) -> Result<bool, Error> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM notes WHERE spent_by = ?1)
            OR EXISTS (SELECT 1 FROM locks WHERE claimed_by = ?1)
            OR EXISTS (SELECT 1 FROM owed_locks WHERE claimed_by = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

/// Take for the recovery of contract `id`, one of `slots`, its share of
/// the wallet's spendable notes, of `keyset`.
fn take_notes(
    t: &rusqlite::Transaction<'_>,
    keyset: &Keyset,
    id: &[u8; 32],
    slots: &[ClaimSlot],
) -> Result<Vec<ClaimNote>, Error> {
    let (open, ys, stock) = claimable(t, slots)?;
    let plan = allocate(&stock, &open);
    let mine = open.iter().position(|s| s.id == *id);
    let picked = mine.map_or(&[][..], |i| &plan[i]);
    let mut notes = Vec::with_capacity(picked.len());
    for y in picked.iter().map(|&i| &ys[i]) {
        let taken = t.execute(
            "UPDATE notes SET spent_by = ?1, spent_kind = ?3
             WHERE y = ?2 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
            params![id, y, crate::Spender::Recovery.name()],
        )?;
        if taken != 1 {
            return Err(Error::NoteTaken);
        }
        notes.push(claim_note(t, keyset, y)?);
    }
    Ok(notes)
}

fn claim_note(c: &rusqlite::Connection, keyset: &Keyset, y: &[u8; 33]) -> Result<ClaimNote, Error> {
    let held = held(c, keyset, y)?;
    Ok(ClaimNote {
        note: held.note,
        ephemeral: held.ephemeral.0,
        branch: held.branch,
        position: held.position,
    })
}

/// Of some slots, those whose recovery took no notes yet, and the notes
/// they may claim: as their points and, beside, as their keysets' ids and
/// denominations, the largest first.
type Claimable = (Vec<ClaimSlot>, Vec<[u8; 33]>, Vec<([u8; 32], u64)>);

/// [`Claimable`] of `slots`: the wallet's spendable notes of their
/// keysets.
fn claimable(c: &rusqlite::Connection, slots: &[ClaimSlot]) -> Result<Claimable, Error> {
    let mut open = Vec::with_capacity(slots.len());
    for slot in slots {
        if !took(c, &slot.id)? {
            open.push(*slot);
        }
    }
    let mut keysets: Vec<[u8; 32]> = open.iter().map(|s| s.keyset).collect();
    keysets.sort_unstable();
    keysets.dedup();
    let mut stmt = c.prepare(
        "SELECT y, denom FROM notes
         WHERE keyset = ?1 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
    )?;
    let mut all = Vec::new();
    for keyset in keysets {
        let rows = stmt.query_map([keyset], |r| {
            Ok((r.get::<_, [u8; 33]>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (y, denom) = row?;
            all.push((y, (keyset, denom)));
        }
    }
    all.sort_by(|(a, (_, x)), (b, (_, y))| y.cmp(x).then(a.cmp(b)));
    let (ys, notes) = all.into_iter().unzip();
    Ok((open, ys, notes))
}

/// The contracts at a hub up to which every order they may be served in
/// is tried, and up to which each is tried covering its cap and filling it
/// only (beyond, each covers it if it can).
const PERMUTED: usize = 4;
const BRANCHED: usize = 6;

/// The steps one search for a contract's share takes at most, before it
/// settles for the best it found.
const STEPS: usize = 10_000;

/// Which of `notes`, each its keyset's id and its denomination, each of
/// `slots` claims, by their positions in `notes`: see the module's doc.
fn allocate(notes: &[([u8; 32], u64)], slots: &[ClaimSlot]) -> Vec<Vec<usize>> {
    let n = slots.len();
    let orders = if n <= PERMUTED {
        permutations(n)
    } else {
        vec![(0..n).collect()]
    };
    let choices: u64 = if n <= BRANCHED { 1 << n } else { 1 };
    let mut searched = Searched::default();
    let mut best: Option<(Score, Vec<Vec<usize>>)> = None;
    for order in &orders {
        for choice in 0..choices {
            let filling = |i: usize| choice >> i & 1 == 1;
            let plan = plan(notes, slots, order, filling, &mut searched);
            let score = score(notes, slots, &plan);
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, plan));
            }
        }
    }
    best.map_or_else(Vec::new, |(_, plan)| plan)
}

/// How good a share is, the better the greater: what it recovers in all,
/// the least face value beyond the caps, what each recovers the first
/// first, the fewest notes.
type Score = (u64, Reverse<u64>, Vec<u64>, Reverse<usize>);

fn score(notes: &[([u8; 32], u64)], slots: &[ClaimSlot], plan: &[Vec<usize>]) -> Score {
    let (mut total, mut beyond, mut count) = (0u64, 0u64, 0usize);
    let mut each = Vec::with_capacity(slots.len());
    for (slot, picked) in slots.iter().zip(plan) {
        let sum = picked
            .iter()
            .map(|&i| notes[i].1)
            .fold(0, u64::saturating_add);
        let got = sum.min(slot.cap);
        total = total.saturating_add(got);
        beyond = beyond.saturating_add(sum - got);
        count += picked.len();
        each.push(got);
    }
    (total, Reverse(beyond), each, Reverse(count))
}

/// The notes each of `slots` claims, served in `order` of what those
/// before left: each covering its cap if it can, but those `filling`
/// only, which take what comes to their cap at most.
fn plan(
    notes: &[([u8; 32], u64)],
    slots: &[ClaimSlot],
    order: &[usize],
    filling: impl Fn(usize) -> bool,
    searched: &mut Searched,
) -> Vec<Vec<usize>> {
    let mut used = vec![false; notes.len()];
    let mut plan = vec![Vec::new(); slots.len()];
    for &i in order {
        let slot = &slots[i];
        // What is left of its keyset, by denomination, the largest first.
        let mut stock: Vec<(u64, Vec<usize>)> = Vec::new();
        for (j, (keyset, denom)) in notes.iter().enumerate() {
            if used[j] || *keyset != slot.keyset || *denom == 0 {
                continue;
            }
            match stock.iter_mut().find(|(d, _)| d == denom) {
                Some((_, at)) => at.push(j),
                None => stock.push((*denom, vec![j])),
            }
        }
        stock.sort_by_key(|(denom, _)| Reverse(*denom));
        let counts: Vec<(u64, usize)> = stock.iter().map(|(d, at)| (*d, at.len())).collect();
        let take = searched.share(slot, counts, filling(i));
        for (k, (_, at)) in take.iter().zip(&stock) {
            for &j in &at[..*k] {
                used[j] = true;
                plan[i].push(j);
            }
        }
        plan[i].sort_unstable();
    }
    plan
}

/// What a contract searches its share of: its cap, its most notes, and
/// what is left of its keyset, as [`cover`] has it.
type Search = (u64, usize, Vec<(u64, usize)>);

/// Each contract's [`fill`] and [`cover`] of what was left it, searched
/// once each.
#[derive(Default)]
struct Searched(HashMap<Search, (Vec<usize>, Option<Vec<usize>>)>);

impl Searched {
    /// How many notes of each denomination of `stock` `slot` takes.
    fn share(&mut self, slot: &ClaimSlot, stock: Vec<(u64, usize)>, filling: bool) -> Vec<usize> {
        let (cap, max) = (slot.cap, slot.max);
        let (fill, cover) = self
            .0
            .entry((cap, max, stock))
            .or_insert_with_key(|(_, _, stock)| {
                let fill = fill(stock, cap, max);
                let cover = (sum(stock, &fill) < cap)
                    .then(|| cover(stock, cap, max))
                    .flatten();
                (fill, cover)
            });
        match cover {
            Some(cover) if !filling => cover.clone(),
            _ => fill.clone(),
        }
    }
}

/// What `take` of each denomination of `stock` comes to.
fn sum(stock: &[(u64, usize)], take: &[usize]) -> u64 {
    (stock.iter().zip(take))
        .map(|((denom, _), k)| denom.saturating_mul(*k as u64))
        .fold(0, u64::saturating_add)
}

/// What the `left` largest notes of `stock` come to.
fn largest(stock: &[(u64, usize)], mut left: usize) -> u64 {
    let mut sum = 0u64;
    for &(denom, count) in stock {
        let k = count.min(left);
        sum = sum.saturating_add(denom.saturating_mul(k as u64));
        left -= k;
        if left == 0 {
            break;
        }
    }
    sum
}

/// Of `stock`, denominations (the largest first) and how many notes of
/// each, how many of each make the least sum of `cap` at least in `max`
/// notes at most, the fewest notes among equal sums: none if no `max` of
/// them come to `cap`.
fn cover(stock: &[(u64, usize)], cap: u64, max: usize) -> Option<Vec<usize>> {
    struct Search<'a> {
        stock: &'a [(u64, usize)],
        cap: u64,
        take: Vec<usize>,
        best: Option<(u64, usize, Vec<usize>)>,
        steps: usize,
    }
    impl Search<'_> {
        fn walk(&mut self, i: usize, sum: u64, notes: usize, left: usize) {
            if sum >= self.cap {
                if (self.best.as_ref()).is_none_or(|(s, n, _)| (sum, notes) < (*s, *n)) {
                    self.best = Some((sum, notes, self.take.clone()));
                }
                return;
            }
            if i == self.stock.len() || left == 0 || self.steps == 0 {
                return;
            }
            self.steps -= 1;
            // Out of reach, or no better than an exact one found.
            let reach = sum.saturating_add(largest(&self.stock[i..], left));
            let exact = |(s, n, _): &(u64, usize, Vec<usize>)| *s == self.cap && *n <= notes + 1;
            if reach < self.cap || self.best.as_ref().is_some_and(exact) {
                return;
            }
            let (denom, count) = self.stock[i];
            let need = (self.cap - sum).div_ceil(denom);
            let most = count
                .min(left)
                .min(usize::try_from(need).unwrap_or(usize::MAX));
            for k in (0..=most).rev() {
                self.take[i] = k;
                let sum = sum.saturating_add(denom.saturating_mul(k as u64));
                self.walk(i + 1, sum, notes + k, left - k);
            }
            self.take[i] = 0;
        }
    }
    let mut search = Search {
        stock,
        cap,
        take: vec![0; stock.len()],
        best: None,
        steps: STEPS,
    };
    search.walk(0, 0, 0, max);
    search.best.map(|(_, _, take)| take)
}

/// Of `stock`, as [`cover`] has it, how many of each make the greatest sum
/// of `cap` at most in `max` notes at most, the fewest notes found among
/// equal sums.
fn fill(stock: &[(u64, usize)], cap: u64, max: usize) -> Vec<usize> {
    struct Search<'a> {
        stock: &'a [(u64, usize)],
        cap: u64,
        take: Vec<usize>,
        best: (u64, usize, Vec<usize>),
        steps: usize,
    }
    impl Search<'_> {
        fn walk(&mut self, i: usize, sum: u64, notes: usize, left: usize) {
            let (s, n, _) = &self.best;
            if sum > *s || (sum == *s && notes < *n) {
                self.best = (sum, notes, self.take.clone());
            }
            if sum == self.cap || i == self.stock.len() || left == 0 || self.steps == 0 {
                return;
            }
            self.steps -= 1;
            let reach = sum.saturating_add(largest(&self.stock[i..], left));
            if reach.min(self.cap) <= self.best.0 {
                return;
            }
            let (denom, count) = self.stock[i];
            let fits = (self.cap - sum) / denom;
            let most = count
                .min(left)
                .min(usize::try_from(fits).unwrap_or(usize::MAX));
            for k in (0..=most).rev() {
                self.take[i] = k;
                let sum = sum.saturating_add(denom.saturating_mul(k as u64));
                self.walk(i + 1, sum, notes + k, left - k);
            }
            self.take[i] = 0;
        }
    }
    let mut search = Search {
        stock,
        cap,
        take: vec![0; stock.len()],
        best: (0, 0, vec![0; stock.len()]),
        steps: STEPS,
    };
    search.walk(0, 0, 0, max);
    search.best.2
}

/// Every order of `0..n`, the first of them first.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn walk(rest: &mut Vec<usize>, order: &mut Vec<usize>, all: &mut Vec<Vec<usize>>) {
        if rest.is_empty() {
            all.push(order.clone());
            return;
        }
        for k in 0..rest.len() {
            let i = rest.remove(k);
            order.push(i);
            walk(rest, order, all);
            order.pop();
            rest.insert(k, i);
        }
    }
    let mut all = Vec::new();
    walk(&mut (0..n).collect(), &mut Vec::new(), &mut all);
    all
}

/// An owed lock as kept ([`OwedLock`]), to claim its pay branch from.
struct OwedRow {
    keyset: [u8; 32],
    proposal: Vec<u8>,
    certificate: Vec<u8>,
    signatures: Vec<u8>,
    preimage: [u8; 32],
    anchor: Option<Vec<u8>>,
    activation: Option<String>,
}

/// The owed locks of keyset `?1` a recovery may claim at tip `?2`: unpaid
/// (no note of its pay branch kept), unclaimed, not a lock of the wallet's
/// own (its refund is claimed, not both branches), activated at depth.
const OWED_DUE: &str = "WHERE o.keyset = ?1 AND o.claimed_by IS NULL
    AND a.id IS NOT NULL AND o.anchor IS NOT NULL
    AND a.height + o.depth <= ?2 + 1
    AND NOT EXISTS (SELECT 1 FROM notes n WHERE n.proposal = o.id AND n.branch = 0)
    AND NOT EXISTS (SELECT 1 FROM locks l WHERE l.id = o.id)";

/// The owed locks the recovery `?1` claims.
const OWED_CLAIMED: &str = "WHERE o.claimed_by = ?1";

fn owed_rows(
    c: &rusqlite::Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<([u8; 32], OwedRow)>, Error> {
    let mut stmt = c.prepare(&format!(
        "SELECT o.id, o.keyset, o.proposal, o.certificate, o.signatures, o.preimage, o.anchor,
            a.activation
         FROM owed_locks o LEFT JOIN owed_lock_activations a ON a.id = o.id
         {filter} ORDER BY o.id"
    ))?;
    let rows = stmt.query_map(params, |r| {
        Ok((
            r.get(0)?,
            OwedRow {
                keyset: r.get(1)?,
                proposal: r.get(2)?,
                certificate: r.get(3)?,
                signatures: r.get(4)?,
                preimage: r.get(5)?,
                anchor: r.get(6)?,
                activation: r.get(7)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: [u8; 32] = [1; 32];

    /// A deterministic stream of numbers, for the searches' cases.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    fn slot(id: u8, cap: u64, max: usize) -> ClaimSlot {
        ClaimSlot {
            id: [id; 32],
            keyset: K,
            cap,
            max,
        }
    }

    fn of(denoms: &[u64]) -> Vec<([u8; 32], u64)> {
        denoms.iter().map(|d| (K, *d)).collect()
    }

    fn stock(denoms: &[u64]) -> Vec<(u64, usize)> {
        let mut sorted = denoms.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let mut stock: Vec<(u64, usize)> = Vec::new();
        for d in sorted {
            match stock.last_mut() {
                Some((last, n)) if *last == d => *n += 1,
                _ => stock.push((d, 1)),
            }
        }
        stock
    }

    /// Each subset of `denoms` of `max` notes at most: its sum and size.
    fn subsets(denoms: &[u64], max: usize) -> impl Iterator<Item = (u64, usize)> + '_ {
        (0u32..1 << denoms.len())
            .filter(move |mask| mask.count_ones() as usize <= max)
            .map(|mask| {
                let sum = (0..denoms.len())
                    .filter(|i| mask >> i & 1 == 1)
                    .map(|i| denoms[i])
                    .sum();
                (sum, mask.count_ones() as usize)
            })
    }

    fn random(rng: &mut Rng, powers: bool, len: usize) -> Vec<u64> {
        (0..len)
            .map(|_| match powers {
                true => 1 << rng.below(8),
                false => 1 + rng.below(100),
            })
            .collect()
    }

    #[test]
    fn a_cover_or_a_fill_is_the_best_any_notes_make() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..3_000 {
            let len = rng.below(12) as usize;
            let denoms = random(&mut rng, round % 2 == 0, len);
            let cap = rng.below(denoms.iter().sum::<u64>() + 40);
            let max = rng.below(len as u64 + 1) as usize;
            let stock = stock(&denoms);
            let case = format!("{denoms:?} cap {cap} max {max}");

            let least = subsets(&denoms, max).filter(|(sum, _)| *sum >= cap).min();
            let covered = cover(&stock, cap, max);
            let covered = covered.map(|take| (sum(&stock, &take), take.iter().sum()));
            assert_eq!(covered, least, "cover of {case}");

            let most = subsets(&denoms, max)
                .filter(|(sum, _)| *sum <= cap)
                .map(|(sum, _)| sum)
                .max();
            let take = fill(&stock, cap, max);
            assert!(take.iter().zip(&stock).all(|(k, (_, n))| k <= n), "{case}");
            assert!(take.iter().sum::<usize>() <= max, "{case}");
            assert_eq!(Some(sum(&stock, &take)), most, "fill of {case}");
        }
    }

    /// The most `slots` recover of `denoms` in all, each note to one of
    /// them or none.
    fn best(denoms: &[u64], slots: &[ClaimSlot]) -> u64 {
        let ways = (slots.len() + 1).pow(denoms.len() as u32);
        let mut best = 0;
        for mut way in 0..ways {
            let mut sums = vec![(0u64, 0usize); slots.len()];
            for d in denoms {
                let to = way % (slots.len() + 1);
                way /= slots.len() + 1;
                if to > 0 {
                    sums[to - 1].0 += d;
                    sums[to - 1].1 += 1;
                }
            }
            if sums.iter().zip(slots).any(|((_, n), s)| *n > s.max) {
                continue;
            }
            let total = sums
                .iter()
                .zip(slots)
                .map(|((sum, _), s)| (*sum).min(s.cap));
            best = best.max(total.sum());
        }
        best
    }

    #[test]
    fn contracts_sharing_notes_recover_the_most_they_can_together() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut short = Vec::new();
        for round in 0..1_500 {
            let len = 2 + rng.below(6) as usize;
            let denoms = random(&mut rng, round % 3 != 0, len);
            let total: u64 = denoms.iter().sum();
            let slots: Vec<_> = (0..2 + rng.below(2) as u8)
                .map(|id| slot(id, 1 + rng.below(total), 1 + rng.below(len as u64) as usize))
                .collect();
            if slots.len() == 3 && len > 6 {
                continue;
            }
            let got: u64 = ClaimSlot::recoverable(&slots, &of(&denoms)).iter().sum();
            let best = best(&denoms, &slots);
            assert!(got <= best);
            if got < best {
                short.push(format!("{denoms:?} {slots:?}: {got} of {best}"));
            }
        }
        assert!(
            short.is_empty(),
            "{} short: {:#?}",
            short.len(),
            &short[..short.len().min(5)]
        );
    }

    #[test]
    fn two_contracts_split_notes_so_that_neither_wastes_what_the_other_needs() {
        let notes = of(&[16, 8, 8, 8, 4, 2, 1, 1]);
        let slots = [slot(1, 25, 3), slot(2, 23, 3)];
        let got = ClaimSlot::recoverable(&slots, &notes);
        assert_eq!(got.iter().sum::<u64>(), 46, "{got:?}");
    }

    #[test]
    fn a_contract_alone_claims_a_note_beyond_its_cap_for_its_cap() {
        // 64 + 4 + 4 recover all 70, 2 sat lost: better than 68.
        let notes = of(&[64, 32, 4, 4]);
        let plan = allocate(&notes, &[slot(1, 70, 32)]);
        assert_eq!(plan, [vec![0, 2, 3]]);
        assert_eq!(ClaimSlot::recoverable(&[slot(1, 70, 32)], &notes), [70]);
        // One note larger than the cap: the cap, of it.
        assert_eq!(ClaimSlot::recoverable(&[slot(1, 50, 32)], &of(&[64])), [50]);
        // As many as it may hold, the largest.
        let plan = allocate(&of(&[64, 32, 4]), &[slot(1, 1_000, 2)]);
        assert_eq!(plan, [vec![0, 1]]);
    }

    #[test]
    fn the_contract_asserting_first_is_served_first_of_too_few_notes() {
        let notes = of(&[4_096, 1_024]);
        let slots = [slot(1, 4_096, 32), slot(2, 4_096, 32)];
        assert_eq!(ClaimSlot::recoverable(&slots, &notes), [4_096, 1_024]);
        let reversed = [slot(2, 4_096, 32), slot(1, 4_096, 32)];
        assert_eq!(ClaimSlot::recoverable(&reversed, &notes), [4_096, 1_024]);
        // Rather all of a note to the second than its excess lost.
        let notes = of(&[4_096, 4_096]);
        let slots = [slot(1, 3_000, 32), slot(2, 5_000, 32)];
        assert_eq!(ClaimSlot::recoverable(&slots, &notes), [3_000, 4_096]);
    }

    #[test]
    fn a_contract_claims_notes_of_its_keyset_only() {
        let other = [2; 32];
        let notes = vec![(other, 4_096), (K, 512), (other, 1_024)];
        let slots = [
            slot(1, 4_096, 32),
            ClaimSlot {
                keyset: other,
                ..slot(2, 4_096, 32)
            },
        ];
        assert_eq!(ClaimSlot::recoverable(&slots, &notes), [512, 4_096]);
        assert_eq!(allocate(&notes, &slots), [vec![1], vec![0]]);
        assert!(ClaimSlot::recoverable(&[], &notes).is_empty());
        assert_eq!(ClaimSlot::recoverable(&slots[..1], &[]), [0]);
    }

    #[test]
    fn many_contracts_and_notes_are_shared_out_promptly() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let denoms = random(&mut rng, false, 300);
        let slots: Vec<_> = (0..8)
            .map(|id| slot(id, 1_000 + rng.below(5_000), 32))
            .collect();
        let started = std::time::Instant::now();
        let got = ClaimSlot::recoverable(&slots[..4], &of(&denoms));
        assert!(got.iter().all(|g| *g > 0));
        let got = ClaimSlot::recoverable(&slots, &of(&denoms));
        assert!(got.iter().all(|g| *g > 0));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
