//! Payouts: notes paid out on Bitcoin through a swap provider
//! ([`licash_api::swaps`]), as a reverse submarine swap.
//!
//! The wallet draws a preimage and a key to claim with, and records them
//! with the payee before it locks the coin's value and the provider's fee
//! to the provider at hub `from`, under the preimage's hash. The provider
//! then locks a coin on Bitcoin under the same hash ([`Htlc`]): the
//! wallet's key and the preimage claim it, the provider's key takes it back
//! after its timeout. Once the wallet found the coin on its own view of the
//! chain, confirmed and worth what was asked, with room left before its
//! timeout, it claims it, paying the payee exactly the amount asked and the
//! rest of the coin as the claim's fee. The claim reveals the preimage,
//! with which the provider claims the wallet's lock. Nothing is revealed
//! before: if the provider never locks a coin the wallet takes, the
//! wallet's lock is refunded once expired.
//!
//! The wallet broadcasts nothing: its claims ([`Wallet::payout_claims`])
//! are the application's to broadcast, again after each follow, until they
//! confirm.

use std::{str::FromStr, sync::PoisonError};

use bitcoin::{
    absolute::LOCK_TIME_THRESHOLD,
    bip158::BlockFilter,
    consensus::{deserialize, serialize},
    hashes::Hash,
    secp256k1::{SecretKey, XOnlyPublicKey},
    Amount, Block, FeeRate, OutPoint, ScriptBuf, Transaction, Txid,
};
use licash::spend::{lock_hash, ChainProfile, ProposalId};
use licash_api::{
    swaps::{self, State as Theirs},
    Info,
};
use licash_onchain::deposit::{self, Htlc};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Row};

use crate::{
    deposits::{random_key, x_only},
    hub_url, invalid, now, sql, unavailable, Blocks, Error, LockState, Transport, Wallet,
};

/// Blocks a payout's coin must have left before its timeout when the
/// wallet claims it, for its claim to confirm before the provider may take
/// the coin back.
pub const CLAIM_ROOM: u32 = 12;

/// A claim is budgeted at this many times the fee rate judged when it was
/// asked for: the rate may be higher once it is broadcast.
const HEADROOM: u64 = 3;

/// The least rate a claim is budgeted at, in sat per 1000 virtual bytes.
const MIN_RATE: u64 = 1_000;

/// The blocks looked in again for a coin's spend, for a reorganization.
const REORG: u32 = 6;

/// Confirmations a coin needs before the wallet claims it, and its claim
/// before the payout is done: a reorganization deeper than that could take
/// the coin away once the preimage is out.
pub fn payout_depth(network: ChainProfile) -> u32 {
    match network {
        ChainProfile::Core30Mainnet => 2,
        _ => 1,
    }
}

/// What a provider asks to pay notes out on Bitcoin
/// ([`Wallet::payout_quote`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayoutQuote {
    /// What the provider said of itself, with what it pays out.
    pub info: swaps::Info,
    /// What the payee gets.
    pub amount: u64,
    /// The claim's fee: the coin is worth the amount and this.
    pub budget: u64,
    /// The provider's fee, its coin's miners included.
    pub fee: u64,
    /// The hub's fee on the wallet's lock, as it charges now.
    pub lock_fee: u64,
}

impl PayoutQuote {
    /// What the provider's coin is worth.
    pub fn value(&self) -> u64 {
        self.amount.saturating_add(self.budget)
    }

    /// What the payout costs on top of the amount.
    pub fn costs(&self) -> u64 {
        self.budget
            .saturating_add(self.fee)
            .saturating_add(self.lock_fee)
    }
}

/// A payout of notes at hub `from` to an address on Bitcoin, and where it
/// stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    /// The hash of the wallet's preimage, in hex: the payout's id here.
    pub hash: String,
    pub provider: String,
    pub from: String,
    /// The payee's address.
    pub address: String,
    /// What the payee gets.
    pub amount: u64,
    /// The claim's fee.
    pub budget: u64,
    /// The provider's fee, which the wallet's lock pays on top.
    pub fee: u64,
    /// The wallet's lock to the provider at `from`, once made.
    pub lock: Option<String>,
    /// The provider's coin, once found.
    pub funding: Option<OutPoint>,
    /// The wallet's claim of it, once made.
    pub claim: Option<Txid>,
    pub state: PayoutState,
    pub created_at: i64,
}

