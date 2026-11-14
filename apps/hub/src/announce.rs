//! The hub's announcement on Nostr relays, for wallets to discover it.

use std::{sync::Arc, time::Duration};

use licash_hub::Hub;
use licash_nostr::{Announcement, Announcer, Role};

/// Announce `hub` at `urls`, named `name`, on `relays`, for good: once its
/// first checkpoint names the network it follows.
pub async fn announce(hub: Arc<Hub>, relays: Vec<String>, urls: Vec<String>, name: Option<String>) {
    let profile = loop {
        if let Some(checkpoint) = hub.checkpoint() {
            break checkpoint.profile();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let announcement = Announcement {
        urls,
        network: licash_chain_client::network(profile).to_string(),
        name,
    };
    match Announcer::new(&hub.nostr_secret(), Role::Hub, &relays).await {
        Ok(announcer) => announcer.run(announcement).await,
        Err(e) => tracing::error!("announcing on Nostr: {e:#}"),
    }
}
