//! A [`Wallet`]'s payments through mailboxes: paying a payment code, and
//! taking what its own mailbox holds; and the swap providers announced
//! that swap between two of its hubs, or pay notes out on Bitcoin.

use std::{collections::HashSet, future::Future, time::Duration};

use anyhow::bail;
use bitcoin::FeeRate;
use futures_util::future::join_all;
use licash_api::{Info, Payment};
use licash_wallet::{
    Error, HubId, Paid, PaymentCode, PayoutQuote, PayoutState, Received, SwapLimits, SwapQuote,
    SwapState, Transport, Wallet,
};

use crate::{mailbox, Found, Role, Route};

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
    let amount = asked(wallet, code, amount)?;
    let hub = match &code.licash {
        Some(licash) => wallet.hub_to_pay(licash, amount)?,
        None => None,
    };
    let Some(hub) = hub else {
        return Err(unpayable(code, amount));
    };
    send(wallet, code, hub, amount, route).await
}

/// How [`pay_planned`] pays a payment code in notes.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The hub to pay at: one the code names.
    pub hub: String,
    pub amount: u64,
    /// The hub's fee on the notes the payment spends: at most this once a
    /// swap brought more.
    pub fee: u64,
    /// The swap bringing what the wallet lacks there first, if it does.
    pub swap: Option<PlannedSwap>,
    /// Whether the wallet trusts the hub, else only visited it
    /// ([`Wallet::visit`]): what the swap brings past the payment stays
    /// there, counted apart, until the owner adds it.
    pub trusted: bool,
}

/// A swap to make before paying ([`Plan::swap`]).
#[derive(Clone, Debug)]
pub struct PlannedSwap {
    /// The hub swapped from: the wallet's other one holding the most.
    pub from: String,
    pub provider: SwapProvider,
    /// The fee of the hub swapped from on the swap's lock.
    pub lock_fee: u64,
}

impl Plan {
    /// Every fee it pays: the hub's on the payment, and on a swap the
    /// provider's and the lock's.
    pub fn fees(&self) -> u64 {
        let swap = self
            .swap
            .as_ref()
            .map_or(0, |s| s.provider.quote.fee.saturating_add(s.lock_fee));
        self.fee.saturating_add(swap)
    }
}

/// How to pay `amount`, or the amount the code asks, to the wallet of
/// `code` in notes: at a hub it names that `wallet` trusts and holds
/// enough at, else at one it names and `wallet` trusts once a swap brings
/// what the wallet lacks there, from its other hub holding the most,
/// through a provider announced on the relays its hubs name, reached
/// through `route` ([`swap_providers`]), within [`SwapLimits::usual`];
/// else so at a hub the code says where to reach, visited
/// ([`Wallet::visit`]) as the hub it names, by a swap bringing at most
/// twice what the wallet lacks there.
/// Refused as [`pay`] refuses when neither is possible, or while a swap
/// to that hub is under way.
pub async fn plan<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
    route: &Route,
) -> anyhow::Result<Plan> {
    let mut plans = plans(wallet, code, amount, route).await?;
    Ok(plans.remove(0))
}

