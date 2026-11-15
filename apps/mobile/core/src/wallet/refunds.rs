//! The coins paid for deposits whose notes never came, taken back once
//! their timeout passed: canceled by their hub (its lock to the wallet
//! left unclaimed, say, the phone asleep) or paid too late for it to see.
//! Each round at the chain's tip, by itself: the refund key is the
//! wallet's alone.

use std::collections::HashSet;

use bitcoin::{Amount, FeeRate, OutPoint, Txid};
use licash_contracts::holder;
use licash_wallet::{Deposit, DepositState};

use super::Wallet;
use crate::ChainSource;

/// The fee rate of a refund, in sat/vB: no estimates known.
const REFUND_RATE: u64 = 2;

/// Blocks past a deposit's timeout its payment is looked for when its hub
/// never saw one, and its refund handed the network again until found in
/// a block: two weeks.
pub(super) const REFUND_BLOCKS: u32 = 2_016;

/// Blocks between looks for the payment of one deposit its hub never saw:
/// a contract's F is read whole to find it.
const LOOK_EVERY: u32 = 6;

impl Wallet {
    /// Take back, through `chain` at `tip`, the coins of each deposit
    /// canceled and past its timeout, paid as far as the wallet knows, to
    /// its fallback address; and hand the network again the refunds made,
    /// until found in a block. Never a deposit whose notes were received,
    /// nor one still open: its notes may yet be claimed.
    pub(super) fn refund_deposits(&self, chain: &dyn ChainSource, tip: u32) {
        let due = match self.wallet.deposits_to_refund(tip) {
            Ok(due) => due,
            Err(e) => return tracing::warn!(%e, "the deposits to refund"),
        };
        for deposit in due {
            let Some(funding) = self.deposit_paid(&deposit, tip) else {
                continue;
            };
            let Some(terms) = &deposit.terms else {
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

    /// Where the payment of `deposit` is, at `tip`: as its hub saw it,
    /// else a transaction of the wallet's in a block paying it, else the
    /// F of a contract paying its change in. Its hub not having seen it,
    /// looked for a while past its timeout, every few blocks.
    fn deposit_paid(&self, deposit: &Deposit, tip: u32) -> Option<OutPoint> {
        if let Some((funding, _)) = deposit.funding {
            return Some(funding);
        }
        let terms = deposit.terms.as_ref()?;
        if tip > terms.timeout.saturating_add(REFUND_BLOCKS) {
            return None;
        }
        {
            let mut looked = self
                .refunds_looked
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let last = looked.get(&deposit.hash).copied();
            if last.is_some_and(|at| tip < at.saturating_add(LOOK_EVERY)) {
                return None;
            }
            looked.insert(deposit.hash.clone(), tip);
        }
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
        paid.unwrap_or_else(|e| {
            tracing::warn!(hash = %deposit.hash, "a contract paying a deposit: {e:#}");
            None
        })
    }

    /// The refunds made of deposits, handed the network again at `tip`
    /// until found in a block (their coin the wallet's), for a while past
    /// their timeout.
    fn refund_again(&self, chain: &dyn ChainSource, tip: u32) {
        let Ok(deposits) = self.wallet.deposits() else {
            return;
        };
        let found: HashSet<Txid> = match self.wallet.silent_coins() {
            Ok(coins) => coins.iter().map(|c| c.outpoint.txid).collect(),
            Err(_) => return,
        };
        for deposit in deposits {
            let DepositState::Refunded(txid) = deposit.state else {
                continue;
            };
            let Some(terms) = &deposit.terms else {
                continue;
            };
            if found.contains(&txid) || tip > terms.timeout.saturating_add(REFUND_BLOCKS) {
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
