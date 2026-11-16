//! The coins paid for deposits whose notes never came, taken back once
//! their timeout passed: canceled by their hub (its lock to the wallet
//! left unclaimed, say, the phone asleep), paid too late for it to see, or
//! never locked by then, its hub gone. Each round at the chain's tip, by
//! itself: the refund key is the wallet's alone.

use bitcoin::{Amount, FeeRate, OutPoint, Txid};
use licash_contracts::holder;
use licash_wallet::{Deposit, DepositState};

use super::Wallet;
use crate::ChainSource;

/// The fee rate of a refund, in sat/vB: no estimates known.
const REFUND_RATE: u64 = 2;

/// Blocks past a deposit's timeout within which its payment is looked
/// for, and its refund handed the network again, every [`LOOK_EVERY`]
/// blocks: two weeks. Every [`LOOK_LATE`] after, and once each time the
/// app starts: a phone asleep for longer gets its coins back all the same.
const REFUND_BLOCKS: u32 = 2_016;

/// Blocks between looks for the payment of one deposit, or hands of its
/// refund to the network: a contract's F may be read whole to find it.
const LOOK_EVERY: u32 = 6;

/// [`LOOK_EVERY`] once [`REFUND_BLOCKS`] past a deposit's timeout: a day.
const LOOK_LATE: u32 = 144;

impl Wallet {
    /// Take back, through `chain` at `tip`, the coins of each deposit
    /// past its timeout whose notes never came
    /// ([`licash_wallet::Wallet::deposits_to_refund`]), paid as far as
    /// the wallet knows, to its fallback address; and hand the network
    /// again the refunds made, until found in a block. Never a deposit
    /// whose notes were received, nor one whose lock it may have claimed.
    pub(super) fn refund_deposits(&self, chain: &dyn ChainSource, tip: u32) {
        let due = match self.wallet.deposits_to_refund(tip) {
            Ok(due) => due,
            Err(e) => return tracing::warn!(%e, "the deposits to refund"),
        };
        for deposit in due {
            let Some(terms) = &deposit.terms else {
                continue;
            };
            if !self.refund_turn(&deposit.hash, terms.timeout, tip) {
                continue;
            }
            let Some(funding) = self.deposit_paid(&deposit, tip) else {
                continue;
            };
            let to = match self.wallet.fallback_address() {
                Ok(to) => to.script_pubkey(),
                Err(e) => return tracing::warn!(%e, "an address to refund to"),
            };
            let rate = FeeRate::from_sat_per_vb(REFUND_RATE).expect("a rate");
            let value = Amount::from_sat(terms.pays);
            let hash = &deposit.hash;
            match self.wallet.refund_deposit(hash, funding, value, to, rate) {
                Ok(tx) => {
                    let txid = tx.compute_txid();
                    tracing::info!(%hash, %txid, %funding, "a deposit's coins taken back");
                    if let Err(e) = chain.broadcast(bitcoin::consensus::serialize(&tx)) {
                        tracing::warn!(%hash, %txid, %e, "broadcasting a deposit's refund");
                    }
                }
                Err(e) => tracing::warn!(%hash, %funding, %e, "refunding a deposit"),
            }
        }
        self.refund_again(chain, tip);
    }

    /// Whether to act at `tip` on the deposit of `hash` timing out at
    /// `timeout`: look for its payment, or hand its refund to the network
    /// again. The first time since the app started, then every
    /// [`LOOK_EVERY`] blocks, every [`LOOK_LATE`] once [`REFUND_BLOCKS`]
    /// past its timeout.
    fn refund_turn(&self, hash: &str, timeout: u32, tip: u32) -> bool {
        let late = tip > timeout.saturating_add(REFUND_BLOCKS);
        let every = if late { LOOK_LATE } else { LOOK_EVERY };
        let mut looked = self
            .refunds_looked
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let last = looked.get(hash).copied();
        if last.is_some_and(|at| tip < at.saturating_add(every)) {
            return false;
        }
        looked.insert(hash.to_owned(), tip);
        true
    }

    /// Where the payment of `deposit` is, as far as the wallet knows: a
    /// transaction of the wallet's in a block paying it, else the F of a
    /// contract paying its change in, else the payment its hub saw.
    fn deposit_paid(&self, deposit: &Deposit, tip: u32) -> Option<OutPoint> {
        let terms = deposit.terms.as_ref()?;
        let address: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
            terms.address.parse().ok()?;
        let script = address.assume_checked().script_pubkey();
        let spends = self
            .wallet
            .silent_spends_paying(&script)
            .unwrap_or_default();
        for spend in spends.iter().filter(|s| s.height.is_some()) {
            let txid = spend.tx.compute_txid();
            let outputs = spend.tx.output.iter().enumerate();
            let mut paying = outputs
                .filter(|(_, o)| o.script_pubkey == script && o.value.to_sat() == terms.pays);
            if let Some((vout, _)) = paying.next() {
                return Some(OutPoint::new(txid, vout as u32));
            }
        }
        let paid = holder::paid_into(&self.wallet, &self.dir, &script, terms.pays, tip);
        let paid = paid.unwrap_or_else(|e| {
            tracing::warn!(hash = %deposit.hash, "a contract paying a deposit: {e:#}");
            None
        });
        paid.or(deposit.funding.map(|(funding, _)| funding))
    }

    /// Whether the refund `txid` made of a deposit was found in a block:
    /// its coin the wallet's, spent since or not.
    pub(super) fn refund_found(&self, txid: Txid) -> bool {
        let coin = OutPoint::new(txid, 0);
        let unspent = self.wallet.silent_coin_output(&coin);
        let spent = self.wallet.silent_spent_at(&coin);
        matches!(unspent, Ok(Some(_))) || matches!(spent, Ok(Some(_)))
    }

    /// The refunds made of deposits, handed the network again at `tip`
    /// until found in a block, each in its turn ([`Wallet::refund_turn`]).
    fn refund_again(&self, chain: &dyn ChainSource, tip: u32) {
        let Ok(deposits) = self.wallet.deposits() else {
            return;
        };
        let refunded = deposits.iter().filter_map(|d| match (&d.state, &d.terms) {
            (DepositState::Refunded(txid), Some(terms)) => Some((d, *txid, terms.timeout)),
            _ => None,
        });
        for (deposit, txid, timeout) in refunded {
            if self.refund_found(txid) || !self.refund_turn(&deposit.hash, timeout, tip) {
                continue;
            }
            if let Ok(Some(tx)) = self.wallet.deposit_refund(&deposit.hash) {
                if let Err(e) = chain.broadcast(bitcoin::consensus::serialize(&tx)) {
                    tracing::debug!(%txid, %e, "broadcasting a deposit's refund again");
                }
            }
        }
    }
}