/// The ways [`plan`] finds of paying `code`: the one it picks first, then
/// those through each other swap provider it could pick instead, in the
/// order it prefers them. Never none: refused as [`plan`] is.
pub async fn plans<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
    route: &Route,
) -> anyhow::Result<Vec<Plan>> {
    let amount = asked(wallet, code, amount)?;
    let Some(licash) = &code.licash else {
        return Err(unpayable(code, amount));
    };
    if let Some(hub) = wallet.hub_to_pay(licash, amount)? {
        let fee = wallet.send_fee(&hub, amount)?;
        return Ok(vec![Plan {
            hub,
            amount,
            fee,
            swap: None,
            trusted: true,
        }]);
    }
    let relays = hub_relays(wallet)?;
    if relays.is_empty() {
        tracing::info!("no swap: no hub trusted names relays to find a provider on");
        return Err(unpayable(code, amount));
    }
    // Swaps lock at both hubs: of those following a chain.
    let hubs: Vec<_> = wallet
        .hubs()?
        .into_iter()
        .filter(|(_, i)| i.locks.is_some())
        .collect();
    let mut held = wallet.balance()?;
    held.extend(wallet.visited_balance()?);
    let available = |hub: &str| {
        let held = held.iter().find(|b| b.hub == hub);
        held.map_or(0, |b| b.available)
    };
    // Pass 1, the hubs it names that the wallet trusts; pass 2, those it
    // says where to reach, visited, proving they are the ones it names.
    let named = licash.hubs.iter().map(|id| (*id, None));
    let located = licash.locations.iter().map(|(id, url)| (*id, Some(url)));
    for (id, at) in named.chain(located) {
        let trusted = hubs.iter().find(|(_, i)| HubId::of(&i.address) == id);
        let (to, to_info, trusted) = match (trusted, at) {
            (Some((to, info)), None) => (to.clone(), info.clone(), true),
            (None, Some(url)) => match visit(wallet, url, id).await {
                Some((to, info)) => (to, info, false),
                None => continue,
            },
            _ => continue,
        };
        let to = &to;
        if swapping_to(wallet, to)? {
            return Err(under_way(to, None));
        }
        let lacks = wallet.lacks(to, amount)?;
        // Held enough at a hub visited, a swap there brought before.
        if lacks == 0 {
            return Ok(vec![Plan {
                hub: to.clone(),
                amount,
                fee: wallet.send_fee(to, amount)?,
                swap: None,
                trusted,
            }]);
        }
        let others = hubs.iter().map(|(url, _)| url).filter(|url| *url != to);
        let Some(from) = others.max_by_key(|url| available(url)) else {
            continue;
        };
        let limits = SwapLimits::usual(lacks);
        let found = swap_providers(wallet, &relays, route, (from, to), lacks, limits).await;
        let found = match found {
            Ok(found) => found,
            Err(e) => {
                tracing::info!(%from, %to, "no swap: {e:#}");
                continue;
            }
        };
        let mut plans = Vec::new();
        for provider in found {
            let quote = &provider.quote;
            // Left at a hub not trusted: not much more than needed.
            if !trusted && quote.amount > lacks.saturating_mul(2) {
                tracing::info!(url = %provider.url, swaps = quote.amount, lacks, "skipped: brings too much to a hub not trusted");
                continue;
            }
            let lock_fee = match wallet.lock_fee(from, quote.amount.saturating_add(quote.fee)) {
                Ok(fee) => fee,
                Err(Error::Insufficient { .. }) => continue,
                // None found yet: refused, as when it was the first.
                Err(e) if plans.is_empty() => return Err(e.into()),
                Err(e) => {
                    tracing::info!(url = %provider.url, "skipped: {e}");
                    continue;
                }
            };
            // At most all it holds there then.
            let fee = to_info
                .fees
                .spend
                .due(available(to).saturating_add(quote.amount));
            plans.push(Plan {
                hub: to.clone(),
                amount,
                fee,
                swap: Some(PlannedSwap {
                    from: from.clone(),
                    provider,
                    lock_fee,
                }),
                trusted,
            });
        }
        if !plans.is_empty() {
            return Ok(plans);
        }
        tracing::info!(%from, %to, lacks, "no swap: no provider found that it can pay");
    }
    Err(unpayable(code, amount))
}

/// The hub of `id`, one the wallet does not trust, visited where it was
/// before, else at `url`, where the code says to reach it: as the wallet
/// reaches it, and what it said of itself, once it proves it is that hub.
async fn visit<T: Transport>(wallet: &Wallet<T>, url: &str, id: HubId) -> Option<(String, Info)> {
    let found = || {
        let visited = wallet.visited().ok()?;
        visited
            .into_iter()
            .find(|(_, i)| HubId::of(&i.address) == id)
    };
    let kept = found().map(|(url, _)| url);
    let same =
        |a: &str| kept.as_deref().map(|k| k.trim_end_matches('/')) == Some(a.trim_end_matches('/'));
    let code = Some(url).filter(|url| !same(url));
    for at in kept.as_deref().into_iter().chain(code) {
        match wallet.visit(at, id).await {
            Ok(_) => return found(),
            Err(e) => tracing::info!(url = %at, "not visited: {e}"),
        }
    }
    None
}

