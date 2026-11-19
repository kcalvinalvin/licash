//! Deposits: coins paid on Bitcoin for a hub's notes ([`licash_api::Deposit`]).
//!
//! The wallet draws a preimage and a refund key, records them with the
//! deposit it asks for, then asks the hub for its terms, and checks that
//! the address answered is that of a taproot output ([`Htlc`]) the hub
//! claims with the preimage, and the wallet takes back with its refund key
//! after the timeout. Once a payment of exactly the amount answered
//! confirms at depth, the hub locks notes of its own to the wallet, which
//! claims them with the preimage, as when paid over Lightning: the hub
//! learns the preimage that claims the coins. A deposit whose notes are
//! never claimed returns to its payer after its timeout
//! ([`Wallet::refund_deposit`]).
//!
//! A deposit's secrets (its preimage, refund key and outputs' ephemeral
//! key) derive from the seed, its hub's address and its number there, the
//! next of each hub's: a wallet of the same seed that lost its file finds
//! the deposits it asked for there by their hashes, the hub answering what
//! was asked and its terms, and takes them on, until [`GAP`] numbers in a
//! row are unknown there ([`Wallet::find_deposits`]).

use bitcoin::{
    hashes::Hash,
    secp256k1::{Secp256k1, SecretKey, XOnlyPublicKey},
    Amount, FeeRate, OutPoint, ScriptBuf, Transaction, Txid,
};
use licash::{
    address::Ephemeral,
    claim::Branch,
    crypto::{scalar_from_hash, tagged_hash},
    spend::{lock_hash, ProposalId},
};
use licash_api::{
    CloseTerms, Code, DepositStatus, Depositing, Hex, Info, Receive, ReceiveState,
};
use licash_onchain::deposit::Htlc;
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Row};

use crate::{hub_url, invalid, now, sql, unavailable, Error, Transport, Wallet};

/// Blocks a hub's tip may move between its info and its answer: the
/// timeout it answers may be that much later than its info promises.
const TIP_MARGIN: u32 = 6;

/// Numbers in a row a hub knows no deposit under, after which a wallet
/// looking for its deposits there stops: asks that never reached it
/// leave gaps.
pub const GAP: u32 = 20;

/// The secrets of the deposit numbered `number` at the hub of `address`,
/// of the wallet whose deposit seed is `seed`: its preimage, refund key and
/// outputs' ephemeral key.
fn secrets(
    seed: &[u8; 32],
    address: &[u8],
    number: u32,
) -> Result<([u8; 32], SecretKey, Ephemeral), Error> {
    let part = |tag| tagged_hash(tag, &[seed, address, &number.to_be_bytes()]);
    let preimage = part("licash/deposit/preimage");
    let refund = SecretKey::from_slice(&part("licash/deposit/refund")).map_err(invalid)?;
    let ephemeral = scalar_from_hash(&part("licash/deposit/ephemeral"));
    let ephemeral = Ephemeral::from_scalar(ephemeral).map_err(invalid)?;
    Ok((preimage, refund, ephemeral))
}

/// A deposit asked for, and where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    /// Its hash, in hex.
    pub hash: String,
    pub hub: String,
    /// The notes it locks to the wallet; the payment is worth the hub's
    /// fee on top.
    pub amount: u64,
    /// What to pay, once the hub answered.
    pub terms: Option<DepositTerms>,
    /// The payment, once its hub saw it confirm: `txid:vout` and height.
    pub funding: Option<(OutPoint, u32)>,
    /// The lock of the hub's notes to the wallet, once paid: the id of
    /// their receipt (in hex).
    pub lock: Option<String>,
    pub state: DepositState,
    pub created_at: i64,
}

/// A deposit that a contract's default close pays her reserve to
/// ([`Wallet::close_deposit`]): its script, for her Base to commit to, and
/// the fee the hub keeps of the reserve if it swaps it for notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseDeposit {
    /// Its hash, in hex.
    pub hash: String,
    pub script: ScriptBuf,
    pub fee: u64,
}

/// Pay exactly `pays` to `address`, confirmed by block `fund_by`: the
/// coins are the payer's again after block `timeout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositTerms {
    pub address: String,
    pub pays: u64,
    pub fund_by: u32,
    pub timeout: u32,
}

/// Where a deposit stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DepositState {
    /// Waiting for its hub's terms, its payment, its depth or the claim.
    Open,
    /// Its notes kept.
    Received,
    /// Refused or canceled, its notes never claimed, and why: a payment
    /// made returns after the timeout ([`Wallet::refund_deposit`]).
    Canceled(String),
    /// Taken back by the refund of this id.
    Refunded(Txid),
}

