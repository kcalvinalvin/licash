//! What the hub's node does for its openings: fund F with its wallet's
//! coins, sign them, lock them meanwhile, and broadcast.

use anyhow::{Context as _, Result};
use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::{absolute, transaction, OutPoint, Transaction, TxIn, TxOut};
use serde_json::{json, Value};

use super::{terms, Openings};

/// What spending a taproot key path weighs: her deposit's input, which
/// the node cannot sign and so cannot weigh itself.
const KEY_SPEND_WEIGHT: u64 = 230;

/// The rate F pays when the node estimates none, in sat/vB.
const FALLBACK_RATE: f64 = 2.0;

fn coin(o: &OutPoint) -> Value {
    json!({ "txid": o.txid.to_string(), "vout": o.vout })
}

impl Openings {
    /// F: her `deposit`, worth `value`, and coins of the node's wallet,
    /// paying `outputs`, with the contract fixed at vout zero, and the
    /// coins it spends, in input order. Its coins are not locked: the caller
    /// stores F, then locks them, all under [`Openings::funding`].
    pub(super) async fn fund(
        &self,
        deposit: OutPoint,
        value: &TxOut,
        outputs: Vec<TxOut>,
    ) -> Result<(Transaction, Vec<TxOut>)> {
        let unsigned = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: deposit,
                ..Default::default()
            }],
            output: outputs,
        };
        let options = json!({
            "add_inputs": true,
            "lock_unspents": false,
            "fee_rate": self.rate().await,
            "changePosition": unsigned.output.len(),
            "input_weights": [{
                "txid": deposit.txid.to_string(),
                "vout": deposit.vout,
                "weight": KEY_SPEND_WEIGHT,
            }],
        });
        let funded: Value = self
            .wallet
            .rpc(
                "fundrawtransaction",
                json!([serialize_hex(&unsigned), options]),
            )
            .await
            .context("funding F")?;
        let hex = funded["hex"].as_str().context("F funded, no hex")?;
        let funding: Transaction = deserialize_hex(hex)?;
        let mut prevouts = Vec::with_capacity(funding.input.len());
        for input in &funding.input {
            let o = input.previous_output;
            if o == deposit {
                prevouts.push(value.clone());
                continue;
            }
            let found: Value = self
                .wallet
                .rpc("gettransaction", json!([o.txid.to_string()]))
                .await?;
            let hex = found["hex"].as_str().context("a coin of F, no hex")?;
            let tx: Transaction = deserialize_hex(hex)?;
            let out = tx.output.get(o.vout as usize).context("a coin of F")?;
            prevouts.push(out.clone());
        }
        Ok((funding, prevouts))
    }

    /// The rate F pays, in sat/vB: the node's estimate for six blocks, at
    /// most the quote's ceiling.
    async fn rate(&self) -> f64 {
        let ceiling = terms::quote_ceiling();
        match self.source.fee_rate(6).await {
            Ok(Some(rate)) => (rate.to_sat_per_kwu() as f64 * 4.0 / 1_000.0).clamp(1.0, ceiling),
            _ => FALLBACK_RATE,
        }
    }

    /// F with the node's wallet's inputs signed, `theirs` (the coins it
    /// spends that are not the node's, with what they hold) left unsigned.
    pub(super) async fn sign(
        &self,
        funding: &Transaction,
        theirs: &[(OutPoint, TxOut)],
    ) -> Result<Transaction> {
        let theirs: Vec<Value> = theirs
            .iter()
            .map(|(o, coin)| {
                json!({
                    "txid": o.txid.to_string(),
                    "vout": o.vout,
                    "scriptPubKey": coin.script_pubkey.to_hex_string(),
                    "amount": coin.value.to_btc(),
                })
            })
            .collect();
        let signed: Value = self
            .wallet
            .rpc(
                "signrawtransactionwithwallet",
                json!([serialize_hex(funding), theirs]),
            )
            .await
            .context("signing F")?;
        let hex = signed["hex"].as_str().context("F signed, no hex")?;
        Ok(deserialize_hex(hex)?)
    }

    /// Whether `deposit` is a confirmed coin of the chain as `value` says.
    pub(super) async fn confirmed(&self, deposit: OutPoint, value: &TxOut) -> Result<bool> {
        let found: Value = self
            .wallet
            .rpc(
                "gettxout",
                json!([deposit.txid.to_string(), deposit.vout, false]),
            )
            .await?;
        if found.is_null() {
            return Ok(false);
        }
        let sats = found["value"]
            .as_f64()
            .map(|btc| (btc * 100_000_000.0).round() as u64);
        let script = found["scriptPubKey"]["hex"].as_str();
        Ok(found["confirmations"].as_u64().unwrap_or(0) >= 1
            && sats == Some(value.value.to_sat())
            && script == Some(value.script_pubkey.to_hex_string().as_str()))
    }

    /// Lock `coins` in the node's wallet, across its restarts where it
    /// can: a failure is logged, the coin spent or locked already.
    pub(super) async fn lock_coins(&self, coins: &[OutPoint]) {
        self.lock(coins, false).await;
    }

    pub(super) async fn unlock_coins(&self, coins: &[OutPoint]) {
        self.lock(coins, true).await;
    }

    async fn lock(&self, coins: &[OutPoint], unlock: bool) {
        for o in coins {
            let args = json!([unlock, [coin(o)], true]);
            let mut done: Result<bool> = self.wallet.rpc("lockunspent", args).await;
            if done.is_err() {
                // Nodes before 0.25 persist no lock.
                let args = json!([unlock, [coin(o)]]);
                done = self.wallet.rpc("lockunspent", args).await;
            }
            if let Err(e) = done {
                tracing::warn!(coin = %o, unlock, "locking a coin of F: {e:#}");
            }
        }
    }
}