/// How far a payout has come, step by step, as the wallet last saw it:
/// for an application to show where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PayoutProgress {
    /// The wallet's lock to the provider once made, where it stands, and
    /// the block from which its hub refunds it unpaid.
    pub lock: Option<(LockState, u32)>,
    /// The block from which the provider may take its coin back, once the
    /// provider took the payout.
    pub timeout: Option<u32>,
    /// The last block the wallet claims the coin at: [`CLAIM_ROOM`] before
    /// the timeout, for the claim to confirm in time. Past it, the payout
    /// fails and the lock is refunded.
    pub claim_by: Option<u32>,
    /// The provider's coin: as found on chain, or else as the provider told
    /// of it since the wallet was opened, maybe not confirmed yet.
    pub coin: Option<Txid>,
    /// The block the coin is in: as found, or else as the provider told.
    pub coin_height: Option<u32>,
    /// The wallet's claim of the coin, once made.
    pub claim: Option<Txid>,
    /// The block the claim is in, once found.
    pub claim_height: Option<u32>,
    /// The confirmations awaited: of the coin before it is claimed, of the
    /// claim before the payout is done.
    pub depth: u32,
}

/// Where a payout stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayoutState {
    /// Under way, nothing revealed yet.
    Open,
    /// The coin claimed: the claim is broadcast until it confirms.
    Claimed,
    /// The claim confirmed: the payee is paid.
    Done,
    /// Given up, and why. Before a claim, nothing was revealed: the
    /// wallet's lock, if made, is refunded once expired. After one, the
    /// provider took its coin back first.
    Failed(String),
}

/// A payout as recorded, to take it on.
struct Record {
    provider: String,
    preimage: [u8; 32],
    claim_key: [u8; 32],
    from: (i64, String),
    script: ScriptBuf,
    amount: u64,
    budget: u64,
    refund: Option<([u8; 32], u32)>,
    /// The coin, the block it is in and its value.
    funding: Option<(OutPoint, u32, u64)>,
    claim: Option<Vec<u8>>,
    scanned: Option<u32>,
    open: bool,
}

impl Record {
    fn value(&self) -> u64 {
        self.amount.saturating_add(self.budget)
    }

    fn claim_key(&self) -> Result<SecretKey, Error> {
        SecretKey::from_slice(&self.claim_key).map_err(invalid)
    }

    /// The coin's terms, once the provider named its own.
    fn htlc(&self) -> Result<Option<Htlc>, Error> {
        let Some((refund, timeout)) = self.refund else {
            return Ok(None);
        };
        Ok(Some(Htlc {
            hash: lock_hash(&self.preimage),
            claim: x_only(&self.claim_key()?),
            refund: XOnlyPublicKey::from_slice(&refund).map_err(invalid)?,
            timeout,
        }))
    }
}

/// Payouts.
impl<T: Transport> Wallet<T> {
    /// What the provider at `provider` asks to pay `amount` to `to`, an
    /// address on Bitcoin, from notes at the hub at `from`, added, as it
    /// last said of itself. `rate` is the fee rate the wallet judges a
    /// claim confirms at soon: the claim is budgeted at a few times that or
    /// the provider's, the higher. Refused as [`Error::Invalid`] if the
    /// provider pays nothing out on Bitcoin, takes no lock at `from`, pays
    /// out not that much, or the payout costs more than `max_fee` on top of
    /// the amount ([`PayoutQuote::costs`]): which bounds the claim's fee
    /// too, whatever rate the provider names. Nothing is recorded or
    /// locked: [`Wallet::payout`] checks it again.
    pub async fn payout_quote(
        &self,
        provider: &str,
        from: &str,
        to: &str,
        amount: u64,
        max_fee: u64,
        rate: FeeRate,
    ) -> Result<PayoutQuote, Error> {
        let provider = hub_url(provider)?;
        let from = hub_url(from)?;
        let (script, _) = self.payout_payee(to)?;
        let (_, from_info) = self.hub(&from)?;
        let info: swaps::Info = self.get(&provider, swaps::SWAPS_INFO).await?;
        let terms = Terms {
            provider: &provider,
            from: (&from, &from_info),
            script: &script,
            amount,
            max_fee,
            rate,
        };
        terms.quote(info)
    }