fn hash_id(hash: &str) -> Result<[u8; 32], Error> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hash, &mut bytes)
        .map_err(|_| Error::Invalid(format!("not a deposit's hash: {hash}")))?;
    Ok(bytes)
}

/// A secret key drawn at random.
pub(crate) fn random_key() -> SecretKey {
    loop {
        let mut bytes = [0; 32];
        OsRng.fill_bytes(&mut bytes);
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

pub(crate) fn x_only(key: &SecretKey) -> XOnlyPublicKey {
    key.x_only_public_key(&Secp256k1::new()).0
}

impl<T: Transport> Wallet<T> {
    /// Ask the hub at `url` for a deposit locking notes worth `amount` to
    /// this wallet: where to pay on Bitcoin, and how much, its fee on top.
    /// [`Wallet::follow_deposit`] claims the notes once the payment
    /// confirmed.
    pub async fn deposit(&self, url: &str, amount: u64) -> Result<Deposit, Error> {
        let url = hub_url(url)?;
        self.not_visited(&url)?;
        let (hub, info) = self.refresh(&url).await?;
        let deposits = info
            .deposits
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("hub {url} takes no deposits")))?;
        if amount < deposits.min || amount > deposits.max {
            return Err(Error::Invalid(format!(
                "hub {url} takes deposits of {} to {}",
                deposits.min, deposits.max
            )));
        }
        let number = self.deposit_number(hub, &url, &info).await?;
        let asked = self.draw_deposit(&url, &info, amount, number, None)?;
        self.store.write(|tx| asked.keep(tx, hub))?;
        tracing::info!(hash = %hex::encode(asked.hash), %url, amount, "asking for a deposit");
        self.advance_deposit(&asked.hash).await
    }

    /// Ask the hub at `url` for a deposit paid exactly `pays` on Bitcoin,
    /// its fee in it: the notes it locks are what the fee leaves. To pay at
    /// once, in a transaction made with its address (a contract's funding
    /// paying its change in, say): recorded only with the terms its hub
    /// answered, checked. Refused if no notes within the hub's limits come,
    /// with its fee, to `pays` exactly.
    pub async fn deposit_paying(&self, url: &str, pays: u64) -> Result<Deposit, Error> {
        let url = hub_url(url)?;
        self.not_visited(&url)?;
        let (hub, info) = self.refresh(&url).await?;
        let deposits = info
            .deposits
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("hub {url} takes no deposits")))?;
        let amount = deposits
            .paid_by(pays)
            .filter(|a| (deposits.min..=deposits.max).contains(a))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "hub {url} takes no deposit paid {pays} sat exactly, its fee in it"
                ))
            })?;
        self.deposit_answered(hub, &info, amount, None).await
    }

    /// Ask the hub at `url` for the deposit that a contract's default close
    /// pays her reserve to, `pays` sat, to time out at block `timeout`
    /// where the contract's Base puts it: the notes it locks the most that
    /// payment covers, the rest, `fee`, the hub's if it swaps. Recorded only
    /// with the terms its hub answered, checked. The wallet asks for its
    /// lock, and claims the notes, once the close is paid
    /// ([`Wallet::follow_deposit`]); else takes the coins back after the
    /// timeout ([`Wallet::deposits_to_refund`]).
    pub async fn close_deposit(
        &self,
        url: &str,
        pays: u64,
        timeout: u32,
    ) -> Result<CloseDeposit, Error> {
        let url = hub_url(url)?;
        self.not_visited(&url)?;
        let (hub, info) = self.refresh(&url).await?;
        let deposits = info
            .deposits
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("hub {url} takes no deposits")))?;
        let amount = deposits
            .most_for(pays)
            .filter(|a| (deposits.min..=deposits.max).contains(a))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "hub {url} takes no deposit of the notes {pays} sat pays for"
                ))
            })?;
        let close = CloseTerms { pays, timeout };
        let deposit = self.deposit_answered(hub, &info, amount, Some(close)).await?;
        let network = licash_onchain::network(self.network());
        let script = deposit
            .terms
            .as_ref()
            .and_then(|t| {
                t.address
                    .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
                    .ok()
                    .and_then(|a| a.require_network(network).ok())
            })
            .map(|a| a.script_pubkey())
            .ok_or_else(|| Error::Invalid(format!("hub {url} named no address to pay")))?;
        Ok(CloseDeposit {
            hash: deposit.hash,
            script,
            fee: pays - amount,
        })
    }

    /// Give up the deposit of `hash`, answered but never to be paid: the
    /// transaction to pay it given up unmade, `why`. Refused once its hub
    /// saw a payment to it, or the deposit is over. One made all the same
    /// returns after its timeout ([`Wallet::deposits_to_refund`]).
    pub fn give_up_deposit(&self, hash: &str, why: &str) -> Result<(), Error> {
        let key = hash_id(hash)?;
        let given_up = self.store.write(|tx| {
            Ok(tx.execute(
                "UPDATE deposits SET canceled = ?2
                 WHERE hash = ?1 AND lock IS NULL
                    AND NOT EXISTS (SELECT 1 FROM deposit_fundings f WHERE f.hash = deposits.hash)
                    AND done_at IS NULL AND canceled IS NULL AND refund IS NULL",
                params![key, why],
            )?)
        })?;
        if given_up == 0 {
            self.deposit_record(&key)?;
            return Err(Error::Invalid(format!(
                "deposit {hash} is paid or over: not given up"
            )));
        }
        tracing::info!(%hash, why, "deposit given up unpaid");
        Ok(())
    }

    /// Ask the hub `hub`, which says `info` of itself, for a deposit of
    /// notes worth `amount`, to pay at once: recorded only with the terms
    /// it answered, checked, as none pays a deposit its hub never answered.
    pub(crate) async fn deposit_answered(
        &self,
        hub: i64,
        info: &Info,
        amount: u64,
        close: Option<CloseTerms>,
    ) -> Result<Deposit, Error> {
        let url = self.reached(hub)?;
        let number = self.deposit_number(hub, &url, info).await?;
        let asked = self.draw_deposit(&url, info, amount, number, close)?;
        let hash = asked.hash;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "asking for a deposit");
        let terms: Depositing = self.post(&url, licash_api::DEPOSIT, &asked.body).await?;
        let htlc = self
            .check_terms(&url, &hash, &asked.refund_key, amount, close, &terms)
            .await?;
        let network = licash_onchain::network(self.network());
        self.store.write(|tx| {
            asked.keep(tx, hub)?;
            keep_terms(tx, &hash, &htlc, &terms, network)
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, address = %terms.address, pays = terms.amount, "deposit terms");
        self.deposit_record(&hash)
    }

    /// Give up the deposit of `hash` its hub never answered: no address
    /// was named to pay, so nothing paid it. Refused once answered.
    pub fn abandon_deposit(&self, hash: &str) -> Result<(), Error> {
        let key = hash_id(hash)?;
        let given_up = self.store.write(|tx| {
            Ok(tx.execute(
                "UPDATE deposits SET canceled = 'abandoned before its hub answered'
                 WHERE hash = ?1 AND done_at IS NULL AND canceled IS NULL
                    AND NOT EXISTS (SELECT 1 FROM deposit_terms t WHERE t.hash = deposits.hash)",
                [key],
            )?)
        })?;
        if given_up == 0 {
            self.deposit_record(&key)?;
            return Err(Error::Invalid(format!(
                "deposit {hash} is answered or over: an address to pay was named"
            )));
        }
        tracing::info!(%hash, "deposit abandoned before its hub answered");
        Ok(())
    }

    /// A deposit of notes worth `amount` to ask the hub at `url`, which
    /// says `info` of itself, for, numbered `number` there.
    fn draw_deposit(
        &self,
        url: &str,
        info: &Info,
        amount: u64,
        number: u32,
        close: Option<CloseTerms>,
    ) -> Result<Asked, Error> {
        let keysets = info.keysets().map_err(|p| unavailable(url, p))?;
        let address = info.address.to_bytes();
        let (preimage, refund_key, ephemeral) = secrets(&self.keys.deposit, &address, number)?;
        let me = self.address();
        let (keyset, outputs) = keysets
            .iter()
            .find_map(|k| {
                let outputs = crate::outputs(k, &ephemeral, Branch::Pay, &[(&me, amount)]);
                Some((k, outputs.ok()?))
            })
            .ok_or_else(|| Error::Invalid(format!("hub {url} has no keyset making {amount}")))?;
        let hash = lock_hash(&preimage);
        let body = licash_api::Deposit {
            receive: Receive::new(keyset.id(), &ephemeral.public(), &outputs, &hash),
            refund: Hex(x_only(&refund_key).serialize().to_vec()),
            close,
        };
        Ok(Asked {
            hash,
            preimage,
            refund_key,
            amount,
            body,
        })
    }
}

