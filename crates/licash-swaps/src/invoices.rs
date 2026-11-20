//! Lightning invoices paid ([`swaps::LightningRequest`]): the provider pays
//! an invoice from its own node for a client's lock at one of its hubs.
//!
//! A client locks the invoice's amount and the provider's fee to the
//! provider at hub `from`, under the invoice's hash, then asks for the
//! payment. The provider checks the invoice and that lock as its hub holds
//! it, records the payment, then has its node pay the invoice along routes
//! whose timelock ends [`swaps::Lightning::slack`] blocks before the lock
//! expires. The preimage the payment buys claims the client's lock. A
//! payment that failed, or that the node never started in time, is given
//! up: the client's lock is refunded once expired.
//!
//! The lock pays a third party, the provider, which may claim it only until
//! it expires: the slack is the provider's time to learn the preimage,
//! once a route's last HTLC timed out on Bitcoin at worst, and claim with
//! it, as a routing node's CLTV delta ([`MIN_SLACK`] at least).
//!
//! Only the node's own word that a payment succeeded or failed resolves it
//! ([`judge`]): a payment recorded, then stopped before the node was asked,
//! is asked for again while it can still be paid in time, and given up
//! once the node showed twice, [`GRACE`] seconds after it was recorded,
//! that it never started it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

use licash::spend::{lock_hash, ProposalId};
use licash_api::{
    swaps::{self, LightningRequest, LightningSwap},
    Hex, Problem,
};
use licash_lightning::{cltv_limit, expires, judge, unpayable, Judged, Node, NodeInfo};
use licash_wallet::Transport;
use lightning_invoice::Bolt11Invoice;
use rusqlite::{params, OptionalExtension};

use crate::{declined, emit, now, refused, unavailable, Error, Hold, Notice, Provider, Row, Stage};

/// The fewest blocks of slack a provider pays invoices with: LND's least
/// CLTV delta.
pub const MIN_SLACK: u32 = 18;

/// Seconds the node tries to pay an invoice for, at most.
const PAY_TIMEOUT: u32 = 60;

/// Seconds after a payment was recorded before the node not knowing it
/// may give it up.
const GRACE: u64 = 600;

/// What a provider pays over Lightning, and through which node.
pub(crate) struct Payer {
    node: Arc<dyn Node>,
    terms: swaps::Lightning,
    /// By payment, the rounds in a row the node knew no attempt too late
    /// to start one.
    misses: Mutex<HashMap<[u8; 32], u32>>,
    /// What the operator was alerted of, once each.
    alerted: Mutex<HashSet<String>>,
}

impl Payer {
    fn forget(&self, id: &[u8; 32]) {
        let mut misses = self.misses.lock().unwrap_or_else(PoisonError::into_inner);
        misses.remove(id);
    }

    /// One more round the node knew no attempt at `id`: how many in a row.
    fn missed(&self, id: &[u8; 32]) -> u32 {
        let mut misses = self.misses.lock().unwrap_or_else(PoisonError::into_inner);
        let missed = misses.entry(*id).or_default();
        *missed += 1;
        *missed
    }

    /// Whether the operator is yet to be alerted of `what`, from now on not.
    fn first(&self, what: String) -> bool {
        let mut alerted = self.alerted.lock().unwrap_or_else(PoisonError::into_inner);
        alerted.insert(what)
    }
}

/// An invoice as recorded.
struct Invoice {
    request: String,
    /// The node that pays it, its public key in hex.
    node: String,
}

/// What the provider checked of an invoice and of the client's lock paying
/// it.
struct Taken<'a> {
    from: &'a str,
    hash: [u8; 32],
    payee: [u8; 33],
    request: &'a str,
    node: &'a str,
    sat: u64,
    fee: u64,
    paid: u64,
    expiry_from: u32,
    /// The height its routes' timelock ends by at the latest.
    deadline: u32,
}

/// The msat `invoice` asks, if the provider pays it on `terms` through the
/// node of `info`.
fn payable(
    invoice: &Bolt11Invoice,
    terms: &swaps::Lightning,
    info: &NodeInfo,
) -> Result<u64, &'static str> {
    let msat = invoice
        .amount_milli_satoshis()
        .filter(|msat| *msat > 0)
        .ok_or("an invoice without an amount")?;
    if msat > terms.max.saturating_mul(1000) {
        return Err("above the most the provider pays");
    }
    if unpayable(invoice) {
        return Err("an invoice of features the provider does not pay");
    }
    if expires(invoice, PAY_TIMEOUT) {
        return Err("the invoice expires");
    }
    if invoice.currency() != info.currency {
        return Err("an invoice of another network");
    }
    if hex::encode(invoice.get_payee_pub_key().serialize()) == info.pubkey {
        return Err("an invoice of the provider's own node");
    }
    Ok(msat)
}

