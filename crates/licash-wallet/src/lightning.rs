//! Lightning, through a hub's gateway ([`licash_api::Lightning`]).
//!
//! Paying an invoice locks notes worth it and the gateway's fee to the hub
//! alone, under the invoice's hash, recording the invoice with the lock.
//! Once the lock is certified the wallet asks the hub to pay the invoice:
//! the preimage the payment buys settles the lock, and a failed payment
//! refunds it. Following the lock asks again until one or the other.
//!
//! Getting paid: the wallet draws a preimage, records it with the receipt
//! it asks for, then asks the hub for an invoice under its hash. Once the
//! invoice is paid, the hub locks notes of its own to the wallet, which
//! claims them with the preimage: that settles the invoice.

use licash::{
    address::Ephemeral,
    claim::Branch,
    spend::{lock_hash, ProposalId},
};
use licash_api::{Pay, PayState, Paying, Receive, ReceiveState, ReceiveStatus, Receiving};
use lightning_invoice::Bolt11Invoice;
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Row};

use crate::{
    hub_url, invalid, locks::latest_expiry, now, sql, unavailable, Error, Lock, Transport, Wallet,
};

/// Blocks a route's timelock may take beyond an invoice's final delta, at
/// least, as hubs require.
const CLTV_MARGIN: u64 = 3;

/// A payment asked for over Lightning, and where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoice {
    /// Its payment hash, in hex.
    pub hash: String,
    pub hub: String,
    /// What it pays the wallet; the invoice asks the hub's fee on top.
    pub amount: u64,
    /// The BOLT11 invoice to hand the payer, once the hub made it.
    pub request: Option<String>,
    pub state: InvoiceState,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvoiceState {
    /// Waiting for its hub's invoice, its payer or the wallet's claim.
    Open,
    /// Paid, and its notes kept.
    Received,
    /// Refused or canceled unpaid, or its lock unclaimed in time, and why.
    Canceled(String),
}

/// An invoice as the wallet reads it.
fn decode(request: &str) -> Result<Bolt11Invoice, Error> {
    request
        .trim()
        .parse()
        .map_err(|e| Error::Invalid(format!("not a Lightning invoice: {e}")))
}

fn hash_id(hash: &str) -> Result<[u8; 32], Error> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hash, &mut bytes)
        .map_err(|_| Error::Invalid(format!("not a payment hash: {hash}")))?;
    Ok(bytes)
}

/// Paying invoices.
impl<T: Transport> Wallet<T> {
    /// Pay `request`, a BOLT11 invoice, through the hub at `url`: lock
    /// notes worth it and the hub's fee to the hub, until the latest expiry
    /// it admits, for the hub to pay it. Where the lock stands: paid on the
    /// invoice's preimage, refunded once the payment failed, or
    /// [`crate::LockState::Locked`] while it is under way, which
    /// [`Wallet::follow_lock`] follows.
    pub async fn pay_invoice(&self, url: &str, request: &str) -> Result<Lock, Error> {
        let url = hub_url(url)?;
        let invoice = decode(request)?;
        let msat = invoice
            .amount_milli_satoshis()
            .filter(|msat| *msat > 0)
            .ok_or_else(|| Error::Invalid("an invoice without an amount".into()))?;
        if invoice.is_expired() {
            return Err(Error::Invalid("the invoice expired".into()));
        }
        let (_, info) = self.refresh(&url).await?;
        let lightning = info
            .lightning
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("hub {url} pays no Lightning invoices")))?;
        if msat > lightning.max_pay.saturating_mul(1000) {
            return Err(Error::Invalid(format!(
                "hub {url} pays invoices of at most {} sat",
                lightning.max_pay
            )));
        }
        let locks = info.locks.as_ref().ok_or_else(|| {
            Error::Invalid(format!("hub {url} follows no chain: it takes no locks"))
        })?;
        let checkpoint = locks.checkpoint().map_err(|p| unavailable(&url, p))?;
        let expiry = latest_expiry(locks, &checkpoint, locks.min_depth.max(1));
        let room = expiry
            .saturating_sub(locks.height)
            .saturating_sub(lightning.pay_slack);
        let delta = invoice.min_final_cltv_expiry_delta();
        if u64::from(room) < delta.saturating_add(CLTV_MARGIN) {
            return Err(Error::Invalid(format!(
                "the invoice's final delta of {delta} blocks outlasts the locks of hub {url}"
            )));
        }
        let hash: [u8; 32] = *invoice.payment_hash().as_ref();
        let amount = lightning.pay_lock(msat);
        let request = invoice.to_string();
        tracing::info!(%url, msat, amount, "paying an invoice");
        self.lock_paying(
            &url,
            &info.address,
            amount,
            hash,
            Some(expiry),
            Some(&request),
        )
        .await
    }

    /// Ask the hub at `url` to pay `invoice` for lock `id`, certified: a
    /// failure or a refusal is recorded, and the lock followed to its
    /// refund. No answer is retried when the lock is followed again.
    pub(crate) async fn ask_payment(
        &self,
        url: &str,
        id: &ProposalId,
        invoice: &str,
    ) -> Result<(), Error> {
        let asked = self
            .post::<Paying>(url, licash_api::LN_PAY, &Pay::new(id, invoice))
            .await;
        let failure = match asked {
            Ok(Paying {
                state: PayState::Failed,
                reason,
                ..
            }) => reason.unwrap_or_else(|| "the payment failed".into()),
            Ok(_) => return Ok(()),
            Err(Error::Refused { problem, .. }) => format!("refused: {problem}"),
            Err(e) => {
                tracing::warn!(id = %hex::encode(id.as_bytes()), error = %e, "cannot ask for a payment");
                return Ok(());
            }
        };
        tracing::warn!(id = %hex::encode(id.as_bytes()), %url, failure, "invoice not paid");
        self.store.write(|tx| {
            tx.execute(
                "UPDATE locks SET failure = ?2 WHERE id = ?1 AND failure IS NULL",
                params![id.as_bytes(), failure],
            )?;
            Ok(())
        })
    }
}

