//! Silent payments (BIP352) to the wallet's address, found without a full
//! node: a hub the wallet trusts serves each block's tweaks, and a light
//! client ([`Blocks`]) the blocks' compact filters (BIP158) and the blocks
//! that match.
//!
//! From a first block its owner chose on ([`Wallet::watch_silent`]), the
//! wallet derives from each tweak the outputs a payment to it would pay
//! first, and fetches a block only if its filter matches one of them or a
//! coin it holds. A hub that withholds or forges tweaks hides payments; it
//! can never take them. Tweaks of another block than the chain's at their
//! height are refused. Each batch of blocks is recorded whole, and a
//! reorganization drops what the blocks it replaced held.
//!
//! Payers of no silent payments pay a fallback address instead, a plain
//! taproot address ([`Scanner::fallback`]) a payment code holds once
//! [`Wallet::payment_code`] hands one out: from then on those handed out,
//! and [`FALLBACK_GAP`] after, are watched in every block, and the next is
//! handed out once one is paid. Their coins are the address's like any.
//!
//! [`Wallet::spend_silent`] spends the coins found, change to the wallet's
//! change address: a transaction for the application to broadcast.
//! [`Wallet::deposit_silent`] spends them on a deposit for a hub's notes.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
};

use bitcoin::{
    absolute::LockTime,
    bip158::BlockFilter,
    hashes::Hash,
    key::{Keypair, Secp256k1, TweakedPublicKey},
    secp256k1::{Message, PublicKey, SecretKey, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache},
    transaction, Amount, Block, BlockHash, FeeRate, OutPoint, ScriptBuf, Sequence, TapSighashType,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use licash_api::{BlockTweaks, MAX_TWEAK_BLOCKS, TWEAKS};
use licash_onchain::silent::{self, Found, Scanner, CHANGE};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Transaction as Sql};

use crate::{
    hub_url, invalid, store::now, unavailable, Deposit, DepositState, Error, Transport, Wallet,
};

/// The blocks looked in that are kept, to notice reorganizations that deep.
const KEEP_BLOCKS: u32 = 288;

/// The fallback addresses watched past the highest handed out: a copy of
/// the wallet may have handed them out.
const FALLBACK_GAP: u32 = 20;

/// What the wallet reads of Bitcoin's best chain to find its silent
/// payments: a light client's view of it, or a full node's.
pub trait Blocks: Send + Sync {
    /// The best chain's height.
    fn height(&self) -> impl Future<Output = Result<u32, String>> + Send;

    /// The hash of the best chain's block at `height`.
    fn hash(&self, height: u32) -> impl Future<Output = Result<BlockHash, String>> + Send;

    /// The basic compact filter (BIP158) of the block of `hash`, checked
    /// against the chain's filter headers, if the source serves filters:
    /// without them, every block that could pay the wallet or spend its
    /// coins is fetched.
    fn filter(
        &self,
        hash: &BlockHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send;

    /// The block of `hash`.
    fn block(&self, hash: &BlockHash) -> impl Future<Output = Result<Block, String>> + Send;
}

/// A coin paying the silent payment address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SilentCoin {
    pub outpoint: OutPoint,
    pub value: u64,
    /// The block it is in.
    pub height: u32,
    /// At the wallet's change address: one of its own spends paid it.
    pub change: bool,
    /// The wallet's transaction spending it, in no block yet.
    pub spending: Option<Txid>,
}

/// What a [`Wallet::scan_silent`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scanned {
    /// The next block to look in.
    pub next: u32,
    /// The chain's height: done once `next` is past it.
    pub height: u32,
    /// The coins found.
    pub found: Vec<SilentCoin>,
    /// The coins seen spent.
    pub spent: Vec<OutPoint>,
    /// The blocks fetched whole, their filters matching.
    pub fetched: u32,
}

/// What a batch of blocks held: recorded whole.
#[derive(Default)]
struct Batch {
    blocks: Vec<(u32, BlockHash)>,
    found: Vec<(OutPoint, u64, Found, u32)>,
    spent: Vec<(OutPoint, Txid, u32)>,
}

/// What finds the outputs paying the wallet: its silent payment address's,
/// with each block's tweaks, and its fallback addresses'.
struct Finder {
    scanner: Scanner,
    /// The fallback outputs watched, by key.
    fallbacks: HashMap<XOnlyPublicKey, Found>,
}

