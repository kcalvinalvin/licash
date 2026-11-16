//! A [`Wallet`]'s payments through mailboxes: paying a payment code, and
//! taking what its own mailbox holds; and the swap providers announced
//! that swap between two of its hubs, or pay notes out on Bitcoin.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use bitcoin::FeeRate;
use futures_util::future::join_all;
use licash_api::{Info, Payment};
use licash_wallet::{
    Error, HubId, Paid, PaymentCode, PayoutQuote, PayoutState, Received, SwapLimits, SwapQuote,
    SwapState, Transport, Wallet,
};

use crate::{mailbox, Found, Role, Route, LIFETIME};

/// How long a swap provider announced has to answer at a URL.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the swap providers announced, as last found, are used without
/// looking for them again: well within an announcement's [`LIFETIME`].
const KEPT_FOR: Duration = Duration::from_secs(30 * 60);

/// The swap providers announced, as last found on some relays for a
/// network: quotes ask them without looking for them first, as the
/// wallet's owner waits, once looked for out of sight
/// ([`Announcements::refresh`]). What they say is only where to ask them:
/// each is still asked, and must answer as the key that announced it.
/// None kept, or none of those answering, a quote looks for them as it
/// did: one never refreshed keeps none, each quote looking. Each provider
/// is asked first at the URL it last answered at, for half an hour after,
/// while it still announces it: one of its URLs down (say, an onion
/// service not opening) is not waited on again for each quote.
#[derive(Default)]
pub struct Announcements {
    kept: Mutex<Option<Kept>>,
    /// By the key that announced it, the URL each provider last answered
    /// at as that key, and since when.
    answered: Mutex<HashMap<String, (String, Instant)>>,
}

/// Announcements found, and where and when.
struct Kept {
    found: Vec<Found>,
    relays: Vec<String>,
    network: String,
    at: Instant,
}

/// `relays` in order, once each: the same set however listed.
fn set_of(relays: &[String]) -> Vec<String> {
    let mut set = relays.to_vec();
    set.sort();
    set.dedup();
    set
}

impl Announcements {
    /// Look for the swap providers announced on `relays` for `network`,
    /// through `route`, and keep them: how many. Those kept before stay,
    /// should none answer.
    pub async fn refresh(
        &self,
        relays: &[String],
        route: &Route,
        network: &str,
    ) -> anyhow::Result<usize> {
        let found = crate::discover(relays, route, Role::Swaps, network).await?;
        let count = found.len();
        let kept = Kept {
            found,
            relays: set_of(relays),
            network: network.to_owned(),
            at: Instant::now(),
        };
        *self.kept.lock().unwrap_or_else(|e| e.into_inner()) = Some(kept);
        Ok(count)
    }

    /// Those kept from `relays` for `network`, found within [`KEPT_FOR`],
    /// those expired since left out: none if none is left.
    fn kept(&self, relays: &[String], network: &str) -> Option<Vec<Found>> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let kept = kept.as_ref()?;
        if kept.network != network || kept.relays != set_of(relays) || kept.at.elapsed() >= KEPT_FOR
        {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let found: Vec<Found> = (kept.found.iter())
            .filter(|f| f.at.saturating_add(LIFETIME.as_secs()) > now)
            .cloned()
            .collect();
        (!found.is_empty()).then_some(found)
    }

    /// The swap providers announced on `relays` for `network` that answer
    /// `ask` as the key that announced them, as `nostr` reads it
    /// ([`answering`]): those kept, else those looked for now through
    /// `route`, should none be kept or none of those answer. Only
    /// [`Announcements::refresh`] keeps them: looking for them here keeps
    /// nothing, so that one never refreshing sees each provider announced
    /// since. Each is asked first where it last answered
    /// ([`Announcements::asked`]).
    async fn answering<Q, Fut>(
        &self,
        (relays, route, network): (&[String], &Route, &str),
        ask: impl Fn(String) -> Fut,
        nostr: fn(&Q) -> Option<&str>,
    ) -> anyhow::Result<Vec<(Found, String, Q)>>
    where
        Fut: Future<Output = Result<Q, Error>>,
    {
        if let Some(found) = self.kept(relays, network) {
            let answered = self.asked(found, &ask, nostr).await?;
            if !answered.is_empty() {
                return Ok(answered);
            }
            tracing::info!("no swap provider kept answered: looking for them again");
        }
        let found = crate::discover(relays, route, Role::Swaps, network).await?;
        Ok(self.asked(found, &ask, nostr).await?)
    }

