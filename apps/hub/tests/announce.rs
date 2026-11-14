//! The hub announces itself on Nostr once it follows a chain, under the key
//! its info names.

use std::{sync::Arc, time::Duration};

use licash_hub::{Hub, Policy};
use licash_hub_service::announce::announce;
use licash_nostr::{discover, Role, Route};
use nostr_sdk::prelude::MockRelay;

mod chain;

#[tokio::test]
async fn a_hub_announces_its_urls_and_network_once_it_has_a_checkpoint() {
    let relay = MockRelay::run().await.unwrap();
    let relays = vec![relay.url().await.to_string()];
    let dir = tempfile::tempdir().unwrap();
    let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
    Hub::init(&seed, &db, 6).unwrap();
    let hub = Arc::new(Hub::open(&seed, &db, Policy::default()).unwrap());
    let urls = vec![
        "https://hub.example".to_owned(),
        "http://hub.onion".to_owned(),
    ];
    let announcing = tokio::spawn(announce(
        hub.clone(),
        relays.clone(),
        Route::Direct,
        urls.clone(),
        Some("a hub".into()),
    ));

    // Following no chain yet, it does not know its network.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
        .await
        .unwrap();
    assert!(found.is_empty());

    hub.add_checkpoint(&chain::Chain::new().checkpoint(0))
        .unwrap();
    let start = std::time::Instant::now();
    let found = loop {
        let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
            .await
            .unwrap();
        if !found.is_empty() || start.elapsed() > Duration::from_secs(30) {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(found.len(), 1);
    let info = hub.advertise().unwrap();
    assert!(found[0].named_by(info.nostr.as_deref()));
    assert_eq!(found[0].announcement.urls, urls);
    assert_eq!(found[0].announcement.name.as_deref(), Some("a hub"));
    announcing.abort();
}