/// Refused unless the hub of `plan`, if visited, visited again, proves it
/// is the one visited and charges no more than planned.
async fn charges_as_planned<T: Transport>(wallet: &Wallet<T>, plan: &Plan) -> anyhow::Result<()> {
    let visited = wallet.visited()?;
    let Some((_, kept)) = visited.into_iter().find(|(url, _)| *url == plan.hub) else {
        // Added since: trusted.
        return Ok(());
    };
    let info = wallet.visit(&plan.hub, HubId::of(&kept.address)).await?;
    let held = wallet.visited_balance()?;
    let held = held
        .into_iter()
        .find(|b| b.hub == plan.hub)
        .map_or(0, |b| b.available);
    let swapped = plan.swap.as_ref().map_or(0, |s| s.provider.quote.amount);
    let fee = info.fees.spend.due(held.saturating_add(swapped));
    if fee > plan.fee {
        bail!(
            "nothing paid: hub {} charges {fee} now, more than the {} planned",
            plan.hub,
            plan.fee
        );
    }
    Ok(())
}

/// Pay as `plan` says, to the wallet of `code`: swap first if it says, on
/// the provider's terms as quoted, then pay as [`pay`] does at the hub it
/// names. Refused, nothing paid, if the swap fails or is still under way:
/// paying again once it is done pays from what it brought.
pub async fn pay_planned<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    plan: &Plan,
    route: impl FnOnce() -> anyhow::Result<Route>,
) -> anyhow::Result<CodePaid> {
    let amount = asked(wallet, code, Some(plan.amount))?;
    if !plan.trusted {
        charges_as_planned(wallet, plan).await?;
    }
    if let Some(swap) = &plan.swap {
        let (provider, hub) = (&swap.provider, &plan.hub);
        let quote = &provider.quote;
        if swapping_to(wallet, hub)? {
            return Err(under_way(hub, None));
        }
        let limits = SwapLimits {
            fee: quote.fee,
            margin: quote.info.margin,
        };
        let swapped = wallet.swap(&provider.url, &swap.from, hub, quote.amount, limits);
        let swapped = swapped.await.map_err(|e| match e {
            Error::Pending { reason, .. } => under_way(hub, Some(&reason)),
            e => e.into(),
        })?;
        match swapped.state {
            SwapState::Done => {}
            SwapState::Failed(why) => bail!(
                "nothing paid: the swap to {hub} failed ({why}), its lock returns once expired"
            ),
            _ => return Err(under_way(hub, None)),
        }
    }
    send(wallet, code, plan.hub.clone(), amount, route).await
}

/// Whether a swap to the hub at `hub` is under way.
fn swapping_to<T: Transport>(wallet: &Wallet<T>, hub: &str) -> Result<bool, Error> {
    let swaps = wallet.swaps()?.into_iter();
    Ok(swaps
        .filter(|s| s.state == SwapState::Open)
        .any(|s| s.to == hub))
}

/// Nothing paid while a swap to `hub` is under way, for `reason` if known.
fn under_way(hub: &str, reason: Option<&str>) -> anyhow::Error {
    let why = reason.map_or(String::new(), |r| format!(" ({r})"));
    anyhow::anyhow!(
        "nothing paid: a swap to {hub} is under way{why}: once `resume` finishes it, pay again"
    )
}

/// What the payer of `code` pays: `amount`, if the code asks no other.
fn asked<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
) -> anyhow::Result<u64> {
    code.check(wallet.network())?;
    Ok(match (amount, code.amount) {
        (Some(amount), Some(asked)) if amount != asked => {
            bail!("the code asks for {asked}, not {amount}")
        }
        (Some(amount), _) | (None, Some(amount)) => amount,
        (None, None) => bail!("how much to pay: the code does not say"),
    })
}

/// Why `code` cannot be paid `amount` in notes: where to pay it on
/// Bitcoin, if it says.
fn unpayable(code: &PaymentCode, amount: u64) -> anyhow::Error {
    let none = format!("no hub it names is one trusted holding {amount} and the fee");
    let onchain = match (&code.silent, &code.address) {
        (Some(silent), _) => Some(silent.to_string()),
        (None, Some(address)) => Some(address.assume_checked_ref().to_string()),
        (None, None) => None,
    };
    match onchain {
        Some(to) => anyhow::anyhow!("{none}: pay {to} on Bitcoin"),
        None => anyhow::anyhow!("{none}"),
    }
}

