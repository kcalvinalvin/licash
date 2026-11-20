//! Which notes a payment spends and which denominations it makes: pure, so
//! that they are tested alone.

use licash_api::NoteFee;

/// The most notes one payment spends, which keeps a spend and its grant
/// well within what a hub reads.
pub const MAX_INPUTS: usize = 64;

/// `amount` in the fewest notes of `denoms`, largest first. None if the
/// denominations cannot make it.
pub fn split(amount: u64, denoms: impl DoubleEndedIterator<Item = u64>) -> Option<Vec<u64>> {
    let mut left = amount;
    let mut notes = Vec::new();
    for denom in denoms.rev() {
        while denom > 0 && left >= denom {
            notes.push(denom);
            left -= denom;
        }
    }
    (left == 0).then_some(notes)
}

/// What a payment spends: `inputs`, of `total` value, pay `amount` and the
/// hub's `fee`; the rest is change.
#[derive(Debug, PartialEq, Eq)]
pub struct Selection<T> {
    pub inputs: Vec<T>,
    pub total: u64,
    pub amount: u64,
    pub fee: u64,
}

impl<T> Selection<T> {
    pub fn change(&self) -> u64 {
        self.total - self.amount - self.fee
    }
}

/// The fee a spend of notes worth `total` pays its hub, `amount` of it
/// going to others in `made` notes and the rest, after the fee, back to
/// the spender in `notes(rest)` notes. The hub charges for those notes
/// too, so the fee is found with them: the fee on as few notes of change
/// as leaves no more of them, which may pay a little more than is due
/// when less change takes more notes. `Err` with the fee due if `total`
/// does not pay it.
pub fn settle(
    total: u64,
    amount: u64,
    made: usize,
    fee: &NoteFee,
    notes: impl Fn(u64) -> usize,
) -> Result<u64, u64> {
    let mut paid_for = 0usize;
    loop {
        let paid = fee.due(total, made.saturating_add(paid_for));
        // Not `amount + paid` saturated: past `u64`, that would be `total`.
        let Some(change) = total.checked_sub(amount).and_then(|v| v.checked_sub(paid)) else {
            return Err(paid);
        };
        // The fee grows with the notes paid for until it leaves no more
        // of them than that, or nothing: it ends.
        let needed = notes(change);
        if needed <= paid_for || fee.due(total, made.saturating_add(needed)) <= paid {
            return Ok(paid);
        }
        paid_for += 1;
    }
}

/// The largest of `notes`, `(denomination, id)`, until they pay `amount`
/// and the `fee` due on them and on the notes made, a value making
/// `made(value)` notes: few inputs, so small spends. `Err` with what
/// paying would take, at most [`MAX_INPUTS`] notes counted.
pub fn select<T: Copy>(
    notes: &[(u64, T)],
    amount: u64,
    fee: &NoteFee,
    made: impl Fn(u64) -> usize,
) -> Result<Selection<T>, u64> {
    let mut sorted = notes.to_vec();
    sorted.sort_by_key(|&(denom, _)| std::cmp::Reverse(denom));
    let settle = |total| settle(total, amount, made(amount), fee, &made);
    let mut total = 0u64;
    let mut inputs = Vec::new();
    for (denom, id) in sorted.into_iter().take(MAX_INPUTS) {
        if !inputs.is_empty() && settle(total).is_ok() {
            break;
        }
        total = total.saturating_add(denom);
        inputs.push(id);
    }
    match settle(total) {
        Ok(fee) if !inputs.is_empty() => Ok(Selection {
            inputs,
            total,
            amount,
            fee,
        }),
        Ok(fee) | Err(fee) => Err(amount.saturating_add(fee)),
    }
}