    /// Pay `amount` to `to`, an address on Bitcoin, from notes at the hub
    /// at `from`, added, through the provider at `provider`, on the terms
    /// of [`Wallet::payout_quote`]: lock the coin's value and the fees to
    /// the provider at `from`, then claim the coin it locks on Bitcoin.
    /// Where the payout stands once its lock is made, whatever the provider
    /// answered; [`Wallet::follow_payout`] takes it on.
    pub async fn payout(
        &self,
        provider: &str,
        from: &str,
        to: &str,
        amount: u64,
        max_fee: u64,
        rate: FeeRate,
    ) -> Result<Payout, Error> {
        let provider = hub_url(provider)?;
        let from = hub_url(from)?;
        let (script, address) = self.payout_payee(to)?;
        let from_hub = self.hub(&from)?.0;
        let (_, from_info) = self.refresh(&from).await?;
        let info: swaps::Info = self.get(&provider, swaps::SWAPS_INFO).await?;
        let terms = Terms {
            provider: &provider,
            from: (&from, &from_info),
            script: &script,
            amount,
            max_fee,
            rate,
        };
        let quote = terms.quote(info)?;
        let locks = from_info.locks.as_ref().ok_or_else(|| {
            Error::Invalid(format!("hub {from} follows no chain: it takes no locks"))
        })?;
        let expiry = locks
            .latest_expiry(locks.min_depth.max(1))
            .map_err(|p| unavailable(&from, p))?;
        let mut preimage = [0; 32];
        OsRng.fill_bytes(&mut preimage);
        let hash = lock_hash(&preimage);
        let claim_key = random_key().secret_bytes();
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO payouts (hash, provider, preimage, claim_key, from_hub, address,
                    script, amount, budget, fee, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    hash,
                    provider,
                    preimage,
                    claim_key,
                    from_hub,
                    address,
                    script.as_bytes(),
                    sql(amount),
                    sql(quote.budget),
                    sql(quote.fee),
                    now()
                ],
            )?;
            Ok(())
        })?;
        tracing::info!(
            hash = %hex::encode(hash), %provider, %from, %address, amount,
            budget = quote.budget, fee = quote.fee, "paying out"
        );
        let total = quote.value().saturating_add(quote.fee);
        match self
            .lock(&from, &quote.info.address, total, hash, Some(expiry))
            .await
        {
            Ok(_) | Err(Error::Pending { .. }) => {}
            Err(e) => {
                self.fail_payout(&hash, &format!("its lock was not made: {e}"))?;
                return Err(e);
            }
        }
        // Asked again by each follow, if this fails.
        if let Err(e) = self.ask_payout(&hash, &self.payout_row(&hash)?).await {
            tracing::debug!(hash = %hex::encode(hash), reason = %e, "payout waits");
        }
        self.payout_record(&hash)
    }

    /// Take the payout of `hash` on, `chain` the application's view of
    /// Bitcoin: have the provider lock its coin, claim it once confirmed
    /// where the provider says, and follow the claim until it confirms.
    /// Where it stands: broadcast [`Wallet::payout_claims`] after.
    pub async fn follow_payout(&self, hash: &str, chain: &impl Blocks) -> Result<Payout, Error> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(hash, &mut bytes)
            .map_err(|_| Error::Invalid(format!("not a payout's hash: {hash}")))?;
        let advanced = self.advance_payout(&bytes, chain).await;
        self.payout_after(&bytes, advanced)
    }

    /// How far the payout of `hash` has come, step by step. Reads only what
    /// the wallet holds and was told since opened: following it learns more.
    pub fn payout_progress(&self, hash: &str) -> Result<PayoutProgress, Error> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(hash, &mut bytes)
            .map_err(|_| Error::Invalid(format!("not a payout's hash: {hash}")))?;
        let payout = self.payout_record(&bytes)?;
        type Heights = (Option<u32>, Option<u32>, Option<u32>);
        let (timeout, found_height, spent_height): Heights = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT timeout, funding_height, spent_height FROM payouts WHERE hash = ?1",
                [bytes],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })?;
        let lock = match &payout.lock {
            Some(id) => {
                let mut lock = [0; 32];
                hex::decode_to_slice(id, &mut lock).map_err(invalid)?;
                let lock = self.lock_record(&ProposalId::from_bytes(lock))?;
                Some((lock.state, lock.expiry))
            }
            None => None,
        };
        let told = self
            .told_coins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&bytes)
            .copied();
        let (coin, coin_height) = match payout.funding {
            Some(funding) => (Some(funding.txid), found_height),
            None => told.map_or((None, None), |(txid, height)| (Some(txid), height)),
        };
        // The coin's spend is the claim's unless the payout failed after.
        let claim_height = match payout.state {
            PayoutState::Failed(_) => None,
            _ if payout.claim.is_some() => spent_height,
            _ => None,
        };
        Ok(PayoutProgress {
            lock,
            timeout,
            claim_by: timeout.and_then(|t| t.checked_sub(CLAIM_ROOM)),
            coin,
            coin_height,
            claim: payout.claim,
            claim_height,
            depth: payout_depth(self.network()),
        })
    }

    /// Take every payout under way on, as [`Wallet::follow_payout`]: where
    /// each stands, and why those still under way wait.
    pub async fn follow_payouts(
        &self,
        chain: &impl Blocks,
    ) -> Result<(Vec<Payout>, Vec<(String, String)>), Error> {
        let (mut followed, mut pending) = (Vec::new(), Vec::new());
        for hash in self.open_payouts()? {
            match self.payout_after(&hash, self.advance_payout(&hash, chain).await) {
                Ok(payout) => followed.push(payout),
                Err(Error::Pending { id, reason }) => {
                    pending.push((id, reason));
                    followed.push(self.payout_record(&hash)?);
                }
                Err(e) => return Err(e),
            }
        }
        Ok((followed, pending))
    }

    /// The claims of payouts not done, to broadcast: again after each
    /// follow, as a claim dropped or not confirmed yet loses the coin to
    /// the provider once it times out.
    pub fn payout_claims(&self) -> Result<Vec<Transaction>, Error> {
        let claims: Vec<Vec<u8>> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT claim_tx FROM payouts WHERE claim_tx IS NOT NULL
                 AND done_at IS NULL AND failed IS NULL ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        claims
            .iter()
            .map(|tx| deserialize(tx).map_err(invalid))
            .collect()
    }

    /// The payouts made, newest first.
    pub fn payouts(&self) -> Result<Vec<Payout>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(&format!(
                "{PAYOUTS} ORDER BY p.created_at DESC, p.rowid DESC"
            ))?;
            let rows = stmt.query_map([], payout_of)?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    fn payout_record(&self, hash: &[u8; 32]) -> Result<Payout, Error> {
        self.store.read(|c| {
            c.query_row(&format!("{PAYOUTS} WHERE p.hash = ?1"), [hash], payout_of)
                .optional()?
                .ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))
        })
    }

    /// The payouts under way, oldest first.
    pub(crate) fn open_payouts(&self) -> Result<Vec<[u8; 32]>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT hash FROM payouts WHERE done_at IS NULL AND failed IS NULL
                 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// Where the payout of `hash` stands after `advanced`: an error but
    /// those that end it as pending.
    fn payout_after(&self, hash: &[u8; 32], advanced: Result<(), Error>) -> Result<Payout, Error> {
        match advanced {
            Ok(()) => self.payout_record(hash),
            Err(e @ (Error::Pending { .. } | Error::UnknownPayment(_))) => Err(e),
            Err(e) => Err(Error::Pending {
                id: hex::encode(hash),
                reason: e.to_string(),
            }),
        }
    }

    async fn advance_payout(&self, hash: &[u8; 32], chain: &impl Blocks) -> Result<(), Error> {
        let record = self.payout_row(hash)?;
        if !record.open {
            return Ok(());
        }
        if record.claim.is_some() {
            return self.watch_claim(hash, &record, chain).await;
        }
        let Some((funding, hub_height)) = self.ask_payout(hash, &record).await? else {
            return Ok(());
        };
        // Its terms, as recorded first.
        let record = self.payout_row(hash)?;
        self.claim_payout(hash, &record, (&funding, hub_height), chain)
            .await
    }

    /// Ask the provider for the payout of the wallet's lock, and record its
    /// terms: its coin, as it says, once confirmed; `None` once the payout
    /// failed.
    async fn ask_payout(
        &self,
        hash: &[u8; 32],
        record: &Record,
    ) -> Result<Option<(licash_api::Funding, u32)>, Error> {
        let from_hub = record.from.0;
        let lock: Option<[u8; 32]> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT id FROM locks WHERE hash = ?1 AND hub = ?2
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![hash, from_hub],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        // Recorded before its lock, which was never made.
        let Some(lock) = lock else {
            self.fail_payout(hash, "its lock was not made")?;
            return Ok(None);
        };
        let id = ProposalId::from_bytes(lock);
        let ours = match self.advance_lock(&id).await {
            Ok(ours) => ours,
            Err(Error::Refused { problem, .. }) => {
                self.fail_payout(hash, &format!("its hub refused its lock: {problem}"))?;
                return Ok(None);
            }
            Err(Error::Abandoned { reason, .. }) => {
                self.fail_payout(hash, &format!("its lock was given up: {reason}"))?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        match ours.state {
            LockState::Requested => {
                return Err(Error::Pending {
                    id: hex::encode(hash),
                    reason: "its lock is not certified yet".into(),
                });
            }
            LockState::Refunded => {
                self.fail_payout(hash, "no coin was claimed: its lock was refunded")?;
                return Ok(None);
            }
            _ => {}
        }
        let provider = &record.provider;
        let (_, from_info) = self.refresh(&record.from.1).await?;
        let claim = x_only(&record.claim_key()?).serialize();
        let request = swaps::ChainRequest::new(&from_info.address, &id, claim, record.value());
        let answer: swaps::ChainSwap = match self.post(provider, swaps::CHAIN_SWAPS, &request).await
        {
            Ok(answer) => answer,
            // Refused before the provider took it.
            Err(Error::Refused { problem, .. }) => {
                self.fail_payout(hash, &format!("the provider declined: {problem}"))?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if answer.id.0 != id.as_bytes() {
            return Err(unavailable(provider, "it answered with another payout"));
        }
        // Where the provider says its coin is, to show before it is checked.
        let told = match (&answer.funding, &answer.txid) {
            (Some(funding), _) => Txid::from_str(&funding.txid)
                .ok()
                .map(|t| (t, Some(funding.height))),
            (None, Some(txid)) => Txid::from_str(txid).ok().map(|t| (t, None)),
            (None, None) => None,
        };
        if let Some(told) = told {
            self.told_coins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(*hash, told);
        }
        if let Theirs::Failed | Theirs::Refunded = answer.state {
            let reason = answer.reason.as_deref().unwrap_or("no reason given");
            self.fail_payout(hash, &format!("the provider gave the payout up: {reason}"))?;
            return Ok(None);
        }
        if answer.value < record.value() {
            let reason = format!(
                "the provider locks a coin of {} of {}",
                answer.value,
                record.value()
            );
            self.fail_payout(hash, &reason)?;
            return Ok(None);
        }
        let refund = answer.refund().map_err(|p| unavailable(provider, p))?;
        XOnlyPublicKey::from_slice(&refund)
            .map_err(|_| unavailable(provider, "its refund key is no key"))?;
        // A time, not a height, under OP_CHECKLOCKTIMEVERIFY: refundable at
        // once.
        if answer.timeout >= LOCK_TIME_THRESHOLD || answer.timeout > ours.expiry {
            let reason = format!(
                "the provider's coin times out at {}, not a block before its lock expires at {}",
                answer.timeout, ours.expiry
            );
            self.fail_payout(hash, &reason)?;
            return Ok(None);
        }
        // Its first terms stand: a coin on others is not taken.
        self.store.write(|tx| {
            tx.execute(
                "UPDATE payouts SET refund_key = ?2, timeout = ?3
                 WHERE hash = ?1 AND refund_key IS NULL",
                params![hash, refund, answer.timeout],
            )?;
            Ok(())
        })?;
        let hub_height = from_info.locks.map_or(0, |l| l.height);
        match answer.funding {
            Some(funding) => Ok(Some((funding, hub_height))),
            None => Err(Error::Pending {
                id: hex::encode(hash),
                reason: "the provider's coin is not confirmed yet".into(),
            }),
        }
    }

    /// Claim the provider's coin, once found where it says on the wallet's
    /// own view of the chain: confirmed, worth what was asked, with room
    /// left before its timeout, at the higher of the wallet's tip and
    /// `hub_height`, its lock's hub's: a view behind the chain sees more
    /// room than there is. Recorded with the claim, before anything is
    /// broadcast.
    async fn claim_payout(
        &self,
        hash: &[u8; 32],
        record: &Record,
        (hint, hub_height): (&licash_api::Funding, u32),
        chain: &impl Blocks,
    ) -> Result<(), Error> {
        let provider = &record.provider;
        let pending = |reason: String| Error::Pending {
            id: hex::encode(hash),
            reason,
        };
        let htlc = record
            .htlc()?
            .ok_or_else(|| invalid("a payout on no terms"))?;
        let txid = Txid::from_str(&hint.txid)
            .map_err(|_| unavailable(provider, "its coin's txid is none"))?;
        let tip = chain.height().await.map_err(Error::Chain)?;
        if hint.height > tip {
            let reason = format!(
                "the provider's coin is in block {}, past the tip {tip}",
                hint.height
            );
            return Err(pending(reason));
        }
        let block = block_at(chain, hint.height).await?;
        let output = block
            .txdata
            .iter()
            .find(|tx| tx.compute_txid() == txid)
            .and_then(|tx| tx.output.get(hint.vout as usize))
            .filter(|o| o.script_pubkey == htlc.script_pubkey());
        let Some(output) = output else {
            let reason = format!(
                "the provider's coin is not in block {} as it says",
                hint.height
            );
            return Err(pending(reason));
        };
        let value = output.value.to_sat();
        if value < record.value() {
            let reason = format!("the provider's coin is worth {value} of {}", record.value());
            return self.fail_payout(hash, &reason);
        }
        let (depth, need) = (tip + 1 - hint.height, payout_depth(self.network()));
        if depth < need {
            let reason = format!("the provider's coin has {depth} of {need} confirmations");
            return Err(pending(reason));
        }
        let at = tip.max(hub_height);
        if htlc.timeout >= LOCK_TIME_THRESHOLD || htlc.timeout < at.saturating_add(CLAIM_ROOM) {
            let reason = format!(
                "too late to claim the provider's coin: it times out at block {}, \
                 the chain is at {at}",
                htlc.timeout
            );
            return self.fail_payout(hash, &reason);
        }
        let funding = OutPoint {
            txid,
            vout: hint.vout,
        };
        let claim = htlc
            .claim_exact(
                funding,
                output.value,
                record.script.clone(),
                Amount::from_sat(record.amount),
                &record.claim_key()?,
                &record.preimage,
            )
            .map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "UPDATE payouts SET funding_txid = ?2, funding_vout = ?3, funding_height = ?4,
                    value = ?5, claim_tx = ?6, revealed_at = ?7, scanned = ?4
                 WHERE hash = ?1 AND claim_tx IS NULL AND failed IS NULL",
                params![
                    hash,
                    txid.to_byte_array(),
                    hint.vout,
                    hint.height,
                    sql(value),
                    serialize(&claim),
                    now()
                ],
            )?;
            Ok(())
        })?;
        tracing::info!(
            hash = %hex::encode(hash), claim = %claim.compute_txid(),
            "claiming the provider's coin"
        );
        Ok(())
    }

    /// Follow the claim of the payout `hash` until it confirms at depth, or
    /// another spend of the coin, the provider's refund, does.
    async fn watch_claim(
        &self,
        hash: &[u8; 32],
        record: &Record,
        chain: &impl Blocks,
    ) -> Result<(), Error> {
        let claim: Transaction =
            deserialize(record.claim.as_deref().unwrap_or_default()).map_err(invalid)?;
        let (funding, funding_height, _) = record
            .funding
            .ok_or_else(|| invalid("a claim of no coin"))?;
        let htlc = record
            .htlc()?
            .ok_or_else(|| invalid("a claim on no terms"))?;
        let script = htlc.script_pubkey();
        let tip = chain.height().await.map_err(Error::Chain)?;
        let scanned = record.scanned.unwrap_or(funding_height);
        let first = scanned.saturating_sub(REORG).max(funding_height) + 1;
        // The first spend of the coin, and whether it is a claim.
        let spent = first_spend(chain, funding, &script, first..=tip)
            .await?
            .map(|(height, spend)| {
                let claimed = spend.compute_txid() == claim.compute_txid()
                    || htlc.preimage_in(&spend, funding).is_some();
                (height, claimed)
            });
        let need = payout_depth(self.network());
        let (scanned, reason) = match spent {
            Some((height, false)) if tip + 1 - height >= need => {
                return self.lose_payout(hash, "the provider took its coin back first");
            }
            // Its claim broadcast still, should the spend leave the chain.
            Some((height, false)) => (
                height - 1,
                format!(
                    "the provider's coin was spent otherwise, {} of {need} confirmations",
                    tip + 1 - height
                ),
            ),
            Some((height, true)) if tip + 1 - height >= need => {
                self.store.write(|tx| {
                    tx.execute(
                        "UPDATE payouts SET spent_height = ?2, scanned = ?3, done_at = ?4
                         WHERE hash = ?1 AND done_at IS NULL AND failed IS NULL",
                        params![hash, height, tip, now()],
                    )?;
                    Ok(())
                })?;
                tracing::info!(hash = %hex::encode(hash), height, "paid out");
                return Ok(());
            }
            // Looked for again next time, to count its confirmations.
            Some((height, true)) => (
                height - 1,
                format!("its claim has {} of {need} confirmations", tip + 1 - height),
            ),
            None => (tip, "its claim is not confirmed yet".to_string()),
        };
        self.store.write(|tx| {
            tx.execute(
                "UPDATE payouts SET scanned = ?2, spent_height = ?3 WHERE hash = ?1",
                params![hash, scanned, spent.map(|(height, _)| height)],
            )?;
            Ok(())
        })?;
        Err(Error::Pending {
            id: hex::encode(hash),
            reason,
        })
    }

    /// Give the payout of `hash` up, unless its claim was made: nothing
    /// was revealed.
    fn fail_payout(&self, hash: &[u8; 32], reason: &str) -> Result<(), Error> {
        tracing::warn!(hash = %hex::encode(hash), reason, "payout failed");
        self.store.write(|tx| {
            tx.execute(
                "UPDATE payouts SET failed = ?2
                 WHERE hash = ?1 AND done_at IS NULL AND failed IS NULL AND claim_tx IS NULL",
                params![hash, reason],
            )?;
            Ok(())
        })
    }

    /// Record the payout of `hash` lost, its claim made: the provider's
    /// refund of its coin confirmed first.
    fn lose_payout(&self, hash: &[u8; 32], reason: &str) -> Result<(), Error> {
        tracing::warn!(hash = %hex::encode(hash), reason, "payout lost");
        self.store.write(|tx| {
            tx.execute(
                "UPDATE payouts SET failed = ?2
                 WHERE hash = ?1 AND done_at IS NULL AND failed IS NULL",
                params![hash, reason],
            )?;
            Ok(())
        })
    }

    fn payout_row(&self, hash: &[u8; 32]) -> Result<Record, Error> {
        let record = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT p.provider, p.preimage, p.claim_key, p.from_hub, r.url, p.script,
                    p.amount, p.budget, p.refund_key, p.timeout, p.funding_txid,
                    p.funding_vout, p.funding_height, p.value, p.claim_tx, p.scanned,
                    p.done_at IS NULL AND p.failed IS NULL
                 FROM payouts p JOIN reach r ON r.hub = p.from_hub WHERE p.hash = ?1",
                [hash],
                |r| {
                    let refund = match (r.get(8)?, r.get(9)?) {
                        (Some(key), Some(timeout)) => Some((key, timeout)),
                        _ => None,
                    };
                    let funding = match (r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?) {
                        (Some(txid), Some(vout), Some(height), Some(value)) => {
                            let txid = Txid::from_byte_array(txid);
                            let value: i64 = value;
                            Some((OutPoint { txid, vout }, height, value as u64))
                        }
                        _ => None,
                    };
                    Ok(Record {
                        provider: r.get(0)?,
                        preimage: r.get(1)?,
                        claim_key: r.get(2)?,
                        from: (r.get(3)?, r.get(4)?),
                        script: ScriptBuf::from_bytes(r.get(5)?),
                        amount: r.get::<_, i64>(6)? as u64,
                        budget: r.get::<_, i64>(7)? as u64,
                        refund,
                        funding,
                        claim: r.get(14)?,
                        scanned: r.get(15)?,
                        open: r.get(16)?,
                    })
                },
            )
            .optional()?)
        })?;
        record.ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))
    }

    /// The output script of `to`, an address on the wallet's network, and
    /// the address as shown.
    fn payout_payee(&self, to: &str) -> Result<(ScriptBuf, String), Error> {
        let address = bitcoin::Address::from_str(to.trim())
            .map_err(|e| Error::Invalid(format!("not an address on Bitcoin: {e}")))?;
        let network = self.network();
        let address = address
            .require_network(licash_onchain::network(network))
            .map_err(|_| Error::Invalid(format!("an address not on {network}")))?;
        Ok((address.script_pubkey(), address.to_string()))
    }
}