impl Finder {
    /// The fallback output `script` pays, if one.
    fn fallback(&self, script: &ScriptBuf) -> Option<&Found> {
        let key = script.is_p2tr().then(|| &script.as_bytes()[2..])?;
        self.fallbacks.get(&XOnlyPublicKey::from_slice(key).ok()?)
    }
}

/// The fallback outputs watched, by key, and the index to hand out next:
/// past every one paid, never before the highest handed out. None watched
/// until one is handed out.
fn fallbacks(
    c: &rusqlite::Connection,
    scanner: &Scanner,
) -> Result<(HashMap<XOnlyPublicKey, Found>, u32), Error> {
    let shown: Option<u32> = c
        .query_row("SELECT shown FROM silent_fallback", [], |r| r.get(0))
        .optional()?;
    let Some(shown) = shown else {
        return Ok((HashMap::new(), 0));
    };
    let paid: HashSet<[u8; 32]> = {
        let mut stmt = c.prepare("SELECT output FROM silent_coins")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut watched = HashMap::new();
    let mut next = shown;
    for index in 0..=shown.saturating_add(FALLBACK_GAP) {
        let found = scanner.fallback(index).map_err(invalid)?;
        if paid.contains(&found.output.serialize()) {
            next = next.max(index.saturating_add(1));
        }
        watched.insert(found.output, found);
    }
    Ok((watched, next))
}

/// A coin the wallet holds, as a spend reads it.
#[derive(Clone)]
struct Held {
    outpoint: OutPoint,
    value: u64,
    found: Found,
}

fn chain_error(reason: impl std::fmt::Display) -> Error {
    Error::Chain(reason.to_string())
}

fn raced() -> Error {
    Error::Invalid("another scan for silent payments ran meanwhile: try again".into())
}

/// Look again from block `from`, if the next block to look in is still
/// `next`: forget the coins found and the spends seen from it on (they are
/// found again if still in the best chain); a coin whose spend is forgotten
/// is the wallet's own spend's again, if one takes it. Whether it was still
/// `next`.
fn rewind(t: &Sql<'_>, next: u32, from: u32) -> Result<bool, Error> {
    if t.execute(
        "UPDATE silent_scan SET next = ?2 WHERE next = ?1",
        [next, from],
    )? == 0
    {
        return Ok(false);
    }
    t.execute("DELETE FROM silent_coins WHERE height >= ?1", [from])?;
    let unspent: Vec<OutPoint> = {
        let mut stmt = t.prepare("SELECT txid, vout FROM silent_coins WHERE spent_at >= ?1")?;
        let rows = stmt.query_map([from], |r| {
            Ok(OutPoint::new(Txid::from_byte_array(r.get(0)?), r.get(1)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let pending = pending_inputs(t)?;
    for outpoint in unspent {
        t.execute(
            "UPDATE silent_coins SET spent_at = NULL, spent_by = ?3
             WHERE txid = ?1 AND vout = ?2",
            params![
                outpoint.txid.to_byte_array(),
                outpoint.vout,
                pending.get(&outpoint).map(|txid| txid.to_byte_array())
            ],
        )?;
    }
    t.execute("DELETE FROM silent_blocks WHERE height >= ?1", [from])?;
    Ok(true)
}

/// Give up the wallet's own spend `txid`, freeing the coins it takes that
/// no block spends: how many.
fn give_up(t: &Sql<'_>, txid: &Txid) -> Result<usize, Error> {
    let freed = t.execute(
        "UPDATE silent_coins SET spent_by = NULL WHERE spent_by = ?1 AND spent_at IS NULL",
        [txid.to_byte_array()],
    )?;
    let known = t.execute(
        "DELETE FROM silent_spends WHERE txid = ?1",
        [txid.to_byte_array()],
    )?;
    Ok(freed.max(known))
}

/// The coins the wallet's own spends take, by the spend.
fn pending_inputs(t: &Sql<'_>) -> Result<HashMap<OutPoint, Txid>, Error> {
    let mut stmt = t.prepare("SELECT txid, tx FROM silent_spends")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut taken = HashMap::new();
    for row in rows {
        let (txid, tx) = row?;
        let tx: Transaction = bitcoin::consensus::deserialize(&tx).map_err(invalid)?;
        for input in tx.input {
            taken.insert(input.previous_output, Txid::from_byte_array(txid));
        }
    }
    Ok(taken)
}

/// The taproot script of output key `key`, as is.
fn p2tr(key: XOnlyPublicKey) -> ScriptBuf {
    ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(key))
}

impl<T: Transport> Wallet<T> {
    /// Look for payments to the silent payment address from block `from`
    /// on, with the tweaks of the hub at `hub`: the chain's height when the
    /// wallet was made, or an earlier block to look again, forgetting what
    /// was found from it on until found again. Never later than it stands,
    /// nor earlier than the hub serves tweaks from.
    pub fn watch_silent(&self, hub: &str, from: u32) -> Result<(), Error> {
        let url = hub_url(hub)?;
        let first = self.tweaks_from(&url)?;
        if from < first {
            return Err(Error::Invalid(format!(
                "hub {url} serves tweaks from block {first} on, not {from}"
            )));
        }
        self.store.write(|t| {
            let next: Option<u32> = t
                .query_row("SELECT next FROM silent_scan", [], |r| r.get(0))
                .optional()?;
            match next {
                None => {
                    t.execute("INSERT INTO silent_scan (one, next) VALUES (1, ?1)", [from])?;
                }
                Some(next) if from < next => {
                    rewind(t, next, from)?;
                }
                Some(_) => {}
            }
            Ok(())
        })?;
        tracing::info!(from, "watching for silent payments");
        Ok(())
    }

    /// The first block the hub at `url` serves tweaks of.
    fn tweaks_from(&self, url: &str) -> Result<u32, Error> {
        let (_, info) = self.hub(url)?;
        info.tweaks
            .ok_or_else(|| Error::Invalid(format!("hub {url} serves no silent payment tweaks")))
    }

    /// The next block to look in for silent payments: none until
    /// [`Wallet::watch_silent`].
    pub fn silent_next(&self) -> Result<Option<u32>, Error> {
        self.store.read(|c| {
            Ok(
                c.query_row("SELECT next FROM silent_scan", [], |r| r.get(0))
                    .optional()?,
            )
        })
    }

    /// Look in up to `max` blocks from the next for payments to the silent
    /// payment address and spends of its coins, with the tweaks of the hub
    /// at `hub` and the blocks of `chain`: up to the chain's height, or the
    /// newest block the hub indexed. Each batch of blocks is recorded whole,
    /// so an interrupted scan goes on from the last.
    pub async fn scan_silent(
        &self,
        hub: &str,
        chain: &impl Blocks,
        max: u32,
    ) -> Result<Scanned, Error> {
        let url = hub_url(hub)?;
        let first = self.tweaks_from(&url)?;
        if self.silent_next()?.is_none() {
            return Err(Error::Invalid(
                "not watching for silent payments: watch_silent first".into(),
            ));
        }
        let height = chain.height().await.map_err(chain_error)?;
        let next = self.unwind(chain, height).await?;
        let mut scanned = Scanned {
            next,
            height,
            found: Vec::new(),
            spent: Vec::new(),
            fetched: 0,
        };
        if next > height || max == 0 {
            return Ok(scanned);
        }
        if next < first {
            return Err(Error::Invalid(format!(
                "hub {url} serves tweaks from block {first} on, not {next}"
            )));
        }
        let end = height.min(next.saturating_add(max - 1));
        // Read after the chain's height: an address handed out meanwhile is
        // paid in a later block than these.
        let scanner = self.keys.silent.scanner([]);
        let (fallbacks, _) = self.store.read(|c| fallbacks(c, &scanner))?;
        let finder = Finder { scanner, fallbacks };
        let mut coins = self.held_outputs()?;
        while scanned.next <= end {
            let count = (end - scanned.next + 1).min(MAX_TWEAK_BLOCKS);
            let path = format!("{TWEAKS}?from={}&count={count}", scanned.next);
            let blocks: Vec<BlockTweaks> = self.get(&url, &path).await?;
            if blocks.is_empty() {
                break;
            }
            let mut batch = Batch::default();
            for tweaks in blocks.into_iter().take(count as usize) {
                let at = scanned.next + batch.blocks.len() as u32;
                let fetched = self
                    .look(&url, chain, &finder, at, &tweaks, &mut coins, &mut batch)
                    .await?;
                scanned.fetched += u32::from(fetched);
            }
            let Some(&(last, _)) = batch.blocks.last() else {
                break;
            };
            self.record_silent(scanned.next, &batch)?;
            scanned.next = last + 1;
            scanned
                .found
                .extend(
                    batch
                        .found
                        .iter()
                        .map(|(outpoint, value, found, height)| SilentCoin {
                            outpoint: *outpoint,
                            value: *value,
                            height: *height,
                            change: found.label == Some(CHANGE),
                            spending: None,
                        }),
                );
            scanned
                .spent
                .extend(batch.spent.iter().map(|(outpoint, ..)| *outpoint));
        }
        if !scanned.found.is_empty() || !scanned.spent.is_empty() {
            tracing::info!(
                found = scanned.found.len(),
                spent = scanned.spent.len(),
                next = scanned.next,
                "silent payments scanned"
            );
        }
        Ok(scanned)
    }

    /// Look in block `at` with its `tweaks` from the hub at `url`, adding
    /// to `batch` what it holds for the wallet, and to `coins` the coins
    /// found: whether the block was fetched.
    #[allow(clippy::too_many_arguments)]
    async fn look(
        &self,
        url: &str,
        chain: &impl Blocks,
        finder: &Finder,
        at: u32,
        tweaks: &BlockTweaks,
        coins: &mut HashMap<OutPoint, XOnlyPublicKey>,
        batch: &mut Batch,
    ) -> Result<bool, Error> {
        if tweaks.height != at {
            return Err(unavailable(
                url,
                format!("tweaks of block {} asked for {at}", tweaks.height),
            ));
        }
        let hash = chain.hash(at).await.map_err(chain_error)?;
        if tweaks.hash != hash.to_string() {
            return Err(unavailable(
                url,
                format!("its block {at} is {}, the chain's {hash}", tweaks.hash),
            ));
        }
        batch.blocks.push((at, hash));
        let points = tweaks
            .tweaks
            .iter()
            .map(|t| PublicKey::from_slice(&t.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| unavailable(url, format!("a tweak of block {at}: {e}")))?;
        let candidates: Vec<Vec<XOnlyPublicKey>> = points
            .iter()
            .map(|p| finder.scanner.candidates(p))
            .collect();
        let scripts: Vec<ScriptBuf> = candidates
            .iter()
            .flatten()
            .chain(coins.values())
            .chain(finder.fallbacks.keys())
            .map(|key| p2tr(*key))
            .collect();
        if scripts.is_empty() {
            return Ok(false);
        }
        let matched = match chain.filter(&hash).await.map_err(chain_error)? {
            None => true,
            Some(filter) => BlockFilter::new(&filter)
                .match_any(&hash, scripts.iter().map(|s| s.as_bytes()))
                .map_err(|e| chain_error(format!("the filter of block {hash}: {e}")))?,
        };
        if !matched {
            return Ok(false);
        }
        let block = chain.block(&hash).await.map_err(chain_error)?;
        if block.block_hash() != hash || !block.check_merkle_root() {
            return Err(chain_error(format!(
                "block {hash} does not match its header"
            )));
        }
        // Which transaction each output key is in.
        let mut keys: HashMap<XOnlyPublicKey, usize> = HashMap::new();
        for (i, tx) in block.txdata.iter().enumerate() {
            for key in silent::taproot_outputs(tx) {
                keys.insert(key, i);
            }
        }
        let mut paying: HashMap<usize, Vec<&PublicKey>> = HashMap::new();
        for (point, candidates) in points.iter().zip(&candidates) {
            if let Some(i) = candidates.iter().find_map(|c| keys.get(c)) {
                paying.entry(*i).or_default().push(point);
            }
        }
        // In the block's order: a coin may be paid and spent in one block.
        for (i, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            for input in &tx.input {
                if coins.remove(&input.previous_output).is_some() {
                    batch.spent.push((input.previous_output, txid, at));
                }
            }
            let mut seen = HashSet::new();
            for point in paying.get(&i).into_iter().flatten() {
                let outputs = silent::taproot_outputs(tx);
                for found in finder.scanner.scan(point, &outputs) {
                    let vout = tx
                        .output
                        .iter()
                        .position(|o| o.script_pubkey == p2tr(found.output));
                    let Some(vout) = vout else { continue };
                    let outpoint = OutPoint::new(txid, vout as u32);
                    if seen.insert(outpoint) {
                        let value = tx.output[vout].value.to_sat();
                        coins.insert(outpoint, found.output);
                        batch.found.push((outpoint, value, found, at));
                    }
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let Some(found) = finder.fallback(&output.script_pubkey) else {
                    continue;
                };
                let outpoint = OutPoint::new(txid, vout as u32);
                if seen.insert(outpoint) {
                    coins.insert(outpoint, found.output);
                    batch
                        .found
                        .push((outpoint, output.value.to_sat(), *found, at));
                }
            }
        }
        Ok(true)
    }

    /// Undo what blocks a reorganization took out of the best chain, of
    /// height `height`, held: the next block to look in.
    async fn unwind(&self, chain: &impl Blocks, height: u32) -> Result<u32, Error> {
        let (next, kept) = self.store.read(|c| {
            let next: u32 = c.query_row("SELECT next FROM silent_scan", [], |r| r.get(0))?;
            let mut stmt =
                c.prepare("SELECT height, hash FROM silent_blocks ORDER BY height DESC")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, [u8; 32]>(1)?)))?;
            let kept = rows.collect::<Result<Vec<_>, _>>()?;
            Ok((next, kept))
        })?;
        let Some(&(top, _)) = kept.first() else {
            return Ok(next);
        };
        // A chain behind the blocks looked in proves nothing: wait for it.
        if top > height {
            return Ok(next);
        }
        let mut fork = None;
        for (at, hash) in &kept {
            let theirs = chain.hash(*at).await.map_err(chain_error)?;
            if theirs.to_byte_array() == *hash {
                fork = Some(*at);
                break;
            }
        }
        let from = match fork {
            Some(at) if at == top => return Ok(next),
            Some(at) => at + 1,
            None => {
                return Err(Error::Invalid(format!(
                    "a reorganization deeper than the {} blocks kept: \
                     watch_silent from an earlier block to look again",
                    kept.len()
                )))
            }
        };
        self.store.write(|t| {
            if !rewind(t, next, from)? {
                return Err(raced());
            }
            Ok(())
        })?;
        tracing::warn!(
            from,
            top,
            "reorganization: silent payments looked for again"
        );
        Ok(from)
    }

    /// Record `batch`, looked in from block `from`, unless another scan
    /// moved on meanwhile.
    fn record_silent(&self, from: u32, batch: &Batch) -> Result<(), Error> {
        let Some(&(last, _)) = batch.blocks.last() else {
            return Ok(());
        };
        self.store.write(|t| {
            let moved = t.execute(
                "UPDATE silent_scan SET next = ?2 WHERE next = ?1",
                [from, last + 1],
            )?;
            if moved == 0 {
                return Err(raced());
            }
            let pending = if batch.found.is_empty() && batch.spent.is_empty() {
                HashMap::new()
            } else {
                pending_inputs(t)?
            };
            for (outpoint, value, found, height) in &batch.found {
                t.execute(
                    "INSERT OR IGNORE INTO silent_coins
                         (txid, vout, value, output, tweak, label, height, spent_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        outpoint.txid.to_byte_array(),
                        outpoint.vout,
                        crate::store::sql(*value),
                        found.output.serialize(),
                        found.tweak.secret_bytes(),
                        found.label,
                        height,
                        pending.get(outpoint).map(|txid| txid.to_byte_array())
                    ],
                )?;
            }
            for (outpoint, by, height) in &batch.spent {
                t.execute(
                    "UPDATE silent_coins SET spent_by = ?3, spent_at = ?4
                     WHERE txid = ?1 AND vout = ?2",
                    params![
                        outpoint.txid.to_byte_array(),
                        outpoint.vout,
                        by.to_byte_array(),
                        height
                    ],
                )?;
                // A spend of the wallet's own that a block's conflicts with
                // can never confirm: its other coins are free again.
                if let Some(own) = pending.get(outpoint).filter(|own| *own != by) {
                    give_up(t, own)?;
                    tracing::info!(txid = %own, "silent payment spend replaced in a block");
                }
            }
            for (height, hash) in &batch.blocks {
                t.execute(
                    "INSERT OR REPLACE INTO silent_blocks (height, hash) VALUES (?1, ?2)",
                    params![height, hash.to_byte_array()],
                )?;
            }
            t.execute(
                "DELETE FROM silent_blocks WHERE height + ?2 <= ?1",
                [last, KEEP_BLOCKS],
            )?;
            Ok(())
        })
    }

    /// The output keys of the coins not spent in a block, by outpoint.
    fn held_outputs(&self) -> Result<HashMap<OutPoint, XOnlyPublicKey>, Error> {
        self.store.read(|c| {
            let mut stmt =
                c.prepare("SELECT txid, vout, output FROM silent_coins WHERE spent_at IS NULL")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, [u8; 32]>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, [u8; 32]>(2)?,
                ))
            })?;
            let mut held = HashMap::new();
            for row in rows {
                let (txid, vout, output) = row?;
                let output = XOnlyPublicKey::from_slice(&output).map_err(invalid)?;
                held.insert(OutPoint::new(Txid::from_byte_array(txid), vout), output);
            }
            Ok(held)
        })
    }

    /// Hand out the fallback address to show: the one handed out last
    /// until it is paid, then the next. Watched from then on.
    pub(crate) fn fallback_address(&self) -> Result<bitcoin::Address, Error> {
        let scanner = self.keys.silent.scanner([]);
        let found = self.store.write(|t| {
            let (_, next) = fallbacks(t, &scanner)?;
            t.execute(
                "INSERT INTO silent_fallback (one, shown) VALUES (1, ?1)
                 ON CONFLICT (one) DO UPDATE SET shown = max(shown, excluded.shown)",
                [next],
            )?;
            scanner.fallback(next).map_err(invalid)
        })?;
        let key = TweakedPublicKey::dangerous_assume_tweaked(found.output);
        Ok(bitcoin::Address::p2tr_tweaked(
            key,
            licash_onchain::network(self.network),
        ))
    }
}