/// Of `notes`, `(denomination, id)`, the largest first that make `amount`
/// exactly, at most [`MAX_INPUTS`]: found whenever any are, if each
/// denomination divides the larger ones.
pub fn exact<T: Copy>(notes: &[(u64, T)], amount: u64) -> Option<Vec<T>> {
    let mut sorted = notes.to_vec();
    sorted.sort_by_key(|&(denom, _)| std::cmp::Reverse(denom));
    let mut left = amount;
    let mut inputs = Vec::new();
    for (denom, id) in sorted {
        if denom <= left && inputs.len() < MAX_INPUTS {
            left -= denom;
            inputs.push(id);
        }
    }
    (left == 0 && !inputs.is_empty()).then_some(inputs)
}

#[cfg(test)]
mod tests {
    use licash_api::Fee;

    use super::*;

    const FREE: Fee = Fee { base: 0, ppm: 0 };

    /// The notes `value` is made in, of every power of two.
    fn bits(value: u64) -> usize {
        value.count_ones() as usize
    }

    /// [`super::select`] under a fee on value alone.
    fn select<T: Copy>(notes: &[(u64, T)], amount: u64, fee: &Fee) -> Result<Selection<T>, u64> {
        let fee = NoteFee {
            fee: *fee,
            output_msat: 0,
        };
        super::select(notes, amount, &fee, bits)
    }

    #[test]
    fn split_takes_the_fewest_notes_largest_first() {
        let denoms = || [1, 2, 4, 8].into_iter();
        assert_eq!(split(13, denoms()), Some(vec![8, 4, 1]));
        assert_eq!(split(0, denoms()), Some(vec![]));
        assert_eq!(split(31, denoms()), Some(vec![8, 8, 8, 4, 2, 1]));
        assert_eq!(split(3, [2, 4].into_iter()), None);
    }

    #[test]
    fn select_takes_the_largest_notes_until_amount_and_fee_are_paid() {
        let notes = [(1, 'a'), (8, 'b'), (2, 'c'), (4, 'd')];
        let s = select(&notes, 9, &FREE).unwrap();
        assert_eq!((s.inputs, s.total, s.fee), (vec!['b', 'd'], 12, 0));

        let fee = Fee { base: 1, ppm: 0 };
        let s = select(&notes, 10, &fee).unwrap();
        assert_eq!((s.inputs.len(), s.total, s.fee, s.change()), (2, 12, 1, 1));
        let s = select(&notes, 12, &fee).unwrap();
        assert_eq!((s.inputs.len(), s.total, s.fee, s.change()), (3, 14, 1, 1));
        assert_eq!(select(&notes, 15, &fee), Err(16));
        assert_eq!(select(&notes, 15, &FREE).unwrap().change(), 0);
    }

    #[test]
    fn the_fee_is_due_on_the_inputs_taken() {
        // 1% of the inputs, rounded up: 100 pays 1, so 99 is the most it
        // pays out.
        let fee = Fee {
            base: 0,
            ppm: 10_000,
        };
        let s = select(&[(100, 0)], 99, &fee).unwrap();
        assert_eq!((s.fee, s.change()), (1, 0));
        assert_eq!(select(&[(100, 0)], 100, &fee), Err(101));
    }

    #[test]
    fn select_spends_at_most_max_inputs() {
        let notes: Vec<_> = (0..MAX_INPUTS as u32 + 10).map(|i| (1, i)).collect();
        let s = select(&notes, MAX_INPUTS as u64, &FREE).unwrap();
        assert_eq!(s.inputs.len(), MAX_INPUTS);
        assert_eq!(select(&notes, MAX_INPUTS as u64 + 1, &FREE), Err(65));
    }

    #[test]
    fn exact_takes_notes_worth_the_amount_or_none() {
        let notes = [(1, 'a'), (8, 'b'), (2, 'c'), (4, 'd'), (4, 'e')];
        assert_eq!(exact(&notes, 9), Some(vec!['b', 'a']));
        assert_eq!(exact(&notes, 10), Some(vec!['b', 'c']));
        assert_eq!(exact(&notes, 16), Some(vec!['b', 'd', 'e']));
        assert_eq!(exact(&notes, 19), Some(vec!['b', 'd', 'e', 'c', 'a']));
        assert_eq!(exact(&notes, 20), None);
        assert_eq!(exact(&notes, 0), None);
        assert_eq!(exact(&[(16, 'a')], 8), None);
    }

