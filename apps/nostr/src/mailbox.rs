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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr_sdk::prelude::{
    Client, ClientNotification, Event, EventBuilder, EventId, Filter, FinalizeEvent,
    FinalizeUnsignedEvent, Keys, Kind, PublicKey, Relay, RelayStatus, RelayUrl, StreamExt,
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

/// How long a mailbox kept takes what its relays push alone, before
/// asking them for all the mail again: a relay that dropped and
/// reconnected pushes none sent in between.
const ASK_EVERY: Duration = Duration::from_secs(30);

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
    /// The payments the last look returned, not [`Mailbox::done`] since:
    /// returned again by the next, whether the relays are asked or not.
    returned: Mutex<BTreeSet<Event>>,
    /// What the relays pushed since the last look, and which push.
    pushed: Arc<Mutex<Pushed>>,
    /// Taking in what the relays push, until closed.
    listening: tokio::task::JoinHandle<()>,
}

/// The mail a mailbox kept is pushed between its looks.
#[derive(Default)]
struct Pushed {
    /// The gift wraps pushed since the last look.
    events: BTreeSet<Event>,
    /// The relays asked to push new mail: for good, asked again as each
    /// reconnects.
    relays: HashSet<RelayUrl>,
    /// When the relays were last asked for all the mail: never if none.
    asked: Option<Instant>,
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        self.listening.abort();
    }
}

impl Mailbox {
    /// The mailbox of `secret` on `relays`, reached through `route`: not
    /// connected until first looked in.
    pub async fn open(secret: &[u8; 32], relays: &[String], route: &Route) -> anyhow::Result<Self> {
        let keys = keys(secret)?;
        // Relays may serve gift wraps only to their addressee (NIP-42).
        let client = client(relays, route, Some(keys.clone())).await?;
        let pushed = Arc::new(Mutex::new(Pushed::default()));
        let mut notifications = client.notifications();
        let into = pushed.clone();
        let listening = tokio::spawn(async move {
            while let Some(notification) = notifications.next().await {
                // Each the first time seen: what the looks return too.
                if let ClientNotification::Event { event, .. } = notification {
                    if event.kind == Kind::GiftWrap {
                        lock(&into).events.insert(*event);
                    }
                }
            }
        });
        Ok(Self {
            client,
            keys,
            relays: relays.to_vec(),
            route: route.clone(),
            done: Mutex::default(),
            returned: Mutex::default(),
            pushed,
            listening,
        })
    }

    /// Whether it is the mailbox of `secret` on `relays`.
    pub fn is(&self, secret: &[u8; 32], relays: &[String]) -> bool {
        let same = keys(secret).is_ok_and(|keys| keys.public_key() == self.keys.public_key());
        same && self.relays == relays
    }

    /// The payments in it, as [`fetch`], but those [`Mailbox::done`]
    /// (those of the last look not done with returned again). A look asks
    /// the relays connected, if any (the others connecting for the next
    /// meanwhile), else each as soon as it connects, for all the mail,
    /// each asked first to push it the new mail from then on. Until
    /// [`ASK_EVERY`] later, while one of those is connected, a look takes
    /// what they pushed, at once, asking nothing.
    pub async fn fetch(&self) -> anyhow::Result<Vec<Mail>> {
        let relays: Vec<_> = self.client.relays().await.into_iter().collect();
        let pushing = {
            let pushed = self.pushed();
            let asked = pushed.asked.is_some_and(|at| at.elapsed() < ASK_EVERY);
            asked
                && relays
                    .iter()
                    .any(|(url, r)| pushed.relays.contains(url) && connected(r))
        };
        let mut events = if pushing {
            BTreeSet::new()
        } else {
            self.ask(relays).await?
        };
        // Each once, whatever relays hold it.
        events.append(&mut self.pushed().events);
        let mut returned = lock(&self.returned);
        events.append(&mut returned);
        let mut done = lock(&self.done);
        let mut mail = Vec::new();
        for event in events {
            if done.contains(&event.id) {
                continue;
            }
            match open(&self.keys, &event) {
                Some(one) => {
                    mail.push(one);
                    returned.insert(event);
                }
                None => {
                    done.insert(event.id);
                }
            }
        }
        mail.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        Ok(mail)
    }

