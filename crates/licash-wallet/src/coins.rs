//! Which notes a payment spends and which denominations it makes: pure, so
//! that they are tested alone.

use licash_api::Fee;

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

/// The largest of `notes`, `(denomination, id)`, until they pay `amount`
/// and the `fee` due on them: few inputs, so small spends. `Err` with what
/// paying would take, at most [`MAX_INPUTS`] notes counted.
pub fn select<T: Copy>(notes: &[(u64, T)], amount: u64, fee: &Fee) -> Result<Selection<T>, u64> {
    let mut sorted = notes.to_vec();
    sorted.sort_by_key(|&(denom, _)| std::cmp::Reverse(denom));
    let mut total = 0u64;
    let mut inputs = Vec::new();
    for (denom, id) in sorted.into_iter().take(MAX_INPUTS) {
        if !inputs.is_empty() && total >= amount.saturating_add(fee.due(total)) {
            break;
        }
        total = total.saturating_add(denom);
        inputs.push(id);
    }
    let due = fee.due(total);
    if inputs.is_empty() || total < amount.saturating_add(due) {
        return Err(amount.saturating_add(due));
    }
    Ok(Selection {
        inputs,
        total,
        amount,
        fee: due,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREE: Fee = Fee { base: 0, ppm: 0 };

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
    fn nothing_selects_from_nothing() {
        assert_eq!(select::<u8>(&[], 0, &Fee { base: 2, ppm: 0 }), Err(2));
    }
}
