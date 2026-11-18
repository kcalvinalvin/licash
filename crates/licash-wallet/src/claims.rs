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
//! that the hub certified and never settled, once it expired at depth
//! ([`LockRefund`]): first, within its cap, before its share of the
//! notes. Taken as the notes are, under its id, given back with them.

use std::cmp::Reverse;
use std::collections::HashMap;

use licash::{
    claim::{Branch, Item, Source},
    crypto::{Point, ScalarBytes},
    note::{Keyset, Message, Note, Ownership},
    recovery::SourceWitness,
    source::Certificate,
    spend::{Output, ProposalId},
};
use rusqlite::{params, OptionalExtension};

use crate::{held, invalid, Error, Held, Transport, Wallet};

/// The refund of a lock this wallet made, claimed on Bitcoin: its hub
/// certified it and settled neither branch, and it expired at depth, when
/// the refund is owed (filed earlier, it would be owed nothing and given
/// up). Its refund outputs paying the wallet, as pending items of the
/// claim, each authorized by its hold key; the hub's certificate of it and
/// its signed body show the hub admitted it.
#[derive(Clone)]
pub struct LockRefund {
    pub id: ProposalId,
    pub items: Vec<Item>,
    pub source: Source,
    pub witness: SourceWitness,
    /// Each item's message and hold key.
    pub keys: Vec<(Message, ScalarBytes)>,
    /// What its items are worth.
    pub value: u64,
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

impl<T: Transport> Wallet<T> {
    /// What each of `slots`, the contracts at a hub with the one asserting
    /// first first, would recover of the wallet's notes now, by id: what
    /// [`Wallet::claim_notes`] would take for it, its cap at most. A
    /// contract whose recovery took its notes already recovers none of
    /// those left, nor keeps any from the others.
    pub fn recoverable(&self, slots: &[ClaimSlot]) -> Result<HashMap<[u8; 32], u64>, Error> {
        self.store.read(|c| {
            let (open, _, notes) = claimable(c, slots)?;
            let amounts = ClaimSlot::recoverable(&open, &notes);
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

    /// [`Wallet::claim_notes`], after the refunds of the wallet's locks
    /// under its slot's keyset that expired at depth by `tip` unsettled
    /// ([`LockRefund`]), the largest first, as many as fit its cap and its
    /// count: its share of the notes is of what they leave of both. All in
    /// one, or none.
    pub fn claim_with_refunds(
        &self,
        id: &[u8; 32],
        slots: &[ClaimSlot],
        tip: u32,
    ) -> Result<(Vec<ClaimNote>, Vec<LockRefund>), Error> {
        let slot = *slot_of(id, slots)?;
        let keyset = self.keyset_of(&slot.keyset)?;
        self.store.write(|t| {
            if took(t, id)? {
                return Err(Error::Invalid(
                    "this recovery took its notes already".into(),
                ));
            }
            let mut stmt = t.prepare(
                "SELECT id, request, certificate, expiry FROM locks
                 WHERE certificate IS NOT NULL AND settled IS NULL AND done_at IS NULL
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
                match self.lock_refund(&keyset, &request, &certificate) {
                    Ok(Some((refund, expired))) if expired <= tip => due.push(refund),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        lock = %hex::encode(lock), "a lock's refund not claimable: {e}"
                    ),
                }
            }
            due.sort_by_key(|r| (Reverse(r.value), *r.id.as_bytes()));
            let (mut cap, mut max) = (slot.cap, slot.max);
            let mut refunds = Vec::new();
            for refund in due {
                if refund.value <= cap && refund.keys.len() <= max {
                    cap -= refund.value;
                    max -= refund.keys.len();
                    refunds.push(refund);
                }
            }
            let left: Vec<_> = (slots.iter())
                .map(|s| match s.id == *id {
                    true => ClaimSlot { cap, max, ..*s },
                    false => *s,
                })
                .collect();
            // Before the refunds are marked, which close its slot.
            let notes = match cap > 0 && max > 0 {
                true => take_notes(t, &keyset, id, &left)?,
                false => vec![],
            };
            for refund in &refunds {
                let taken = t.execute(
                    "UPDATE locks SET claimed_by = ?1 WHERE id = ?2 AND claimed_by IS NULL",
                    params![id, refund.id.as_bytes()],
                )?;
                if taken != 1 {
                    return Err(Error::NoteTaken);
                }
            }
            Ok((notes, refunds))
        })
    }

    /// The lock refunds the recovery of contract `id` took
    /// ([`Wallet::claim_with_refunds`]), of the keyset of id `keyset`.
    pub fn claimed_refunds(
        &self,
        id: &[u8; 32],
        keyset: &[u8; 32],
    ) -> Result<Vec<LockRefund>, Error> {
        let keyset = self.keyset_of(keyset)?;
        let rows: Vec<(String, Vec<u8>)> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT request, certificate FROM locks WHERE claimed_by = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut refunds = Vec::with_capacity(rows.len());
        for (request, certificate) in rows {
            if let Some((refund, _)) = self.lock_refund(&keyset, &request, &certificate)? {
                refunds.push(refund);
            }
        }
        Ok(refunds)
    }

    /// The refund of the lock of `request`, certified by `certificate`,
    /// under `keyset`, and the height at which it is owed, its expiry at
    /// depth: none if under another keyset or paying the wallet nothing.
    fn lock_refund(
        &self,
        keyset: &Keyset,
        request: &str,
        certificate: &[u8],
    ) -> Result<Option<(LockRefund, u32)>, Error> {
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
        let refund = LockRefund {
            id: proposal.id(),
            items,
            source: Source::Admitted(certificate),
            witness: SourceWitness::Signed { signed, notes },
            keys,
            value,
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
            let freed = t.execute("UPDATE notes SET spent_by = NULL WHERE spent_by = ?1", [id])?;
            let locks = t.execute(
                "UPDATE locks SET claimed_by = NULL WHERE claimed_by = ?1",
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
    fn keyset_of(&self, id: &[u8; 32]) -> Result<Keyset, Error> {
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

fn slot_of<'s>(id: &[u8; 32], slots: &'s [ClaimSlot]) -> Result<&'s ClaimSlot, Error> {
    (slots.iter().find(|s| s.id == *id))
        .ok_or_else(|| Error::Invalid("a recovery claiming in no slot".into()))
}

/// Whether the recovery of contract `id` took notes or lock refunds.
fn took(c: &rusqlite::Connection, id: &[u8; 32]) -> Result<bool, Error> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM notes WHERE spent_by = ?1)
            OR EXISTS (SELECT 1 FROM locks WHERE claimed_by = ?1)",
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
            "UPDATE notes SET spent_by = ?1
             WHERE y = ?2 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
            params![id, y],
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
