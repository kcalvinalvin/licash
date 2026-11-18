//! Payments out: invoices the gateway pays for locks that pay the hub.

use std::collections::HashMap;

use licash::spend::ProposalId;
use licash_api::{Code, Hex, Pay, PayState, Paying, Problem};
use licash_hub::{Error, Forward, Forwarded, Refusal, LOCAL};
use licash_lightning::{expires, judge, unpayable, Judged};
use lightning_invoice::Bolt11Invoice;

use super::{now, refused, Gateway, View};
use crate::Failure;

/// Why an invoice is refused while the hub's payments out are paused
/// ([`crate::payouts`]).
const PAUSED: &str = "the gateway's payments out are paused";

/// Why an invoice is refused past [`Config::max_pay_day`].
const OVER_THE_DAY: &str = "above the most the gateway pays in a day";

/// The seconds [`Config::max_pay_day`] is counted over.
const DAY: u64 = 24 * 60 * 60;

impl Gateway {
    /// Pay the invoice of `body` for its lock, or say where its payment
    /// stands. An invoice the gateway does not pay for the lock fails the
    /// payment at once, which refunds the lock, if the lock is of the
    /// invoice's hash and pays the hub alone. An invoice of the gateway's
    /// own node is paid inside the hub, if it is a receipt's still open.
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
            match forward.state {
                Forwarded::Pending if forward.node == LOCAL => {
                    self.resume_local(forward.hash).await
                }
                Forwarded::Pending => {
                    if let Ok(view) = self.view().await {
                        self.act(&id, &view, &mut misses).await;
                    }
                }
                _ => {}
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
        if hex::encode(invoice.get_payee_pub_key().serialize()) == node {
            return self.fulfill(id, &invoice, &view, expiry).await;
        }
        if self.paused() {
            return self.refuse(id, hash, request, node, PAUSED).await;
        }
        let least = match self.payable(&invoice, &view, expiry) {
            Ok(msat) => self.config.lightning(node.clone()).pay_lock(msat),
            Err(why) => return self.refuse(id, hash, request, node, why).await,
        };
        if self.over_the_day(least).await? {
            return self.refuse(id, hash, request, node, OVER_THE_DAY).await;
        }
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
    /// forwarded it; refused as is otherwise. Never recorded pending, it
    /// is never started.
    pub(super) async fn refuse(
        &self,
        id: ProposalId,
        hash: [u8; 32],
        request: String,
        node: String,
        why: &'static str,
    ) -> Result<Paying, Failure> {
        let recorded = self
            .on_hub(move |hub| hub.forward_refused(&id, &hash, &request, &node, why))
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

    /// Whether a lock paying the hub `amount` for an invoice would take
    /// what the gateway paid out in the last 24 hours, and may yet pay,
    /// past [`Config::max_pay_day`]. The operator is alerted, once a day:
    /// the limit is too low for the hub's holders, or someone drains it.
    async fn over_the_day(&self, amount: u64) -> Result<bool, Failure> {
        let Some(most) = self.config.max_pay_day else {
            return Ok(false);
        };
        let now = now();
        let since = i64::try_from(now.saturating_sub(DAY)).unwrap_or(i64::MAX);
        let paid = self.on_hub(move |hub| hub.forwarded_since(since)).await?;
        if paid.saturating_add(amount) <= most {
            return Ok(false);
        }
        let message = format!(
            "The Lightning gateway refuses invoices: the {paid} sat it paid out in the last 24 \
             hours leave no room for {amount} more within --ln-max-pay-day {most}. Their locks \
             are refunded, until earlier payments are a day old."
        );
        self.alert(format!("day {}", now / DAY), message).await;
        Ok(true)
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
        if unpayable(invoice) {
            return Err("an invoice of features the gateway does not pay");
        }
        if expires(invoice, self.config.pay_timeout) {
            return Err("the invoice expires");
        }
        if invoice.currency() != view.info.currency {
            return Err("an invoice of another network");
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
        licash_lightning::cltv_limit(expiry, tip, self.config.pay_slack, invoice)
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
        // Paid inside the hub, by its receipt's claim: no node pays it.
        if forward.node == LOCAL {
            misses.remove(id.as_bytes());
            if view.tip >= forward.expiry {
                self.late(&forward).await;
            }
            return;
        }
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
        let recorded = u64::try_from(forward.created_at).unwrap_or(0);
        let known = match judge(attempt, recorded) {
            Judged::Paid(preimage) => return self.paid(&forward, preimage).await,
            Judged::InFlight => return,
            Judged::Failed(reason) => {
                misses.remove(id.as_bytes());
                return self.failed(&forward, &reason).await;
            }
            Judged::Unstarted { known } => known,
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
        let waited = now().saturating_sub(recorded) >= self.config.grace;
        if known || (waited && *missed >= 2) {
            misses.remove(id.as_bytes());
            self.failed(&forward, "not started in time").await;
        }
    }

    /// The timelock and fee limit to start `forward` with, if it can
    /// still be paid in time, and is one the gateway pays, for no more
    /// than its lock pays the hub. A forward recorded pending though
    /// refused, as a refusal was recorded before it failed at once, is
    /// never started: given up as one too late. Nor is any while the
    /// hub's payments out are paused.
    fn resend(&self, forward: &Forward, view: &View) -> Option<(u32, u64)> {
        if self.paused() {
            return None;
        }
        let invoice: Bolt11Invoice = forward.request.parse().ok()?;
        let msat = self.payable(&invoice, view, forward.expiry).ok()?;
        let limit = self.cltv_limit(forward.expiry, view.tip, &invoice)?;
        let sat = msat.div_ceil(1000);
        if sat > forward.amount {
            return None;
        }
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

    pub(super) async fn paying_of(&self, id: ProposalId) -> Result<Paying, Failure> {
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