/// What a payout is asked on, to judge a provider's offer by.
struct Terms<'a> {
    provider: &'a str,
    from: (&'a str, &'a Info),
    script: &'a ScriptBuf,
    amount: u64,
    max_fee: u64,
    rate: FeeRate,
}

impl Terms<'_> {
    /// The quote of the provider saying `info` of itself, unless it pays
    /// out nothing for a lock at `from`, not that much now, or asks more
    /// than allowed.
    fn quote(&self, info: swaps::Info) -> Result<PayoutQuote, Error> {
        let (provider, (from, from_info)) = (self.provider, self.from);
        let refuse = |why: String| Err(Error::Invalid(format!("provider {provider} {why}")));
        if self.amount == 0 {
            return Err(Error::Invalid("nothing to pay out".into()));
        }
        let Some(chain) = info.chain else {
            return refuse("pays nothing out on Bitcoin".into());
        };
        if info.hub(&from_info.address).is_none() {
            return refuse(format!("takes no lock at {from}"));
        }
        // A lock to a hub's own address is the hub's to settle.
        if info.address == from_info.address {
            return refuse("names a hub's address as its own".into());
        }
        let locks = from_info.locks.as_ref().ok_or_else(|| {
            Error::Invalid(format!("hub {from} follows no chain: it takes no locks"))
        })?;
        let latest = locks
            .latest_expiry(locks.min_depth.max(1))
            .map_err(|p| unavailable(from, p))?;
        if latest.saturating_sub(chain.margin) < locks.height.saturating_add(2 * CLAIM_ROOM) {
            return refuse(format!(
                "times its coin out {} blocks before a lock at {from} expires: too soon to claim it",
                chain.margin
            ));
        }
        let rate = chain
            .rate
            .max(MIN_RATE)
            .max(self.rate.to_sat_per_kwu().saturating_mul(4));
        let budget = claim_vsize(self.script, self.amount)?
            .saturating_mul(rate)
            .saturating_mul(HEADROOM)
            .div_ceil(1000);
        let value = self.amount.saturating_add(budget);
        if value < chain.min || value > chain.max {
            return refuse(format!(
                "pays out coins of {} to {} sat now, not {value}",
                chain.min, chain.max
            ));
        }
        let fee = chain.pays(value) - value;
        let lock_fee = from_info.fees.lock.due(value.saturating_add(fee));
        let quote = PayoutQuote {
            info,
            amount: self.amount,
            budget,
            fee,
            lock_fee,
        };
        if quote.costs() > self.max_fee {
            return refuse(format!(
                "asks {} sat to pay out {}, more than {}",
                quote.costs(),
                self.amount,
                self.max_fee
            ));
        }
        Ok(quote)
    }
}

