//! Hubs and swap providers announce themselves on Nostr relays, and wallets
//! discover them there.
//!
//! An announcement is an addressable event (NIP-01) of kind [`KIND`]. Its
//! `d` tag names the [`Role`], so that a key keeps one announcement per
//! role; its `t` tag, `licash-hub` or `licash-swaps`, lets clients ask
//! relays for one role; and its NIP-40 `expiration` drops it [`LIFETIME`]
//! after it was signed, unless renewed, as [`Announcer::run`] does every
//! [`EVERY`]. Its content is an [`Announcement`] in JSON.
//!
//! Anyone may announce any URL: a client uses one only once the service
//! there names the key that announced it ([`Found::named_by`]), as a hub
//! does in its `/v1/info`.
//!
//! Wallets are paid at their [`mailbox`] on Nostr too.
//!
//! Relays are reached directly, through a SOCKS5 proxy, or, with the `tor`
//! feature, through a Tor client of the application's own ([`Route`]).

pub mod mailbox;
#[cfg(feature = "tor")]
mod tor;

use std::{collections::HashMap, net::SocketAddr, time::Duration};

use anyhow::{ensure, Context};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, Filter, FinalizeEvent, Keys, Kind, Proxy, RelayUrl, SecretKey,
    SignerAuthenticator, Tag, Timestamp,
};
use serde::{Deserialize, Serialize};

/// The kind of announcements: addressable.
pub const KIND: u16 = 38338;

/// How often a service renews its announcement.
pub const EVERY: Duration = Duration::from_secs(30 * 60);

/// How long an announcement lasts unless renewed.
pub const LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);

/// How long to wait for relays to connect, take an event or answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The same through Tor, whose circuits take longer, as may its first
/// connection.
const TOR_TIMEOUT: Duration = Duration::from_secs(60);

/// How soon to try again after no relay took an announcement.
const RETRY: Duration = Duration::from_secs(60);

/// The largest content read: an announcement is far smaller.
const MAX_CONTENT: usize = 8 * 1024;

/// The most URLs read from one announcement, and the longest.
const MAX_URLS: usize = 4;
const MAX_URL: usize = 256;

/// The most announcements asked of each relay.
const MAX_EVENTS: usize = 500;

/// What announces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Hub,
    Swaps,
}

impl Role {
    /// Its `d` tag.
    fn id(self) -> &'static str {
        match self {
            Role::Hub => "hub",
            Role::Swaps => "swaps",
        }
    }

    /// Its `t` tag.
    fn topic(self) -> &'static str {
        match self {
            Role::Hub => "licash-hub",
            Role::Swaps => "licash-swaps",
        }
    }
}

/// The content of an announcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    /// Where the service answers: `https://` URLs, or `http://` ones of
    /// onion services.
    pub urls: Vec<String>,
    /// The Bitcoin network of its locks: `bitcoin`, `testnet4`, `signet` or
    /// `regtest`.
    pub network: String,
    /// A name to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Announcement {
    /// Fails unless it names 1 to 4 URLs, each `http[s]://` and at most 256
    /// bytes long: clients skip the others.
    pub fn check(&self) -> anyhow::Result<()> {
        ensure!(
            (1..=MAX_URLS).contains(&self.urls.len()),
            "an announcement names 1 to {MAX_URLS} URLs"
        );
        for url in &self.urls {
            ensure!(
                usable(url),
                "{url}: not an http:// or https:// URL of at most {MAX_URL} bytes"
            );
        }
        Ok(())
    }
}

/// An announcement found on the relays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The key that announced it: x-only, in hex.
    pub key: String,
    pub announcement: Announcement,
    /// When it was signed, in seconds since the epoch.
    pub at: u64,
}

impl Found {
    /// Whether a service that names `nostr` as its key, in hex, made this
    /// announcement: only then may its URL be used.
    pub fn named_by(&self, nostr: Option<&str>) -> bool {
        nostr.is_some_and(|key| key.eq_ignore_ascii_case(&self.key))
    }
}