    /// All the mail on `relays`, as [`Mailbox::fetch`] asks for it, each
    /// relay asked first to push the new mail from then on.
    async fn ask(&self, mut relays: Vec<(RelayUrl, Relay)>) -> anyhow::Result<BTreeSet<Event>> {
        let to = self.keys.public_key();
        let all = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(to)
            .limit(MAX_MAIL);
        // None held already: what comes from now on.
        let new = Filter::new().kind(Kind::GiftWrap).pubkey(to).limit(0);
        let timeout = self.route.timeout();
        let asked = Instant::now();
        if relays.iter().any(|(_, r)| connected(r)) {
            for (_, relay) in relays.iter().filter(|(_, r)| !connected(r)) {
                let relay = relay.clone();
                // Nothing if connecting already.
                tokio::spawn(async move { relay.try_connect().timeout(timeout).await });
            }
            relays.retain(|(_, r)| connected(r));
        }
        let fetched = ask_each(relays, &self.route, MIN_GRACE, |relay| {
            let (all, new, pushed) = (all.clone(), new.clone(), self.pushed.clone());
            async move {
                let url = relay.url().clone();
                let pushing = lock(&pushed).relays.contains(&url);
                if !pushing {
                    // Kept by the client, asked again as the relay reconnects.
                    relay.subscribe(new).await?;
                    lock(&pushed).relays.insert(url);
                }
                Ok(relay.fetch_events(all).timeout(timeout).await?)
            }
        })
        .await?;
        self.pushed().asked = Some(asked);
        Ok(fetched.into_iter().flatten().collect())
    }

    fn pushed(&self) -> MutexGuard<'_, Pushed> {
        lock(&self.pushed)
    }

    /// The mail `id` (in hex) dealt with for good: taken, or taken before.
    /// Not opened again.
    pub fn done(&self, id: &str) {
        if let Ok(id) = EventId::from_hex(id) {
            lock(&self.done).insert(id);
        }
    }

    /// Disconnect from its relays.
    pub async fn close(self) {
        self.client.shutdown().await;
    }
}

fn connected(relay: &Relay) -> bool {
    relay.status() == RelayStatus::Connected
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::MockRelay;

    use super::*;

    #[tokio::test]
    async fn a_mailbox_kept_is_pushed_the_mail_sent_once_looked_in() {
        let relay = MockRelay::run().await.unwrap();
        let relays = [relay.url().await.to_string()];
        let secret = [1; 32];
        let to = keys(&secret).unwrap().public_key().to_bytes();
        let kept = Mailbox::open(&secret, &relays, &Route::Direct)
            .await
            .unwrap();
        assert!(kept.fetch().await.unwrap().is_empty());
        let asked = kept.pushed().asked;
        assert!(asked.is_some());
        for paid in 1..=2 {
            let content = format!(r#"{{"paid":{paid}}}"#);
            send(&to, &content, &relays, &Route::Direct).await.unwrap();
            let waited = Instant::now();
            while kept.pushed().events.is_empty() && waited.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Taken from what was pushed: the relay not asked again.
            let mail = kept.fetch().await.unwrap();
            assert_eq!(mail.len(), 1);
            assert_eq!(mail[0].content, content);
            kept.done(&mail[0].id);
            assert_eq!(kept.pushed().asked, asked);
            assert!(kept.pushed().events.is_empty());
        }
        // Asked for all the mail again once it is time.
        kept.pushed().asked = Some(Instant::now() - ASK_EVERY);
        assert!(kept.fetch().await.unwrap().is_empty());
        assert!(kept.pushed().asked > asked);
        kept.close().await;
    }
}
