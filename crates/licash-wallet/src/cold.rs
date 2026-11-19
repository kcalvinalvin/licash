//! Cold storage: notes withdrawn now and then to an external wallet, by its
//! extended public key, each time to an address of its own, never one used
//! before; and what that wallet holds, looked for in blocks.
//!
//! A `zpub` (`vpub` off mainnet) names native segwit addresses (P2WPKH); an
//! `xpub` (`tpub`) either those or taproot ones (P2TR, BIP86), as chosen.
//! Addresses are the key's `0/i` (its receiving chain), `i` from 0: the
//! first not paid out to nor holding a coin found, its next. Blocks from
//! the wallet's birthday are looked in for the addresses up to [`GAP`]
//! past the last used, so that coins paid there by other means count too.
//!
//! A withdrawal is due when the notes held exceed the rules' threshold: all
//! but what they keep, from the hub holding the most, the swap provider's
//! fees at most the rules' percent of it, and one at a time
//! ([`Wallet::cold_due`]). The apps pay it out ([`Wallet::payout`]).

use std::{collections::HashMap, str::FromStr};

use bitcoin::hashes::Hash as _;
use bitcoin::{
    bip32::{ChildNumber, Xpub},
    key::{CompressedPublicKey, Secp256k1},
    Address, NetworkKind, OutPoint, ScriptBuf, Txid,
};
use rusqlite::{params, OptionalExtension};

use crate::{payouts::checked_block, sql, Blocks, Error, PayoutState, Transport, Wallet};

/// Addresses looked at past the last used.
pub const GAP: u32 = 20;
/// The last blocks looked in again each time, for a reorganization.
const AGAIN: u32 = 6;

const XPUB: [u8; 4] = [0x04, 0x88, 0xb2, 0x1e];
const TPUB: [u8; 4] = [0x04, 0x35, 0x87, 0xcf];
const ZPUB: [u8; 4] = [0x04, 0xb2, 0x47, 0x46];
const VPUB: [u8; 4] = [0x04, 0x5f, 0x1c, 0x42];

/// The kind of address a cold wallet's key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColdScript {
    /// Native segwit, P2WPKH.
    Wpkh,
    /// Taproot, P2TR with no script (BIP86).
    Tr,
}

impl ColdScript {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wpkh => "wpkh",
            Self::Tr => "tr",
        }
    }
}

impl FromStr for ColdScript {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "wpkh" => Ok(Self::Wpkh),
            "tr" => Ok(Self::Tr),
            other => Err(Error::Invalid(format!(
                "a script of wpkh or tr, not {other}"
            ))),
        }
    }
}

/// When to withdraw: once the notes held exceed `threshold`, all but
/// `keep`, the provider's fees at most `max_fee_percent` of the amount.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColdRules {
    pub threshold: u64,
    pub keep: u64,
    pub max_fee_percent: f64,
}

impl ColdRules {
    fn check(&self) -> Result<(), Error> {
        if self.keep > self.threshold {
            return Err(Error::Invalid(
                "the amount kept must not exceed the threshold".into(),
            ));
        }
        let percent = self.max_fee_percent;
        if !(percent.is_finite() && percent > 0.0 && percent <= 100.0) {
            return Err(Error::Invalid(
                "a fee limit above 0% and at most 100%".into(),
            ));
        }
        Ok(())
    }

    /// The withdrawal due of `total` held, `most` of it at one hub: its
    /// amount and the fees it may cost, if due. Within what that hub
    /// holds, fees included.
    pub fn due(&self, total: u64, most: u64) -> Option<(u64, u64)> {
        if total <= self.threshold {
            return None;
        }
        let fee_of = |amount: u64| (amount as f64 * self.max_fee_percent / 100.0) as u64;
        let fits = (most as f64 / (1.0 + self.max_fee_percent / 100.0)) as u64;
        let amount = (total - self.keep).min(fits);
        (amount > 0).then(|| (amount, fee_of(amount)))
    }
}

/// The wallet's cold storage, as configured and last looked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Cold {
    /// The extended public key, as given.
    pub key: String,
    pub script: ColdScript,
    pub rules: ColdRules,
    /// The first block looked in.
    pub birthday: u32,
    /// The last block looked in, if any yet.
    pub scanned: Option<u32>,
    /// What its addresses hold in the blocks looked in, unspent there.
    pub balance: u64,
    pub coins: usize,
    /// The address the next withdrawal pays, and its index.
    pub next: String,
    pub next_index: u32,
    /// The withdrawal under way, by its payout's hash.
    pub pending: Option<String>,
}

/// A withdrawal due ([`Wallet::cold_due`]): `amount` from notes at `from`
/// to `to`, costing at most `max_fee`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdWithdrawal {
    pub from: String,
    pub to: String,
    pub amount: u64,
    pub max_fee: u64,
}