    /// Those of `found` answering `ask` ([`answering`]), each asked first
    /// at the URL it last answered at, within [`KEPT_FOR`] and while it
    /// still announces it, then at the others in the order announced:
    /// where each answers kept for the next, forgotten for one that does
    /// not.
    async fn asked<Q, Fut>(
        &self,
        mut found: Vec<Found>,
        ask: impl Fn(String) -> Fut,
        nostr: fn(&Q) -> Option<&str>,
    ) -> Result<Vec<(Found, String, Q)>, Error>
    where
        Fut: Future<Output = Result<Q, Error>>,
    {
        let lock = || self.answered.lock().unwrap_or_else(|e| e.into_inner());
        {
            let answered = lock();
            for service in &mut found {
                let Some((url, at)) = answered.get(&service.key) else {
                    continue;
                };
                let urls = &mut service.announcement.urls;
                match urls.iter().position(|u| u == url) {
                    Some(i) if at.elapsed() < KEPT_FOR => urls[..=i].rotate_right(1),
                    _ => {}
                }
            }
        }
        let keys: Vec<String> = found.iter().map(|f| f.key.clone()).collect();
        let answering = answering(found, ask, nostr).await?;
        let mut answered = lock();
        for key in keys {
            let at = answering.iter().find(|(f, ..)| f.key == key);
            let Some((_, url, _)) = at else {
                answered.remove(&key);
                continue;
            };
            let same = |(kept, at): &(String, Instant)| kept == url && at.elapsed() < KEPT_FOR;
            if !answered.get(&key).is_some_and(same) {
                answered.insert(key, (url.clone(), Instant::now()));
            }
        }
        Ok(answering)
    }
}

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
    plans_with(wallet, code, amount, route, &Announcements::default()).await
}

/// [`plans`], the swap providers announced as `announced` keeps them.
pub async fn plans_with<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
    route: &Route,
    announced: &Announcements,
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
        let pair = (from.as_str(), to.as_str());
        let found =
            swap_providers_with(wallet, announced, &relays, route, pair, lacks, limits).await;
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
            // At most all it holds there then, in as many notes as it takes.
            let fee = to_info
                .fees
                .spending()
                .at_most(available(to).saturating_add(quote.amount));
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
    let fee = info.fees.spending().at_most(held.saturating_add(swapped));
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
            margin: quote.margin,
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
    let started = std::time::Instant::now();
    let paid = wallet.send(&hub, &licash.address, amount).await?;
    let sent = started.elapsed();
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
    tracing::info!(
        hub = %hub,
        sent = ?sent,
        mailbox = ?started.elapsed() - sent,
        relays = delivered.unwrap_or(0),
        "paid a payment code"
    );
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
    let relays = mailbox_relays(wallet)?;
    let mail = mailbox::fetch(&wallet.mailbox_secret(), &relays, route).await?;
    Ok(take(wallet, mail, |_| {}))
}

