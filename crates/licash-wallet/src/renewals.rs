//! Renewals of notes a hub no longer unlinks since it restarted
//! ([`Wallet::unlink`]): the wallet pays them to itself, for notes it
//! unlinks then.
//!
//! A hub answers an unlink only under the nonce it drew for the note in
//! the run that issued it: once it restarted, it refuses notes it issued
//! before as [`Code::Expired`]. The wallet records that refusal, then
//! renews the notes, once each: a renewal's notes stay linked if their
//! hub restarted again meanwhile. Notes whose refusal an older version
//! did not record are never renewed. A renewal is recorded and its notes
//! taken before their hub hears of it, as a payment is, and only
//! [`Wallet::unlink`] takes it on: [`Wallet::resume`] leaves it.

use licash::{
    address::Ephemeral,
    claim::Branch,
    note::Keyset,
    spend::{Proposal, ProposalId},
};
use licash_api::{Code, Fees, Info, Offer};
use rusqlite::{params, OptionalExtension};

use crate::{
    code_name,
    coins::{self, MAX_INPUTS},
    invalid, made, now, outputs, sql, take, Error, Transport, Wallet, RENEWAL_REFUSED,
};

/// Notes renewed together, of one keyset: `(y, denomination)` each.
type Batch = (Keyset, Vec<([u8; 33], u64)>);

/// How a renewal taken on ended for now.
enum Renewed {
    Done,
    /// Refused for its fee: the hub's fees were asked again, its notes
    /// still renewable.
    Fee,
    /// Given up, or not done yet.
    No,
}

impl<T: Transport> Wallet<T> {
    /// Renew the notes held at the hub `hub`, reached at `url`, that it no
    /// longer unlinks: take the renewals under way on, then make those its
    /// fees allow. Whether any finished.
    pub(crate) async fn renew(&self, hub: i64, url: &str) -> Result<bool, Error> {
        let mut renewed = false;
        let under_way: Vec<[u8; 32]> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM sends
                 WHERE hub = ?1 AND renewal = 1 AND done_at IS NULL AND abandoned IS NULL
                 ORDER BY created_at",
            )?;
            let rows = stmt.query_map([hub], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        for id in under_way {
            let id = ProposalId::from_bytes(id);
            let ys = self.inputs_of(&id)?;
            renewed |= matches!(self.advance_renewal(url, &id, &ys).await?, Renewed::Done);
        }
        for (keyset, batch) in self.renewable(hub)? {
            renewed |= self.renew_batch(hub, url, &keyset, &batch).await?;
        }
        Ok(renewed)
    }