impl<T: Transport> Wallet<T> {
    /// The next number of a deposit at the hub `hub`, reached at `url`,
    /// saying `info` of itself, taken whether or not the deposit is then
    /// asked: once the deposits of the wallet's seed were looked for there.
    async fn deposit_number(&self, hub: i64, url: &str, info: &Info) -> Result<u32, Error> {
        if self.deposit_next(hub)?.is_none() {
            self.find_deposits_at(hub, url, info).await?;
        }
        self.store.write(|tx| {
            let taken: Option<i64> = tx
                .query_row(
                    "UPDATE hubs SET deposit_next = deposit_next + 1
                     WHERE id = ?1 AND deposit_next IS NOT NULL RETURNING deposit_next - 1",
                    [hub],
                    |r| r.get(0),
                )
                .optional()?;
            taken
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| Error::Invalid(format!("hub {url}: no deposit number left")))
        })
    }

    /// The next number of a deposit at the hub `hub`: none until its
    /// deposits were looked for there.
    pub(crate) fn deposit_next(&self, hub: i64) -> Result<Option<u32>, Error> {
        self.store.read(|c| {
            Ok(
                c.query_row("SELECT deposit_next FROM hubs WHERE id = ?1", [hub], |r| {
                    r.get(0)
                })?,
            )
        })
    }

    /// Look for the deposits the wallet's seed asked of the hub at `url`,
    /// by number from the first, until `GAP` in a row are unknown there:
    /// those it kept no record of, as a wallet restored from its seed,
    /// kept as the hub answers them, checked to be the wallet's, then
    /// taken on as any other ([`Wallet::follow_deposit`], refunded if
    /// paid and never received). Those found; the next number after them
    /// from then on.
    pub async fn find_deposits(&self, url: &str) -> Result<Vec<Deposit>, Error> {
        let url = hub_url(url)?;
        let (hub, info) = self.refresh(&url).await?;
        self.find_deposits_at(hub, &url, &info).await
    }

    pub(crate) async fn find_deposits_at(
        &self,
        hub: i64,
        url: &str,
        info: &Info,
    ) -> Result<Vec<Deposit>, Error> {
        let address = info.address.to_bytes();
        let (mut number, mut unknown, mut next) = (0u32, 0u32, 0u32);
        let mut found = Vec::new();
        while unknown < GAP {
            let (preimage, refund, ephemeral) = secrets(&self.keys.deposit, &address, number)?;
            let hash = lock_hash(&preimage);
            let kept = self.store.read(|c| {
                let kept =
                    c.query_row("SELECT 1 FROM deposits WHERE hash = ?1", [hash], |_| Ok(()));
                Ok(kept.optional()?.is_some())
            })?;
            let known = kept
                || match self.get(url, &licash_api::deposit_state(&hash)).await {
                    Ok(status) => {
                        let drawn = (preimage, refund, ephemeral.public());
                        match self.found_deposit(hub, url, drawn, status) {
                            Ok(()) => found.push(self.deposit_record(&hash)?),
                            Err(Error::Invalid(reason)) => {
                                tracing::warn!(hash = %hex::encode(hash), %url, %reason, "a deposit found not this wallet's");
                            }
                            Err(e) => return Err(e),
                        }
                        true
                    }
                    Err(Error::Refused { problem, .. }) if problem.code == Code::UnknownLock => {
                        false
                    }
                    Err(e) => return Err(e),
                };
            match known {
                true => (unknown, next) = (0, number.saturating_add(1)),
                false => unknown += 1,
            }
            number = number
                .checked_add(1)
                .ok_or_else(|| Error::Invalid(format!("hub {url}: no deposit number left")))?;
        }
        self.store.write(|tx| {
            tx.execute(
                "UPDATE hubs SET deposit_next = MAX(COALESCE(deposit_next, 0), ?2) WHERE id = ?1",
                params![hub, next],
            )?;
            Ok(())
        })?;
        tracing::info!(%url, found = found.len(), next, "deposits looked for");
        Ok(found)
    }

    /// Keep the deposit of this wallet's seed `drawn` (its preimage,
    /// refund key and outputs' ephemeral key) as the hub `hub` at `url`
    /// says it stands, `status`: what was asked and the terms answered
    /// checked to be of its hash and keys. Refused as invalid otherwise.
    fn found_deposit(
        &self,
        hub: i64,
        url: &str,
        drawn: ([u8; 32], SecretKey, licash::crypto::Point),
        status: DepositStatus,
    ) -> Result<(), Error> {
        let (preimage, refund_key, mine) = drawn;
        let wrong = |why: &str| Error::Invalid(format!("hub {url} found a deposit {why}"));
        let (Some(request), Some(terms)) = (status.request, status.terms) else {
            return Err(wrong("but not what was asked"));
        };
        let (_, ephemeral, hash) = request.receive.terms().map_err(|p| unavailable(url, p))?;
        let refund = Hex(x_only(&refund_key).serialize().to_vec());
        if hash != lock_hash(&preimage) || ephemeral != mine || request.refund != refund {
            return Err(wrong("asked by another"));
        }
        let outputs = request.receive.outputs().map_err(|p| unavailable(url, p))?;
        let amount = outputs
            .iter()
            .try_fold(0u64, |sum, o| sum.checked_add(o.denomination()))
            .ok_or_else(|| wrong("of outputs overflowing"))?;
        let claim =
            XOnlyPublicKey::from_slice(&terms.claim.0).map_err(|_| wrong("claimed by no key"))?;
        let htlc = Htlc {
            hash,
            claim,
            refund: x_only(&refund_key),
            timeout: terms.timeout,
        };
        let network = licash_onchain::network(self.network());
        if htlc.address(network).to_string() != terms.address {
            return Err(wrong(&format!("at {}, not its terms", terms.address)));
        }
        let found = Asked {
            hash,
            preimage,
            refund_key,
            amount,
            body: request,
        };
        self.store.write(|tx| {
            found.keep(tx, hub)?;
            keep_terms(tx, &hash, &htlc, &terms, network)
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "deposit found");
        Ok(())
    }
}