/// The x-only public key of `secret`, in hex, as services name it.
pub fn public_key(secret: &[u8; 32]) -> anyhow::Result<String> {
    Ok(keys(secret)?.public_key().to_hex())
}

fn keys(secret: &[u8; 32]) -> anyhow::Result<Keys> {
    let secret = SecretKey::from_slice(secret).context("a Nostr secret key")?;
    Ok(Keys::new(secret))
}

/// How a client reaches the relays.
#[derive(Clone, Default)]
pub enum Route {
    #[default]
    Direct,
    /// Through a SOCKS5 proxy, such as Tor's.
    Socks(SocketAddr),
    /// Through a Tor client of the application's own.
    #[cfg(feature = "tor")]
    Tor(licash_tor::Tor),
}

impl Route {
    /// Through the `socks5[h]://host:port` proxy `url`.
    pub fn socks(url: &str) -> anyhow::Result<Self> {
        let at = url
            .strip_prefix("socks5h://")
            .or_else(|| url.strip_prefix("socks5://"))
            .context("Nostr relays are reached through a socks5h:// proxy only")?;
        let address = std::net::ToSocketAddrs::to_socket_addrs(at.trim_end_matches('/'))
            .with_context(|| format!("the proxy {at}"))?
            .next()
            .with_context(|| format!("the proxy {at}: no address"))?;
        Ok(Self::Socks(address))
    }

    /// How long to wait for relays to connect, take an event or answer.
    fn timeout(&self) -> Duration {
        match self {
            Self::Direct => TIMEOUT,
            _ => TOR_TIMEOUT,
        }
    }
}

/// A client of `relays`, through `route`, authenticating as `auth` to
/// relays that ask (NIP-42), not connected yet.
async fn client(relays: &[String], route: &Route, auth: Option<Keys>) -> anyhow::Result<Client> {
    ensure!(!relays.is_empty(), "no Nostr relay");
    let mut builder = match route {
        Route::Direct => Client::builder(),
        Route::Socks(proxy) => Client::builder().proxy(Proxy::all(*proxy)),
        #[cfg(feature = "tor")]
        Route::Tor(tor) => Client::builder().websocket_transport(tor::OverTor(tor.clone())),
    };
    if let Some(keys) = auth {
        builder = builder.authenticator(SignerAuthenticator::new(keys));
    }
    let client = builder.build();
    for relay in relays {
        client
            .add_relay(relay.as_str())
            .await
            .with_context(|| format!("Nostr relay {relay}"))?;
    }
    Ok(client)
}

/// Announces a service on Nostr relays.
pub struct Announcer {
    client: Client,
    keys: Keys,
    role: Role,
}

impl Announcer {
    /// An announcer of `role` on `relays`, reached through `route`,
    /// signing with `secret`. It keeps connecting to the relays down.
    pub async fn new(
        secret: &[u8; 32],
        role: Role,
        relays: &[String],
        route: &Route,
    ) -> anyhow::Result<Self> {
        let keys = keys(secret)?;
        let client = client(relays, route, None).await?;
        client.connect().and_wait(route.timeout()).await;
        Ok(Self { client, keys, role })
    }

    /// Its public key, x-only in hex.
    pub fn key(&self) -> String {
        self.keys.public_key().to_hex()
    }

    /// Sign `announcement` and send it to the relays: how many took it, at
    /// least one.
    pub async fn announce(&self, announcement: &Announcement) -> anyhow::Result<usize> {
        announcement.check()?;
        let event = self.event(announcement, Timestamp::now())?;
        let sent = self.client.send_event(&event).await?;
        if sent.success.is_empty() {
            let failed: Vec<String> = sent
                .failed
                .iter()
                .map(|(relay, e)| format!("{relay}: {e}"))
                .collect();
            anyhow::bail!("no relay took the announcement: {}", failed.join("; "));
        }
        Ok(sent.success.len())
    }