    #[test]
    fn nothing_selects_from_nothing() {
        assert_eq!(select::<u8>(&[], 0, &Fee { base: 2, ppm: 0 }), Err(2));
    }

    /// 1 and a sat for each note made for others than the hub.
    const PER_NOTE: NoteFee = NoteFee {
        fee: Fee { base: 1, ppm: 0 },
        output_msat: 1_000,
    };

    #[test]
    fn the_fee_pays_for_the_notes_paid_and_the_notes_of_the_change() {
        // 4 to the payee in one note: a fee of 3 would leave 9, in two
        // notes it does not pay for, and 4 leaves 8, in the one it does.
        assert_eq!(settle(16, 4, 1, &PER_NOTE, bits), Ok(4));
        // No change, no note of it to pay for.
        assert_eq!(settle(6, 4, 1, &PER_NOTE, bits), Ok(2));
        // Less change in more notes costs more than it is worth: 12 pays
        // 9 in two notes and 3, where the 1 left as change would cost 1.
        assert_eq!(settle(12, 9, 2, &PER_NOTE, bits), Ok(3));
        // What does not pay the fee, with what it lacks.
        assert_eq!(settle(5, 4, 1, &PER_NOTE, bits), Err(2));
        assert_eq!(settle(3, 4, 1, &PER_NOTE, bits), Err(2));
        // A fee on value alone is found at once.
        let plain = NoteFee {
            output_msat: 0,
            ..PER_NOTE
        };
        assert_eq!(settle(16, 4, 1, &plain, bits), Ok(1));
        assert_eq!(settle(u64::MAX, 1, 1, &plain, |_| usize::MAX), Ok(1));
    }

    /// A fee past what the total leaves after the amount is never paid,
    /// whatever the amount and the fee add up to.
    #[test]
    fn a_fee_past_u64_with_the_amount_is_not_paid() {
        let huge = NoteFee {
            fee: Fee {
                base: u64::MAX,
                ppm: 0,
            },
            output_msat: 0,
        };
        assert_eq!(settle(u64::MAX, 1, 1, &huge, bits), Err(u64::MAX));
    }

    #[test]
    fn a_settled_fee_is_what_its_hub_asks_of_the_notes_it_leaves() {
        for output_msat in [0, 100, 600, 1_000, 2_500] {
            let fee = NoteFee {
                fee: Fee { base: 1, ppm: 300 },
                output_msat,
            };
            for total in (1..4_000u64).step_by(7) {
                for amount in (0..=total).step_by(13) {
                    let made = bits(amount);
                    match settle(total, amount, made, &fee, bits) {
                        Ok(paid) => {
                            let change = total - amount - paid;
                            assert!(paid >= fee.due(total, made + bits(change)));
                        }
                        Err(due) => assert!(amount + due > total, "{total} {amount}"),
                    }
                }
            }
        }
    }

    #[test]
    fn select_takes_more_notes_when_those_made_cost_more() {
        let notes = [(8, 'a'), (4, 'b'), (2, 'c'), (1, 'd')];
        // 8 pays 4 and 3, for the note paid and one of change: 1.
        let s = super::select(&notes, 4, &PER_NOTE, bits).unwrap();
        assert_eq!((s.change(), s.total, s.fee, s.inputs), (1, 8, 3, vec!['a']));
        // 8 does not pay 6 in two notes and their fee of 3: 12 does, and
        // 4 for a change of 2 in one note more.
        let s = super::select(&notes, 6, &PER_NOTE, bits).unwrap();
        assert_eq!(
            (s.change(), s.total, s.fee, s.inputs),
            (2, 12, 4, vec!['a', 'b'])
        );
        assert_eq!(super::select(&notes, 13, &PER_NOTE, bits), Err(17));
    }
}