impl<T: Transport> Wallet<T> {
    /// The coins at the silent payment address not spent in a block,
    /// oldest first.
    pub fn silent_coins(&self) -> Result<Vec<SilentCoin>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT txid, vout, value, height, label, spent_by FROM silent_coins
                 WHERE spent_at IS NULL ORDER BY height, txid, vout",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SilentCoin {
                    outpoint: OutPoint::new(
                        Txid::from_byte_array(r.get::<_, [u8; 32]>(0)?),
                        r.get(1)?,
                    ),
                    value: r.get::<_, i64>(2)? as u64,
                    height: r.get(3)?,
                    change: r.get::<_, Option<u32>>(4)? == Some(CHANGE),
                    spending: r.get::<_, Option<[u8; 32]>>(5)?.map(Txid::from_byte_array),
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// The wallet's transactions spending its coins that are in no block
    /// yet, oldest first: to broadcast again until they are.
    pub fn silent_spends(&self) -> Result<Vec<Transaction>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT tx FROM silent_spends s
                 WHERE EXISTS (SELECT 1 FROM silent_coins c
                               WHERE c.spent_by = s.txid AND c.spent_at IS NULL)
                 ORDER BY created_at, txid",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            rows.map(|tx| bitcoin::consensus::deserialize(&tx?).map_err(invalid))
                .collect()
        })
    }

    /// Give up the wallet's spend `txid`, never broadcast or dropped by the
    /// network, freeing its coins: should it reach a block after all, a
    /// scan sees them spent.
    pub fn abandon_silent(&self, txid: &Txid) -> Result<(), Error> {
        let freed = self.store.write(|t| give_up(t, txid))?;
        if freed == 0 {
            return Err(Error::UnknownPayment(txid.to_string()));
        }
        tracing::info!(%txid, freed, "silent payment spend abandoned");
        Ok(())
    }

    /// A transaction paying `amount` to `to`, or everything if `None`, of
    /// the coins at the silent payment address that no transaction spends,
    /// the fee at `rate` on top and change to the wallet's change address
    /// unless it would be dust: signed and recorded as spending its coins,
    /// for the application to broadcast.
    pub fn spend_silent(
        &self,
        to: ScriptBuf,
        amount: Option<u64>,
        rate: FeeRate,
    ) -> Result<Transaction, Error> {
        let keys = &self.keys.silent;
        let tx = self.store.write(|t| {
            let plan = plan(&free_coins(t)?, &to, amount, rate)?;
            let secrets = plan
                .inputs
                .iter()
                .map(|h| keys.spend_secret(&h.found))
                .collect::<Result<Vec<SecretKey>, _>>()
                .map_err(invalid)?;
            let mut outputs = vec![TxOut {
                value: Amount::from_sat(plan.pay),
                script_pubkey: to.clone(),
            }];
            if let Some(change) = plan.change {
                let keyed: Vec<(SecretKey, bool)> = secrets.iter().map(|s| (*s, true)).collect();
                let outpoints: Vec<OutPoint> = plan.inputs.iter().map(|h| h.outpoint).collect();
                let address = keys.scanner([]).labeled(CHANGE);
                let paid = silent::outputs(&keyed, &outpoints, &[address]).map_err(invalid)?;
                outputs.push(TxOut {
                    value: Amount::from_sat(change),
                    script_pubkey: p2tr(paid[0]),
                });
                if OsRng.next_u32() & 1 == 1 {
                    outputs.swap(0, 1);
                }
            }
            let tx = sign(&plan.inputs, &secrets, outputs)?;
            let txid = tx.compute_txid();
            t.execute(
                "INSERT INTO silent_spends (txid, tx, created_at) VALUES (?1, ?2, ?3)",
                params![
                    txid.to_byte_array(),
                    bitcoin::consensus::serialize(&tx),
                    now()
                ],
            )?;
            for input in &plan.inputs {
                let taken = t.execute(
                    "UPDATE silent_coins SET spent_by = ?3
                     WHERE txid = ?1 AND vout = ?2 AND spent_by IS NULL",
                    params![
                        input.outpoint.txid.to_byte_array(),
                        input.outpoint.vout,
                        txid.to_byte_array()
                    ],
                )?;
                if taken != 1 {
                    return Err(Error::NoteTaken);
                }
            }
            Ok(tx)
        })?;
        tracing::info!(txid = %tx.compute_txid(), inputs = tx.input.len(), "silent payment coins spent");
        Ok(tx)
    }

    /// Deposit at the hub at `url` the coins at the silent payment address
    /// that no transaction spends: notes of as much as they pay, the fee
    /// at `rate` and the hub's on top, up to the most the hub takes. The
    /// deposit, and the transaction paying it, signed and recorded as
    /// spending its coins, for the application to broadcast. Should it
    /// never confirm, [`Wallet::abandon_silent`] frees the coins, and the
    /// deposit lapses unpaid.
    pub async fn deposit_silent(
        &self,
        url: &str,
        rate: FeeRate,
    ) -> Result<(Deposit, Transaction), Error> {
        let url = hub_url(url)?;
        let (_, info) = self.refresh(&url).await?;
        let offer = info
            .deposits
            .ok_or_else(|| Error::Invalid(format!("hub {url} takes no deposits")))?;
        // What the coins pay a taproot output, as a deposit's is.
        let taproot = ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0; 32]].concat());
        let held = self.store.read(free_coins)?;
        let total = held.iter().map(|h| h.value).sum::<u64>();
        let available = plan(&held, &taproot, None, rate).map_or(0, |p| p.pay);
        let fits = |amount: u64| offer.pays(amount).is_some_and(|pays| pays <= available);
        let (mut amount, mut over) = (0, offer.max.saturating_add(1));
        while over - amount > 1 {
            let mid = amount + (over - amount) / 2;
            match fits(mid) {
                true => amount = mid,
                false => over = mid,
            }
        }
        if amount < offer.min {
            let output = TxOut {
                value: Amount::ZERO,
                script_pubkey: taproot,
            };
            let fee = fee(held.len().max(1), &[output], rate);
            return Err(Error::Insufficient {
                needed: offer
                    .pays(offer.min)
                    .map_or(u64::MAX, |p| p.saturating_add(fee)),
                available: total,
            });
        }
        let deposit = self.deposit(&url, amount).await?;
        let terms = match (&deposit.state, &deposit.terms) {
            (DepositState::Open, Some(terms)) => terms,
            (state, _) => {
                return Err(Error::Invalid(format!(
                    "deposit {} is not open to pay: {state:?}",
                    deposit.hash
                )))
            }
        };
        let to = terms
            .address
            .parse::<bitcoin::Address<_>>()
            .ok()
            .and_then(|a| {
                a.require_network(licash_onchain::network(self.network()))
                    .ok()
            })
            .ok_or_else(|| Error::Invalid(format!("deposit address {}", terms.address)))?;
        let tx = self.spend_silent(to.script_pubkey(), Some(terms.pays), rate)?;
        tracing::info!(hash = %deposit.hash, amount, txid = %tx.compute_txid(), "silent payment coins deposited");
        Ok((deposit, tx))
    }
}