    /// The event announcing `announcement`, signed at `at`.
    pub fn event(&self, announcement: &Announcement, at: Timestamp) -> anyhow::Result<Event> {
        let tags = [
            Tag::identifier(self.role.id()),
            Tag::hashtag(self.role.topic()),
            Tag::expiration(at + LIFETIME),
        ];
        let content = serde_json::to_string(announcement)?;
        Ok(EventBuilder::new(Kind::Custom(KIND), content)
            .tags(tags)
            .custom_created_at(at)
            .finalize(&self.keys)?)
    }

    /// Announce `announcement` now and every [`EVERY`], for good.
    pub async fn run(self, announcement: Announcement) {
        let mut announced = false;
        loop {
            let wait = match self.announce(&announcement).await {
                Ok(relays) => {
                    if !announced {
                        tracing::info!(relays, key = %self.key(), "announced on Nostr");
                    }
                    announced = true;
                    EVERY
                }
                Err(e) => {
                    tracing::warn!("announcing on Nostr: {e:#}");
                    announced = false;
                    RETRY
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// The announcements of `role` for `network` on `relays`, asked through
/// `route`: the newest of each key, newest first, unexpired and
/// well formed. Relays that do not answer in time are skipped; fails only
/// if none answered.
pub async fn discover(
    relays: &[String],
    route: &Route,
    role: Role,
    network: &str,
) -> anyhow::Result<Vec<Found>> {
    let client = client(relays, route, None).await?;
    let connected = match connected(&client, route).await {
        Ok(relays) => relays,
        Err(e) => {
            client.shutdown().await;
            return Err(e);
        }
    };
    let filter = Filter::new()
        .kind(Kind::Custom(KIND))
        .identifier(role.id())
        .hashtag(role.topic())
        .limit(MAX_EVENTS);
    let targets: Vec<(RelayUrl, Vec<Filter>)> = connected
        .into_iter()
        .map(|relay| (relay, vec![filter.clone()]))
        .collect();
    let events = client.fetch_events(targets).timeout(route.timeout()).await;
    client.shutdown().await;
    let events = events.context("ask the Nostr relays")?;
    let mut newest: HashMap<String, Found> = HashMap::new();
    for event in events {
        let Some(found) = read(&event, role, network) else {
            continue;
        };
        if newest.get(&found.key).is_none_or(|old| old.at < found.at) {
            newest.insert(found.key.clone(), found);
        }
    }
    let mut found: Vec<Found> = newest.into_values().collect();
    found.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.key.cmp(&b.key)));
    Ok(found)
}

/// The relays of `client` that connected in time, to ask only them: the
/// others would hold the answer up. Fails if none did.
async fn connected(client: &Client, route: &Route) -> anyhow::Result<Vec<RelayUrl>> {
    let connected = client.try_connect().timeout(route.timeout()).await;
    if connected.success.is_empty() {
        let failed: Vec<String> = connected
            .failed
            .iter()
            .map(|(relay, e)| format!("{relay}: {e}"))
            .collect();
        anyhow::bail!("no Nostr relay answered: {}", failed.join("; "));
    }
    Ok(connected.success.into_keys().collect())
}

/// `event`, if a valid, unexpired announcement of `role` for `network`.
fn read(event: &Event, role: Role, network: &str) -> Option<Found> {
    let valid = event.kind == Kind::Custom(KIND)
        && event.tags.identifier().as_deref() == Some(role.id())
        && !event.is_expired()
        && event.content.len() <= MAX_CONTENT
        && event.verify().is_ok();
    if !valid {
        return None;
    }
    let mut announcement: Announcement = serde_json::from_str(&event.content).ok()?;
    if announcement.network != network {
        return None;
    }
    announcement.urls.retain(|url| usable(url));
    announcement.urls.truncate(MAX_URLS);
    if announcement.urls.is_empty() {
        return None;
    }
    Some(Found {
        key: event.pubkey.to_hex(),
        announcement,
        at: event.created_at.as_secs(),
    })
}

/// Whether `url` is an `http[s]://` URL of reasonable length.
fn usable(url: &str) -> bool {
    url.len() <= MAX_URL
        && (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(char::is_whitespace)
}
