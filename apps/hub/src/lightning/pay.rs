//! Payments out: invoices the gateway pays for locks that pay the hub.

use std::{collections::HashMap, time::Duration};

use licash::spend::ProposalId;
use licash_api::{Code, Hex, Pay, PayState, Paying, Problem};
use licash_hub::{Error, Forward, Forwarded, Refusal};
use lightning_invoice::Bolt11Invoice;

use super::{now, refused, Attempt, Gateway, Status, View};
use crate::Failure;

/// Blocks a route's timelock may take beyond the invoice's final delta,
/// at least.
const CLTV_MARGIN: u32 = 3;

/// Seconds an invoice must stay payable past the routing timeout.
const EXPIRY_MARGIN: u64 = 60;

impl Gateway {
    /// Pay the invoice of `body` for its lock, or say where its payment
    /// stands. An invoice the gateway does not pay for the lock fails the
    /// payment at once, which refunds the lock, if the lock is of the
    /// invoice's hash and pays the hub alone.
    pub async fn pay(&self, body: &Pay) -> Result<Paying, Failure> {
        let id = body.lock()?;
        let invoice: Bolt11Invoice = body
            .request
            .trim()
            .parse()
            .map_err(|e| Failure(Problem::invalid(format!("invoice: {e}"))))?;
        let request = invoice.to_string();
        let mut misses = self.paying.lock().await;
        if let Some(forward) = self.on_hub(move |hub| hub.forward_of(&id)).await? {
            if forward.request != request {
                return Err(refused("the lock pays another invoice"));
            }
            if forward.state == Forwarded::Pending {
                if let Ok(view) = self.view().await {
                    self.act(&id, &view, &mut misses).await;
                }
            }
            return self.paying_of(id).await;
        }
        let view = self.view().await?;
        let lock = self.on_hub(move |hub| hub.locked(&id)).await?;
        let expiry = lock
            .proposal
            .lock()
            .ok_or_else(|| refused("not a lock"))?
            .terms()
            .expiry();
        let (hash, node) = (*invoice.payment_hash().as_ref(), view.info.pubkey.clone());
        let least = match self.payable(&invoice, &view, expiry) {
            Ok(msat) => self.config.lightning(node.clone()).pay_lock(msat),
            Err(why) => return self.refuse(id, hash, request, node, why).await,
        };
        let (paid, node_of) = (request.clone(), node.clone());
        match self
            .on_hub(move |hub| hub.forward(&id, &hash, &paid, &node_of, least))
            .await
        {
            Ok(_) => {}
            Err(Error::Refused(Refusal::Fee { .. })) => {
                let why = "the lock pays less than the invoice and the gateway's fee";
                return self.refuse(id, hash, request, node, why).await;
            }
            Err(e) => return Err(e.into()),
        }
        tracing::info!(lock = %hex::encode(id.as_bytes()), least, "paying an invoice");
        self.act(&id, &view, &mut misses).await;
        drop(misses);
        self.paying_of(id).await
    }

    /// Refuse to pay `request` for lock `id` for `why`: the payment is
    /// recorded failed, which refunds the lock, if the engine would have
    /// forwarded it; refused as is otherwise.
    async fn refuse(
        &self,
        id: ProposalId,
        hash: [u8; 32],
        request: String,
        node: String,
        why: &'static str,
    ) -> Result<Paying, Failure> {
        let recorded = self
            .on_hub(move |hub| {
                hub.forward(&id, &hash, &request, &node, 0)?;
                hub.forward_failed(&id, why)
            })
            .await;
        match recorded {
            Ok(()) => {
                tracing::info!(lock = %hex::encode(id.as_bytes()), why, "invoice refused, lock refunded");
                self.paying_of(id).await
            }
            Err(Error::Refused(Refusal::Unsynced | Refusal::NoChain)) => {
                Err(super::unavailable("the hub is behind the chain"))
            }
            Err(_) => Err(refused(why)),
        }
    }