/// What a spend takes and pays: to its payee, and the change.
struct Plan {
    inputs: Vec<Held>,
    pay: u64,
    change: Option<u64>,
}

/// The coins no transaction spends, largest first.
fn free_coins(t: &rusqlite::Connection) -> Result<Vec<Held>, Error> {
    let mut stmt = t.prepare(
        "SELECT txid, vout, value, output, tweak, label FROM silent_coins
         WHERE spent_by IS NULL AND spent_at IS NULL ORDER BY value DESC, txid, vout",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, [u8; 32]>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, [u8; 32]>(3)?,
            r.get::<_, [u8; 32]>(4)?,
            r.get::<_, Option<u32>>(5)?,
        ))
    })?;
    let mut held = Vec::new();
    for row in rows {
        let (txid, vout, value, output, tweak, label) = row?;
        held.push(Held {
            outpoint: OutPoint::new(Txid::from_byte_array(txid), vout),
            value: value as u64,
            found: Found {
                output: XOnlyPublicKey::from_slice(&output).map_err(invalid)?,
                tweak: SecretKey::from_slice(&tweak).map_err(invalid)?,
                label,
            },
        });
    }
    Ok(held)
}

/// The fee at `rate` of a spend of `inputs` coins paying `outputs`.
fn fee(inputs: usize, outputs: &[TxOut], rate: FeeRate) -> u64 {
    let input = TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        // A key path signature's 64 bytes.
        witness: Witness::from_slice(&[[0; 64]]),
    };
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![input; inputs],
        output: outputs.to_vec(),
    };
    rate.fee_wu(tx.weight()).map_or(u64::MAX, Amount::to_sat)
}

