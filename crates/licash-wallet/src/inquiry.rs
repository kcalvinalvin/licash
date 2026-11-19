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
    note::Keyset,
    recovery::{voids_note, Evidence},
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
    /// The granted spends among that evidence, each of which may have paid
    /// the wallet's change.
    pub payments: Vec<Payment>,
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
                if let Evidence::Spend {
                    signed,
                    notes,
                    unlock: Some(unlock),
                } = e
                {
                    if spends.insert(*signed.proposal().id().as_bytes()) {
                        shown
                            .payments
                            .push(Payment::new(url, signed, notes, unlock));
                    }
                }
            }
            if spent {
                shown.spent.push(*y);
            }
        }
        Ok(())
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
                for payment in &shown.payments {
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