/// A deposit drawn: what only the wallet knows of it, and what it asks.
struct Asked {
    hash: [u8; 32],
    preimage: [u8; 32],
    refund_key: SecretKey,
    amount: u64,
    body: licash_api::Deposit,
}

impl Asked {
    /// Record it, as asked of the hub `hub`.
    fn keep(&self, tx: &rusqlite::Transaction, hub: i64) -> Result<(), Error> {
        let body = serde_json::to_string(&self.body).map_err(invalid)?;
        tx.execute(
            "INSERT INTO deposits (hash, hub, preimage, refund_key, amount, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.hash,
                hub,
                self.preimage,
                self.refund_key.secret_bytes(),
                sql(self.amount)?,
                body,
                now()
            ],
        )?;
        Ok(())
    }
}

/// Record `terms`, checked to be those of `htlc`, as the hub's answer to
/// the deposit of `hash`.
fn keep_terms(
    tx: &rusqlite::Transaction,
    hash: &[u8; 32],
    htlc: &Htlc,
    terms: &Depositing,
    network: bitcoin::Network,
) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO deposit_terms (hash, claim_key, timeout_height, pays, fund_by_height, address)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6
         WHERE EXISTS (SELECT 1 FROM deposits WHERE hash = ?1 AND canceled IS NULL)
         ON CONFLICT (hash) DO UPDATE SET
            claim_key = excluded.claim_key, timeout_height = excluded.timeout_height,
            pays = excluded.pays, fund_by_height = excluded.fund_by_height,
            address = excluded.address",
        params![
            hash,
            htlc.claim.serialize(),
            htlc.timeout,
            sql(terms.amount)?,
            terms.fund_by,
            htlc.address(network).to_string(),
        ],
    )?;
    Ok(())
}

