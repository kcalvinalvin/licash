//! The deposits that return a contract's reserve, found on Bitcoin from a
//! wallet's seed alone.
//!
//! The address of such a deposit is of the hash and refund key the seed
//! gives, the hub's deposit root and nothing else: the hub's claim key
//! derives from the root, and the refund waits blocks after the payment,
//! naming no height. A wallet that lost its file, its hub gone or silent,
//! so needs only the seed, the hub's address (the bytes its deposits' secrets
//! derive with) and its deposit root, both public, to look for the
//! payments of its contracts' defaults and take them back.

use std::collections::HashMap;

use bitcoin::{secp256k1::XOnlyPublicKey, Amount, FeeRate, OutPoint, ScriptBuf, Transaction, Txid};
use licash::spend::lock_hash;
use licash_onchain::deposit::{Htlc, CLOSE_WAIT};

use crate::{
    deposits::{close_htlc, secrets, x_only},
    silent::chain_error,
    Blocks, Error, Transport, Wallet,
};

/// A payment found on Bitcoin to a deposit that returns a contract's
/// reserve, which this wallet's seed asked of a hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosePayment {
    /// The deposit's number at its hub.
    pub number: u32,
    /// Its hash.
    pub hash: [u8; 32],
    pub outpoint: OutPoint,
    /// What it pays, in sat.
    pub value: u64,
    /// The block it is in.
    pub height: u32,
    /// The transaction a block holds that spent it: the hub's claim, after
    /// the wallet's notes, or a refund.
    pub spent_by: Option<Txid>,
}

impl ClosePayment {
    /// The height after which its payer may take the coins back: a refund
    /// confirms [`CLOSE_WAIT`] blocks after the payment, at the earliest.
    pub fn refund_height(&self) -> u32 {
        self.height
            .saturating_add(u32::from(CLOSE_WAIT))
            .saturating_sub(1)
    }
}

impl<T: Transport> Wallet<T> {
    /// The deposits `numbers` first of the hub of `address`, whose deposit
    /// root is `root`, as the seed asked them.
    fn close_htlcs(
        &self,
        address: &[u8],
        root: &XOnlyPublicKey,
        numbers: u32,
    ) -> Result<Vec<(u32, Htlc)>, Error> {
        (0..numbers)
            .map(|number| {
                let (preimage, refund, _) = secrets(&self.keys.deposit, address, number)?;
                let htlc = close_htlc(root, &lock_hash(&preimage), &x_only(&refund));
                Ok((number, htlc))
            })
            .collect()
    }

    /// The payments `chain` shows, from block `from` to its tip, to the
    /// deposits that return a contract's reserve that this wallet's seed
    /// asked at its hub, numbered below `numbers`, and what spent each,
    /// found without the hub: by the hub's `address` bytes and deposit
    /// `root`. Blocks are read where a filter says one pays them, else all.
    pub async fn find_close_payments(
        &self,
        address: &[u8],
        root: &XOnlyPublicKey,
        numbers: u32,
        chain: &impl Blocks,
        from: u32,
    ) -> Result<Vec<ClosePayment>, Error> {
        let htlcs = self.close_htlcs(address, root, numbers)?;
        let by_script: HashMap<ScriptBuf, (u32, [u8; 32])> = htlcs
            .iter()
            .map(|(number, htlc)| (htlc.script_pubkey(), (*number, htlc.hash)))
            .collect();
        let scripts: Vec<&[u8]> = by_script.keys().map(|script| script.as_bytes()).collect();
        let tip = chain.height().await.map_err(chain_error)?;
        let mut found: HashMap<OutPoint, ClosePayment> = HashMap::new();
        for height in from..=tip {
            let hash = chain.hash(height).await.map_err(chain_error)?;
            // A block that pays one of them, or spends one, holds its script.
            if let Some(filter) = chain.filter(&hash).await.map_err(chain_error)? {
                let filter = bitcoin::bip158::BlockFilter::new(&filter);
                let matches = filter
                    .match_any(&hash, scripts.iter().copied())
                    .map_err(chain_error)?;
                if !matches {
                    continue;
                }
            }
            let block = chain.block(&hash).await.map_err(chain_error)?;
            if block.block_hash() != hash {
                return Err(chain_error(format!(
                    "block {hash} does not match its header"
                )));
            }
            for tx in &block.txdata {
                for input in &tx.input {
                    if let Some(payment) = found.get_mut(&input.previous_output) {
                        payment.spent_by = Some(tx.compute_txid());
                    }
                }
                let txid = tx.compute_txid();
                for (vout, output) in (0u32..).zip(&tx.output) {
                    let Some((number, hash)) = by_script.get(&output.script_pubkey) else {
                        continue;
                    };
                    let outpoint = OutPoint { txid, vout };
                    found.insert(
                        outpoint,
                        ClosePayment {
                            number: *number,
                            hash: *hash,
                            outpoint,
                            value: output.value.to_sat(),
                            height,
                            spent_by: None,
                        },
                    );
                }
            }
        }
        let mut payments: Vec<_> = found.into_values().collect();
        payments.sort_by_key(|p| (p.height, p.number, p.outpoint.vout));
        Ok(payments)
    }

    /// The refund of `payment`, found by [`Wallet::find_close_payments`] at
    /// the hub of `address` and deposit `root`, paying `to` all of it but the
    /// fee at `rate`: it confirms from [`ClosePayment::refund_height`] on,
    /// and is refused by the network before.
    pub fn refund_close_payment(
        &self,
        address: &[u8],
        root: &XOnlyPublicKey,
        payment: &ClosePayment,
        to: ScriptBuf,
        rate: FeeRate,
    ) -> Result<Transaction, Error> {
        let (preimage, refund, _) = secrets(&self.keys.deposit, address, payment.number)?;
        let htlc = close_htlc(root, &lock_hash(&preimage), &x_only(&refund));
        if htlc.hash != payment.hash {
            return Err(Error::Invalid(
                "a payment of another deposit than its number's".into(),
            ));
        }
        htlc.refund_tx(
            payment.outpoint,
            Amount::from_sat(payment.value),
            to,
            rate,
            &refund,
        )
        .map_err(|e| Error::Invalid(e.to_string()))
    }
}