/// The virtual size of a claim paying `amount` to `script`, refused if
/// that is dust: the same whatever its keys and coin.
fn claim_vsize(script: &ScriptBuf, amount: u64) -> Result<u64, Error> {
    let key = SecretKey::from_slice(&[1; 32]).expect("a secret key");
    let preimage = [0; 32];
    let htlc = Htlc {
        hash: lock_hash(&preimage),
        claim: x_only(&key),
        refund: x_only(&key),
        timeout: 0,
    };
    let to = script.clone();
    let paid = Amount::from_sat(amount);
    match htlc.claim_exact(
        OutPoint::null(),
        Amount::MAX_MONEY,
        to,
        paid,
        &key,
        &preimage,
    ) {
        Ok(claim) => Ok(claim.vsize() as u64),
        Err(deposit::Error::Exact { .. }) => Err(Error::Invalid(format!(
            "{amount} sat is too little to pay on Bitcoin"
        ))),
        Err(e) => Err(invalid(e)),
    }
}

/// The first spend of `coin`, paid to `script`, in the best chain's blocks
/// at `heights`, and the block it is in: blocks whose filter matches
/// `script` are fetched, and checked against their hashes.
pub async fn first_spend(
    chain: &impl Blocks,
    coin: OutPoint,
    script: &bitcoin::Script,
    heights: std::ops::RangeInclusive<u32>,
) -> Result<Option<(u32, Transaction)>, Error> {
    for height in heights {
        let block_hash = chain.hash(height).await.map_err(Error::Chain)?;
        let matched = match chain.filter(&block_hash).await.map_err(Error::Chain)? {
            None => true,
            Some(filter) => BlockFilter::new(&filter)
                .match_any(&block_hash, std::iter::once(script.as_bytes()))
                .map_err(|e| Error::Chain(format!("the filter of block {block_hash}: {e}")))?,
        };
        if !matched {
            continue;
        }
        let block = checked_block(chain, &block_hash).await?;
        let spend = block
            .txdata
            .into_iter()
            .find(|tx| tx.input.iter().any(|i| i.previous_output == coin));
        if let Some(spend) = spend {
            return Ok(Some((height, spend)));
        }
    }
    Ok(None)
}