/// Pay `amount` to the wallet of `code` at `hub`, then send the payment to
/// the payee's mailbox on that hub's relays, reached through `route`.
async fn send<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    hub: String,
    amount: u64,
    route: impl FnOnce() -> anyhow::Result<Route>,
) -> anyhow::Result<CodePaid> {
    let Some(licash) = &code.licash else {
        return Err(unpayable(code, amount));
    };
    let paid = wallet.send(&hub, &licash.address, amount).await?;
    // At a hub visited too: the payee's.
    let relays = wallet
        .hubs()?
        .into_iter()
        .chain(wallet.visited()?)
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
/// through `route`, that swap at least `amount` from hub `from` to hub
/// `to`, both added, within `limits` ([`Wallet::swap_quote`]): `amount`,
/// or the least one swaps if more. Each is at the first URL
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
    let ask = |url: String| async move { wallet.swap_quote(&url, from, to, amount, limits).await };
    let answering = answering(found, ask, |q: &SwapQuote| q.info.nostr.as_deref()).await?;
    let mut providers: Vec<SwapProvider> = answering
        .into_iter()
        .map(|(service, url, quote)| SwapProvider {
            known: known(&service, &used),
            url,
            key: service.key,
            name: service.announcement.name,
            quote,
        })
        .collect();
    // Stable: the newest announced first among equals.
    providers.sort_by_key(|p| (!p.known, p.quote.fee, p.quote.info.margin));
    Ok(providers)
}

/// A hub announced on Nostr, and what it says of itself.
#[derive(Clone, Debug)]
pub struct AnnouncedHub {
    /// The first URL announced that answered as the hub.
    pub url: String,
    /// The key that announced it, x-only in hex.
    pub key: String,
    pub name: Option<String>,
    pub info: Info,
}

/// The hubs announced on `relays` for `wallet`'s network, asked through
/// `route`, but those the wallet trusts: each at the first URL it was
/// announced at that answers as the key that announced it. What a hub says
/// of itself there is only shown: adding it is the owner's call.
pub async fn hubs_announced<T: Transport>(
    wallet: &Wallet<T>,
    relays: &[String],
    route: &Route,
) -> anyhow::Result<Vec<AnnouncedHub>> {
    Ok(hubs_found(wallet, relays, route).await?.hubs)
}

/// What looking for hubs found ([`hubs_found`]).
#[derive(Clone, Debug)]
pub struct HubsFound {
    /// Those answering, as [`hubs_announced`] finds them.
    pub hubs: Vec<AnnouncedHub>,
    /// The announcements of the others, but those naming a URL of a hub the
    /// wallet trusts or found: answering as the key that announced them at
    /// none of their URLs, they are only told of, never to be added.
    pub silent: Vec<Found>,
}

/// [`hubs_announced`], and the announcements of hubs that did not answer:
/// down, refusing the route, or reached only through Tor.
pub async fn hubs_found<T: Transport>(
    wallet: &Wallet<T>,
    relays: &[String],
    route: &Route,
) -> anyhow::Result<HubsFound> {
    let network = wallet.network().name();
    let found = crate::discover(relays, route, Role::Hub, network).await?;
    let hubs_used = wallet.hubs()?;
    let trusted: HashSet<_> = hubs_used.iter().map(|(_, info)| info.address).collect();
    let ask = |url: String| async move { wallet.hub_info(&url).await };
    let answering = answering(found.clone(), ask, |i: &Info| i.nostr.as_deref()).await?;
    let mut hubs: Vec<AnnouncedHub> = answering
        .into_iter()
        .filter(|(_, _, info)| !trusted.contains(&info.address))
        .map(|(service, url, info)| AnnouncedHub {
            url,
            key: service.key,
            name: service.announcement.name,
            info,
        })
        .collect();
    // One hub announced by two keys: once.
    let mut seen = HashSet::new();
    hubs.retain(|h| seen.insert(h.info.address));
    let answered: HashSet<String> = (hubs.iter().map(|h| h.url.clone()))
        .chain(hubs_used.into_iter().map(|(url, _)| url))
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .collect();
    let silent = found
        .into_iter()
        .filter(|service| {
            let mut urls = service.announcement.urls.iter();
            !hubs.iter().any(|h| h.key == service.key)
                && !urls.any(|u| answered.contains(u.trim().trim_end_matches('/')))
        })
        .collect();
    Ok(HubsFound { hubs, silent })
}