    /// Follow the payments under way.
    pub(super) async fn follow_payments(&self) -> Result<(), Failure> {
        let pending = self.on_hub(|hub| hub.forwards()).await?;
        if pending.is_empty() {
            return Ok(());
        }
        let view = match self.view().await {
            Ok(view) => view,
            Err(e) => {
                let tip = self
                    .on_hub(|hub| hub.scanned())
                    .await?
                    .map_or(0, |(h, _)| h);
                for forward in pending.iter().filter(|f| tip >= f.expiry) {
                    self.late(forward).await;
                }
                return Err(e);
            }
        };
        let mut misses = self.paying.lock().await;
        for forward in pending {
            self.act(&forward.lock, &view, &mut misses).await;
        }
        Ok(())
    }

    /// The msat an invoice asks for, if the gateway pays it, now at `view`,
    /// for a lock expiring at `expiry`.
    fn payable(
        &self,
        invoice: &Bolt11Invoice,
        view: &View,
        expiry: u32,
    ) -> Result<u64, &'static str> {
        let msat = invoice
            .amount_milli_satoshis()
            .filter(|msat| *msat > 0)
            .ok_or("an invoice without an amount")?;
        if msat > self.config.max_pay.saturating_mul(1000) {
            return Err("above the most the gateway pays");
        }
        if invoice.features().is_some_and(|f| {
            // AMP, bits 30 and 31, splits a payment under other hashes.
            f.requires_unknown_bits() || f.le_flags().get(3).is_some_and(|b| b & 0xc0 != 0)
        }) {
            return Err("an invoice of features the gateway does not pay");
        }
        if expires(invoice, self.config.pay_timeout) {
            return Err("the invoice expires");
        }
        if invoice.currency() != view.info.currency {
            return Err("an invoice of another network");
        }
        if hex::encode(invoice.get_payee_pub_key().serialize()) == view.info.pubkey {
            return Err("an invoice of the hub's own node");
        }
        if self.cltv_limit(expiry, view.tip, invoice).is_none() {
            return Err("the lock expires too soon for the invoice");
        }
        Ok(msat)
    }

    /// The timelock of routes paying `invoice` for a lock expiring at
    /// `expiry`, at height `tip`: ending [`super::Config::pay_slack`]
    /// blocks before, if that leaves the invoice its final delta.
    fn cltv_limit(&self, expiry: u32, tip: u32, invoice: &Bolt11Invoice) -> Option<u32> {
        let delta = u32::try_from(invoice.min_final_cltv_expiry_delta()).ok()?;
        let limit = expiry
            .checked_sub(tip)?
            .checked_sub(self.config.pay_slack)?;
        (limit >= delta.checked_add(CLTV_MARGIN)?).then_some(limit)
    }
}

