//! Announcements of a role watched on relays kept connected: each relay
//! asked for them all once, and to push each new one from then on, so a
//! look later takes what is kept at once, asking nothing, and a service
//! announced since shows as soon as a relay has it. Over Tor, connecting
//! to each relay anew takes seconds; a watch connects once.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nostr_sdk::prelude::{
    Client, ClientNotification, Filter, Kind, Relay, RelayStatus, RelayUrl, StreamExt,
};

use crate::{ask_each, client, read, Found, Role, Route, KIND, LIFETIME, MAX_EVENTS, MIN_GRACE};

/// How long what the relays pushed is taken as all there is, while one
/// of those pushing is connected: then they are asked for all again, in
/// case a push was missed.
const ASK_EVERY: Duration = Duration::from_secs(30 * 60);

/// The announcements of a role for a network on some relays, watched.
pub struct Watch {
    client: Client,
    relays: Vec<String>,
    route: Route,
    role: Role,
    network: String,
    /// The newest announcement of each key, as asked for or pushed.
    kept: Arc<Mutex<Kept>>,
    /// Taking in what the relays push, until dropped.
    listening: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct Kept {
    found: HashMap<String, Found>,
    /// The relays asked to push new announcements: for good, asked again
    /// as each reconnects.
    pushing: HashSet<RelayUrl>,
    /// When the relays were last asked for all of them: never if none.
    asked: Option<Instant>,
}

impl Kept {
    /// `found`, if newer than what its key announced before.
    fn take(&mut self, found: Found) {
        if (self.found.get(&found.key)).is_none_or(|old| old.at < found.at) {
            self.found.insert(found.key.clone(), found);
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.listening.abort();
        let client = self.client.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { client.shutdown().await });
        }
    }
}

impl Watch {
    /// A watch of `role`'s announcements for `network` on `relays`,
    /// reached through `route`: not connected until first looked at.
    pub async fn open(
        relays: &[String],
        route: &Route,
        role: Role,
        network: &str,
    ) -> anyhow::Result<Self> {
        let client = client(relays, route, None).await?;
        let kept = Arc::new(Mutex::new(Kept::default()));
        let mut notifications = client.notifications();
        let (into, owned) = (kept.clone(), network.to_owned());
        let listening = tokio::spawn(async move {
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    if let Some(found) = read(&event, role, &owned) {
                        lock(&into).take(found);
                    }
                }
            }
        });
        Ok(Self {
            client,
            relays: relays.to_vec(),
            route: route.clone(),
            role,
            network: network.to_owned(),
            kept,
            listening,
        })
    }

    /// Whether it watches `role` for `network` on `relays`.
    pub fn is(&self, relays: &[String], role: Role, network: &str) -> bool {
        self.relays == relays && self.role == role && self.network == network
    }

    /// The newest unexpired announcement of each key, newest first. The
    /// relays connected are asked for them all, each first asked to push
    /// the new ones (the others connecting for the next look), unless
    /// within [`ASK_EVERY`] of the last ask one of those pushing is still
    /// connected: then what was kept, at once. Fails only if none was
    /// kept and no relay answered.
    pub async fn found(&self) -> anyhow::Result<Vec<Found>> {
        let mut relays: Vec<(RelayUrl, Relay)> = self.client.relays().await.into_iter().collect();
        let pushing = {
            let kept = lock(&self.kept);
            let recent = kept.asked.is_some_and(|at| at.elapsed() < ASK_EVERY);
            recent && (relays.iter()).any(|(url, r)| kept.pushing.contains(url) && connected(r))
        };
        if !pushing {
            let timeout = self.route.timeout();
            if relays.iter().any(|(_, r)| connected(r)) {
                for (_, relay) in relays.iter().filter(|(_, r)| !connected(r)) {
                    let relay = relay.clone();
                    tokio::spawn(async move { relay.try_connect().timeout(timeout).await });
                }
                relays.retain(|(_, r)| connected(r));
            }
            let all = Filter::new()
                .kind(Kind::Custom(KIND))
                .identifier(self.role.id())
                .hashtag(self.role.topic())
                .limit(MAX_EVENTS);
            // None held already: those from now on.
            let new = all.clone().limit(0);
            let asked = Instant::now();
            let fetched = ask_each(relays, &self.route, MIN_GRACE, |relay| {
                let (all, new, kept) = (all.clone(), new.clone(), self.kept.clone());
                async move {
                    let url = relay.url().clone();
                    if !lock(&kept).pushing.contains(&url) {
                        // Kept by the client, asked again as the relay reconnects.
                        relay.subscribe(new).await?;
                        lock(&kept).pushing.insert(url);
                    }
                    Ok(relay.fetch_events(all).timeout(timeout).await?)
                }
            })
            .await;
            let mut kept = lock(&self.kept);
            match fetched {
                Ok(fetched) => {
                    for event in fetched.into_iter().flatten() {
                        if let Some(found) = read(&event, self.role, &self.network) {
                            kept.take(found);
                        }
                    }
                    kept.asked = Some(asked);
                }
                Err(e) if kept.found.is_empty() => return Err(e),
                Err(e) => tracing::info!("no relay answered: what was kept stands: {e:#}"),
            }
        }
        Ok(self.kept())
    }

    /// What it kept, unexpired, newest first.
    pub fn kept(&self) -> Vec<Found> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let kept = lock(&self.kept);
        let mut found: Vec<Found> = (kept.found.values())
            .filter(|f| f.at.saturating_add(LIFETIME.as_secs()) > now)
            .cloned()
            .collect();
        found.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.key.cmp(&b.key)));
        found
    }
}

fn connected(relay: &Relay) -> bool {
    relay.status() == RelayStatus::Connected
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
