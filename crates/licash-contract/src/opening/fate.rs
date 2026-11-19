//! Funding remains binding until it confirms or a conflicting spend does.
use super::{at, Holder, Hub};
use lichen_btc::bitcoin::{OutPoint, Transaction, TxOut, Txid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Live,
    Dead,
    Open,
}
pub type Spent = Option<(Txid, u32)>;
#[derive(Clone, Copy, Debug)]
pub struct Funding<'a> {
    pub tx: &'a Transaction,
    pub prevouts: &'a [TxOut],
    pub deposit: usize,
}
impl<'a> Funding<'a> {
    pub fn hub_coins(&self) -> Vec<(OutPoint, &'a TxOut)> {
        self.tx
            .input
            .iter()
            .zip(self.prevouts)
            .enumerate()
            .filter(|(i, _)| *i != self.deposit)
            .map(|(_, (input, coin))| (input.previous_output, coin))
            .collect()
    }
}

fn fate<E>(
    tx: &Transaction,
    depth: u32,
    mut spent: impl FnMut(OutPoint) -> Result<Spent, E>,
) -> Result<Fate, E> {
    let id = tx.compute_txid();
    for input in &tx.input {
        if let Some((by, deep)) = spent(input.previous_output)? {
            if deep >= depth.max(1) {
                return Ok(if by == id { Fate::Live } else { Fate::Dead });
            }
        }
    }
    Ok(Fate::Open)
}
impl Holder {
    pub fn funding(&self) -> Option<Funding<'_>> {
        let o = self.offer.as_ref()?;
        Some(Funding {
            tx: self.funding_tx()?,
            prevouts: &o.prevouts,
            deposit: at(&o.funding, self.open.deposit),
        })
    }
    pub fn lapsed(&self, tip: u32) -> bool {
        self.base().is_some_and(|b| !b.fundable_at(tip))
    }
    pub fn fate<E>(
        &self,
        spent: impl FnMut(OutPoint) -> Result<Spent, E>,
    ) -> Result<Option<Fate>, E> {
        match (self.funding(), self.base()) {
            (Some(f), Some(b)) => fate(f.tx, b.parts().chain.funding_depth, spent).map(Some),
            _ => Ok(None),
        }
    }
}
impl Hub {
    pub fn funding(&self) -> Option<Funding<'_>> {
        let (tx, prevouts) = self.funds.as_ref()?;
        Some(Funding {
            tx: self.funding_tx()?,
            prevouts,
            deposit: at(tx, self.open.as_ref()?.deposit),
        })
    }
    pub fn lapsed(&self, tip: u32) -> bool {
        self.base().is_some_and(|b| !b.fundable_at(tip))
    }
    pub fn fate<E>(
        &self,
        spent: impl FnMut(OutPoint) -> Result<Spent, E>,
    ) -> Result<Option<Fate>, E> {
        match (self.funding(), self.base()) {
            (Some(f), Some(b)) => fate(f.tx, b.parts().chain.funding_depth, spent).map(Some),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lichen_btc::bitcoin::{absolute, hashes::Hash, transaction, TxIn};
    use std::convert::Infallible;
    fn spending(outpoints: &[OutPoint]) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: outpoints
                .iter()
                .map(|o| TxIn {
                    previous_output: *o,
                    ..Default::default()
                })
                .collect(),
            output: vec![],
        }
    }
    fn coin(n: u8) -> OutPoint {
        OutPoint::new(Txid::from_byte_array([n; 32]), 0)
    }
    fn of(spends: &[(OutPoint, Txid, u32)], depth: u32) -> Fate {
        let f = spending(&[coin(1), coin(2)]);
        let lookup = |o: OutPoint| -> Result<Spent, Infallible> {
            let spend = spends.iter().find(|(c, _, _)| *c == o);
            Ok(spend.map(|(_, by, deep)| (*by, *deep)))
        };
        fate(&f, depth, lookup).unwrap()
    }
    #[test]
    fn f_lives_once_buried_and_dies_once_another_spend_is() {
        let f = spending(&[coin(1), coin(2)]).compute_txid();
        let other = spending(&[coin(2)]).compute_txid();
        assert_eq!(of(&[], 2), Fate::Open);
        let both = |deep| [(coin(1), f, deep), (coin(2), f, deep)];
        assert_eq!(of(&both(0), 2), Fate::Open);
        assert_eq!(of(&both(1), 2), Fate::Open);
        assert_eq!(of(&both(2), 2), Fate::Live);
        assert_eq!(of(&[(coin(2), other, 1)], 2), Fate::Open);
        assert_eq!(of(&[(coin(2), other, 3)], 2), Fate::Dead);
        assert_eq!(of(&[(coin(2), other, 0)], 0), Fate::Open);
        assert_eq!(of(&both(1), 0), Fate::Live);
    }
    #[test]
    fn only_a_deep_f_or_a_deep_conflict_settles_its_fate() {
        let coins: Vec<_> = (1..=2)
            .map(|n| OutPoint::new(Txid::from_byte_array([n; 32]), 0))
            .collect();
        let f = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: coins
                .iter()
                .map(|o| TxIn {
                    previous_output: *o,
                    ..Default::default()
                })
                .collect(),
            output: vec![],
        };
        let other = Txid::from_byte_array([9; 32]);
        let check = |spends: [Spent; 2]| {
            fate::<Infallible>(&f, 2, |o| {
                Ok(spends[coins.iter().position(|c| *c == o).unwrap()])
            })
            .unwrap()
        };
        let unspent = None;
        assert_eq!(check([unspent; 2]), Fate::Open);
        assert_eq!(check([Some((f.compute_txid(), 1)), unspent]), Fate::Open);
        assert_eq!(check([Some((f.compute_txid(), 2)), unspent]), Fate::Live);
        assert_eq!(check([unspent, Some((other, 1))]), Fate::Open);
        assert_eq!(check([unspent, Some((other, 2))]), Fate::Dead);
    }
}