impl<T: Transport> Wallet<T> {
    /// Take the deposit of `hash` on: learn its hub's terms, then claim the
    /// notes its hub locks to the wallet once it is paid. Where it stands.
    pub async fn follow_deposit(&self, hash: &str) -> Result<Deposit, Error> {
        self.advance_deposit(&hash_id(hash)?).await
    }

    pub(crate) async fn advance_deposit(&self, hash: &[u8; 32]) -> Result<Deposit, Error> {
        let pending = |e: Error| match e {
            Error::Pending { .. } | Error::UnknownPayment(_) => e,
            e => Error::Pending {
                id: hex::encode(hash),
                reason: e.to_string(),
            },
        };
        self.advance_deposit_from(hash).await.map_err(pending)?;
        self.deposit_record(hash)
    }
}

impl<T: Transport> Wallet<T> {
    async fn advance_deposit_from(&self, hash: &[u8; 32]) -> Result<(), Error> {
        type Row = (String, [u8; 32], [u8; 32], i64, String, bool, bool);
        let row: Option<Row> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT r.url, d.preimage, d.refund_key, d.amount, d.body,
                    EXISTS (SELECT 1 FROM deposit_terms t WHERE t.hash = d.hash),
                    d.done_at IS NULL AND d.canceled IS NULL AND d.refund IS NULL
                 FROM deposits d JOIN reach r ON r.hub = d.hub WHERE d.hash = ?1",
                [hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let (url, preimage, refund_key, amount, body, answered, open) =
            row.ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))?;
        if !open {
            return Ok(());
        }
        // What was asked, which a deposit answered need not read again: of a
        // body it cannot, it is no contract's close.
        let parsed = serde_json::from_str::<licash_api::Deposit>(&body);
        if !answered {
            let body = parsed.as_ref().map_err(|e| invalid(e.to_string()))?;
            let asked = self
                .post::<Depositing>(&url, licash_api::DEPOSIT, body)
                .await;
            let terms = match asked {
                Ok(terms) => terms,
                // Refused before the hub named an address.
                Err(Error::Refused { problem, .. }) => {
                    return self.cancel_deposit(hash, &format!("refused: {problem}"));
                }
                Err(e) => return Err(e),
            };
            let refund = SecretKey::from_slice(&refund_key).map_err(invalid)?;
            let htlc = match self
                .check_terms(&url, hash, &refund, amount as u64, body.close, &terms)
                .await
            {
                Ok(htlc) => htlc,
                Err(Error::Invalid(reason)) => return self.cancel_deposit(hash, &reason),
                Err(e) => return Err(e),
            };
            let network = licash_onchain::network(self.network());
            self.store
                .write(|tx| keep_terms(tx, hash, &htlc, &terms, network))?;
            let (address, pays) = (&terms.address, terms.amount);
            tracing::info!(hash = %hex::encode(hash), %url, %address, pays, "deposit terms");
        }
        let status: DepositStatus = self.get(&url, &licash_api::deposit_state(hash)).await?;
        if let Some(funding) = &status.funding {
            let txid: Txid = funding.txid.parse().map_err(|e| unavailable(&url, e))?;
            self.store.write(|tx| {
                tx.execute(
                    "INSERT INTO deposit_fundings (hash, txid, vout, height) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (hash) DO UPDATE SET
                        txid = excluded.txid, vout = excluded.vout, height = excluded.height",
                    params![hash, txid.to_byte_array(), funding.vout, funding.height],
                )?;
                Ok(())
            })?;
        }
        let lock = status.status.lock().map_err(|p| unavailable(&url, p))?;
        let lock: ProposalId = match (status.status.state, lock) {
            (ReceiveState::Open, _) => {
                // A contract's close is locked when the wallet asks, now it
                // is back and its payment seen, not at the block it was
                // paid in.
                let close = parsed.as_ref().is_ok_and(|body| body.close.is_some());
                if close && status.funding.is_some() {
                    self.post::<DepositStatus>(&url, &licash_api::deposit_lock(hash), &())
                        .await?;
                }
                return Ok(());
            }
            (ReceiveState::Canceled, _) => {
                return self.cancel_deposit(hash, "canceled by its hub");
            }
            (_, None) => return Err(unavailable(&url, "a deposit locked to no lock")),
            (_, Some(lock)) => lock,
        };
        self.store.write(|tx| {
            tx.execute(
                "UPDATE deposits SET lock = ?2 WHERE hash = ?1",
                params![hash, lock.as_bytes()],
            )?;
            Ok(())
        })?;
        // Its deposit's coin buys the notes: revealed only while the
        // wallet can still activate the lock, should the hub keep them.
        match self
            .claim_protected(
                &url,
                &hex::encode(lock.as_bytes()),
                &preimage,
                amount as u64,
            )
            .await
        {
            Ok(_) | Err(Error::AlreadyReceived) => {}
            // Refunded, too late to claim, or not a lock to this wallet.
            Err(Error::Invalid(reason)) => return self.cancel_deposit(hash, &reason),
            Err(Error::PaysNothing) => {
                return self.cancel_deposit(hash, "its lock pays this wallet nothing");
            }
            Err(e) => return Err(e),
        }
        self.store.write(|tx| {
            tx.execute(
                "UPDATE deposits SET done_at = ?2 WHERE hash = ?1 AND done_at IS NULL",
                params![hash, now()],
            )?;
            Ok(())
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "deposit received");
        Ok(())
    }
}