/// `text`, an extended public key on `network`, and the script it implies,
/// if any.
fn parse_key(text: &str, network: NetworkKind) -> Result<(Xpub, Option<ColdScript>), Error> {
    let bad = |why: &str| Error::Invalid(format!("not an extended public key: {why}"));
    let mut data = bitcoin::base58::decode_check(text.trim()).map_err(|_| bad("its encoding"))?;
    if data.len() != 78 {
        return Err(bad("its length"));
    }
    let version: [u8; 4] = data[..4].try_into().expect("4 bytes");
    let (version, implied) = match version {
        ZPUB => (XPUB, Some(ColdScript::Wpkh)),
        VPUB => (TPUB, Some(ColdScript::Wpkh)),
        XPUB | TPUB => (version, None),
        _ => return Err(bad("an xpub, zpub, tpub or vpub only")),
    };
    data[..4].copy_from_slice(&version);
    let key = Xpub::decode(&data).map_err(|e| bad(&e.to_string()))?;
    if key.network != network {
        return Err(Error::Invalid(
            "an extended public key of another network".into(),
        ));
    }
    Ok((key, implied))
}

/// The output script of `key`'s address `0/index`, of `script`'s kind.
fn script_at(key: &Xpub, script: ColdScript, index: u32) -> Result<ScriptBuf, Error> {
    let secp = Secp256k1::verification_only();
    let path = [
        ChildNumber::from_normal_idx(0).expect("normal"),
        ChildNumber::from_normal_idx(index).map_err(|e| Error::Invalid(e.to_string()))?,
    ];
    let child = key
        .derive_pub(&secp, &path)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(match script {
        ColdScript::Wpkh => {
            ScriptBuf::new_p2wpkh(&CompressedPublicKey(child.public_key).wpubkey_hash())
        }
        ColdScript::Tr => ScriptBuf::new_p2tr(&secp, child.public_key.into(), None),
    })
}

/// The configuration as stored.
struct Config {
    key: String,
    xpub: Xpub,
    script: ColdScript,
    rules: ColdRules,
    birthday: u32,
    scanned: Option<u32>,
}