/// [`collect`], looking through `kept`, the mailbox kept connected between
/// calls ([`mailbox::Mailbox`]): opened the first time, and anew once the
/// relays its hubs name change, or when none of those kept answers (their
/// connections gone with the network, say). Mail taken, or taken before,
/// is not opened again while it is kept.
pub async fn collect_kept<T: Transport>(
    wallet: &Wallet<T>,
    route: &Route,
    kept: &mut Option<mailbox::Mailbox>,
) -> anyhow::Result<Collected> {
    let relays = mailbox_relays(wallet)?;
    let secret = wallet.mailbox_secret();
    let fetched = match kept.as_ref().filter(|m| m.is(&secret, &relays)) {
        Some(mailbox) => mailbox.fetch().await.map_err(|e| {
            tracing::info!("the mailbox kept, opened anew: {e:#}");
        }),
        None => Err(()),
    };
    let mail = match fetched {
        Ok(mail) => mail,
        Err(()) => {
            if let Some(old) = kept.take() {
                old.close().await;
            }
            let mailbox = kept.insert(mailbox::Mailbox::open(&secret, &relays, route).await?);
            mailbox.fetch().await?
        }
    };
    let mailbox = kept.as_ref().context("a mailbox kept")?;
    Ok(take(wallet, mail, |id| mailbox.done(id)))
}

/// The relays `wallet`'s mailbox is on: those its hubs name.
fn mailbox_relays<T: Transport>(wallet: &Wallet<T>) -> anyhow::Result<Vec<String>> {
    let relays = hub_relays(wallet)?;
    if relays.is_empty() {
        bail!("no hub trusted names mailbox relays: adding a hub again learns them");
    }
    Ok(relays)
}

/// The payments of `mail` `wallet` takes, each once, told to `done` by
/// their mail's identifier as are those taken before and those that are
/// no payment: none of which to take again.
fn take<T: Transport>(
    wallet: &Wallet<T>,
    mail: Vec<mailbox::Mail>,
    mut done: impl FnMut(&str),
) -> Collected {
    let mut collected = Collected::default();
    for mail in mail {
        let Ok(payment) = serde_json::from_str::<Payment>(&mail.content) else {
            done(&mail.id);
            collected.skipped.push((mail.id, "is not a payment".into()));
            continue;
        };
        match wallet.receive(&payment) {
            Ok(received) => {
                done(&mail.id);
                collected.received.push(received);
            }
            Err(Error::AlreadyReceived) => done(&mail.id),
            // Perhaps once its hub is trusted, say.
            Err(e) => collected.skipped.push((mail.id, format!("not taken: {e}"))),
        }
    }
    collected
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
    /// The URL that answered as the provider: the first announced that
    /// does, or where it last answered ([`Announcements`]).
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
    pair: (&str, &str),
    amount: u64,
    limits: SwapLimits,
) -> anyhow::Result<Vec<SwapProvider>> {
    let announced = Announcements::default();
    swap_providers_with(wallet, &announced, relays, route, pair, amount, limits).await
}

/// [`swap_providers`], those announced as `announced` keeps them, each
/// asked first where it last answered.
pub async fn swap_providers_with<T: Transport>(
    wallet: &Wallet<T>,
    announced: &Announcements,
    relays: &[String],
    route: &Route,
    (from, to): (&str, &str),
    amount: u64,
    limits: SwapLimits,
) -> anyhow::Result<Vec<SwapProvider>> {
    let network = wallet.network().name();
    let used: HashSet<String> = wallet
        .swaps()?
        .into_iter()
        .filter(|s| s.state == SwapState::Done)
        .map(|s| s.provider)
        .collect();
    let ask = |url: String| async move { wallet.swap_quote(&url, from, to, amount, limits).await };
    let on = (relays, route, network);
    let answering = announced.answering(on, ask, |q: &SwapQuote| q.info.nostr.as_deref());
    let answering = answering.await?;
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
    providers.sort_by_key(|p| (!p.known, p.quote.fee, p.quote.margin));
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
    /// The URL that answered as the provider: the first announced that
    /// does, or where it last answered ([`Announcements`]).
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
    pair: (&str, &str),
    amount: u64,
    limits: (u64, FeeRate),
) -> anyhow::Result<Vec<PayoutProvider>> {
    let announced = Announcements::default();
    payout_providers_with(wallet, &announced, relays, route, pair, amount, limits).await
}