/// The most an invoice may ask, in sat, for the provider to pay it on
/// `terms` from `local` sat in its channels: what it asks and the routing
/// fees its fee allows, both. All its channels together, an upper bound:
/// reserves and routes may leave less.
fn most_paid(terms: &swaps::Lightning, local: u64) -> u64 {
    let (mut fits, mut over) = (0, local.saturating_add(1));
    while over - fits > 1 {
        let mid = fits + (over - fits) / 2;
        if terms.pays(mid.saturating_mul(1000)) <= local {
            fits = mid;
        } else {
            over = mid;
        }
    }
    fits
}

/// Invoices taken.
impl<T: Transport> Provider<T> {
    /// Pay Lightning invoices for clients' locks at any of its hubs, on
    /// `terms`, through `node`, whose public key they name: the provider
    /// pays none otherwise. Refused with a slack below [`MIN_SLACK`].
    pub fn pay_lightning(
        mut self,
        node: Arc<dyn Node>,
        terms: swaps::Lightning,
    ) -> Result<Self, Error> {
        if terms.slack < MIN_SLACK {
            return Err(Error::Setup(format!(
                "a Lightning slack of {} blocks: {MIN_SLACK} at least",
                terms.slack
            )));
        }
        self.lightning = Some(Payer {
            node,
            terms,
            misses: Mutex::default(),
            alerted: Mutex::default(),
        });
        Ok(self)
    }

    /// What it pays over Lightning, as its operator set it.
    pub fn lightning_terms(&self) -> Option<&swaps::Lightning> {
        self.lightning.as_ref().map(|payer| &payer.terms)
    }

    /// What it pays over Lightning now: as its operator set it, up to what
    /// its node's channels pay, its fee in it; nothing if the node does
    /// not answer. Clients ask no more, so that none locks notes for a
    /// payment the provider would decline or fail.
    pub async fn lightning_now(&self) -> Option<swaps::Lightning> {
        let payer = self.lightning.as_ref()?;
        let local = match payer.node.liquidity().await {
            Ok(liquidity) => liquidity.local,
            Err(e) => {
                tracing::warn!(error = %e, "the Lightning node tells not what it pays");
                0
            }
        };
        let terms = &payer.terms;
        Some(swaps::Lightning {
            max: terms.max.min(most_paid(terms, local)),
            ..terms.clone()
        })
    }

