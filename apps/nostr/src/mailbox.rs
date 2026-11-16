//! Mailboxes: a wallet's key on Nostr, to which payers send the payments
//! they make it, as private messages are (NIP-17): a rumor of kind
//! [`PAYMENT`] holding the payment, sealed by a key of the payer's own for
//! that message and gift wrapped to the mailbox (NIP-59). Relays see only
//! that something is addressed to the mailbox, at a time up to two days
//! off; its payee alone opens it.
//!
//! Nothing says a payment arrived: a payer sends it again until its payee
//! has it, or hands it over otherwise, and the payee takes each once.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, EventId, Filter, FinalizeEvent, FinalizeUnsignedEvent, Keys, Kind,
    PublicKey, Relay, RelayStatus,
};

use crate::{ask_each, client, keys, Route, GRACE, MIN_GRACE};

/// The kind of the rumors: a payment, in JSON.
pub const PAYMENT: u16 = 8338;

/// How long relays keep a payment sent: past this, two days less at most,
/// it expires (NIP-40).
pub const KEEP: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// The most gift wraps asked of each relay.
const MAX_MAIL: usize = 500;

/// The largest gift wrap opened: a payment is far smaller.
const MAX_WRAP: usize = 512 * 1024;

/// A payment found in a mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    /// The gift wrap's identifier, in hex.
    pub id: String,
    /// The payment, as its payer wrote it.
    pub content: String,
    /// When its payer says it sent it, in seconds since the epoch.
    pub at: u64,
}

/// Send `content`, a payment, to the mailbox of x-only key `to` on
/// `relays`, reached through `route`: how many took it, at least one.
pub async fn send(
    to: &[u8; 32],
    content: &str,
    relays: &[String],
    route: &Route,
) -> anyhow::Result<usize> {
    let to = PublicKey::from_slice(to).context("a mailbox key")?;
    let sealer = Keys::generate();
    let rumor =
        EventBuilder::new(Kind::Custom(PAYMENT), content).finalize_unsigned(sealer.public_key());
    let wrap = GiftWrapBuilder::new(to, rumor)
        .expiration(KEEP)
        .finalize(&sealer)
        .context("gift wrap the payment")?;
    let client = client(relays, route, Some(sealer)).await?;
    // To each as soon as it connects: one slow would hold the others up.
    let sent = ask_each(client.relays().await, route, GRACE, |relay| {
        let wrap = wrap.clone();
        async move { Ok(relay.send_event(&wrap).await?) }
    })
    .await;
    client.shutdown().await;
    let sent = sent.context("no relay took the payment")?;
    Ok(sent.len())
}

/// The payments in the mailbox of `secret` on `relays`, asked through
/// `route`, oldest first: each once, whatever relays hold it. Relays that
/// do not answer in time are skipped; fails only if none answered.
pub async fn fetch(
    secret: &[u8; 32],
    relays: &[String],
    route: &Route,
) -> anyhow::Result<Vec<Mail>> {
    let mailbox = Mailbox::open(secret, relays, route).await?;
    let mail = mailbox.fetch().await;
    mailbox.close().await;
    mail
}

/// A mailbox looked in again and again, its relays kept connected in
/// between, for a wallet waiting for payments (an app open): a look asks
/// those connected at once, rather than connecting to each anew, which
/// takes seconds over Tor. Relays see what they would of each look apart:
/// the mailbox asking for its mail, through the same circuits.
pub struct Mailbox {
    client: Client,
    keys: Keys,
    relays: Vec<String>,
    route: Route,
    /// The gift wraps dealt with for good ([`Mailbox::done`]), or that are
    /// no payment of its: not opened again, a look then costing nothing
    /// for each (opening a payment taken already takes milliseconds, and a
    /// mailbox holds two weeks of them).
    done: Mutex<HashSet<EventId>>,
}

impl Mailbox {
    /// The mailbox of `secret` on `relays`, reached through `route`: not
    /// connected until first looked in.
    pub async fn open(secret: &[u8; 32], relays: &[String], route: &Route) -> anyhow::Result<Self> {
        let keys = keys(secret)?;
        // Relays may serve gift wraps only to their addressee (NIP-42).
        let client = client(relays, route, Some(keys.clone())).await?;
        Ok(Self {
            client,
            keys,
            relays: relays.to_vec(),
            route: route.clone(),
            done: Mutex::default(),
        })
    }

    /// Whether it is the mailbox of `secret` on `relays`.
    pub fn is(&self, secret: &[u8; 32], relays: &[String]) -> bool {
        let same = keys(secret).is_ok_and(|keys| keys.public_key() == self.keys.public_key());
        same && self.relays == relays
    }

    /// The payments in it, as [`fetch`], but those [`Mailbox::done`]: from
    /// the relays connected, if any, the others connecting for the next
    /// look meanwhile; else from each as soon as it connects.
    pub async fn fetch(&self) -> anyhow::Result<Vec<Mail>> {
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.keys.public_key())
            .limit(MAX_MAIL);
        let timeout = self.route.timeout();
        let mut relays: Vec<_> = self.client.relays().await.into_iter().collect();
        let connected = |r: &Relay| r.status() == RelayStatus::Connected;
        if relays.iter().any(|(_, r)| connected(r)) {
            for (_, relay) in relays.iter().filter(|(_, r)| !connected(r)) {
                let relay = relay.clone();
                // Nothing if connecting already.
                tokio::spawn(async move { relay.try_connect().timeout(timeout).await });
            }
            relays.retain(|(_, r)| connected(r));
        }
        let fetched = ask_each(relays, &self.route, MIN_GRACE, |relay| {
            let filter = filter.clone();
            async move { Ok(relay.fetch_events(filter).timeout(timeout).await?) }
        })
        .await?;
        // Each once, whatever relays hold it.
        let events: BTreeSet<Event> = fetched.into_iter().flatten().collect();
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        let mut mail = Vec::new();
        for event in &events {
            if done.contains(&event.id) {
                continue;
            }
            match open(&self.keys, event) {
                Some(one) => mail.push(one),
                None => {
                    done.insert(event.id);
                }
            }
        }
        mail.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        Ok(mail)
    }

    /// The mail `id` (in hex) dealt with for good: taken, or taken before.
    /// Not opened again.
    pub fn done(&self, id: &str) {
        if let Ok(id) = EventId::from_hex(id) {
            let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
            done.insert(id);
        }
    }

    /// Disconnect from its relays.
    pub async fn close(self) {
        self.client.shutdown().await;
    }
}

/// `event`, if a payment gift wrapped to `keys` and sealed by its sender.
fn open(keys: &Keys, event: &Event) -> Option<Mail> {
    if event.kind != Kind::GiftWrap || event.content.len() > MAX_WRAP || event.is_expired() {
        return None;
    }
    let gift = UnwrappedGift::from_gift_wrap(keys, event).ok()?;
    if gift.rumor.kind != Kind::Custom(PAYMENT) {
        return None;
    }
    Some(Mail {
        id: event.id.to_hex(),
        content: gift.rumor.content,
        at: gift.rumor.created_at.as_secs(),
    })
}