/// Getting paid over Lightning.
impl<T: Transport> Wallet<T> {
    /// Ask the hub at `url` for an invoice paying this wallet `amount`, its
    /// fee on top, to hand the payer. [`Wallet::follow_invoice`] claims the
    /// payment once made.
    pub async fn invoice(&self, url: &str, amount: u64) -> Result<Invoice, Error> {
        let url = hub_url(url)?;
        let (hub, info) = self.refresh(&url).await?;
        let lightning = info
            .lightning
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("hub {url} takes no Lightning payments")))?;
        if amount < lightning.min_receive || amount > lightning.max_receive {
            return Err(Error::Invalid(format!(
                "hub {url} takes {} to {} over Lightning",
                lightning.min_receive, lightning.max_receive
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
        let body = Receive::new(keyset.id(), &ephemeral.public(), &outputs, &hash);
        let body = serde_json::to_string(&body).map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO invoices (hash, hub, preimage, amount, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![hash, hub, preimage, sql(amount), body, now()],
            )?;
            Ok(())
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "asking for an invoice");
        self.advance_invoice(&hash).await
    }

    /// Take the invoice of `hash` on: have its hub make it, then claim the
    /// notes its hub locks to the wallet once it is paid. Where it stands.
    pub async fn follow_invoice(&self, hash: &str) -> Result<Invoice, Error> {
        self.advance_invoice(&hash_id(hash)?).await
    }

    pub(crate) async fn advance_invoice(&self, hash: &[u8; 32]) -> Result<Invoice, Error> {
        let pending = |e: Error| match e {
            Error::Pending { .. } | Error::UnknownPayment(_) => e,
            e => Error::Pending {
                id: hex::encode(hash),
                reason: e.to_string(),
            },
        };
        self.advance_invoice_from(hash).await.map_err(pending)?;
        self.invoice_record(hash)
    }

    async fn advance_invoice_from(&self, hash: &[u8; 32]) -> Result<(), Error> {
        type Row = (String, [u8; 32], i64, String, Option<String>, bool);
        let row: Option<Row> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT r.url, i.preimage, i.amount, i.body, i.request,
                    i.done_at IS NULL AND i.canceled IS NULL
                 FROM invoices i JOIN reach r ON r.hub = i.hub WHERE i.hash = ?1",
                [hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let (url, preimage, amount, body, request, open) =
            row.ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))?;
        if !open {
            return Ok(());
        }
        if request.is_none() {
            let body: Receive = serde_json::from_str(&body).map_err(invalid)?;
            let asked = self
                .post::<Receiving>(&url, licash_api::LN_RECEIVE, &body)
                .await;
            let receiving = match asked {
                Ok(receiving) => receiving,
                // Refused before the hub made an invoice.
                Err(Error::Refused { problem, .. }) => {
                    return self.cancel_invoice(hash, &format!("refused: {problem}"));
                }
                Err(e) => return Err(e),
            };
            let invoice = decode(&receiving.request).map_err(|e| unavailable(&url, e))?;
            let of: [u8; 32] = *invoice.payment_hash().as_ref();
            let least = (amount as u64).saturating_mul(1000);
            if of != *hash
                || invoice
                    .amount_milli_satoshis()
                    .is_none_or(|msat| msat < least)
            {
                return Err(unavailable(
                    &url,
                    "it answered an invoice of another payment",
                ));
            }
            self.store.write(|tx| {
                tx.execute(
                    "UPDATE invoices SET request = ?2 WHERE hash = ?1",
                    params![hash, invoice.to_string()],
                )?;
                Ok(())
            })?;
            tracing::info!(hash = %hex::encode(hash), %url, "invoice made");
        }
        let status: ReceiveStatus = self.get(&url, &licash_api::ln_receive_state(hash)).await?;
        let lock = status.lock().map_err(|p| unavailable(&url, p))?;
        let lock = match (status.state, lock) {
            (ReceiveState::Open, _) => return Ok(()),
            (ReceiveState::Canceled, _) => return self.cancel_invoice(hash, "canceled by its hub"),
            (_, None) => return Err(unavailable(&url, "a payment locked to no lock")),
            (_, Some(lock)) => lock,
        };
        self.store.write(|tx| {
            tx.execute(
                "UPDATE invoices SET lock = ?2 WHERE hash = ?1",
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
            Err(Error::Invalid(reason)) => return self.cancel_invoice(hash, &reason),
            Err(Error::PaysNothing) => {
                return self.cancel_invoice(hash, "its lock pays this wallet nothing");
            }
            Err(e) => return Err(e),
        }
        self.store.write(|tx| {
            tx.execute(
                "UPDATE invoices SET done_at = ?2 WHERE hash = ?1 AND done_at IS NULL",
                params![hash, now()],
            )?;
            Ok(())
        })?;
        tracing::info!(hash = %hex::encode(hash), %url, amount, "invoice paid");
        Ok(())
    }

    fn cancel_invoice(&self, hash: &[u8; 32], reason: &str) -> Result<(), Error> {
        tracing::warn!(hash = %hex::encode(hash), reason, "invoice canceled");
        self.store.write(|tx| {
            tx.execute(
                "UPDATE invoices SET canceled = ?2
                 WHERE hash = ?1 AND done_at IS NULL AND canceled IS NULL",
                params![hash, reason],
            )?;
            Ok(())
        })
    }

    /// The invoices asked for, newest first.
    pub fn invoices(&self) -> Result<Vec<Invoice>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(&format!(
                "{INVOICES} ORDER BY i.created_at DESC, i.rowid DESC"
            ))?;
            let rows = stmt.query_map([], invoice_of)?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    fn invoice_record(&self, hash: &[u8; 32]) -> Result<Invoice, Error> {
        self.store.read(|c| {
            c.query_row(&format!("{INVOICES} WHERE i.hash = ?1"), [hash], invoice_of)
                .optional()?
                .ok_or_else(|| Error::UnknownPayment(hex::encode(hash)))
        })
    }

    /// The invoices under way, oldest first.
    pub(crate) fn open_invoices(&self) -> Result<Vec<[u8; 32]>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT hash FROM invoices WHERE done_at IS NULL AND canceled IS NULL
                 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

const INVOICES: &str = "SELECT i.hash, r.url, i.amount, i.request, i.done_at IS NOT NULL,
    i.canceled, i.created_at
    FROM invoices i JOIN reach r ON r.hub = i.hub";

fn invoice_of(r: &Row<'_>) -> rusqlite::Result<Invoice> {
    let state = match (r.get::<_, bool>(4)?, r.get::<_, Option<String>>(5)?) {
        (_, Some(reason)) => InvoiceState::Canceled(reason),
        (true, None) => InvoiceState::Received,
        (false, None) => InvoiceState::Open,
    };
    Ok(Invoice {
        hash: hex::encode(r.get::<_, [u8; 32]>(0)?),
        hub: r.get(1)?,
        amount: r.get::<_, i64>(2)? as u64,
        request: r.get(3)?,
        state,
        created_at: r.get(6)?,
    })
}