/// The coins of `held`, largest first, that pay `amount` to `to`, or all
/// of them everything, and the fee at `rate`.
fn plan(held: &[Held], to: &ScriptBuf, amount: Option<u64>, rate: FeeRate) -> Result<Plan, Error> {
    let out = |value: u64, script: &ScriptBuf| TxOut {
        value: Amount::from_sat(value),
        script_pubkey: script.clone(),
    };
    // Change is a taproot output: any 32 bytes size it.
    let change_script = ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0; 32]].concat());
    let (dust, change_dust) = (
        to.minimal_non_dust().to_sat(),
        change_script.minimal_non_dust().to_sat(),
    );
    let total = |n: usize| held[..n].iter().map(|h| h.value).sum::<u64>();
    let Some(amount) = amount else {
        let fee = fee(held.len(), &[out(0, to)], rate);
        let available = total(held.len());
        let pay = available
            .checked_sub(fee)
            .filter(|pay| !held.is_empty() && *pay >= dust)
            .ok_or(Error::Insufficient {
                needed: fee.saturating_add(dust),
                available,
            })?;
        return Ok(Plan {
            inputs: held.to_vec(),
            pay,
            change: None,
        });
    };
    if amount < dust {
        return Err(Error::Invalid(format!("{amount} is dust to that script")));
    }
    for n in 1..=held.len() {
        let paid = total(n);
        let with = fee(n, &[out(amount, to), out(0, &change_script)], rate);
        let change = paid.checked_sub(amount.saturating_add(with));
        if let Some(change) = change.filter(|c| *c >= change_dust) {
            return Ok(Plan {
                inputs: held[..n].to_vec(),
                pay: amount,
                change: Some(change),
            });
        }
        if paid >= amount.saturating_add(fee(n, &[out(amount, to)], rate)) {
            return Ok(Plan {
                inputs: held[..n].to_vec(),
                pay: amount,
                change: None,
            });
        }
    }
    Err(Error::Insufficient {
        needed: amount.saturating_add(fee(held.len().max(1), &[out(amount, to)], rate)),
        available: total(held.len()),
    })
}

