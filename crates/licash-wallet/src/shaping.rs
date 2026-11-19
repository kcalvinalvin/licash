//! A common note distribution for every wallet client: within a note
//! budget, use the smallest possible largest denomination. No contract's
//! cap or wallet identity is encoded in the distribution.

use licash::{
    address::{Address, Ephemeral},
    claim::Branch,
    crypto::tagged_hash,
    note::Keyset,
    spend::Proposal,
};
use licash_api::{Info, NoteFee, Offer};
use rusqlite::{params, Connection, OptionalExtension};

use crate::coins::{self, Selection, MAX_INPUTS};
use crate::{
    denominated_outputs, denominations, hub_url, invalid, now, sql, take, Error, Transport, Wallet,
};

const DAY: i64 = 24 * 60 * 60;
const STALE: i64 = 7 * DAY;

impl crate::Sent {
    /// Whether this operation rearranges `owner`'s notes, rather than
    /// paying an amount away. Ordinary sends reject zero amounts and
    /// renewals always leave a positive amount.
    pub fn is_optimization(&self, owner: &Address) -> bool {
        self.amount == 0 && self.payee == *owner
    }
}

/// A snapshot of a possible note optimization. `value` is the input value
/// reminted, not an amount paid away. Note counts cover the hub's currently
/// held notes, not notes pending in other operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptimizationQuote {
    pub hub: String,
    pub value: u64,
    pub fee: u64,
    pub notes_before: usize,
    pub notes_after: usize,
}

/// A completed optimization, recorded as a zero-amount self-payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Optimization {
    pub id: String,
    pub quote: OptimizationQuote,
}

#[derive(Clone, Copy)]
enum Mode {
    Automatic,
    /// Read-only scheduling: automatic authority, ignoring temporary waits.
    Future,
    Manual {
        max_fee: u64,
    },
}

struct Remint {
    keyset: Keyset,
    plan: Plan<[u8; 33]>,
    quote: OptimizationQuote,
    short_rest: bool,
}