    /// Invoices whose payment is under way, which only
    /// [`Provider::follow_invoices`] takes on.
    pub fn open_invoices(&self) -> Result<u64, Error> {
        self.store.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM provider_swaps WHERE kind = 'lightning' AND state = 'accepted'",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }

    /// Pay the invoice `request` asks for, or answer where its payment
    /// stands if taken already. Declined unless the provider pays such an
    /// invoice, and the client's lock, as its hub holds it, is under the
    /// invoice's hash, pays the provider the invoice and its fee,
    /// unsettled, and expires late enough to pay the invoice along routes
    /// ending the slack before.
    pub async fn take_invoice(&self, request: &LightningRequest) -> Result<LightningSwap, Problem> {
        let id = request.lock()?;
        let key = *id.as_bytes();
        let invoice: Bolt11Invoice = request
            .request
            .trim()
            .parse()
            .map_err(|e| Problem::invalid(format!("invoice: {e}")))?;
        let text = invoice.to_string();
        if let Some(row) = self.row(&key).map_err(unavailable)? {
            return self.invoice_again(&row, request, &text);
        }
        let Some(payer) = &self.lightning else {
            return Err(declined("no Lightning payments"));
        };
        let terms = &payer.terms;
        let hubs = self.wallet.hubs().map_err(unavailable)?;
        let Some((from, _)) = hubs.iter().find(|(_, info)| info.address == request.from) else {
            return Err(declined("no payments from this hub"));
        };
        let info = payer
            .node
            .info()
            .await
            .map_err(|e| unavailable(format!("its Lightning node: {e}")))?;
        let msat = payable(&invoice, terms, &info).map_err(declined)?;
        // Its fee pays the routes: what the channels must pay, both.
        let local = (payer.node.liquidity().await)
            .map_err(|e| unavailable(format!("its Lightning node: {e}")))?
            .local;
        if terms.pays(msat) > local {
            return Err(declined("above what the provider's channels pay now"));
        }
        let sat = msat.div_ceil(1000);
        let Some(_hold) = Hold::take(&self.busy, key) else {
            return Err(unavailable("the payment is under way"));
        };
        let incoming = self
            .wallet
            .incoming(from, &hex::encode(key))
            .await
            .map_err(refused)?;
        let hash = *invoice.payment_hash().as_ref();
        if incoming.hash != hash {
            return Err(declined("the lock is not under the invoice's hash"));
        }
        let due = terms.pays(msat);
        if incoming.amount < due {
            return Err(declined(format!(
                "the lock pays {}, the invoice and the fee {due}",
                incoming.amount
            )));
        }
        if incoming.settled.is_some() || incoming.preimage.is_some() {
            return Err(declined("the lock is settled"));
        }
        if incoming.height == 0 {
            return Err(declined(format!("hub {from} follows no chain")));
        }
        // The higher of either: the node's routes end by its own height.
        let tip = info.height.max(incoming.height);
        let Some(limit) = cltv_limit(incoming.expiry, tip, terms.slack, &invoice) else {
            return Err(declined(format!(
                "the lock expires at {}: too soon to pay the invoice along routes ending {} \
                 blocks earlier",
                incoming.expiry, terms.slack
            )));
        };
        let taken = Taken {
            from,
            hash,
            payee: invoice.get_payee_pub_key().serialize(),
            request: &text,
            node: &info.pubkey,
            sat,
            fee: due - sat,
            paid: incoming.amount,
            expiry_from: incoming.expiry,
            deadline: tip.saturating_add(limit),
        };
        self.record_invoice(&key, &taken)?;
        tracing::info!(
            id = %hex::encode(key), from, sat, fee = taken.fee, limit, "invoice accepted"
        );
        self.start(payer, &key, text.clone(), taken.fee, limit)
            .await;
        self.invoice_answer(&key)
            .map_err(unavailable)?
            .ok_or_else(|| unavailable("the payment is not recorded"))
    }

    /// The payment `row` asked for again: where it stands, if it is the
    /// same, of invoice `text`.
    fn invoice_again(
        &self,
        row: &Row,
        request: &LightningRequest,
        text: &str,
    ) -> Result<LightningSwap, Problem> {
        let hubs = self.wallet.hubs().map_err(unavailable)?;
        let from = hubs
            .iter()
            .find(|(u, _)| *u == row.from)
            .map(|(_, i)| i.address);
        let invoice = self.invoice_record(&row.id).map_err(unavailable)?;
        let same = row.lightning
            && from == Some(request.from)
            && invoice.is_some_and(|i| i.request == text);
        if !same {
            return Err(declined("another swap of this lock"));
        }
        self.invoice_answer(&row.id)
            .map_err(unavailable)?
            .ok_or_else(|| unavailable("the payment is not recorded"))
    }

    /// Record the payment `id` on `taken`, if the provider may yet take as
    /// many of hub `from`'s notes: before its node is asked to pay.
    fn record_invoice(&self, id: &[u8; 32], taken: &Taken<'_>) -> Result<(), Problem> {
        let _taking = self.taking.lock().unwrap_or_else(PoisonError::into_inner);
        self.admit(taken.from, taken.paid)?;
        let to = format!(
            "lightning:{}",
            licash_onchain::network(self.wallet.network())
        );
        let notice = Notice::Accepted {
            id: hex::encode(id),
            amount: taken.sat,
            fee: taken.fee,
            from: taken.from.to_owned(),
            to: to.clone(),
        };
        let node = hex::decode(taken.node)
            .map_err(|e| unavailable(format!("the node's key is no hex: {e}")))?;
        let recorded = self.store.write(|tx| {
            let at = now();
            let inserted = tx.execute(
                "INSERT INTO provider_swaps (id, hash, from_url, to_url, address, amount, fee, paid,
                    expiry_from_height, expiry_height, state, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'accepted', 'lightning', ?11, ?11)
                 ON CONFLICT DO NOTHING",
                params![
                    id,
                    taken.hash,
                    taken.from,
                    to,
                    taken.payee.as_slice(),
                    crate::store::sat(taken.sat)?,
                    crate::store::sat(taken.fee)?,
                    crate::store::sat(taken.paid)?,
                    taken.expiry_from,
                    taken.deadline,
                    at
                ],
            )?;
            if inserted == 1 {
                tx.execute(
                    "INSERT INTO provider_invoices (id, request, node) VALUES (?1, ?2, ?3)",
                    params![id, taken.request, node],
                )?;
                emit(tx, &notice)?;
            }
            Ok(inserted == 1)
        });
        match recorded {
            Ok(true) => Ok(()),
            Ok(false) => Err(declined("a swap under this hash was taken before")),
            Err(e) => Err(unavailable(e)),
        }
    }

