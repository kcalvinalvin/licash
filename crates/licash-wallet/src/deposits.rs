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

use bitcoin::{
    hashes::Hash,
    secp256k1::{Secp256k1, SecretKey, XOnlyPublicKey},
    Amount, FeeRate, OutPoint, ScriptBuf, Transaction, Txid,
};
use licash::{
    address::Ephemeral,
    claim::Branch,
    spend::{lock_hash, ProposalId},
};
use licash_api::{DepositStatus, Depositing, Hex, Info, Receive, ReceiveState};
use licash_onchain::deposit::Htlc;
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Row};

use crate::{hub_url, invalid, now, sql, unavailable, Error, Transport, Wallet};

/// Blocks a hub's tip may move between its info and its answer: the
/// timeout it answers may be that much later than its info promises.
const TIP_MARGIN: u32 = 6;

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
        let asked = self.draw_deposit(&url, &info, amount)?;
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
        self.deposit_answered(hub, &info, amount).await
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
                 WHERE hash = ?1 AND txid IS NULL AND lock IS NULL
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
    ) -> Result<Deposit, Error> {
        let url = self.reached(hub)?;
        let asked = self.draw_deposit(&url, info, amount)?;
        let hash = asked.hash;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "asking for a deposit");
        let terms: Depositing = self.post(&url, licash_api::DEPOSIT, &asked.body).await?;
        let htlc = self
            .check_terms(&url, &hash, &asked.refund_key, amount, &terms)
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
                 WHERE hash = ?1 AND claim IS NULL AND done_at IS NULL AND canceled IS NULL",
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
    /// says `info` of itself, for.
    fn draw_deposit(&self, url: &str, info: &Info, amount: u64) -> Result<Asked, Error> {
        let keysets = info.keysets().map_err(|p| unavailable(url, p))?;
        let (ephemeral, me) = (Ephemeral::random(), self.address());
        let (keyset, outputs) = keysets
            .iter()
            .find_map(|k| {
                let outputs = crate::outputs(k, &ephemeral, Branch::Pay, &[(&me, amount)]);
                Some((k, outputs.ok()?))
            })
            .ok_or_else(|| Error::Invalid(format!("hub {url} has no keyset making {amount}")))?;
        let mut preimage = [0; 32];
        OsRng.fill_bytes(&mut preimage);
        let refund_key = random_key();
        let hash = lock_hash(&preimage);
        let body = licash_api::Deposit {
            receive: Receive::new(keyset.id(), &ephemeral.public(), &outputs, &hash),
            refund: Hex(x_only(&refund_key).serialize().to_vec()),
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
                sql(self.amount),
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
        "UPDATE deposits SET claim = ?2, timeout = ?3, pays = ?4, fund_by = ?5, address = ?6
         WHERE hash = ?1 AND canceled IS NULL",
        params![
            hash,
            htlc.claim.serialize(),
            htlc.timeout,
            sql(terms.amount),
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
                "SELECT r.url, d.preimage, d.refund_key, d.amount, d.body, d.claim IS NOT NULL,
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
        if !answered {
            let body: licash_api::Deposit = serde_json::from_str(&body).map_err(invalid)?;
            let asked = self
                .post::<Depositing>(&url, licash_api::DEPOSIT, &body)
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
                .check_terms(&url, hash, &refund, amount as u64, &terms)
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
                    "UPDATE deposits SET txid = ?2, vout = ?3, height = ?4 WHERE hash = ?1",
                    params![hash, txid.to_byte_array(), funding.vout, funding.height],
                )?;
                Ok(())
            })?;
        }
        let lock = status.status.lock().map_err(|p| unavailable(&url, p))?;
        let lock: ProposalId = match (status.status.state, lock) {
            (ReceiveState::Open, _) => return Ok(()),
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
        match self
            .claim_paying(
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
        if deposits.pays(amount) != Some(terms.amount) {
            return Err(wrong(format!("of {} for {amount}", terms.amount)));
        }
        let latest = height
            .saturating_add(deposits.timeout)
            .saturating_add(TIP_MARGIN);
        if terms.timeout > latest || terms.fund_by.saturating_add(deposits.depth) > terms.timeout {
            return Err(wrong(format!(
                "timing out at block {}, paid by {}",
                terms.timeout, terms.fund_by
            )));
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
                "SELECT refund_key, claim, timeout, done_at IS NOT NULL FROM deposits
                 WHERE hash = ?1",
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

const DEPOSITS: &str = "SELECT d.hash, r.url, d.amount, d.address, d.timeout, d.pays,
    d.fund_by, d.txid, d.vout, d.height, d.done_at IS NOT NULL, d.canceled, d.refund,
    d.created_at, d.lock
    FROM deposits d JOIN reach r ON r.hub = d.hub";

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