/// A transaction spending `inputs` on the key path with `secrets`, paying
/// `outputs`: replaceable.
fn sign(inputs: &[Held], secrets: &[SecretKey], output: Vec<TxOut>) -> Result<Transaction, Error> {
    let secp = Secp256k1::new();
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: inputs
            .iter()
            .map(|h| TxIn {
                previous_output: h.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output,
    };
    let prevouts: Vec<TxOut> = inputs
        .iter()
        .map(|h| TxOut {
            value: Amount::from_sat(h.value),
            script_pubkey: p2tr(h.found.output),
        })
        .collect();
    let mut cache = SighashCache::new(&tx);
    let mut signatures = Vec::with_capacity(inputs.len());
    for (i, (held, secret)) in inputs.iter().zip(secrets).enumerate() {
        let keypair = Keypair::from_secret_key(&secp, secret);
        if keypair.x_only_public_key().0 != held.found.output {
            return Err(Error::Invalid(format!("no key for coin {}", held.outpoint)));
        }
        let sighash = cache
            .taproot_key_spend_signature_hash(i, &Prevouts::All(&prevouts), TapSighashType::Default)
            .map_err(invalid)?;
        let message = Message::from_digest(sighash.to_byte_array());
        let mut aux = [0; 32];
        OsRng.fill_bytes(&mut aux);
        signatures.push(bitcoin::taproot::Signature {
            signature: secp.sign_schnorr_with_aux_rand(&message, &keypair, &aux),
            sighash_type: TapSighashType::Default,
        });
    }
    for (input, signature) in tx.input.iter_mut().zip(&signatures) {
        input.witness = Witness::p2tr_key_spend(signature);
    }
    Ok(tx)
}