impl Gateway {
    /// Act on the payment for lock `id` as the node knows it, while
    /// holding the payments: resolve it once the node paid or failed it,
    /// start it while it may still be paid in time, and give up one the
    /// node never started once too late. `misses` counts, by lock, the
    /// rounds in a row the node knew no attempt too late to start one.
    async fn act(&self, id: &ProposalId, view: &View, misses: &mut HashMap<[u8; 32], u32>) {
        let lock = *id;
        let forward = match self.on_hub(move |hub| hub.forward_of(&lock)).await {
            Ok(Some(forward)) if forward.state == Forwarded::Pending => forward,
            Ok(_) => {
                misses.remove(id.as_bytes());
                return;
            }
            Err(e) => return tracing::warn!(error = %e, "cannot read a payment"),
        };
        let name = hex::encode(id.as_bytes());
        if forward.node != view.info.pubkey {
            let message = format!(
                "The payment for lock {name} was started by Lightning node {}, not this one: \
                 resolve it by hand.",
                forward.node
            );
            return self.alert(format!("node {name}"), message).await;
        }
        if view.tip >= forward.expiry {
            self.late(&forward).await;
        }
        let attempt = match self.node.payment(forward.hash).await {
            Ok(attempt) => attempt,
            Err(e) => return tracing::warn!(error = %e, "the Lightning node does not answer"),
        };
        // The node keeps one attempt per hash: one started before the
        // forward was recorded is not the forward's.
        let recorded = u64::try_from(forward.created_at)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000);
        let known = match attempt {
            Some(Attempt {
                status: Status::Succeeded(preimage),
                ..
            }) => return self.paid(&forward, preimage).await,
            Some(Attempt {
                status: Status::InFlight,
                ..
            }) => return,
            Some(Attempt {
                status: Status::Failed(reason),
                created_ns,
            }) if created_ns >= recorded => {
                misses.remove(id.as_bytes());
                return self.failed(&forward, &reason).await;
            }
            known => known.is_some(),
        };
        if let Some((limit, fee_limit)) = self.resend(&forward, view) {
            misses.remove(id.as_bytes());
            let request = forward.request.clone();
            let timeout = self.config.pay_timeout;
            match self.node.pay(request, fee_limit, limit, timeout).await {
                Ok(()) => tracing::info!(lock = %name, limit, fee_limit, "payment started"),
                Err(e) => tracing::warn!(lock = %name, error = %e, "cannot start a payment"),
            }
            return;
        }
        // Too late to start: the forward never started is given up, once
        // the node shows that twice after the grace period.
        let missed = misses.entry(*id.as_bytes()).or_default();
        *missed += 1;
        let waited = now().saturating_sub(recorded / 1_000_000_000) >= self.config.grace;
        if known || (waited && *missed >= 2) {
            misses.remove(id.as_bytes());
            self.failed(&forward, "not started in time").await;
        }
    }

    /// The timelock and fee limit to start `forward` with, if it can
    /// still be paid in time.
    fn resend(&self, forward: &Forward, view: &View) -> Option<(u32, u64)> {
        let invoice: Bolt11Invoice = forward.request.parse().ok()?;
        if expires(&invoice, self.config.pay_timeout) {
            return None;
        }
        let limit = self.cltv_limit(forward.expiry, view.tip, &invoice)?;
        let sat = invoice.amount_milli_satoshis()?.div_ceil(1000);
        let fee_limit = self
            .config
            .pay
            .due(sat)
            .min(forward.amount.saturating_sub(sat));
        Some((limit, fee_limit))
    }

    async fn paid(&self, forward: &Forward, preimage: [u8; 32]) {
        let (id, name) = (forward.lock, hex::encode(forward.lock.as_bytes()));
        match self
            .on_hub(move |hub| hub.forward_paid(&id, &preimage))
            .await
        {
            Ok(()) => tracing::info!(lock = %name, "invoice paid, lock settled"),
            Err(Error::Refused(Refusal::Preimage)) => {
                let message = format!(
                    "The Lightning node paid for lock {name} with a preimage not of its hash."
                );
                self.alert(format!("preimage {name}"), message).await;
            }
            // Told failed before, the engine alerted.
            Err(e) => tracing::error!(lock = %name, error = %e, "cannot settle a paid lock"),
        }
    }

    async fn failed(&self, forward: &Forward, reason: &str) {
        let (id, name) = (forward.lock, hex::encode(forward.lock.as_bytes()));
        let why = reason.to_owned();
        match self.on_hub(move |hub| hub.forward_failed(&id, &why)).await {
            Ok(()) => tracing::warn!(lock = %name, reason, "payment failed, lock refunded"),
            // Paid by its preimage, which resolved the payment.
            Err(Error::Refused(Refusal::Settled)) => {}
            Err(e) => tracing::error!(lock = %name, error = %e, "cannot refund a failed payment"),
        }
    }

    /// Alert once of `forward` unresolved past its lock's expiry.
    async fn late(&self, forward: &Forward) {
        let name = hex::encode(forward.lock.as_bytes());
        let message = format!(
            "The payment for lock {name} is unresolved past the lock's expiry at block {}.",
            forward.expiry
        );
        self.alert(format!("late {name}"), message).await;
    }

    async fn paying_of(&self, id: ProposalId) -> Result<Paying, Failure> {
        let forward = self.on_hub(move |hub| hub.forward_of(&id)).await?;
        let forward = forward.ok_or(Failure(Problem::new(Code::UnknownLock, "no payment")))?;
        let (state, preimage, reason) = match forward.state {
            Forwarded::Paid(preimage) => (PayState::Paid, Some(Hex(preimage.to_vec())), None),
            Forwarded::Failed(reason) => (PayState::Failed, None, Some(reason)),
            _ => (PayState::Pending, None, None),
        };
        Ok(Paying {
            state,
            preimage,
            reason,
        })
    }
}

/// Whether `invoice` expires within `timeout` seconds and a margin.
fn expires(invoice: &Bolt11Invoice, timeout: u32) -> bool {
    let at = now().saturating_add(u64::from(timeout) + EXPIRY_MARGIN);
    invoice.would_expire(Duration::from_secs(at))
}