    /// Have the node start paying `request` for the payment `id`, along
    /// routes whose timelock ends within `limit` blocks, for at most
    /// `fee_limit` of routing fees. Not started, it is followed and asked
    /// for again.
    async fn start(
        &self,
        payer: &Payer,
        id: &[u8; 32],
        request: String,
        fee_limit: u64,
        limit: u32,
    ) {
        let name = hex::encode(id);
        match payer.node.pay(request, fee_limit, limit, PAY_TIMEOUT).await {
            Ok(()) => tracing::info!(id = %name, limit, fee_limit, "payment started"),
            Err(e) => tracing::warn!(id = %name, error = %e, "cannot start a payment"),
        }
    }
}

/// What the provider recorded of invoices.
impl<T: Transport> Provider<T> {
    /// The payment for the client's lock `id`, as its client is told.
    pub fn invoice(&self, id: &ProposalId) -> Result<Option<LightningSwap>, Error> {
        self.invoice_answer(id.as_bytes())
    }

    fn invoice_answer(&self, id: &[u8; 32]) -> Result<Option<LightningSwap>, Error> {
        let Some(row) = self.row(id)?.filter(|row| row.lightning) else {
            return Ok(None);
        };
        Ok(Some(LightningSwap {
            id: Hex(id.to_vec()),
            state: row.stage.state(),
            amount: row.amount,
            preimage: row.preimage.map(|p| Hex(p.to_vec())),
            reason: row.reason.clone(),
        }))
    }

    fn invoice_record(&self, id: &[u8; 32]) -> Result<Option<Invoice>, Error> {
        self.store.read(|c| {
            Ok(c.query_row(
                "SELECT request, node FROM provider_invoices WHERE id = ?1",
                [id],
                |r| {
                    Ok(Invoice {
                        request: r.get(0)?,
                        node: hex::encode(r.get::<_, Vec<u8>>(1)?),
                    })
                },
            )
            .optional()?)
        })
    }
}

/// Invoices taken on.
impl<T: Transport> Provider<T> {
    /// Take every invoice payment under way on, as the provider's node
    /// knows it: claim the client's lock once paid, give the payment up
    /// once failed or never started in time, start it again while it may
    /// still be paid in time. The payments that could not move now, by id
    /// and why; `node` if the node did not answer.
    pub async fn follow_invoices(&self) -> Result<Vec<(String, String)>, Error> {
        let Some(payer) = &self.lightning else {
            return Ok(Vec::new());
        };
        let open: Vec<([u8; 32], String)> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, state FROM provider_swaps WHERE kind = 'lightning'
                    AND state IN ('accepted', 'paid')
                 ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut pending = Vec::new();
        if open.is_empty() {
            return Ok(pending);
        }
        let info = match payer.node.info().await {
            Ok(info) => Some(info),
            Err(e) => {
                tracing::warn!(error = %e, "the Lightning node does not answer");
                pending.push(("node".to_owned(), e));
                None
            }
        };
        for (id, state) in open {
            let Some(_hold) = Hold::take(&self.busy, id) else {
                continue;
            };
            // Claimed whether the node answers or not: it was paid.
            let followed = match (state.as_str(), &info) {
                // A row unread is pending, as a claim failed: the others
                // are followed all the same.
                ("paid", _) => match self.row(&id) {
                    Ok(Some(row)) if row.stage == Stage::Paid => self.claim(&id, &row).await,
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                },
                (_, Some(info)) => self.follow_invoice(payer, &id, info).await,
                (_, None) => Ok(()),
            };
            if let Err(e) = followed {
                tracing::warn!(id = %hex::encode(id), "payment pending: {e}");
                pending.push((hex::encode(id), e.to_string()));
            }
        }
        Ok(pending)
    }

