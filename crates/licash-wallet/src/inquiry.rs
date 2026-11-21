//! Asking a hub what it holds against the wallet's notes, and believing only
//! proof.
//!
//! The hub answers an inquiry (`licash::inquiry`) with its evidence against
//! a note: the owner-signed spend that retired it, or the hold-key signature
//! of another claim of it. The wallet verifies that itself
//! (`licash::recovery::voids_note`, the statement's own test of an item) and
//! drops a note only on a proof: a hub that says "spent" and shows nothing
//! drops nothing. A spend the hub shows that paid the wallet's change
//! returns that change, for the wallet's scan key opens it
//! ([`Wallet::check`]).
//!
//! Only a signer of a note's hold key can ask about it, so nobody learns the
//! state of the notes of another. The signature is of an inquiry alone and
//! is evidence of nothing against the note.

use std::collections::{BTreeMap, BTreeSet};

use licash::{
    inquiry,
    note::{Keyset, Note},
    recovery::{voids_note, Evidence},
    spend::ProposalId,
};
use licash_api::{Evidenced, Inquire, Payment};
use rusqlite::{params, OptionalExtension};

use crate::{held, now, unavailable, Error, Held, Transport, Wallet};

/// How many spends deep [`Wallet::check`] follows a note's change: each
/// round asks about the notes the last returned.
const ROUNDS: usize = 8;

/// The notes held and not given up, each with its hub's URL, by URL. A
/// note taken by a payment or a recovery is not asked about.
const HELD: &str = "SELECT r.url, n.y FROM notes n
    JOIN keysets k ON k.id = n.keyset JOIN reach r ON r.hub = k.hub
    WHERE n.spent_by IS NULL AND n.gone_at IS NULL";

/// What a hub proved of notes asked about.
#[derive(Default)]
pub(crate) struct Shown {
    /// The notes its evidence proves spent, or claimed elsewhere.
    pub spent: Vec<[u8; 33]>,
    /// The granted spends among that evidence, by id, each of which may
    /// have paid the wallet's change.
    pub payments: Vec<(ProposalId, Payment)>,
    /// The wallet's own locks its hub never certified among that evidence,
    /// each with the notes it spends: its hub made them, whatever it said.
    pub locks: Vec<(ProposalId, Vec<[u8; 33]>)>,
}

/// What [`Wallet::check`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// The value of the notes dropped, shown spent.
    pub dropped: u64,
    /// The value of the change of those spends the wallet found and took.
    pub recovered: u64,
}

impl<T: Transport> Wallet<T> {
    /// Ask the hub at `url` about the notes `ys` held, and what it proves.
    pub(crate) async fn shown(&self, url: &str, ys: &[[u8; 33]]) -> Result<Shown, Error> {
        let by_keyset: BTreeMap<[u8; 32], Vec<[u8; 33]>> = self.store.read(|c| {
            let mut stmt = c.prepare("SELECT keyset FROM notes WHERE y = ?1")?;
            let mut by_keyset = BTreeMap::<_, Vec<_>>::new();
            for y in ys {
                if let Some(id) = stmt
                    .query_row([y], |r| r.get::<_, [u8; 32]>(0))
                    .optional()?
                {
                    by_keyset.entry(id).or_default().push(*y);
                }
            }
            Ok(by_keyset)
        })?;
        let mut shown = Shown::default();
        let mut spends = BTreeSet::new();
        for (id, ys) in by_keyset {
            let keyset = self.keyset_of(&id)?;
            for ys in ys.chunks(inquiry::MAX_NOTES) {
                self.inquire(url, &keyset, ys, &mut shown, &mut spends)
                    .await?;
            }
        }
        Ok(shown)
    }