/// A swap provider announced on Nostr that pays notes out on Bitcoin, and
/// what it asks for a payout.
#[derive(Clone, Debug)]
pub struct PayoutProvider {
    /// The first URL announced that answered as the provider.
    pub url: String,
    /// The key that announced it, x-only in hex.
    pub key: String,
    pub name: Option<String>,
    pub quote: PayoutQuote,
    /// Whether the wallet paid out through it before.
    pub known: bool,
}

/// The swap providers announced on `relays` for `wallet`'s network, asked
/// through `route`, that pay `amount` out to `to`, an address on Bitcoin,
/// from notes at hub `from`, added, within `max_fee`, a claim confirming
/// soon at `rate` ([`Wallet::payout_quote`]). Each is at the first URL it
/// was announced at that answers as the key that announced it. Those the
/// wallet paid out through before first, then the cheapest.
pub async fn payout_providers<T: Transport>(
    wallet: &Wallet<T>,
    relays: &[String],
    route: &Route,
    (from, to): (&str, &str),
    amount: u64,
    (max_fee, rate): (u64, FeeRate),
) -> anyhow::Result<Vec<PayoutProvider>> {
    let network = wallet.network().name();
    let found = crate::discover(relays, route, Role::Swaps, network).await?;
    let used: HashSet<String> = wallet
        .payouts()?
        .into_iter()
        .filter(|p| p.state == PayoutState::Done)
        .map(|p| p.provider)
        .collect();
    let ask = |url: String| async move {
        let quote = wallet.payout_quote(&url, from, to, amount, max_fee, rate);
        quote.await
    };
    let answering = answering(found, ask, |q: &PayoutQuote| q.info.nostr.as_deref()).await?;
    let mut providers: Vec<PayoutProvider> = answering
        .into_iter()
        .map(|(service, url, quote)| PayoutProvider {
            known: known(&service, &used),
            url,
            key: service.key,
            name: service.announcement.name,
            quote,
        })
        .collect();
    // Stable: the newest announced first among equals.
    providers.sort_by_key(|p| (!p.known, p.quote.costs()));
    Ok(providers)
}

/// Whether `service` announced a URL of those in `used`.
fn known(service: &Found, used: &HashSet<String>) -> bool {
    let mut urls = service.announcement.urls.iter();
    urls.any(|u| used.contains(u.trim().trim_end_matches('/')))
}

/// Each service of `found` at the first URL it announced whose answer to
/// `ask` names the key that announced it (as `nostr` reads it), with that
/// answer: only then may its URL be used. The others are skipped, as are
/// those not answering in time, but [`Error::UnknownHub`] fails them all:
/// no service is one to ask.
async fn answering<Q, Fut>(
    found: Vec<Found>,
    ask: impl Fn(String) -> Fut,
    nostr: fn(&Q) -> Option<&str>,
) -> Result<Vec<(Found, String, Q)>, Error>
where
    Fut: Future<Output = Result<Q, Error>>,
{
    let ask = &ask;
    let asked = found.into_iter().map(|service| async move {
        let mut answered = None;
        for url in &service.announcement.urls {
            match tokio::time::timeout(ANSWER_TIMEOUT, ask(url.clone())).await {
                Ok(Ok(answer)) if service.named_by(nostr(&answer)) => {
                    answered = Some((url.clone(), answer));
                    break;
                }
                Ok(Ok(_)) => tracing::warn!(%url, "skipped: not the provider that announced it"),
                Ok(Err(e @ Error::UnknownHub(_))) => return Err(e),
                Ok(Err(e)) => tracing::debug!(%url, "skipped: {e}"),
                Err(_) => tracing::debug!(%url, "skipped: no answer in time"),
            }
        }
        Ok(answered.map(|(url, answer)| (service, url, answer)))
    });
    let mut answering = Vec::new();
    for asked in join_all(asked).await {
        answering.extend(asked?);
    }
    Ok(answering)
}
