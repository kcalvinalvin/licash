//! Lightning invoices paid through swap providers announced on Nostr: the
//! provider pays an invoice from its own node for a lock of the wallet's
//! notes at a hub, and claims the lock with the preimage the payment buys.
//! The app compares them with the gateways of the hubs it trusts and pays
//! the cheapest. A payment through a provider that fails returns its notes
//! only once its lock expires, which the quote tells.

use licash_nostr::wallet::{hub_relays, invoice_providers_with};
use licash_wallet::{Payable, SwapLimits};

use super::Wallet;
use crate::{Activity, InvoiceQuote, LicashError, ViaProvider};

/// Lightning invoices through providers.
#[uniffi::export]
impl Wallet {
    /// The ways to pay the Lightning invoice `text` with the notes held
    /// now, the cheapest first: through the gateway of each hub trusted
    /// that pays it, and through each swap provider announced on the
    /// relays the hubs name that pays it for a lock at a hub holding
    /// enough. Among equals a hub's gateway first, whose failed payments
    /// return their notes at once. Refused if none can.
    pub fn invoice_offers(&self, text: String) -> Result<Vec<InvoiceQuote>, LicashError> {
        let _timed = crate::log::timed("invoice_offers");
        let invoice = self.wallet.read_invoice(&text)?;
        let (mut offers, refused) = self.gateway_offers(&invoice)?;
        offers.extend(self.provider_offers(&invoice)?);
        // Stable: gateways, listed first, first among equals.
        offers.sort_by_key(|o| o.fee);
        if !offers.is_empty() {
            return Ok(offers);
        }
        Err(match refused {
            Some(e) => e.into(),
            None => LicashError::invalid("no hub you trust pays Lightning invoices"),
        })
    }

    /// Pay the Lightning invoice `text` through the swap provider at
    /// `provider`, from notes at `hub`, its fee at most `max_fee`, as
    /// [`Wallet::invoice_offers`] quoted it: under way until
    /// [`Wallet::resume`] finds its lock paid, or refunded once expired
    /// if the payment failed.
    pub fn pay_invoice_via(
        &self,
        text: String,
        hub: String,
        provider: String,
        max_fee: u64,
    ) -> Result<Activity, LicashError> {
        let _timed = crate::log::timed("pay_invoice_via");
        let invoice = self.wallet.read_invoice(&text)?;
        let pay = self
            .wallet
            .pay_invoice_via(&provider, &hub, &invoice.request, max_fee);
        let lock = self.operation(pay)?;
        Ok(super::lightning_activity(&lock, &invoice.request))
    }
}

impl Wallet {
    /// What paying `invoice` costs through the gateway of each hub trusted
    /// that pays it, and why one refused: lacking notes says the most.
    pub(super) fn gateway_offers(
        &self,
        invoice: &Payable,
    ) -> Result<(Vec<InvoiceQuote>, Option<licash_wallet::Error>), LicashError> {
        let (mut offers, mut refused) = (Vec::new(), None::<licash_wallet::Error>);
        for (url, info) in self.wallet.hubs()? {
            if info.lightning.is_none() {
                continue;
            }
            match self.wallet.invoice_fee(&url, &invoice.request) {
                Ok(fee) => offers.push(InvoiceQuote {
                    hub: url,
                    amount: invoice.amount,
                    fee,
                    description: invoice.description.clone(),
                    provider: None,
                }),
                Err(e)
                    if refused.is_none()
                        || matches!(e, licash_wallet::Error::Insufficient { .. }) =>
                {
                    refused = Some(e)
                }
                Err(_) => {}
            }
        }
        Ok((offers, refused))
    }

    /// What paying `invoice` costs through each swap provider announced
    /// that pays it, from each hub trusted holding at least what it asks:
    /// none if no hub names relays, or none answers.
    fn provider_offers(&self, invoice: &Payable) -> Result<Vec<InvoiceQuote>, LicashError> {
        let relays = hub_relays(&self.wallet)?;
        if relays.is_empty() {
            return Ok(Vec::new());
        }
        let max_fee = SwapLimits::usual(invoice.amount).fee;
        let mut offers = Vec::new();
        for balance in self.wallet.balance()? {
            if balance.available <= invoice.amount {
                continue;
            }
            let found = invoice_providers_with(
                &self.wallet,
                &self.announced,
                (&relays, &self.route),
                &balance.hub,
                &invoice.request,
                max_fee,
            );
            let found = match self.operation(found) {
                Ok(found) => found,
                Err(e) => {
                    tracing::info!(hub = %balance.hub, "no provider pays the invoice: {e:#}");
                    continue;
                }
            };
            offers.extend(found.into_iter().map(|p| InvoiceQuote {
                hub: balance.hub.clone(),
                amount: invoice.amount,
                fee: p.quote.costs(),
                description: invoice.description.clone(),
                provider: Some(ViaProvider {
                    url: p.url,
                    name: p.name,
                    fee: p.quote.fee,
                    lock_fee: p.quote.lock_fee,
                    refund_blocks: p.quote.refund_blocks,
                }),
            }));
        }
        Ok(offers)
    }
}
