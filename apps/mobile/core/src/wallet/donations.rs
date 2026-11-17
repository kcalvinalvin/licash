//! The wallet's static address of donations: its code as an LNURL of a
//! swap provider announced on Nostr that takes donations at a hub of it.
//! Any Lightning wallet pays it, again and again, while the phone is
//! offline: the provider pays each donation in notes at the code's hub,
//! kept there for the wallet or mailed to it.

use licash_nostr::wallet::{donation_providers_with, hub_relays};

use super::Wallet;
use crate::{DonationAddress, LicashError};

#[uniffi::export]
impl Wallet {
    /// The wallet's static address of donations through the provider that
    /// suits best ([`donation_providers_with`]): none if no provider
    /// announced on the relays the hubs name takes donations at a hub the
    /// wallet trusts. The same while the hubs and the providers stay.
    pub fn donation_address(&self) -> Result<Option<DonationAddress>, LicashError> {
        let relays = hub_relays(&self.wallet)?;
        if relays.is_empty() {
            return Ok(None);
        }
        let found = donation_providers_with(&self.wallet, &self.announced, (&relays, &self.route));
        let found = self
            .runtime
            .block_on(found)
            .map_err(|e| LicashError::from_any(e, |reason| LicashError::Unavailable { reason }))?;
        Ok(found.into_iter().next().map(|p| {
            let donations = p.address.donations;
            DonationAddress {
                lnurl: p.address.lnurl,
                provider: p.url,
                name: p.name,
                fee_base: donations.fee.base,
                fee_ppm: donations.fee.ppm,
                min: donations.min,
                max: donations.max,
                stocked: p.address.stocked,
            }
        }))
    }
}