/// [`payout_providers`], those announced as `announced` keeps them, each
/// asked first where it last answered.
pub async fn payout_providers_with<T: Transport>(
    wallet: &Wallet<T>,
    announced: &Announcements,
    relays: &[String],
    route: &Route,
    (from, to): (&str, &str),
    amount: u64,
    (max_fee, rate): (u64, FeeRate),
) -> anyhow::Result<Vec<PayoutProvider>> {
    let network = wallet.network().name();
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
    let on = (relays, route, network);
    let answering = announced.answering(on, ask, |q: &PayoutQuote| q.info.nostr.as_deref());
    let answering = answering.await?;
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

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::MockRelay;

    use super::*;
    use crate::{Announcement, Announcer};

    fn provider(at: u64) -> Found {
        Found {
            key: "a".repeat(64),
            announcement: Announcement {
                urls: vec!["https://swaps.example".into()],
                network: "regtest".into(),
                name: None,
            },
            at,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A provider's answer: the key it names.
    struct Answer(String);

    fn named(answer: &Answer) -> Option<&str> {
        Some(&answer.0)
    }

    #[tokio::test]
    async fn providers_found_are_kept_for_quotes_without_the_relays() {
        let relay = MockRelay::run().await.unwrap();
        let relays = [relay.url().await.to_string()];
        let announcer = Announcer::new(&[9; 32], Role::Swaps, &relays, &Route::Direct);
        let announcement = provider(0).announcement;
        announcer
            .await
            .unwrap()
            .announce(&announcement)
            .await
            .unwrap();
        let key = crate::public_key(&[9; 32]).unwrap();
        let answers = |_: String| {
            let key = key.clone();
            async move { Ok(Answer(key)) }
        };
        let down = |_: String| async { Err::<Answer, _>(Error::Invalid("down".into())) };
        let route = Route::Direct;
        let announced = Announcements::default();

        // Looked for as asked, but not kept: a provider announced since
        // would be found the next time.
        let found = announced
            .answering((&relays[..], &route, "regtest"), answers, named)
            .await;
        assert_eq!(found.unwrap()[0].1, "https://swaps.example");
        assert!(announced.kept(&relays, "regtest").is_none());
        let found = announced.refresh(&relays, &Route::Direct, "regtest");
        assert_eq!(found.await.unwrap(), 1);

        // The relay gone, those kept answer, however its URL is listed.
        relay.shutdown();
        let twice = [relays[0].clone(), relays[0].clone()];
        let found = announced
            .answering((&twice[..], &route, "regtest"), answers, named)
            .await;
        assert_eq!(found.unwrap().len(), 1);
        // None of those kept answering, they are looked for again.
        assert!(announced
            .answering((&relays[..], &route, "regtest"), down, named)
            .await
            .is_err());
        // Not for another network, nor other relays: those are asked.
        assert!(announced.kept(&relays, "signet").is_none());
        let others = ["ws://127.0.0.1:1".to_owned()];
        assert!(announced.kept(&others, "regtest").is_none());
        assert!(announced
            .answering((&others[..], &route, "regtest"), answers, named)
            .await
            .is_err());
        // Failing to find them again, those kept stay.
        assert!(announced
            .refresh(&relays, &Route::Direct, "regtest")
            .await
            .is_err());
        assert_eq!(announced.kept(&relays, "regtest").unwrap().len(), 1);
    }

    #[test]
    fn providers_kept_too_long_or_expired_since_are_not_used() {
        let relays = ["ws://relay.example".to_owned()];
        let keep = |found: Vec<Found>, at: Instant| {
            let announced = Announcements::default();
            let relays = set_of(&relays);
            let network = "regtest".to_owned();
            *announced.kept.lock().unwrap() = Some(Kept {
                found,
                relays,
                network,
                at,
            });
            announced
        };
        let fresh = keep(vec![provider(now())], Instant::now());
        assert_eq!(fresh.kept(&relays, "regtest").unwrap().len(), 1);
        // Found a while ago: looked for again.
        if let Some(then) = Instant::now().checked_sub(KEPT_FOR) {
            assert!(keep(vec![provider(now())], then)
                .kept(&relays, "regtest")
                .is_none());
        }
        // Announced so long ago that it expired since: left out.
        let old = now() - LIFETIME.as_secs();
        let kept = keep(vec![provider(old), provider(now())], Instant::now());
        assert_eq!(kept.kept(&relays, "regtest").unwrap().len(), 1);
        assert!(keep(vec![provider(old)], Instant::now())
            .kept(&relays, "regtest")
            .is_none());
        // None found: looked for again, a provider may have announced since.
        assert!(keep(Vec::new(), Instant::now())
            .kept(&relays, "regtest")
            .is_none());
    }

    #[tokio::test]
    async fn providers_are_asked_first_where_they_last_answered_as_themselves() {
        const ONION: &str = "http://swaps.onion";
        const CLEAR: &str = "https://swaps.example";
        let announced = Announcements::default();
        let mut two = provider(now());
        two.announcement.urls = vec![ONION.into(), CLEAR.into()];
        let key = two.key.clone();
        // Those down, those answering as another key, and each URL asked.
        let down: Mutex<HashSet<&str>> = Mutex::new(HashSet::new());
        let other: Mutex<HashSet<&str>> = Mutex::new(HashSet::new());
        let asked: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let ask = |url: String| {
            asked.lock().unwrap().push(url.clone());
            let up = !down.lock().unwrap().contains(url.as_str());
            let named = match other.lock().unwrap().contains(url.as_str()) {
                true => "b".repeat(64),
                false => key.clone(),
            };
            async move {
                match up {
                    true => Ok(Answer(named)),
                    false => Err(Error::Invalid("down".into())),
                }
            }
        };
        let quote = async |set: (&[&'static str], &[&'static str])| {
            *down.lock().unwrap() = set.0.iter().copied().collect();
            *other.lock().unwrap() = set.1.iter().copied().collect();
            let answered = announced.asked(vec![two.clone()], &ask, named).await;
            let at = answered.unwrap().first().map(|(_, url, _)| url.clone());
            (at, std::mem::take(&mut *asked.lock().unwrap()))
        };
        let both = || vec![ONION.to_owned(), CLEAR.to_owned()];

        // Its onion service down: asked there, then at its other URL, where
        // it is asked first from then on.
        assert_eq!(quote((&[ONION], &[])).await, (Some(CLEAR.into()), both()));
        assert_eq!(
            quote((&[], &[])).await,
            (Some(CLEAR.into()), vec![CLEAR.into()])
        );
        // Answering there as another key: not where it answered, forgotten.
        let back = || vec![CLEAR.to_owned(), ONION.to_owned()];
        assert_eq!(quote((&[ONION], &[CLEAR])).await, (None, back()));
        assert_eq!(
            quote((&[], &[])).await,
            (Some(ONION.into()), vec![ONION.into()])
        );
        // Down where it last answered: the others, in the order announced.
        assert_eq!(quote((&[ONION], &[])).await, (Some(CLEAR.into()), both()));
        assert_eq!(quote((&[CLEAR], &[])).await, (Some(ONION.into()), back()));
        assert_eq!(
            quote((&[], &[])).await,
            (Some(ONION.into()), vec![ONION.into()])
        );

        // Kept for so long: asked in the order announced again.
        let since = |url: &str, at| {
            let kept = (url.to_owned(), at);
            announced.answered.lock().unwrap().insert(key.clone(), kept);
        };
        if let Some(then) = Instant::now().checked_sub(KEPT_FOR) {
            since(CLEAR, then);
            assert_eq!(
                quote((&[], &[])).await,
                (Some(ONION.into()), vec![ONION.into()])
            );
        }
        // No longer announced there: neither.
        since("https://moved.example", Instant::now());
        assert_eq!(
            quote((&[], &[])).await,
            (Some(ONION.into()), vec![ONION.into()])
        );
    }
}
