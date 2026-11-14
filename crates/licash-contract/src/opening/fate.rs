//! What the chain says of an opening once F is known ([`Fate`]), and when
//! its funding gates lapse. Each party's driver watches F's inputs from its
//! signature on: the holder from [`super::Phase::Funded`], the hub from
//! [`super::HubPhase::Signed`]. Short of those it may forget the opening
//! ([`Holder::abandonable`], [`Hub::abandonable`]); from them on it gives
//! the opening up only once F is dead: once its gate has lapsed with F
//! unconfirmed, by spending a coin of its own of F (the holder her deposit,
//! the hub one of its inputs), unless F confirms first.

use lngap_btc::bitcoin::{OutPoint, Transaction, TxOut, Txid};

use super::{at, Holder, Hub};

/// Where an opening's F stands on the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// F buried as deep as Base's funding depth: the contract stands.
    Live,
    /// An input of F spent by another transaction buried as deep: F can
    /// never confirm, and the opening is over.
    Dead,
    /// Neither, yet.
    Open,
}

/// What spends a coin, as a driver looks it up: the spending transaction
/// and how deep it is buried, 0 unconfirmed; none while unspent.
pub type Spent = Option<(Txid, u32)>;

/// F as a party knows it, from the hub's commitment to it on.
#[derive(Clone, Copy, Debug)]
pub struct Funding<'a> {
    /// Signed as far as the party has it, whole once the holder signed it:
    /// of one txid however signed, its every input a native segwit one.
    pub tx: &'a Transaction,
    /// The coins it spends, one per input.
    pub prevouts: &'a [TxOut],
    /// Its input spending her deposit.
    pub deposit: usize,
}

impl<'a> Funding<'a> {
    /// The hub's own coins F spends, all but her deposit: those it keeps
    /// reserved while the opening may yet stand.
    pub fn hub_coins(&self) -> Vec<(OutPoint, &'a TxOut)> {
        let inputs = self.tx.input.iter().zip(self.prevouts).enumerate();
        let theirs = inputs.filter(|(i, _)| *i != self.deposit);
        theirs.map(|(_, (i, o))| (i.previous_output, o)).collect()
    }
}

/// The fate of `funding` buried `depth` deep (at least once), its inputs
/// looked up by `spent`.
fn fate<E>(
    funding: &Transaction,
    depth: u32,
    mut spent: impl FnMut(OutPoint) -> Result<Spent, E>,
) -> Result<Fate, E> {
    let txid = funding.compute_txid();
    for input in &funding.input {
        if let Some((by, deep)) = spent(input.previous_output)? {
            if deep >= depth.max(1) {
                return Ok(if by == txid { Fate::Live } else { Fate::Dead });
            }
        }
    }
    Ok(Fate::Open)
}

impl Holder {
    /// F as she knows it, from the hub's commitment to it on.
    pub fn funding(&self) -> Option<Funding<'_>> {
        let (c, frozen, _) = self.commit.as_ref()?;
        let signed = self.signed.as_ref().map(|s| &s.funding);
        Some(Funding {
            tx: self.funding.as_ref().or(signed).unwrap_or(&frozen.funding),
            prevouts: &c.prevouts,
            deposit: at(&frozen.funding, self.start.deposit),
        })
    }

    /// Whether her gate has lapsed at `tip`: F funded from it on would not
    /// let her complete a recovery before the deadline, so she signs none.
    /// Once she has signed one, and it is unconfirmed, her driver spends
    /// her deposit otherwise. Never before Base.
    pub fn lapsed(&self, tip: u32) -> bool {
        let base = self.base.as_ref().map(|(base, _)| base);
        base.is_some_and(|base| !base.fundable_at(tip))
    }

    /// What the chain says of her F, its inputs looked up by `spent`;
    /// nothing before she knows F.
    pub fn fate<E>(
        &self,
        spent: impl FnMut(OutPoint) -> Result<Spent, E>,
    ) -> Result<Option<Fate>, E> {
        let (Some(f), Some((base, _))) = (self.funding(), &self.base) else {
            return Ok(None);
        };
        fate(f.tx, base.parts().chain.funding_depth, spent).map(Some)
    }
}

impl Hub {
    /// F as it knows it, from its funding on.
    pub fn funding(&self) -> Option<Funding<'_>> {
        let (frozen, c, _) = self.frozen.as_ref()?;
        let (open, _) = self.base.as_ref()?;
        let signed = self.signed.as_ref().map(|s| &s.funding);
        Some(Funding {
            tx: self.funded.as_ref().or(signed).unwrap_or(&frozen.funding),
            prevouts: &c.prevouts,
            deposit: at(&frozen.funding, open.deposit),
        })
    }

    /// Whether its gate has lapsed at `tip`, as the holder's does: it signs
    /// no F from it on. Once it has signed one, and it is unconfirmed, its
    /// driver may spend one of its coins of F otherwise, or sooner by its
    /// own policy. Never before an Open.
    pub fn lapsed(&self, tip: u32) -> bool {
        let base = self.base.as_ref().map(|(_, base)| base);
        base.is_some_and(|base| !base.fundable_at(tip))
    }

    /// What the chain says of its F, its inputs looked up by `spent`;
    /// nothing before it funded F.
    pub fn fate<E>(
        &self,
        spent: impl FnMut(OutPoint) -> Result<Spent, E>,
    ) -> Result<Option<Fate>, E> {
        let Some(f) = self.funding() else {
            return Ok(None);
        };
        fate(f.tx, self.base().parts().chain.funding_depth, spent).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lngap_btc::bitcoin::hashes::Hash as _;
    use lngap_btc::bitcoin::{absolute, transaction, TxIn};
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

    /// The fate of F spending coins 1 and 2, buried 2 deep, as `spends`
    /// says what spends each.
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
        // In the mempool, then not yet deep enough, then deep enough.
        assert_eq!(of(&both(0), 2), Fate::Open);
        assert_eq!(of(&both(1), 2), Fate::Open);
        assert_eq!(of(&both(2), 2), Fate::Live);
        // Its second coin spent otherwise: dead once as deep.
        assert_eq!(of(&[(coin(2), other, 1)], 2), Fate::Open);
        assert_eq!(of(&[(coin(2), other, 3)], 2), Fate::Dead);
        // A depth of none still asks a block.
        assert_eq!(of(&[(coin(2), other, 0)], 0), Fate::Open);
        assert_eq!(of(&both(1), 0), Fate::Live);
    }
}