    /// Act on the payment `id` under way as the node of `info` knows it.
    async fn follow_invoice(
        &self,
        payer: &Payer,
        id: &[u8; 32],
        info: &NodeInfo,
    ) -> Result<(), Error> {
        let (Some(row), Some(invoice)) = (self.row(id)?, self.invoice_record(id)?) else {
            return Ok(());
        };
        if row.stage != Stage::Accepted {
            return Ok(());
        }
        let name = hex::encode(id);
        if invoice.node != info.pubkey {
            let message = format!(
                "The payment for lock {name} was started by Lightning node {}, not this one: \
                 resolve it by hand.",
                invoice.node
            );
            return self.alert_once(payer, format!("node {name}"), &message);
        }
        let tip = self.tip_of(&row.from, info.height)?;
        if tip >= row.expiry {
            let message = format!(
                "The payment for lock {name} is unresolved past its routes' end at block {}: \
                 once the node learns the preimage, the provider claims the client's lock at \
                 {} before it expires at {}.",
                row.expiry, row.from, row.expiry_from
            );
            self.alert_once(payer, format!("late {name}"), &message)?;
        }
        let attempt = payer
            .node
            .payment(row.hash)
            .await
            .map_err(Error::Lightning)?;
        let recorded = u64::try_from(row.created_at).unwrap_or(0);
        let known = match judge(attempt, recorded) {
            Judged::Paid(preimage) => {
                payer.forget(id);
                return self.invoice_paid(payer, id, &row, preimage).await;
            }
            Judged::InFlight => return Ok(()),
            Judged::Failed(reason) => {
                payer.forget(id);
                return self.fail(id, &row, &format!("not paid: {reason}"));
            }
            Judged::Unstarted { known } => known,
        };
        // Started again along routes ending by the deadline recorded, the
        // invoice still payable.
        let invoice_of: Option<Bolt11Invoice> = invoice.request.parse().ok();
        let limit = invoice_of
            .as_ref()
            .filter(|i| !expires(i, PAY_TIMEOUT))
            .and_then(|i| cltv_limit(row.expiry, tip, 0, i));
        if let Some(limit) = limit {
            payer.forget(id);
            self.start(payer, id, invoice.request, row.fee, limit).await;
            return Ok(());
        }
        // Too late to start: given up once the node shows it never started
        // twice after the grace period, or knows another attempt.
        let missed = payer.missed(id);
        let waited = licash_lightning::now().saturating_sub(recorded) >= GRACE;
        if known || (waited && missed >= 2) {
            payer.forget(id);
            return self.fail(id, &row, "not paid: not started in time");
        }
        Ok(())
    }

    /// The payment `row` made, buying `preimage`: claim the client's lock
    /// with it.
    async fn invoice_paid(
        &self,
        payer: &Payer,
        id: &[u8; 32],
        row: &Row,
        preimage: [u8; 32],
    ) -> Result<(), Error> {
        let name = hex::encode(id);
        if lock_hash(&preimage) != row.hash {
            let message =
                format!("The Lightning node paid for lock {name} with a preimage not of its hash.");
            return self.alert_once(payer, format!("preimage {name}"), &message);
        }
        self.advance(id, Stage::Accepted, Stage::Paid, Some(&preimage), None)?;
        tracing::info!(id = %name, "invoice paid");
        let row = self.row(id)?.expect("recorded");
        self.claim(id, &row).await
    }

    /// The height the payment from hub `from` is timed by: the higher of
    /// the node's, `node`, and the hub's as it last said.
    fn tip_of(&self, from: &str, node: u32) -> Result<u32, Error> {
        let hubs = self.wallet.hubs()?;
        let hub = hubs
            .iter()
            .find(|(u, _)| u == from)
            .and_then(|(_, info)| info.locks.as_ref())
            .map_or(0, |l| l.height);
        Ok(node.max(hub))
    }

    fn alert_once(&self, payer: &Payer, what: String, message: &str) -> Result<(), Error> {
        if payer.first(what) {
            tracing::error!("{message}");
            self.alert(message)?;
        }
        Ok(())
    }
}
