//! What the wallet agreed to pay the hubs of its recovery contracts: the
//! fees each charged as a contract opened there. A hub may charge more
//! whenever it likes, and is then paid nothing ([`LicashError::FeesRaised`])
//! until the user approves what it charges now.

use licash_contracts::holder;

use super::Wallet;
use crate::{Fees, FeesRaised, LicashError, Rate};

#[uniffi::export]
impl Wallet {
    /// The hubs trusted charging more than agreed, as the wallet last
    /// heard each, with what was agreed: to show before
    /// [`Wallet::approve_fees`]. A payment refused for it asked its hub
    /// anew first.
    pub fn fees_raised(&self) -> Result<Vec<FeesRaised>, LicashError> {
        let mut raised = Vec::new();
        for (url, info) in self.wallet.hubs()? {
            match self.wallet.fee_ceilings(&url)? {
                Some(agreed) if info.fees.above(&agreed) => raised.push(FeesRaised {
                    hub: url,
                    charged: shown(&info.fees),
                    agreed: shown(&agreed),
                }),
                _ => {}
            }
        }
        Ok(raised)
    }

    /// Agree to pay `hub` up to `charged`, what [`Wallet::fees_raised`]
    /// showed of it: no more, if it charges more since. Kept with the
    /// wallet's contracts there.
    pub fn approve_fees(&self, hub: String, charged: Fees) -> Result<(), LicashError> {
        let fees = licash_api::Fees {
            spend: fee(charged.spend),
            lock: fee(charged.lock),
            refund: fee(charged.refund),
            output_msat: charged.output_msat,
        };
        let hub = hub.trim().trim_end_matches('/');
        if self.wallet.hub_urls(hub)?.is_empty() {
            return Err(LicashError::invalid(format!("{hub} is no hub you use")));
        }
        holder::fees::approve(
            &self.wallet,
            self.wallet.path(),
            hub,
            &fees,
            self.known_tip(),
        )
        .map_err(|e| LicashError::from_any(e, |reason| LicashError::Failed { reason }))
    }
}

impl Wallet {
    /// Hold each hub to the fees the wallet's contracts there opened
    /// under, or approved since: as the wallet opens. Contracts that
    /// cannot be read hold no hub to any, as none do.
    pub(super) fn agree_fees(&self) {
        if let Err(e) = holder::fees::agree(&self.wallet, self.wallet.path(), self.known_tip()) {
            tracing::warn!("what the contracts' hubs may charge: {e:#}");
        }
    }

    /// The chain's height ([`Wallet::tip`]), if known.
    fn known_tip(&self) -> Option<u32> {
        Some(self.tip()).filter(|tip| *tip > 0)
    }
}

fn shown(fees: &licash_api::Fees) -> Fees {
    let rate = |fee: licash_api::Fee| Rate {
        base: fee.base,
        ppm: fee.ppm,
    };
    Fees {
        spend: rate(fees.spend),
        lock: rate(fees.lock),
        refund: rate(fees.refund),
        output_msat: fees.output_msat,
    }
}

fn fee(rate: Rate) -> licash_api::Fee {
    licash_api::Fee {
        base: rate.base,
        ppm: rate.ppm,
    }
}
