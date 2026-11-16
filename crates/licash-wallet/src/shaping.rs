//! A common note distribution for every wallet client: within a note
//! budget, use the smallest possible largest denomination. No contract's
//! cap or wallet identity is encoded in the distribution.

use licash::{crypto::tagged_hash, note::Keyset};
use licash_api::NoteFee;

use crate::coins::{self, Selection, MAX_INPUTS};
use crate::{Error, Transport, Wallet};

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
        let noise = tagged_hash(
            "licash/shaping/target",
            &[&self.keys.shaping, &ordinary.inputs[0]],
        );
        let target = 18 + usize::from(noise[0]) % 7;
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