impl<T: Transport> Wallet<T> {
    /// The deposit `terms` answers for `hash`, checked against what the
    /// hub at `url` says of its deposits: its address that of the hub's
    /// claim and the wallet's `refund`, its amount the notes and the fee,
    /// its timeout no later than the hub promises. A hub answering other
    /// terms gets nothing paid to them: the deposit is canceled unpaid.
    async fn check_terms(
        &self,
        url: &str,
        hash: &[u8; 32],
        refund: &SecretKey,
        amount: u64,
        close: Option<CloseTerms>,
        terms: &Depositing,
    ) -> Result<Htlc, Error> {
        let (_, info) = self.refresh(url).await?;
        let wrong = |why: String| Error::Invalid(format!("hub {url} answered a deposit {why}"));
        let deposits = info
            .deposits
            .ok_or_else(|| wrong("but takes none".into()))?;
        let height = info.locks.as_ref().map(|l| l.height);
        let height = height.ok_or_else(|| wrong("but follows no chain".into()))?;
        let claim = XOnlyPublicKey::from_slice(&terms.claim.0)
            .map_err(|_| wrong("claimed by no key".into()))?;
        let htlc = Htlc {
            hash: *hash,
            claim,
            refund: x_only(refund),
            timeout: terms.timeout,
        };
        let network = licash_onchain::network(self.network());
        let address = terms
            .address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .ok()
            .and_then(|a| a.require_network(network).ok());
        if address.is_none_or(|a| a.script_pubkey() != htlc.script_pubkey()) {
            return Err(wrong(format!("at {}, not its terms", terms.address)));
        }
        match close {
            // Her contract's reserve exactly, for the most notes it covers,
            // timing out where her Base says.
            Some(close) => {
                if terms.amount != close.pays
                    || deposits.most_for(close.pays) != Some(amount)
                    || terms.timeout != close.timeout
                {
                    return Err(wrong(format!(
                        "of {} timing out at block {}, not as her contract's close",
                        terms.amount, terms.timeout
                    )));
                }
            }
            None => {
                if deposits.pays(amount) != Some(terms.amount) {
                    return Err(wrong(format!("of {} for {amount}", terms.amount)));
                }
                let latest = height
                    .saturating_add(deposits.timeout)
                    .saturating_add(TIP_MARGIN);
                if terms.timeout > latest
                    || terms.fund_by.saturating_add(deposits.depth) > terms.timeout
                {
                    return Err(wrong(format!(
                        "timing out at block {}, paid by {}",
                        terms.timeout, terms.fund_by
                    )));
                }
            }
        }
        Ok(htlc)
    }