impl<T: Transport> Wallet<T> {
    /// Whether a later sync round can finish note work, including privacy
    /// rest, cooldown and rolling-budget waits. Reads cached hub fees only,
    /// and never reserves notes or starts network work. A remint that cannot
    /// fit its fee authority or even a fresh daily budget is not a reason
    /// to keep a periodic job alive.
    pub fn note_maintenance_needed(&self) -> Result<bool, Error> {
        // Like a payment line, work stuck for a week (its hub gone) is
        // retried when the app is opened, not kept alive in the background.
        let since = now().saturating_sub(STALE);
        let pending: bool = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM sends s JOIN reach r ON r.hub = s.hub
                 WHERE s.payee = ?1 AND s.done_at IS NULL AND s.abandoned IS NULL
                 AND s.created_at > ?2)
                 OR EXISTS (SELECT 1 FROM notes n JOIN keysets k ON k.id = n.keyset
                 JOIN reach r ON r.hub = k.hub WHERE n.unlinked = 0
                 AND n.spent_by IS NULL AND n.gone_at IS NULL AND n.added_at > ?2)",
                params![self.address().to_bytes(), since],
                |r| r.get(0),
            )?)
        })?;
        if pending {
            return Ok(true);
        }
        let reachable: Vec<(i64, String)> = self.store.read(|c| {
            let mut stmt = c.prepare("SELECT hub, MIN(url) FROM reach GROUP BY hub")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        for (hub, url) in reachable {
            let info = self.hub(&url)?.1;
            if self.renewal_needed(hub, &info)? {
                return Ok(true);
            }
        }
        for (url, info) in self.hubs()? {
            let hub = self.hub(&url)?.0;
            if let Some(remint) = self.remint_plan(hub, &url, &info, Mode::Future, now())? {
                let value: i64 = self.store.read(|c| {
                    Ok(c.query_row(
                        "SELECT COALESCE(SUM(n.denom), 0) FROM notes n
                         JOIN keysets k ON k.id = n.keyset WHERE k.hub = ?1
                         AND n.spent_by IS NULL AND n.gone_at IS NULL",
                        [hub],
                        |r| r.get(0),
                    )?)
                })?;
                if within_daily_budget(value.try_into().map_err(invalid)?, 0, remint.quote.fee) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Preview explicit optimization at `url`, as its hub last advertised
    /// its fees. Only unlinked notes are used. Unlike automatic maintenance,
    /// explicit optimization may bypass rest periods and the daily budget.
    pub fn optimization_quote(&self, url: &str) -> Result<Option<OptimizationQuote>, Error> {
        let url = hub_url(url)?;
        let (hub, info) = self.hub(&url)?;
        self.agreed_fees(hub, &url, &info)?;
        Ok(self
            .remint_plan(hub, &url, &info, Mode::Manual { max_fee: u64::MAX }, now())?
            .map(|remint| remint.quote))
    }

    /// Explicitly optimize notes at `url`, paying at most `max_fee` sat.
    /// This bypasses automatic rest and daily-budget guards, but never
    /// contract ceilings or the unlinked-input rule. A fee refusal is not
    /// retried at a higher price. None if no improvement is available or
    /// another optimization is still pending.
    pub async fn optimize_notes(
        &self,
        url: &str,
        max_fee: u64,
    ) -> Result<Option<Optimization>, Error> {
        self.optimize_at(url, Mode::Manual { max_fee }).await
    }

    /// Opportunistically maintain notes at each trusted hub, at most one
    /// remint per hub. Inputs rest after unlinking, attempts rest after
    /// being recorded, and fees fit both their authority and a rolling
    /// 1%-of-current-note-value daily budget (at least 1 sat). Unknown
    /// unlink times begin a rest, never request a remint. Call after
    /// resuming pending work and unlinking, from the application's loop.
    pub async fn maintain_notes(&self) -> Result<usize, Error> {
        let mut completed = 0;
        let mut failed = None;
        for (url, _) in self.hubs()? {
            match self.optimize_at(&url, Mode::Automatic).await {
                Ok(Some(_)) => completed += 1,
                Ok(None) => {}
                Err(e) => {
                    tracing::info!(%url, %e, "note maintenance deferred");
                    failed.get_or_insert(e);
                }
            }
        }
        failed.map_or(Ok(completed), Err)
    }

    async fn optimize_at(&self, url: &str, mode: Mode) -> Result<Option<Optimization>, Error> {
        let url = hub_url(url)?;
        let (hub, info) = self.hub(&url)?;
        self.agreed_fees(hub, &url, &info)?;
        let at = now();
        if matches!(mode, Mode::Automatic) {
            // A partial/imported row may lack this metadata even after
            // migration. Unknown age starts here, not at its receipt.
            self.store.write(|tx| {
                tx.execute(
                    "UPDATE notes SET unlinked_at = ?2 WHERE unlinked = 1
                     AND unlinked_at IS NULL AND spent_by IS NULL AND gone_at IS NULL
                     AND keyset IN (SELECT id FROM keysets WHERE hub = ?1)",
                    params![hub, at],
                )?;
                // A crash may leave a refused attempt without an end
                // clock. Its first observation starts a conservative
                // rest rather than treating its old creation as enough.
                tx.execute(
                    "UPDATE sends SET done_at = ?3 WHERE hub = ?1 AND payee = ?2
                     AND amount = 0 AND renewal = 0 AND abandoned IS NOT NULL AND done_at IS NULL",
                    params![hub, self.address().to_bytes(), at],
                )?;
                Ok(())
            })?;
        }
        let Some(remint) = self.remint_plan(hub, &url, &info, mode, at)? else {
            return Ok(None);
        };
        if let Mode::Manual { max_fee } = mode {
            if remint.quote.fee > max_fee {
                return Err(Error::Invalid(format!(
                    "optimizing costs {} sat, more than the limit of {max_fee}",
                    remint.quote.fee
                )));
            }
        }
        if !self
            .store
            .read(|c| self.remint_allowed(c, hub, mode, at, &remint))?
        {
            return Ok(None);
        }
        let inputs = self.notes_of(&remint.keyset, &remint.plan.selection.inputs)?;
        let ephemeral = Ephemeral::random();
        let me = self.address();
        let parts = [
            (
                &info.address,
                denominations(&remint.keyset, remint.quote.fee)?,
            ),
            (&me, remint.plan.change.clone()),
        ];
        let outputs = denominated_outputs(&ephemeral, Branch::Pay, &parts)?;
        let proposal = Proposal::new(&remint.keyset, &inputs, ephemeral.public(), outputs, None)?;
        let id = proposal.id();
        let offer = serde_json::to_string(&Offer::new(&proposal, &inputs)).map_err(invalid)?;
        let started = self.store.write(|tx| {
            let recorded = now();
            if matches!(mode, Mode::Automatic) && recorded < at {
                return Ok(false);
            }
            // Disjoint inputs do not prevent concurrent paid remints.
            // Recheck the hub-wide guards under this IMMEDIATE lock.
            if !self.remint_allowed(tx, hub, mode, recorded, &remint)? {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO sends (id, hub, payee, amount, fee, offer, created_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
                params![
                    id.as_bytes(),
                    hub,
                    me.to_bytes(),
                    sql(remint.quote.fee)?,
                    offer,
                    recorded
                ],
            )?;
            take(tx, &id, crate::Spender::Send, &remint.plan.selection.inputs)?;
            Ok(true)
        });
        match started {
            Ok(true) => {}
            Ok(false) | Err(Error::NoteTaken) => return Ok(None),
            Err(e) => return Err(e),
        }
        tracing::info!(%url, fee = remint.quote.fee, before = remint.quote.notes_before,
            after = remint.quote.notes_after, "optimizing notes");
        if let Err(e) = self.advance(&id).await {
            if matches!(&e, Error::Refused { problem, .. } if problem.code == licash_api::Code::Fee)
            {
                // Learn for the next preview/attempt, never silently pay
                // more now. The recorded attempt still anchors the rest.
                if let Ok((_, info)) = self.refresh(&url).await {
                    self.agreed_fees(hub, &url, &info)?;
                }
            }
            return Err(e);
        }
        // Blind the replacement notes promptly, but do not recursively
        // pay for a renewal if the hub restarted in the meantime.
        if let Err(e) = self.unlink_linked(hub, &url).await {
            tracing::info!(%url, %e, "optimized notes stay linked for now");
        }
        Ok(Some(Optimization {
            id: hex::encode(id.as_bytes()),
            quote: remint.quote,
        }))
    }

    fn remint_plan(
        &self,
        hub: i64,
        url: &str,
        info: &Info,
        mode: Mode,
        at: i64,
    ) -> Result<Option<Remint>, Error> {
        type Row = (u64, [u8; 33], [u8; 32], bool, Option<i64>);
        let rows: Vec<Row> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT n.denom, n.y, n.keyset, n.unlinked = 1 AND n.session IS NULL, n.unlinked_at
                 FROM notes n JOIN keysets k ON k.id = n.keyset
                 WHERE k.hub = ?1 AND n.spent_by IS NULL AND n.gone_at IS NULL
                 ORDER BY n.denom DESC, n.y",
            )?;
            let rows = stmt.query_map([hub], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let noise = self.shape_noise("licash/shaping/target", &first.1);
        let target = 18 + (noise % 7) as usize;
        let pressure = rows.len() >= 28 + (noise % 5) as usize;
        let rest = match mode {
            Mode::Automatic => self.shape_rest(&first.1, pressure),
            Mode::Manual { .. } | Mode::Future => 0,
        };
        let agreed = self.ceilings_of(hub);
        let willing = |total, fee| match mode {
            Mode::Manual { .. } => fee < total,
            Mode::Automatic | Mode::Future => match agreed {
                Some(agreed) => !info.fees.above(&agreed) && fee < total,
                None => crate::renewals::allowed(fee, total, self.renew_limit(), &info.fees),
            },
        };
        let mut best: Option<Remint> = None;
        // The healthy band is fixed. A newly drawn target must not turn
        // yesterday's healthy 24-note layout into recurring paid work.
        let before_score = (rows.len().saturating_sub(24), first.0);
        let mut best_score = before_score;
        for keyset in self.keysets(hub)? {
            let denoms: Vec<_> = keyset.denominations().collect();
            let notes: Vec<_> = rows
                .iter()
                .filter(|r| &r.2 == keyset.id())
                .map(|&(d, y, _, unlinked, clock)| {
                    (
                        d,
                        y,
                        unlinked
                            && match mode {
                                Mode::Manual { .. } | Mode::Future => true,
                                Mode::Automatic => {
                                    clock.is_some_and(|clock| rested(at, clock, rest))
                                }
                            },
                    )
                })
                .collect();
            let eligible: Vec<_> = notes
                .iter()
                .filter(|n| n.2)
                .map(|&(d, y, _)| (d, y))
                .collect();
            let spending = info.fees.spending();
            let Ok(ordinary) = coins::select(&eligible, 0, &spending, |v| {
                coins::split(v, denoms.iter().copied()).map_or(0, |n| n.len())
            }) else {
                continue;
            };
            let Some(plan) = payment(
                &notes,
                ordinary,
                &spending,
                &denoms,
                target.saturating_sub(rows.len() - notes.len()),
                willing,
            ) else {
                continue;
            };
            if !willing(plan.selection.total, plan.selection.fee) {
                continue;
            }
            let after = rows.len() - plan.selection.inputs.len() + plan.change.len();
            let largest = rows
                .iter()
                .filter(|r| !plan.selection.inputs.contains(&r.1))
                .map(|r| r.0)
                .chain(plan.change.iter().copied())
                .max()
                .unwrap_or(0);
            let score = (after.saturating_sub(24), largest);
            if score >= best_score {
                continue;
            }
            best_score = score;
            let quote = OptimizationQuote {
                hub: url.to_owned(),
                value: plan.selection.total,
                fee: plan.selection.fee,
                notes_before: rows.len(),
                notes_after: after,
            };
            best = Some(Remint {
                keyset,
                plan,
                quote,
                short_rest: pressure && after < 28,
            });
        }
        Ok(best)
    }

    fn remint_allowed(
        &self,
        c: &Connection,
        hub: i64,
        mode: Mode,
        at: i64,
        remint: &Remint,
    ) -> Result<bool, Error> {
        let me = self.address().to_bytes();
        let pending: bool = c.query_row(
            "SELECT EXISTS (SELECT 1 FROM sends WHERE hub = ?1 AND payee = ?2 AND amount = 0
             AND renewal = 0 AND done_at IS NULL AND abandoned IS NULL)",
            params![hub, me],
            |r| r.get(0),
        )?;
        if pending {
            return Ok(false);
        }
        if matches!(mode, Mode::Manual { .. }) {
            return Ok(true);
        }
        let last: Option<([u8; 32], i64)> = c
            .query_row(
                "SELECT id, MAX(created_at, COALESCE(done_at, created_at)) AS rested_from
             FROM sends WHERE hub = ?1 AND payee = ?2 AND amount = 0
             AND renewal = 0 ORDER BY rested_from DESC, rowid DESC LIMIT 1",
                params![hub, me],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, clock)) = last {
            if !rested(at, clock, self.shape_rest(&id, remint.short_rest)) {
                return Ok(false);
            }
        }
        let used: i64 = c.query_row(
            "SELECT COALESCE(SUM(fee), 0) FROM sends WHERE hub = ?1 AND payee = ?2 AND amount = 0
             AND renewal = 0 AND abandoned IS NULL
             AND MAX(created_at, COALESCE(done_at, created_at)) >= ?3",
            params![hub, me, at.saturating_sub(DAY)],
            |r| r.get(0),
        )?;
        let value: i64 = c.query_row(
            "SELECT COALESCE(SUM(n.denom), 0) FROM notes n JOIN keysets k ON k.id = n.keyset
             WHERE k.hub = ?1 AND n.spent_by IS NULL AND n.gone_at IS NULL",
            [hub],
            |r| r.get(0),
        )?;
        let (Ok(used), Ok(value)) = (u64::try_from(used), u64::try_from(value)) else {
            return Err(Error::Invalid(
                "negative note-maintenance accounting".into(),
            ));
        };
        Ok(within_daily_budget(value, used, remint.quote.fee))
    }

    fn shape_noise(&self, domain: &str, generation: &[u8]) -> u64 {
        let hash = tagged_hash(domain, &[&self.keys.shaping, generation]);
        u64::from_le_bytes(hash[..8].try_into().expect("eight hash bytes"))
    }

    fn shape_rest(&self, generation: &[u8], short: bool) -> i64 {
        let noise = self.shape_noise("licash/shaping/rest", generation);
        if short {
            120 + (noise % 481) as i64
        } else {
            3_600 + (noise % 18_001) as i64
        }
    }
}

fn rested(at: i64, since: i64, rest: i64) -> bool {
    since > 0 && at >= since && at.saturating_sub(since) >= rest
}

fn within_daily_budget(value: u64, used: u64, fee: u64) -> bool {
    used.checked_add(fee)
        .is_some_and(|total| total <= (value / 100).max(1))
}

impl<T: Transport> Wallet<T> {
    /// Select ordinary payment inputs, then shape change and add only
    /// unlinked extras when doing so improves the wallet's distribution.
    pub(crate) fn payment_plan(
        &self,
        hub: i64,
        amount: u64,
        fees: &licash_api::Fees,
    ) -> Result<(Keyset, Plan<[u8; 33]>), Error> {
        let (keyset, ordinary) = self.select(hub, amount, &fees.spending())?;
        let rows = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT n.denom, n.y, n.unlinked = 1 AND n.session IS NULL, n.keyset
                 FROM notes n JOIN keysets k ON k.id = n.keyset
                 WHERE k.hub = ?1 AND n.spent_by IS NULL AND n.gone_at IS NULL
                 ORDER BY n.denom, n.y",
            )?;
            let rows = stmt.query_map([hub], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, [u8; 33]>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, [u8; 32]>(3)?,
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        // Common bounds, varied by private generation-specific noise:
        // stable between quoting and paying, not predictable by a hub.
        let target =
            18 + (self.shape_noise("licash/shaping/target", &ordinary.inputs[0]) % 7) as usize;
        let others = rows.iter().filter(|row| &row.3 != keyset.id()).count();
        let notes: Vec<_> = rows
            .into_iter()
            .filter(|row| &row.3 == keyset.id())
            .map(|(d, y, unlinked, _)| (d, y, unlinked))
            .collect();
        let agreed = self.ceilings_of(hub);
        let willing = |total, paid| match agreed {
            Some(agreed) => !fees.above(&agreed),
            None => crate::renewals::allowed(paid, total, self.renew_limit(), fees),
        };
        let denoms: Vec<_> = keyset.denominations().collect();
        let plan = payment(
            &notes,
            ordinary,
            &fees.spending(),
            &denoms,
            target.saturating_sub(others),
            willing,
        )
        .ok_or_else(|| Error::Invalid("the keyset cannot make the payment's outputs".into()))?;
        Ok((keyset, plan))
    }
}

/// A payment's selected inputs and the exact denominations of its change.
/// Fee calculation and output construction use this same plan.
pub(crate) struct Plan<T> {
    pub selection: Selection<T>,
    pub change: Vec<u64>,
}

/// Improve an ordinary payment without making linked notes additional
/// inputs. Try prefixes of the smallest unlinked extras, retaining the
/// ordinary selection when no candidate improves the wallet's shape or
/// the automatic fee allowance refuses it.
fn payment<T: Copy + Eq>(
    notes: &[(u64, T, bool)],
    ordinary: Selection<T>,
    fee: &NoteFee,
    denoms: &[u64],
    target: usize,
    willing: impl Fn(u64, u64) -> bool,
) -> Option<Plan<T>> {
    let minimum = |value| coins::split(value, denoms.iter().copied());
    let mut best = Plan {
        change: minimum(ordinary.change())?,
        selection: ordinary,
    };
    let mut chosen = best.selection.inputs.clone();
    let mut total = best.selection.total;
    let amount = best.selection.amount;
    let ordinary_fee = best.selection.fee;
    let paid_notes = minimum(amount)?.len();
    // Count pressure wins first, then smaller denominations. Do not pay
    // more merely to fill unused note slots with an unchanged largest
    // denomination.
    let score = |plan: &Plan<T>| {
        let (count, largest) = notes
            .iter()
            .filter(|(_, id, _)| !plan.selection.inputs.contains(id))
            .fold((plan.change.len(), 0), |(count, largest), &(d, _, _)| {
                (count + 1, largest.max(d))
            });
        (
            count.saturating_sub(target),
            largest.max(plan.change.first().copied().unwrap_or(0)),
            plan.selection.fee,
            count.abs_diff(target),
        )
    };
    let mut best_score = score(&best);
    let mut extras: Vec<_> = notes
        .iter()
        .filter(|(_, id, unlinked)| *unlinked && !chosen.contains(id))
        .map(|&(d, id, _)| (d, id))
        .collect();
    extras.sort_by_key(|&(d, _)| d);
    let mut extras = extras.into_iter();
    loop {
        let retained = notes
            .iter()
            .filter(|(_, id, _)| !chosen.contains(id))
            .count();
        let room = target.saturating_sub(retained);
        let change = |value| distribution(value, denoms, room).or_else(|| minimum(value));
        let settled = coins::settle(total, amount, paid_notes, fee, |value| {
            change(value).map_or(0, |notes| notes.len())
        });
        if let Ok(paid) = settled {
            if paid <= ordinary_fee || willing(total, paid) {
                if let Some(change) = change(total - amount - paid) {
                    let candidate = Plan {
                        selection: Selection {
                            inputs: chosen.clone(),
                            total,
                            amount,
                            fee: paid,
                        },
                        change,
                    };
                    let candidate_score = score(&candidate);
                    if candidate_score < best_score {
                        best = candidate;
                        best_score = candidate_score;
                    }
                }
            }
        }
        if chosen.len() == MAX_INPUTS {
            break;
        }
        let Some((value, id)) = extras.next() else {
            break;
        };
        let Some(sum) = total.checked_add(value) else {
            break;
        };
        total = sum;
        chosen.push(id);
    }
    Some(best)
}

/// Make `value` in at most `target` notes of `denoms`, minimizing the
/// largest denomination, then splitting the largest notes while room
/// remains. `denoms` must be increasing and positive, each dividing the
/// next, as a keyset's powers of two do. Missing denominations are fine.
/// None if the budget or the denominations cannot represent the value.
fn distribution(value: u64, denoms: &[u64], target: usize) -> Option<Vec<u64>> {
    if value == 0 {
        return Some(Vec::new());
    }
    // Try the smallest ceiling first. Never allocate more than the
    // budget, even for a large value under a tiny denomination.
    let mut notes = None;
    for ceiling in 0..denoms.len() {
        let mut left = value;
        let mut candidate = Vec::new();
        for &denom in denoms[..=ceiling].iter().rev() {
            let count = left / denom;
            if count > (target - candidate.len()) as u64 {
                break;
            }
            candidate.extend(std::iter::repeat_n(denom, count as usize));
            left %= denom;
        }
        if left == 0 {
            notes = Some(candidate);
            break;
        }
    }
    let mut notes = notes?;
    loop {
        notes.sort_unstable_by(|a, b| b.cmp(a));
        let mut split = None;
        for (at, &denom) in notes.iter().enumerate() {
            let index = denoms.binary_search(&denom).ok()?;
            let Some(smaller) = index.checked_sub(1).map(|i| denoms[i]) else {
                continue;
            };
            let count = denom / smaller;
            if count - 1 <= (target - notes.len()) as u64 {
                split = Some((at, smaller, count as usize));
                break;
            }
        }
        let Some((at, smaller, count)) = split else {
            return Some(notes);
        };
        notes.splice(at..=at, std::iter::repeat_n(smaller, count));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use licash_api::Fee;

    fn denoms() -> Vec<u64> {
        (0..16).map(|power| 1 << power).collect()
    }

    const FREE: NoteFee = NoteFee {
        fee: Fee { base: 0, ppm: 0 },
        output_msat: 0,
    };

    #[test]
    fn unknown_or_future_clocks_never_prove_that_notes_rested() {
        assert!(!rested(10_000, 0, 120));
        assert!(!rested(10_000, -1, 120));
        assert!(!rested(10_000, 10_001, 120));
        assert!(!rested(10_000, 9_881, 120));
        assert!(rested(10_000, 9_880, 120));
        assert!(!rested(i64::MAX, i64::MAX, 1));
    }

    #[test]
    fn the_daily_budget_is_one_percent_with_a_one_sat_floor() {
        assert!(within_daily_budget(0, 0, 0));
        assert!(within_daily_budget(99, 0, 1));
        assert!(!within_daily_budget(99, 0, 2));
        assert!(within_daily_budget(200, 1, 1));
        assert!(!within_daily_budget(200, 2, 1));
        assert!(!within_daily_budget(200, u64::MAX, 1));
    }

    #[test]
    fn consolidation_adds_only_unlinked_extras() {
        let mut notes = vec![(64, 0, false)];
        notes.extend((1..=40).map(|id| (1, id, id > 20)));
        let ordinary = coins::select(
            &notes.iter().map(|&(d, id, _)| (d, id)).collect::<Vec<_>>(),
            8,
            &FREE,
            |v| v.count_ones() as usize,
        )
        .unwrap();
        let plan = payment(&notes, ordinary, &FREE, &denoms(), 24, |_, _| true).unwrap();
        assert!(plan.selection.inputs.len() > 1);
        assert!(plan.selection.inputs.iter().all(|id| *id == 0 || *id > 20));
        let retained = notes.len() - plan.selection.inputs.len();
        assert!(retained + plan.change.len() <= 24);
        assert_eq!(
            plan.change.iter().sum::<u64>() + plan.selection.amount + plan.selection.fee,
            plan.selection.total
        );
    }

    #[test]
    fn ordinary_payment_availability_is_preserved_when_extras_are_linked() {
        let notes = [(64, 'a', false), (32, 'b', false), (16, 'c', false)];
        let ordinary = coins::select(
            &notes.iter().map(|&(d, id, _)| (d, id)).collect::<Vec<_>>(),
            8,
            &FREE,
            |v| v.count_ones() as usize,
        )
        .unwrap();
        let plan = payment(&notes, ordinary, &FREE, &denoms(), 24, |_, _| true).unwrap();
        assert_eq!(plan.selection.inputs, ['a']);
    }

    #[test]
    fn shaping_pays_for_its_exact_outputs_and_obeys_automatic_fee_authority() {
        let fee = NoteFee {
            fee: Fee { base: 1, ppm: 100 },
            output_msat: 100,
        };
        let notes = [(64, 'a', true)];
        let ordinary =
            || coins::select(&[(64, 'a')], 10, &fee, |v| v.count_ones() as usize).unwrap();
        let before = ordinary();
        let original_fee = before.fee;
        let original_change = coins::split(before.change(), denoms().into_iter()).unwrap();
        let refused = payment(&notes, before, &fee, &denoms(), 24, |_, _| false).unwrap();
        assert_eq!(refused.selection.fee, original_fee);
        assert_eq!(refused.change, original_change);

        let shaped = payment(&notes, ordinary(), &fee, &denoms(), 24, |_, _| true).unwrap();
        assert!(shaped.selection.fee > original_fee);
        assert!(shaped.change.len() <= 24);
        assert!(shaped.change[0] < original_change[0]);
        let due = fee.due(64, 10u64.count_ones() as usize + shaped.change.len());
        assert!(shaped.selection.fee >= due);
        assert_eq!(
            shaped.change.iter().sum::<u64>() + shaped.selection.amount + shaped.selection.fee,
            64
        );
    }

    #[test]
    fn shaping_never_exceeds_the_input_limit_even_when_the_target_is_unreachable() {
        let notes: Vec<_> = (0..100).map(|id| (1, id, true)).collect();
        let ordinary = coins::select(
            &notes.iter().map(|&(d, id, _)| (d, id)).collect::<Vec<_>>(),
            1,
            &FREE,
            |v| v.count_ones() as usize,
        )
        .unwrap();
        let plan = payment(&notes, ordinary, &FREE, &denoms(), 24, |_, _| true).unwrap();
        assert!(plan.selection.inputs.len() <= MAX_INPUTS);
        assert!(notes.len() - plan.selection.inputs.len() + plan.change.len() < 99);
    }

    #[test]
    fn a_distribution_favors_small_notes_with_receiving_room() {
        let denoms: Vec<_> = (0..32).map(|power| 1u64 << power).collect();
        for (value, largest) in [(64, 4), (1_000, 64), (1_000_000, 65_536)] {
            let notes = distribution(value, &denoms, 24).unwrap();
            assert_eq!(notes.len(), 24);
            assert_eq!(notes.iter().sum::<u64>(), value);
            assert_eq!(notes[0], largest);
            assert!(notes.windows(2).all(|pair| pair[0] >= pair[1]));
        }
        assert_eq!(distribution(3, &denoms, 24), Some(vec![1, 1, 1]));
        assert_eq!(distribution(0, &[], 0), Some(vec![]));
        assert_eq!(distribution(1, &denoms, 0), None);
        assert_eq!(distribution(3, &[2, 4], 24), None);
        assert_eq!(distribution(1, &[], 24), None);
        assert_eq!(distribution(u64::MAX, &[1, 2, 4], 24), None);
    }

    #[test]
    fn complete_and_sparse_keysets_use_the_smallest_feasible_largest_note() {
        for denoms in [
            vec![1, 2, 4, 8, 16, 32, 64],
            vec![1, 4, 16, 64],
            vec![2, 8, 32],
        ] {
            for value in 1..=256 {
                // Independent minimum-note dynamic programming for each
                // ceiling, not another greedy implementation.
                let mut minima = Vec::new();
                for &ceiling in &denoms {
                    let mut needed = vec![usize::MAX; value as usize + 1];
                    needed[0] = 0;
                    for amount in 1..=value as usize {
                        needed[amount] = denoms
                            .iter()
                            .copied()
                            .filter(|&d| d <= ceiling && d <= amount as u64)
                            .map(|d| needed[amount - d as usize].saturating_add(1))
                            .min()
                            .unwrap_or(usize::MAX);
                    }
                    minima.push((ceiling, needed[value as usize]));
                }
                for target in 1..=24 {
                    let smallest = minima.iter().find(|(_, count)| *count <= target);
                    match (distribution(value, &denoms, target), smallest) {
                        (None, None) => {}
                        (Some(notes), Some(&(largest, _))) => {
                            assert_eq!(notes[0], largest, "{value}, {target}, {denoms:?}");
                            assert_eq!(notes.iter().sum::<u64>(), value);
                            assert!(notes.len() <= target);
                        }
                        pair => panic!("{value}, {target}, {denoms:?}: {pair:?}"),
                    }
                }
            }
        }
    }
}
