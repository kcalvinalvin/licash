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
use licash_api::{DepositStatus, Depositing, Hex, Receive, ReceiveState};
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
fn random_key() -> SecretKey {
    loop {
        let mut bytes = [0; 32];
        OsRng.fill_bytes(&mut bytes);
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

fn x_only(key: &SecretKey) -> XOnlyPublicKey {
    key.x_only_public_key(&Secp256k1::new()).0
}

impl<T: Transport> Wallet<T> {
    /// Ask the hub at `url` for a deposit locking notes worth `amount` to
    /// this wallet: where to pay on Bitcoin, and how much, its fee on top.
    /// [`Wallet::follow_deposit`] claims the notes once the payment
    /// confirmed.
    pub async fn deposit(&self, url: &str, amount: u64) -> Result<Deposit, Error> {
        let url = hub_url(url)?;
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
        let keysets = info.keysets().map_err(|p| unavailable(&url, p))?;
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
        let hash = lock_hash(&preimage);
        let refund_key = random_key();
        let body = licash_api::Deposit {
            receive: Receive::new(keyset.id(), &ephemeral.public(), &outputs, &hash),
            refund: Hex(x_only(&refund_key).serialize().to_vec()),
        };
        let body = serde_json::to_string(&body).map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO deposits (hash, hub, preimage, refund_key, amount, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    hash,
                    hub,
                    preimage,
                    refund_key.secret_bytes(),
                    sql(amount),
                    body,
                    now()
                ],
            )?;
            Ok(())
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "asking for a deposit");
        self.advance_deposit(&hash).await
    }

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
            self.store.write(|tx| {
                tx.execute(
                    "UPDATE deposits SET claim = ?2, timeout = ?3, pays = ?4, fund_by = ?5,
                         address = ?6
                     WHERE hash = ?1",
                    params![
                        hash,
                        htlc.claim.serialize(),
                        htlc.timeout,
                        sql(terms.amount),
                        terms.fund_by,
                        htlc.address(licash_onchain::network(self.network()))
                            .to_string(),
                    ],
                )?;
                Ok(())
            })?;
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
            .claim(&url, &hex::encode(lock.as_bytes()), &preimage)
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
    d.created_at
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
        state,
        created_at: r.get(13)?,
    })
}