    fn cancel_deposit(&self, hash: &[u8; 32], reason: &str) -> Result<(), Error> {
        tracing::warn!(hash = %hex::encode(hash), reason, "deposit canceled");
        self.store.write(|tx| {
            tx.execute(
                "UPDATE deposits SET canceled = ?2
                 WHERE hash = ?1 AND done_at IS NULL AND canceled IS NULL",
                params![hash, reason],
            )?;
            Ok(())
        })
    }

    /// Take back `funding`, the payment worth `value` of the deposit of
    /// `hash`, paying `to` all of it but the fee at `rate`: a transaction
    /// that confirms after the deposit's timeout, recorded as the
    /// deposit's refund. Again, at a higher rate, to replace it. Refused
    /// once its notes were received: the coins are its hub's.
    pub fn refund_deposit(
        &self,
        hash: &str,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        rate: FeeRate,
    ) -> Result<Transaction, Error> {
        let hash = hash_id(hash)?;
        type Row = ([u8; 32], Option<[u8; 32]>, Option<u32>, bool);
        let row: Option<Row> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT d.refund_key, t.claim_key, t.timeout_height, d.done_at IS NOT NULL
                 FROM deposits d LEFT JOIN deposit_terms t ON t.hash = d.hash
                 WHERE d.hash = ?1",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?)
        })?;
        let (refund_key, claim, timeout, received) =
            row.ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))?;
        if received {
            return Err(Error::Invalid(
                "its notes were received: the coins are its hub's".into(),
            ));
        }
        let (Some(claim), Some(timeout)) = (claim, timeout) else {
            return Err(Error::Invalid(
                "its hub named no address: nothing was paid to it".into(),
            ));
        };
        let secret = SecretKey::from_slice(&refund_key).map_err(invalid)?;
        let htlc = Htlc {
            hash,
            claim: XOnlyPublicKey::from_slice(&claim).map_err(invalid)?,
            refund: x_only(&secret),
            timeout,
        };
        let tx = htlc
            .refund_tx(funding, value, to, rate, &secret)
            .map_err(invalid)?;
        self.store.write(|t| {
            t.execute(
                "UPDATE deposits SET refund = ?2,
                     canceled = COALESCE(canceled, 'refunded by its owner')
                 WHERE hash = ?1",
                params![hash, bitcoin::consensus::serialize(&tx)],
            )?;
            Ok(())
        })?;
        let txid = tx.compute_txid();
        tracing::info!(hash = %hex::encode(hash), %txid, timeout, "deposit refund made");
        Ok(tx)
    }
}