/// The best chain's block at `height`, checked against its hash.
async fn block_at(chain: &impl Blocks, height: u32) -> Result<Block, Error> {
    let hash = chain.hash(height).await.map_err(Error::Chain)?;
    checked_block(chain, &hash).await
}

async fn checked_block(chain: &impl Blocks, hash: &bitcoin::BlockHash) -> Result<Block, Error> {
    let block = chain.block(hash).await.map_err(Error::Chain)?;
    if block.block_hash() != *hash || !block.check_merkle_root() {
        return Err(Error::Chain(format!(
            "block {hash} does not match its header"
        )));
    }
    Ok(block)
}

const PAYOUTS: &str = "SELECT p.hash, p.provider, r.url, p.address, p.amount, p.budget, p.fee,
    (SELECT l.id FROM locks l WHERE l.hash = p.hash AND l.hub = p.from_hub
        ORDER BY l.created_at DESC, l.rowid DESC LIMIT 1),
    p.funding_txid, p.funding_vout, p.claim_tx, p.done_at IS NOT NULL, p.failed, p.created_at
    FROM payouts p JOIN reach r ON r.hub = p.from_hub";

fn payout_of(r: &Row<'_>) -> rusqlite::Result<Payout> {
    let claim: Option<Vec<u8>> = r.get(10)?;
    let state = match (r.get::<_, bool>(11)?, r.get::<_, Option<String>>(12)?) {
        (_, Some(reason)) => PayoutState::Failed(reason),
        (true, None) => PayoutState::Done,
        (false, None) if claim.is_some() => PayoutState::Claimed,
        (false, None) => PayoutState::Open,
    };
    let funding = match (r.get::<_, Option<[u8; 32]>>(8)?, r.get(9)?) {
        (Some(txid), Some(vout)) => Some(OutPoint {
            txid: Txid::from_byte_array(txid),
            vout,
        }),
        _ => None,
    };
    let claim = claim
        .and_then(|tx| deserialize::<Transaction>(&tx).ok())
        .map(|tx| tx.compute_txid());
    Ok(Payout {
        hash: hex::encode(r.get::<_, [u8; 32]>(0)?),
        provider: r.get(1)?,
        from: r.get(2)?,
        address: r.get(3)?,
        amount: r.get::<_, i64>(4)? as u64,
        budget: r.get::<_, i64>(5)? as u64,
        fee: r.get::<_, i64>(6)? as u64,
        lock: r.get::<_, Option<[u8; 32]>>(7)?.map(hex::encode),
        funding,
        claim,
        state,
        created_at: r.get(13)?,
    })
}
