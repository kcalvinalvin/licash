//! A [`Wallet`]'s payments through mailboxes: paying a payment code, and
//! taking what its own mailbox holds; and the swap providers announced
//! that swap between two of its hubs.

use std::{collections::HashSet, time::Duration};

use anyhow::bail;
use futures_util::future::join_all;
use licash_api::Payment;
use licash_wallet::{
    Error, Paid, PaymentCode, Received, SwapLimits, SwapQuote, SwapState, Transport, Wallet,
};

use crate::{mailbox, Role, Route};

/// How long a swap provider announced has to answer at a URL.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// A payment made to a payment code.
#[derive(Debug)]
pub struct CodePaid {
    pub paid: Paid,
    pub hub: String,
    pub amount: u64,
    /// How many of the hub's relays took it for the payee's mailbox: none
    /// if it was not sent there, and must be handed over.
    pub delivered: Option<usize>,
    /// Why it was not sent to the mailbox the code names.
    pub undelivered: Option<String>,
}

/// Pay `amount`, or the amount the code asks, to the wallet of `code` in
/// notes, at a hub it names that `wallet` trusts and holds enough at, then
/// send the payment to the payee's mailbox on that hub's relays, reached
/// through `route`. Refused, saying where to pay on Bitcoin if the code
/// says, when no such hub is named.
pub async fn pay<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
    route: impl FnOnce() -> anyhow::Result<Route>,
) -> anyhow::Result<CodePaid> {
    code.check(wallet.network())?;
    let amount = match (amount, code.amount) {
        (Some(amount), Some(asked)) if amount != asked => {
            bail!("the code asks for {asked}, not {amount}")
        }
        (Some(amount), _) | (None, Some(amount)) => amount,
        (None, None) => bail!("how much to pay: the code does not say"),
    };
    let hub = match &code.licash {
        Some(licash) => wallet.hub_to_pay(licash, amount)?,
        None => None,
    };
    let (Some(licash), Some(hub)) = (&code.licash, hub) else {
        let none = format!("no hub it names is one trusted holding {amount} and the fee");
        let onchain = match (&code.silent, &code.address) {
            (Some(silent), _) => Some(silent.to_string()),
            (None, Some(address)) => Some(address.assume_checked_ref().to_string()),
            (None, None) => None,
        };
        match onchain {
            Some(to) => bail!("{none}: pay {to} on Bitcoin"),
            None => bail!("{none}"),
        }
    };
    let paid = wallet.send(&hub, &licash.address, amount).await?;
    let relays = wallet
        .hubs()?
        .into_iter()
        .find(|(url, _)| *url == hub)
        .map(|(_, info)| info.relays)
        .unwrap_or_default();
    let (delivered, undelivered) = match licash.mailbox {
        Some(to) if !relays.is_empty() => {
            let content = serde_json::to_string(&paid.payment)?;
            let sent = async { mailbox::send(&to, &content, &relays, &route()?).await };
            match sent.await {
                Ok(relays) => (Some(relays), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }
        Some(_) => (None, Some(format!("hub {hub} names no mailbox relays"))),
        None => (None, None),
    };
    Ok(CodePaid {
        paid,
        hub,
        amount,
        delivered,
        undelivered,
    })
}

/// What [`collect`] found in the mailbox.
#[derive(Debug, Default)]
pub struct Collected {
    /// The payments taken, oldest first.
    pub received: Vec<Received>,
    /// Mail not taken, by its identifier, and why.
    pub skipped: Vec<(String, String)>,
}

/// Take the payments in `wallet`'s mailbox, on the relays its hubs name,
/// reached through `route`: each once, whenever asked again.
pub async fn collect<T: Transport>(wallet: &Wallet<T>, route: &Route) -> anyhow::Result<Collected> {
    let relays = hub_relays(wallet)?;
    if relays.is_empty() {
        bail!("no hub trusted names mailbox relays: adding a hub again learns them");
    }
    let mut collected = Collected::default();
    for mail in mailbox::fetch(&wallet.mailbox_secret(), &relays, route).await? {
        let Ok(payment) = serde_json::from_str::<Payment>(&mail.content) else {
            collected.skipped.push((mail.id, "is not a payment".into()));
            continue;
        };
        match wallet.receive(&payment) {
            Ok(received) => collected.received.push(received),
            Err(Error::AlreadyReceived) => {}
            Err(e) => collected.skipped.push((mail.id, format!("not taken: {e}"))),
        }
    }
    Ok(collected)
}

/// The relays `wallet`'s hubs name, each once, in the order of its hubs.
pub fn hub_relays<T: Transport>(wallet: &Wallet<T>) -> Result<Vec<String>, Error> {
    let mut relays: Vec<String> = Vec::new();
    for (_, info) in wallet.hubs()? {
        for relay in info.relays {
            if !relays.contains(&relay) {
                relays.push(relay);
            }
        }
    }
    Ok(relays)
}

/// A swap provider announced on Nostr, and what it asks for a swap.
#[derive(Clone, Debug)]
pub struct SwapProvider {
    /// The first URL announced that answered as the provider.
    pub url: String,
    /// The key that announced it, x-only in hex.
    pub key: String,
    pub name: Option<String>,
    pub quote: SwapQuote,
    /// Whether the wallet swapped through it before.
    pub known: bool,
}

/// The swap providers announced on `relays` for `wallet`'s network, asked
/// through `route`, that swap `amount` from hub `from` to hub `to`, both
/// added, within `limits` ([`Wallet::swap_quote`]): each at the first URL
/// it was announced at that answers as the key that announced it. Those
/// the wallet swapped through before first, then the cheapest, then the
/// soonest to free its notes should they never lock.
pub async fn swap_providers<T: Transport>(
    wallet: &Wallet<T>,
    relays: &[String],
    route: &Route,
    (from, to): (&str, &str),
    amount: u64,
    limits: SwapLimits,
) -> anyhow::Result<Vec<SwapProvider>> {
    let network = wallet.network().name();
    let found = crate::discover(relays, route, Role::Swaps, network).await?;
    let used: HashSet<String> = wallet
        .swaps()?
        .into_iter()
        .filter(|s| s.state == SwapState::Done)
        .map(|s| s.provider)
        .collect();
    let used = &used;
    let asked = found.into_iter().map(|service| async move {
        for url in &service.announcement.urls {
            let quote = wallet.swap_quote(url, from, to, amount, limits);
            match tokio::time::timeout(ANSWER_TIMEOUT, quote).await {
                Ok(Ok(quote)) if service.named_by(quote.info.nostr.as_deref()) => {
                    let urls = service.announcement.urls.iter();
                    let known = urls
                        .map(|u| u.trim().trim_end_matches('/'))
                        .any(|u| used.contains(u));
                    return Ok(Some(SwapProvider {
                        url: url.clone(),
                        key: service.key,
                        name: service.announcement.name,
                        quote,
                        known,
                    }));
                }
                Ok(Ok(_)) => tracing::warn!(%url, "skipped: not the provider that announced it"),
                // Not a provider of the wallet's to ask: none is.
                Ok(Err(e @ Error::UnknownHub(_))) => return Err(e),
                Ok(Err(e)) => tracing::debug!(%url, "skipped: {e}"),
                Err(_) => tracing::debug!(%url, "skipped: no answer in time"),
            }
        }
        Ok(None)
    });
    let mut providers = Vec::new();
    for asked in join_all(asked).await {
        providers.extend(asked?);
    }
    // Stable: the newest announced first among equals.
    providers.sort_by_key(|p| (!p.known, p.quote.fee, p.quote.info.margin));
    Ok(providers)
}