    /// The notes held at the hub `hub` it no longer unlinks, but a
    /// renewal's: by keyset, those of one payment together, apart from the
    /// others', at most [`MAX_INPUTS`] at once, the largest first.
    fn renewable(&self, hub: i64) -> Result<Vec<Batch>, Error> {
        type Row = ([u8; 32], Vec<u8>, [u8; 33], i64);
        let rows: Vec<Row> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT n.keyset, n.proposal, n.y, n.denom FROM notes n
                 JOIN keysets k ON k.id = n.keyset
                 WHERE k.hub = ?1 AND n.unlinked = -1 AND n.stays_linked = ?2
                    AND n.spent_by IS NULL AND n.gone_at IS NULL AND n.session IS NULL
                    AND NOT EXISTS (SELECT 1 FROM sends s WHERE s.id = n.proposal AND s.renewal = 1)
                 ORDER BY n.keyset, n.proposal, n.denom DESC, n.y",
            )?;
            let rows = stmt.query_map(params![hub, code_name(Code::Expired)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let keysets = self.keysets(hub)?;
        let mut batches = Vec::new();
        for payment in rows.chunk_by(|a, b| (a.0, &a.1) == (b.0, &b.1)) {
            let Some(keyset) = keysets.iter().find(|k| k.id() == &payment[0].0) else {
                continue;
            };
            for batch in payment.chunks(MAX_INPUTS) {
                let notes = batch.iter().map(|r| (r.2, r.3 as u64)).collect();
                batches.push((keyset.clone(), notes));
            }
        }
        Ok(batches)
    }

    /// Renew the notes `batch` of `keyset`, `(y, denomination)`, at the
    /// hub `hub`, reached at `url`, if its fee on them is within the limit:
    /// its fees asked again once if it refuses the fee it advertised.
    /// Whether the renewal finished.
    async fn renew_batch(
        &self,
        hub: i64,
        url: &str,
        keyset: &Keyset,
        batch: &[([u8; 33], u64)],
    ) -> Result<bool, Error> {
        let ys: Vec<[u8; 33]> = batch.iter().map(|n| n.0).collect();
        let total = batch.iter().fold(0u64, |sum, n| sum.saturating_add(n.1));
        for asked in [false, true] {
            let info = self.hub(url)?.1;
            let (fee, refused) = self.renewal_price(hub, keyset, total, &info);
            let limit = self.renew_limit();
            if let Some(why) = refused {
                tracing::info!(%url, total, fee, limit, "notes stay linked: {why}");
                return Ok(false);
            }
            let Some(id) = self.start_renewal(hub, keyset, &ys, total, fee, &info)? else {
                return Ok(false);
            };
            let key = hex::encode(id.as_bytes());
            tracing::info!(id = %key, %url, total, fee, "renewing notes their hub no longer unlinks");
            match self.advance_renewal(url, &id, &ys).await? {
                Renewed::Done => return Ok(true),
                Renewed::Fee if !asked => {}
                Renewed::Fee => {
                    self.renewal_refused(&ys)?;
                    return Ok(false);
                }
                Renewed::No => return Ok(false),
            }
        }
        Ok(false)
    }

    /// The same batches and fee decision as execution, without starting a
    /// renewal. Refused notes alone do not justify endless periodic work.
    pub(crate) fn renewal_needed(&self, hub: i64, info: &Info) -> Result<bool, Error> {
        Ok(self.renewable(hub)?.iter().any(|(keyset, batch)| {
            let total = batch.iter().fold(0u64, |sum, n| sum.saturating_add(n.1));
            self.renewal_price(hub, keyset, total, info).1.is_none()
        }))
    }

    fn renewal_price(
        &self,
        hub: i64,
        keyset: &Keyset,
        total: u64,
        info: &Info,
    ) -> (u64, Option<&'static str>) {
        let fee = coins::settle(total, 0, 0, &info.fees.spending(), |value| {
            made(keyset, value)
        })
        .unwrap_or_else(|due| due);
        let refused = match self.ceilings_of(hub) {
            Some(agreed) if info.fees.above(&agreed) => Some("their hub charges more than agreed"),
            Some(_) if fee >= total => Some("renewing them would leave nothing"),
            Some(_) => None,
            None if !allowed(fee, total, self.renew_limit(), &info.fees) => {
                Some("renewing them costs more than the limit")
            }
            None => None,
        };
        (fee, refused)
    }

    /// The renewal of the notes `ys` of `keyset`, worth `total`, paying the
    /// hub `fee`, recorded and its notes taken: none if they were taken or
    /// stopped being renewable meanwhile.
    fn start_renewal(
        &self,
        hub: i64,
        keyset: &Keyset,
        ys: &[[u8; 33]],
        total: u64,
        fee: u64,
        info: &Info,
    ) -> Result<Option<ProposalId>, Error> {
        let inputs = self.notes_of(keyset, ys)?;
        let me = self.address();
        let ephemeral = Ephemeral::random();
        let parts = [(&me, total - fee), (&info.address, fee)];
        let outputs = outputs(keyset, &ephemeral, Branch::Pay, &parts)?;
        let proposal = Proposal::new(keyset, &inputs, ephemeral.public(), outputs, None)?;
        let id = proposal.id();
        let offer = serde_json::to_string(&Offer::new(&proposal, &inputs)).map_err(invalid)?;
        let expired = code_name(Code::Expired);
        let started = self.store.write(|tx| {
            for y in ys {
                let renewable: bool = tx.query_row(
                    "SELECT stays_linked IS ?2 FROM notes WHERE y = ?1",
                    params![y, expired],
                    |r| r.get(0),
                )?;
                if !renewable {
                    return Err(Error::NoteTaken);
                }
            }
            tx.execute(
                "INSERT INTO sends (id, hub, payee, amount, fee, offer, created_at, renewal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
                params![
                    id.as_bytes(),
                    hub,
                    me.to_bytes(),
                    sql(total - fee)?,
                    sql(fee)?,
                    offer,
                    now()
                ],
            )?;
            take(tx, &id, crate::Spender::Send, ys)
        });
        match started {
            Ok(()) => Ok(Some(id)),
            Err(Error::NoteTaken) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Take the renewal `id` of the notes `ys` on, at `url`. Refused, it is
    /// given up and its notes freed: they stay linked, unless only its fee
    /// fell short.
    async fn advance_renewal(
        &self,
        url: &str,
        id: &ProposalId,
        ys: &[[u8; 33]],
    ) -> Result<Renewed, Error> {
        match self.advance(id).await {
            Ok(_) => Ok(Renewed::Done),
            Err(Error::Refused { problem, .. }) if problem.code == Code::Fee => {
                tracing::info!(%url, %problem, "fees changed, asking them again");
                self.refresh(url).await?;
                Ok(Renewed::Fee)
            }
            Err(Error::Refused { problem, .. }) => {
                tracing::info!(%url, %problem, "notes stay linked: their hub refused to renew them");
                self.renewal_refused(ys)?;
                Ok(Renewed::No)
            }
            Err(Error::Abandoned { .. }) => Ok(Renewed::No),
            Err(e) => Err(e),
        }
    }

    /// The notes the payment `id` takes: those it holds, and, of a send,
    /// those its offer spends that a recovery took from it.
    pub(crate) fn inputs_of(&self, id: &ProposalId) -> Result<Vec<[u8; 33]>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare("SELECT y FROM notes WHERE spent_by = ?1 ORDER BY y")?;
            let rows = stmt.query_map([id.as_bytes()], |r| r.get(0))?;
            let mut ys: Vec<[u8; 33]> = rows.collect::<Result<_, _>>()?;
            let offer: Option<String> = c
                .query_row(
                    "SELECT offer FROM sends WHERE id = ?1",
                    [id.as_bytes()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(offer) = offer {
                ys.extend(crate::claims::send_inputs(&offer)?);
                ys.sort_unstable();
                ys.dedup();
            }
            Ok(ys)
        })
    }

    /// The notes `ys`, renewable, are no longer: their hub refused to renew
    /// them, or their owner gave their renewal up.
    pub(crate) fn renewal_refused(&self, ys: &[[u8; 33]]) -> Result<(), Error> {
        let expired = code_name(Code::Expired);
        self.store.write(|tx| {
            for y in ys {
                tx.execute(
                    "UPDATE notes SET stays_linked = ?3 WHERE y = ?1 AND stays_linked = ?2",
                    params![y, expired, RENEWAL_REFUSED],
                )?;
            }
            Ok(())
        })
    }
}

/// Whether `fee` on renewing notes worth `total` is at most `ppm` parts
/// per million of it, leaving some.
fn within(fee: u64, total: u64, ppm: u32) -> bool {
    fee < total && u128::from(fee) * 1_000_000 <= u128::from(total) * u128::from(ppm)
}

/// Without contract ceilings, a renewal may pay within the amount-based
/// limit or a modest advertised spending schedule. A zero limit still
/// permits only a free renewal, and some value must always remain.
pub(crate) fn allowed(fee: u64, total: u64, ppm: u32, fees: &Fees) -> bool {
    within(fee, total, ppm)
        || (fee < total
            && ppm > 0
            && fees.spend.base <= 1
            && fees.spend.ppm <= u64::from(ppm)
            && fees.output_msat <= 100)
}

#[cfg(test)]
mod tests {
    use licash_api::Fee;

    use super::*;

    #[test]
    fn a_modest_schedule_allows_small_renewals_but_zero_means_free_only() {
        let fees = Fees {
            spend: Fee { base: 1, ppm: 100 },
            output_msat: 100,
            ..Fees::default()
        };
        assert!(allowed(2, 64, 1_000, &fees));
        assert!(!allowed(2, 64, 0, &fees));
        assert!(!allowed(2, 2, 1_000, &fees));
        assert!(!allowed(0, 0, 1_000, &fees));
        for expensive in [
            Fees {
                spend: Fee { base: 2, ppm: 100 },
                ..fees
            },
            Fees {
                spend: Fee {
                    base: 1,
                    ppm: 1_001,
                },
                ..fees
            },
            Fees {
                output_msat: 101,
                ..fees
            },
        ] {
            assert!(!allowed(2, 64, 1_000, &expensive));
            // Expensive schedules may still pass the amount-based limit.
            assert!(allowed(2, 10_000, 1_000, &expensive));
        }
        assert!(allowed(0, 64, 0, &fees));
    }

    #[test]
    fn a_renewal_costs_at_most_its_limit_and_leaves_some() {
        assert!(within(0, 1, 0));
        assert!(!within(1, 1_000_000, 0));
        assert!(within(1, 10_000, 100));
        assert!(!within(1, 9_999, 100));
        assert!(within(100, 1_000_000, 100));
        assert!(!within(101, 1_000_000, 100));
        assert!(!within(5, 5, 1_000_000));
        assert!(within(u64::MAX - 1, u64::MAX, u32::MAX));
        assert!(!within(0, 0, u32::MAX));
    }
}
