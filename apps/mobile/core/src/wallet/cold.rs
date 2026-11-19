//! Cold storage: notes withdrawn now and then to an external wallet by its
//! extended public key, never to an address twice, and what it holds,
//! looked for in the light client's blocks ([`licash_wallet::Cold`]).

use std::sync::Arc;

use licash_wallet::{ColdRules, ColdScript};

use super::{Chain, Wallet};
use crate::{ChainSource, LicashError};

/// Blocks looked in for cold storage's coins at a time.
const BATCH: u32 = 1_000;

/// Cold storage, as configured and last looked for.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ColdStorage {
    /// The extended public key, as given.
    pub key: String,
    /// Its addresses' kind: "wpkh" (native segwit) or "tr" (taproot).
    pub script: String,
    /// The notes held, in sat, past which a withdrawal is due.
    pub threshold: u64,
    /// The notes kept, in sat, when withdrawing.
    pub keep: u64,
    /// The most a withdrawal may cost, in percent of it.
    pub max_fee_percent: f64,
    /// The first block looked in.
    pub birthday: u32,
    /// The last block looked in, if any yet.
    pub scanned: Option<u32>,
    /// What it holds in the blocks looked in, in sat.
    pub balance: u64,
    pub coins: u32,
    /// The address the next withdrawal pays.
    pub next: String,
    /// The withdrawal under way, by its payout's hash.
    pub pending: Option<String>,
}

fn shown(cold: licash_wallet::Cold) -> ColdStorage {
    ColdStorage {
        key: cold.key,
        script: cold.script.as_str().into(),
        threshold: cold.rules.threshold,
        keep: cold.rules.keep,
        max_fee_percent: cold.rules.max_fee_percent,
        birthday: cold.birthday,
        scanned: cold.scanned,
        balance: cold.balance,
        coins: u32::try_from(cold.coins).unwrap_or(u32::MAX),
        next: cold.next,
        pending: cold.pending,
    }
}

#[uniffi::export]
impl Wallet {
    /// Cold storage as configured and last looked for, if any.
    pub fn cold(&self) -> Result<Option<ColdStorage>, LicashError> {
        Ok(self.wallet.cold()?.map(shown))
    }

    /// Withdraw to the wallet of `key`, an extended public key of its
    /// account: a zpub (vpub off mainnet) for native segwit addresses, or
    /// an xpub (tpub) with `script` chosen, "wpkh" or "tr". Once the notes
    /// exceed `threshold`, all but `keep` is paid out to its next address
    /// unused, through a swap provider charging at most `max_fee_percent`
    /// of it. Its coins are looked for from block `birthday`, by default
    /// the chain's height as last seen.
    pub fn set_cold(
        &self,
        key: String,
        script: Option<String>,
        threshold: u64,
        keep: u64,
        max_fee_percent: f64,
        birthday: Option<u32>,
    ) -> Result<ColdStorage, LicashError> {
        let script = script.map(|s| s.parse::<ColdScript>()).transpose()?;
        let rules = ColdRules {
            threshold,
            keep,
            max_fee_percent,
        };
        let birthday = birthday.unwrap_or_else(|| self.tip());
        Ok(shown(self.wallet.set_cold(&key, script, rules, birthday)?))
    }

    /// Stop withdrawing to cold storage: what it holds stays there.
    pub fn clear_cold(&self) -> Result<(), LicashError> {
        Ok(self.wallet.clear_cold()?)
    }

    /// Look for what cold storage holds through `chain`, up to its height,
    /// then withdraw if due, through the provider preferred, within the
    /// rules' fee limit: where it stands. None if not configured. A
    /// withdrawal not made now is tried again next time.
    pub fn run_cold(
        &self,
        chain: Arc<dyn ChainSource>,
    ) -> Result<Option<ColdStorage>, LicashError> {
        let chain = Chain(chain, self.wallet.network());
        let tip = self
            .runtime
            .block_on(licash_wallet::Blocks::height(&chain))
            .map_err(|reason| LicashError::Unavailable { reason })?;
        let cold = loop {
            let cold = self
                .runtime
                .block_on(self.wallet.scan_cold(&chain, BATCH))?;
            match &cold {
                Some(c) if c.scanned.is_some_and(|s| s < tip) => continue,
                _ => break cold,
            }
        };
        if cold.is_none() {
            return Ok(None);
        }
        if let Some(due) = self.wallet.cold_due()? {
            match self.withdraw(&due) {
                Ok(hash) => {
                    tracing::info!(%hash, amount = due.amount, "withdrawing to cold storage")
                }
                Err(e) => tracing::info!(
                    amount = due.amount,
                    "no withdrawal to cold storage now: {e}"
                ),
            }
        }
        self.cold()
    }
}

impl Wallet {
    /// Pay `due` out, if quoted within its fee limit: the payout's hash.
    fn withdraw(&self, due: &licash_wallet::ColdWithdrawal) -> Result<String, LicashError> {
        let quote = self.payout_quote(due.to.clone(), Some(due.amount), Some(due.from.clone()))?;
        if quote.fee > due.max_fee {
            return Err(LicashError::invalid(format!(
                "its fees, {} sat, exceed the limit of {} sat",
                quote.fee, due.max_fee
            )));
        }
        Ok(self.pay_out(quote)?.id)
    }
}