    async fn inquire(
        &self,
        url: &str,
        keyset: &Keyset,
        ys: &[[u8; 33]],
        shown: &mut Shown,
        spends: &mut BTreeSet<[u8; 32]>,
    ) -> Result<(), Error> {
        let at = u64::try_from(now()).unwrap_or(0);
        let notes: Vec<Held> = self
            .store
            .read(|c| ys.iter().map(|y| held(c, keyset, y)).collect())?;
        let signed = notes
            .iter()
            .map(|h| {
                let message = h.note.message();
                let key = self.derivation(h)?.hold_key(self.owner.hold_root())?;
                Ok((
                    message.clone(),
                    inquiry::sign(keyset.id(), at, message, &key)?,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let answer: Evidenced = self
            .post(
                url,
                licash_api::INQUIRE,
                &Inquire::new(keyset.id(), at, &signed),
            )
            .await?;
        let evidence = answer
            .decode(keyset, notes.len())
            .map_err(|problem| unavailable(url, problem))?;
        for ((y, held), evidence) in ys.iter().zip(&notes).zip(&evidence) {
            let mut spent = false;
            for e in evidence {
                match voids_note(keyset, held.note.message(), e) {
                    Ok(true) => spent = true,
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::warn!(%url, %error, "evidence of a spent note that does not verify");
                        continue;
                    }
                }
                let Evidence::Spend {
                    signed,
                    notes,
                    unlock,
                } = e
                else {
                    continue;
                };
                let id = signed.proposal().id();
                if !spends.insert(*id.as_bytes()) {
                    continue;
                }
                match unlock {
                    Some(unlock) => {
                        let payment = Payment::new(url, signed, notes, unlock);
                        shown.payments.push((id, payment));
                    }
                    // Perhaps a lock of the wallet's given up on its hub's
                    // word, which it served all the same.
                    None => {
                        if let Some(inputs) = self.revive_lock(&id, notes)? {
                            shown.locks.push((id, inputs));
                        }
                    }
                }
            }
            if spent {
                shown.spent.push(*y);
            }
        }
        Ok(())
    }

    /// Take the lock `id` on again, if the wallet gave it up uncertified,
    /// its hub having shown its signed request spending `inputs`: the
    /// hub's refusal was a lie. Those of its inputs still free are its own
    /// again, not dropped as spent; a lock its hub certified or settled is
    /// as it was. The points of its inputs if `id` is a lock of the
    /// wallet's, not certified, now under way.
    fn revive_lock(
        &self,
        id: &ProposalId,
        inputs: &[Note],
    ) -> Result<Option<Vec<[u8; 33]>>, Error> {
        let ys = (inputs.iter())
            .map(|n| Ok(n.message().identifier()?.0))
            .collect::<Result<Vec<_>, Error>>()?;
        let (revived, live) = self.store.write(|tx| {
            let revived = tx.execute(
                "UPDATE locks SET abandoned = NULL
                 WHERE id = ?1 AND abandoned IS NOT NULL AND certificate IS NULL",
                [id.as_bytes()],
            )? == 1;
            if revived {
                reserve_for_lock(tx, id, &ys)?;
            }
            let live: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM locks
                 WHERE id = ?1 AND abandoned IS NULL AND certificate IS NULL)",
                [id.as_bytes()],
                |r| r.get(0),
            )?;
            Ok((revived, live))
        })?;
        if revived {
            let id = hex::encode(id.as_bytes());
            tracing::warn!(%id, "a lock given up on its hub's refusal, shown made, taken on again");
        }
        Ok(live.then_some(ys))
    }

    /// Ask each hub about the notes held and drop those it proves spent:
    /// after restoring an old copy of the wallet file, whose notes a newer
    /// copy may have spent. A spend that paid the wallet's change returns
    /// it, and that change is asked about in turn. A hub learns that one
    /// party holds the notes asked about; no other party can ask.
    pub async fn check(&self) -> Result<Checked, Error> {
        let held = self.store.read(|c| {
            let mut stmt = c.prepare(&format!("{HELD} ORDER BY r.url"))?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        self.check_held(held).await
    }

    /// [`Wallet::check`] of the notes `ys` alone, those of them held: as a
    /// close that its hub refused for notes it says are spent asks.
    pub async fn check_notes(&self, ys: &[[u8; 33]]) -> Result<Checked, Error> {
        let mut held = self.store.read(|c| {
            let mut stmt = c.prepare(&format!("{HELD} AND n.y = ?1"))?;
            let mut held = Vec::new();
            for y in ys {
                let rows = stmt.query_map([y], |r| Ok((r.get(0)?, r.get(1)?)))?;
                for row in rows {
                    held.push(row?);
                }
            }
            Ok(held)
        })?;
        held.sort();
        self.check_held(held).await
    }

    /// [`Wallet::check`] of `held`, each note with its hub's URL, in URL order.
    async fn check_held(&self, held: Vec<(String, [u8; 33])>) -> Result<Checked, Error> {
        let mut checked = Checked::default();
        for at in held.chunk_by(|a, b| a.0 == b.0) {
            let url = &at[0].0;
            let mut ys: Vec<[u8; 33]> = at.iter().map(|(_, y)| *y).collect();
            for _ in 0..ROUNDS {
                if ys.is_empty() {
                    break;
                }
                let shown = self.shown(url, &ys).await?;
                checked.dropped += self.drop_spent(url, &shown.spent)?;
                ys = Vec::new();
                for (_, payment) in &shown.payments {
                    let Ok(opened) = self.open_payment(payment) else {
                        continue;
                    };
                    let change = opened
                        .ours
                        .iter()
                        .filter_map(|(_, note)| Some(note.message().identifier().ok()?.0))
                        .collect::<Vec<_>>();
                    match self.keep(
                        &opened.keyset,
                        opened.url,
                        opened.signed.proposal(),
                        &opened.ours,
                        None,
                    ) {
                        Ok(received) => {
                            checked.recovered += received.amount;
                            ys.extend(change);
                        }
                        Err(Error::AlreadyReceived | Error::PaysNothing) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(checked)
    }

    /// Drop the notes `spent` that the hub at `url` proved so, still held:
    /// their value.
    pub(crate) fn drop_spent(&self, url: &str, spent: &[[u8; 33]]) -> Result<u64, Error> {
        let dropped = self.store.write(|tx| {
            let mut dropped = 0;
            for y in spent {
                let denom: Option<i64> = tx
                    .query_row(
                        "UPDATE notes SET gone_at = ?2, session = NULL
                         WHERE y = ?1 AND spent_by IS NULL AND gone_at IS NULL
                         RETURNING denom",
                        params![y, now()],
                        |r| r.get(0),
                    )
                    .optional()?;
                dropped += denom.map_or(0, |d| d as u64);
            }
            Ok(dropped)
        })?;
        if !spent.is_empty() {
            tracing::warn!(%url, notes = spent.len(), "notes shown spent by a copy of the wallet dropped");
        }
        Ok(dropped)
    }
}

/// Reserve for the lock `id` those of its inputs `ys` that are free: in no
/// payment, unlink session or recovery, and not dropped.
pub(crate) fn reserve_for_lock(
    tx: &rusqlite::Transaction<'_>,
    id: &ProposalId,
    ys: &[[u8; 33]],
) -> Result<(), Error> {
    for y in ys {
        tx.execute(
            "UPDATE notes SET spent_by = ?1, spent_kind = ?3
             WHERE y = ?2 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
            params![id.as_bytes(), y, crate::Spender::Lock.name()],
        )?;
    }
    Ok(())
}