impl<T: Transport> Wallet<T> {
    /// Withdraw to cold storage, the wallet of `key` (an xpub, zpub, tpub
    /// or vpub of the wallet's network), as `rules` say, its addresses of
    /// `script`'s kind (implied by a zpub or vpub; an xpub's or tpub's to
    /// choose), looked for from block `birthday`. A key or kind other than
    /// before forgets the coins found, looked for anew.
    pub fn set_cold(
        &self,
        key: &str,
        script: Option<ColdScript>,
        rules: ColdRules,
        birthday: u32,
    ) -> Result<Cold, Error> {
        let network = licash_onchain::network(self.network()).into();
        let (_, implied) = parse_key(key, network)?;
        let script = match (implied, script) {
            (Some(implied), Some(chosen)) if implied != chosen => {
                return Err(Error::Invalid(format!(
                    "a zpub or vpub names {} addresses only",
                    implied.as_str()
                )))
            }
            (Some(implied), _) => implied,
            (None, Some(chosen)) => chosen,
            (None, None) => {
                return Err(Error::Invalid(
                    "an xpub or tpub needs its addresses' kind chosen: wpkh or tr".into(),
                ))
            }
        };
        rules.check()?;
        let key = key.trim();
        self.store.write(|tx| {
            let before: Option<(String, String, u32)> = tx
                .query_row("SELECT key, script, birthday FROM cold", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let same = before
                .as_ref()
                .is_some_and(|(k, s, b)| k == key && s == script.as_str() && *b <= birthday);
            if !same {
                tx.execute("DELETE FROM cold_coins", [])?;
            }
            tx.execute(
                "INSERT INTO cold (id, key, script, threshold, keep, max_fee_percent, birthday)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (id) DO UPDATE SET key = ?1, script = ?2, threshold = ?3,
                    keep = ?4, max_fee_percent = ?5, birthday = ?6,
                    scanned = CASE WHEN ?7 THEN scanned ELSE NULL END",
                params![
                    key,
                    script.as_str(),
                    sql(rules.threshold)?,
                    sql(rules.keep)?,
                    rules.max_fee_percent,
                    birthday,
                    same
                ],
            )?;
            Ok(())
        })?;
        Ok(self.cold()?.expect("just set"))
    }

    /// Stop withdrawing to cold storage, forgetting its coins found.
    pub fn clear_cold(&self) -> Result<(), Error> {
        self.store.write(|tx| {
            tx.execute("DELETE FROM cold_coins", [])?;
            tx.execute("DELETE FROM cold", [])?;
            Ok(())
        })
    }

    fn cold_config(&self) -> Result<Option<Config>, Error> {
        let row = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT key, script, threshold, keep, max_fee_percent, birthday, scanned FROM cold",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, f64>(4)?,
                        r.get::<_, u32>(5)?,
                        r.get::<_, Option<u32>>(6)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let Some((key, script, threshold, keep, max_fee_percent, birthday, scanned)) = row else {
            return Ok(None);
        };
        let network = licash_onchain::network(self.network()).into();
        let (xpub, _) = parse_key(&key, network)?;
        Ok(Some(Config {
            key,
            xpub,
            script: script.parse()?,
            rules: ColdRules {
                threshold: threshold as u64,
                keep: keep as u64,
                max_fee_percent,
            },
            birthday,
            scanned,
        }))
    }

    /// The indexes of `config`'s addresses used: paid out to, or holding a
    /// coin found; and the scripts of those up to [`GAP`] past the last.
    fn cold_used(&self, config: &Config) -> Result<(Option<u32>, HashMap<ScriptBuf, u32>), Error> {
        let found: Option<u32> = self
            .store
            .read(|c| Ok(c.query_row("SELECT max(idx) FROM cold_coins", [], |r| r.get(0))?))?;
        let paid: Vec<String> = self.payouts()?.into_iter().map(|p| p.address).collect();
        let network = licash_onchain::network(self.network());
        let mut last = found;
        let mut scripts = HashMap::new();
        let mut index = 0;
        while index <= last.map_or(0, |l| l + 1) + GAP {
            let script = script_at(&config.xpub, config.script, index)?;
            let address = Address::from_script(&script, network)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_string();
            if paid.contains(&address) {
                last = last.max(Some(index));
            }
            scripts.insert(script, index);
            index += 1;
        }
        Ok((last, scripts))
    }

    /// The cold storage configured, as last looked for, if any.
    pub fn cold(&self) -> Result<Option<Cold>, Error> {
        let Some(config) = self.cold_config()? else {
            return Ok(None);
        };
        let (last, scripts) = self.cold_used(&config)?;
        let next_index = last.map_or(0, |l| l + 1);
        let network = licash_onchain::network(self.network());
        let next = script_at(&config.xpub, config.script, next_index)?;
        let next = Address::from_script(&next, network)
            .map_err(|e| Error::Invalid(e.to_string()))?
            .to_string();
        let (balance, coins): (i64, i64) = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT coalesce(sum(value), 0), count(*) FROM cold_coins WHERE spent_height IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })?;
        let ours: Vec<String> = (scripts.keys())
            .filter_map(|s| Address::from_script(s, network).ok())
            .map(|a| a.to_string())
            .collect();
        let pending = self.payouts()?.into_iter().find(|p| {
            matches!(p.state, PayoutState::Open | PayoutState::Claimed) && ours.contains(&p.address)
        });
        Ok(Some(Cold {
            key: config.key,
            script: config.script,
            rules: config.rules,
            birthday: config.birthday,
            scanned: config.scanned,
            balance: balance as u64,
            coins: coins as usize,
            next,
            next_index,
            pending: pending.map(|p| p.hash),
        }))
    }

    /// The withdrawal due to cold storage now, if any: none while one is
    /// under way, nor with notes held up to the threshold.
    pub fn cold_due(&self) -> Result<Option<ColdWithdrawal>, Error> {
        let Some(cold) = self.cold()? else {
            return Ok(None);
        };
        if cold.pending.is_some() {
            return Ok(None);
        }
        let balance = self.balance()?;
        let total: u64 = balance.iter().map(|b| b.available).sum();
        let Some(richest) = balance.iter().max_by_key(|b| b.available) else {
            return Ok(None);
        };
        Ok(cold
            .rules
            .due(total, richest.available)
            .map(|(amount, max_fee)| ColdWithdrawal {
                from: richest.hub.clone(),
                to: cold.next,
                amount,
                max_fee,
            }))
    }

    /// Look in the blocks of `chain` for what cold storage's addresses hold,
    /// from the block after the last looked in (the last [`AGAIN`] again,
    /// as a reorganization may have changed them), at most `max` blocks,
    /// if the chain has new ones: where it stands then. None if not
    /// configured.
    pub async fn scan_cold(&self, chain: &impl Blocks, max: u32) -> Result<Option<Cold>, Error> {
        let Some(config) = self.cold_config()? else {
            return Ok(None);
        };
        let tip = chain.height().await.map_err(Error::Chain)?;
        // Looked in up to the tip already: no block new to look in.
        if config.scanned.is_some_and(|s| s >= tip) {
            return self.cold();
        }
        let from = match config.scanned {
            Some(scanned) => (scanned + 1).min(tip.saturating_sub(AGAIN - 1)),
            None => config.birthday,
        }
        .max(config.birthday);
        if from > tip {
            return self.cold();
        }
        let end = tip.min(from.saturating_add(max.max(1) - 1));
        // Looked in again: as if never seen.
        self.store.write(|tx| {
            tx.execute("DELETE FROM cold_coins WHERE height >= ?1", [from])?;
            tx.execute(
                "UPDATE cold_coins SET spent_height = NULL WHERE spent_height >= ?1",
                [from],
            )?;
            Ok(())
        })?;
        let (_, mut scripts) = self.cold_used(&config)?;
        for height in from..=end {
            let hash = chain.hash(height).await.map_err(Error::Chain)?;
            let matched = match chain.filter(&hash).await.map_err(Error::Chain)? {
                None => true,
                Some(filter) => bitcoin::bip158::BlockFilter::new(&filter)
                    .match_any(&hash, scripts.keys().map(|s| s.as_bytes()))
                    .map_err(|e| Error::Chain(format!("the filter of block {hash}: {e}")))?,
            };
            if !matched {
                continue;
            }
            let block = checked_block(chain, &hash).await?;
            let mut found = false;
            self.store.write(|tx| {
                for t in &block.txdata {
                    let txid: Txid = t.compute_txid();
                    for input in &t.input {
                        let OutPoint { txid: prev, vout } = input.previous_output;
                        tx.execute(
                            "UPDATE cold_coins SET spent_height = ?3
                             WHERE txid = ?1 AND vout = ?2 AND spent_height IS NULL",
                            params![prev.to_byte_array(), vout, height],
                        )?;
                    }
                    for (vout, out) in t.output.iter().enumerate() {
                        if let Some(index) = scripts.get(&out.script_pubkey) {
                            found = true;
                            tx.execute(
                                "INSERT OR IGNORE INTO cold_coins (txid, vout, idx, value, height)
                                 VALUES (?1, ?2, ?3, ?4, ?5)",
                                params![
                                    txid.to_byte_array(),
                                    vout as u32,
                                    index,
                                    sql(out.value.to_sat())?,
                                    height
                                ],
                            )?;
                        }
                    }
                }
                Ok(())
            })?;
            // The addresses looked for reach further past the last used.
            if found {
                scripts = self.cold_used(&config)?.1;
            }
        }
        self.store.write(|tx| {
            tx.execute("UPDATE cold SET scanned = ?1", [end])?;
            Ok(())
        })?;
        self.cold()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_withdrawal_is_due_past_the_threshold_of_all_but_what_is_kept() {
        let rules = ColdRules {
            threshold: 100_000,
            keep: 20_000,
            max_fee_percent: 1.0,
        };
        assert_eq!(rules.due(100_000, 100_000), None);
        assert_eq!(rules.due(150_000, 150_000), Some((130_000, 1_300)));
        // Within what the richest hub holds, its fees too.
        let (amount, fee) = rules.due(150_000, 60_000).unwrap();
        assert!(amount + fee <= 60_000, "{amount} {fee}");
        assert_eq!(amount, 59_405);
        assert!(rules.check().is_ok());
        let kept_more = ColdRules {
            keep: 200_000,
            ..rules
        };
        assert!(kept_more.check().is_err());
        let free = ColdRules {
            max_fee_percent: 0.0,
            ..rules
        };
        assert!(free.check().is_err());
    }

    #[test]
    fn zpubs_name_segwit_and_xpubs_either_address_of_their_receiving_chain() {
        // BIP84's first account, and BIP86's.
        let zpub = "zpub6rFR7y4Q2AijBEqTUquhVz398htDFrtymD9xYYfG1m4wAcvPhXNfE3EfH1r1ADqtfSdVCToUG868RvUUkgDKf31mGDtKsAYz2oz2AGutZYs";
        let (key, implied) = parse_key(zpub, NetworkKind::Main).unwrap();
        assert_eq!(implied, Some(ColdScript::Wpkh));
        let first = script_at(&key, ColdScript::Wpkh, 0).unwrap();
        let address = Address::from_script(&first, bitcoin::Network::Bitcoin).unwrap();
        assert_eq!(
            address.to_string(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );
        let xpub = "xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ";
        let (key, implied) = parse_key(xpub, NetworkKind::Main).unwrap();
        assert_eq!(implied, None);
        let first = script_at(&key, ColdScript::Tr, 0).unwrap();
        let address = Address::from_script(&first, bitcoin::Network::Bitcoin).unwrap();
        assert_eq!(
            address.to_string(),
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
        );
        assert!(parse_key(zpub, NetworkKind::Test).is_err());
        assert!(parse_key("zpub-not", NetworkKind::Main).is_err());
    }
}