impl<T: Transport> Wallet<T> {
    /// The deposits whose coins, if paid, may be taken back at `tip`
    /// ([`Wallet::refund_deposit`]): an address named, past their timeout,
    /// not taken back yet, and canceled, their notes never claimed, or
    /// still open with no lock to them ever seen, so never claimed (their
    /// hub gone, say): the preimage the hub would take the coins with was
    /// never sent, and no lock to them is made so late. Paid or not:
    /// [`Deposit::funding`] is the payment its hub saw, none for one it
    /// did not (made too late, say), which only its payer knows of.
    pub fn deposits_to_refund(&self, tip: u32) -> Result<Vec<Deposit>, Error> {
        let mut due = self.deposits()?;
        due.retain(|d| {
            let past = d.terms.as_ref().is_some_and(|t| tip >= t.timeout);
            let never_locked = d.state == DepositState::Open && d.lock.is_none();
            past && (matches!(d.state, DepositState::Canceled(_)) || never_locked)
        });
        Ok(due)
    }

    /// The refund made of the deposit of `hash`, if one was: to hand the
    /// network again until it confirms.
    pub fn deposit_refund(&self, hash: &str) -> Result<Option<Transaction>, Error> {
        let key = hash_id(hash)?;
        let raw: Option<Option<Vec<u8>>> = self.store.read(|c| {
            Ok(
                c.query_row("SELECT refund FROM deposits WHERE hash = ?1", [key], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })?;
        let raw = raw.ok_or_else(|| Error::UnknownPayment(hash.to_owned()))?;
        raw.map(|tx| bitcoin::consensus::deserialize(&tx).map_err(invalid))
            .transpose()
    }

    /// The deposits asked for, newest first.
    pub fn deposits(&self) -> Result<Vec<Deposit>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(&format!(
                "{DEPOSITS} ORDER BY d.created_at DESC, d.rowid DESC"
            ))?;
            let rows = stmt.query_map([], deposit_of)?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// The deposit of `hash`.
    pub fn deposit_of(&self, hash: &str) -> Result<Deposit, Error> {
        self.deposit_record(&hash_id(hash)?)
    }

    fn deposit_record(&self, hash: &[u8; 32]) -> Result<Deposit, Error> {
        self.store.read(|c| {
            c.query_row(&format!("{DEPOSITS} WHERE d.hash = ?1"), [hash], deposit_of)
                .optional()?
                .ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))
        })
    }

    /// The deposits under way, oldest first.
    pub(crate) fn open_deposits(&self) -> Result<Vec<[u8; 32]>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT hash FROM deposits
                 WHERE done_at IS NULL AND canceled IS NULL AND refund IS NULL
                 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

const DEPOSITS: &str = "SELECT d.hash, r.url, d.amount, t.address, t.timeout_height, t.pays,
    t.fund_by_height, f.txid, f.vout, f.height, d.done_at IS NOT NULL, d.canceled, d.refund,
    d.created_at, d.lock
    FROM deposits d JOIN reach r ON r.hub = d.hub
    LEFT JOIN deposit_terms t ON t.hash = d.hash
    LEFT JOIN deposit_fundings f ON f.hash = d.hash";

fn deposit_of(r: &Row<'_>) -> rusqlite::Result<Deposit> {
    let terms = match (r.get(3)?, r.get(4)?, r.get::<_, Option<i64>>(5)?, r.get(6)?) {
        (Some(address), Some(timeout), Some(pays), Some(fund_by)) => Some(DepositTerms {
            address,
            pays: pays as u64,
            fund_by,
            timeout,
        }),
        _ => None,
    };
    let funding = match (r.get::<_, Option<[u8; 32]>>(7)?, r.get(8)?, r.get(9)?) {
        (Some(txid), Some(vout), Some(height)) => Some((
            OutPoint {
                txid: Txid::from_byte_array(txid),
                vout,
            },
            height,
        )),
        _ => None,
    };
    let refunded = match r.get::<_, Option<Vec<u8>>>(12)? {
        Some(raw) => {
            let tx: Transaction = bitcoin::consensus::deserialize(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Blob, e.into())
            })?;
            Some(tx.compute_txid())
        }
        None => None,
    };
    let state = match (
        refunded,
        r.get::<_, bool>(10)?,
        r.get::<_, Option<String>>(11)?,
    ) {
        (Some(txid), _, _) => DepositState::Refunded(txid),
        (None, true, _) => DepositState::Received,
        (None, false, Some(reason)) => DepositState::Canceled(reason),
        (None, false, None) => DepositState::Open,
    };
    Ok(Deposit {
        hash: hex::encode(r.get::<_, [u8; 32]>(0)?),
        hub: r.get(1)?,
        amount: r.get::<_, i64>(2)? as u64,
        terms,
        funding,
        lock: r.get::<_, Option<Vec<u8>>>(14)?.map(hex::encode),
        state,
        created_at: r.get(13)?,
    })
}
