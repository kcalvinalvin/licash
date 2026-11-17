//! Announcements through a relay in this process.

use std::time::Duration;

use licash_nostr::{discover, public_key, Announcement, Announcer, Role, Route, KIND, LIFETIME};
use nostr_sdk::prelude::{
    Client, EventBuilder, FinalizeEvent, Keys, Kind, MockRelay, SecretKey, Tag, Timestamp,
};

fn announcement(url: &str, network: &str) -> Announcement {
    Announcement {
        urls: vec![url.into()],
        network: network.into(),
        name: Some("a hub".into()),
    }
}

/// A client sending events as they are, valid or not.
async fn raw(relay: &str) -> Client {
    let client = Client::default();
    client.add_relay(relay).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    client
}

#[tokio::test]
async fn a_hub_announced_is_discovered_once_by_its_newest_announcement() {
    let relay = MockRelay::run().await.unwrap();
    let url = relay.url().await.to_string();
    let relays = [url.clone()];
    let secret = [7; 32];
    let hub = Announcer::new(&secret, Role::Hub, &relays, &Route::Direct)
        .await
        .unwrap();
    assert_eq!(hub.key(), public_key(&secret).unwrap());
    assert_eq!(
        hub.announce(&announcement("https://hub.example", "regtest"))
            .await
            .unwrap(),
        1
    );

    let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, hub.key());
    assert_eq!(found[0].announcement.urls, ["https://hub.example"]);
    assert_eq!(found[0].announcement.name.as_deref(), Some("a hub"));
    assert!(found[0].named_by(Some(&hub.key().to_uppercase())));
    assert!(!found[0].named_by(Some(&public_key(&[8; 32]).unwrap())));
    assert!(!found[0].named_by(None));
    assert!(discover(&relays, &Route::Direct, Role::Hub, "bitcoin")
        .await
        .unwrap()
        .is_empty());
    assert!(discover(&relays, &Route::Direct, Role::Swaps, "regtest")
        .await
        .unwrap()
        .is_empty());

    // An older announcement of the same key is not the one found, even
    // where a relay keeps it.
    let client = raw(&url).await;
    let older = hub
        .event(
            &announcement("https://old.example", "regtest"),
            Timestamp::now() - Duration::from_secs(60),
        )
        .unwrap();
    client.send_event(&older).await.ok();
    let newer = hub
        .event(
            &announcement("https://new.example", "regtest"),
            Timestamp::now() + Duration::from_secs(1),
        )
        .unwrap();
    client.send_event(&newer).await.unwrap();
    let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].announcement.urls, ["https://new.example"]);
}

#[tokio::test]
async fn expired_malformed_and_unusable_announcements_are_skipped() {
    let relay = MockRelay::run().await.unwrap();
    let url = relay.url().await.to_string();
    let relays = [url.clone()];
    let client = raw(&url).await;
    let event = |keys: &Keys, content: &str, tags: Vec<Tag>, at: Timestamp| {
        EventBuilder::new(Kind::Custom(KIND), content)
            .tags(tags)
            .custom_created_at(at)
            .finalize(keys)
            .unwrap()
    };
    let hub_tags = |expiry: Timestamp| {
        vec![
            Tag::identifier("hub"),
            Tag::hashtag("licash-hub"),
            Tag::expiration(expiry),
        ]
    };
    let now = Timestamp::now();
    let good = serde_json::to_string(&announcement("http://hub.onion", "regtest")).unwrap();
    let bad_urls = serde_json::to_string(&Announcement {
        urls: vec!["ftp://hub.example".into(), "javascript:alert(1)".into()],
        network: "regtest".into(),
        name: None,
    })
    .unwrap();
    let mixed_urls = serde_json::to_string(&Announcement {
        urls: vec![
            "file:///etc/passwd".into(),
            "https://a.example".into(),
            "https://b.example".into(),
        ],
        network: "regtest".into(),
        name: None,
    })
    .unwrap();
    let keys: Vec<Keys> = (1..=6u8)
        .map(|i| Keys::new(SecretKey::from_slice(&[i; 32]).unwrap()))
        .collect();
    let sent = [
        // Expired: signed long ago, and its expiration past.
        event(
            &keys[0],
            &good,
            hub_tags(now - LIFETIME),
            now - LIFETIME - LIFETIME,
        ),
        // Not JSON of an announcement.
        event(&keys[1], "{\"urls\": 7}", hub_tags(now + LIFETIME), now),
        // No usable URL.
        event(&keys[2], &bad_urls, hub_tags(now + LIFETIME), now),
        // A swap provider's, tagged as a hub's.
        event(
            &keys[3],
            &good,
            vec![
                Tag::identifier("swaps"),
                Tag::hashtag("licash-hub"),
                Tag::expiration(now + LIFETIME),
            ],
            now,
        ),
        event(&keys[4], &mixed_urls, hub_tags(now + LIFETIME), now),
        event(&keys[5], &good, hub_tags(now + LIFETIME), now),
    ];
    for event in &sent {
        // The relay may refuse the expired one itself.
        client.send_event(event).await.ok();
    }
    let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
        .await
        .unwrap();
    let mut keys_found: Vec<&str> = found.iter().map(|f| f.key.as_str()).collect();
    keys_found.sort();
    let mut expected = vec![keys[4].public_key().to_hex(), keys[5].public_key().to_hex()];
    expected.sort();
    assert_eq!(keys_found, expected);
    let mixed = found
        .iter()
        .find(|f| f.key == keys[4].public_key().to_hex())
        .unwrap();
    assert_eq!(
        mixed.announcement.urls,
        ["https://a.example", "https://b.example"]
    );
}

#[tokio::test]
async fn a_relay_down_is_skipped_while_another_answers() {
    let relay = MockRelay::run().await.unwrap();
    let up = relay.url().await.to_string();
    let down = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("ws://{}", listener.local_addr().unwrap())
    };
    let relays = [down.clone(), up];
    let hub = Announcer::new(&[9; 32], Role::Hub, &relays, &Route::Direct)
        .await
        .unwrap();
    assert_eq!(
        hub.announce(&announcement("https://hub.example", "regtest"))
            .await
            .unwrap(),
        1
    );
    let found = discover(&relays, &Route::Direct, Role::Hub, "regtest")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);

    let failed = discover(&[down], &Route::Direct, Role::Hub, "regtest").await;
    let failed = failed.unwrap_err().to_string();
    assert!(failed.contains("no Nostr relay answered"), "{failed}");
    assert!(discover(&[], &Route::Direct, Role::Hub, "regtest")
        .await
        .is_err());
}

#[tokio::test]
async fn a_watch_is_pushed_each_provider_announced_once_looked_at() {
    use licash_nostr::watch::Watch;
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let first = Announcer::new(&[7; 32], Role::Swaps, &relays, &Route::Direct)
        .await
        .unwrap();
    first
        .announce(&announcement("https://one.example", "regtest"))
        .await
        .unwrap();
    let watch = Watch::open(&relays, &Route::Direct, Role::Swaps, "regtest")
        .await
        .unwrap();
    assert!(watch.is(&relays, Role::Swaps, "regtest"));
    assert!(!watch.is(&relays, Role::Hub, "regtest"));
    // Nothing kept until looked at; then what the relay holds.
    assert!(watch.kept().is_empty());
    let found = watch.found().await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, first.key());

    // Announced since: pushed, kept without asking.
    let second = Announcer::new(&[8; 32], Role::Swaps, &relays, &Route::Direct)
        .await
        .unwrap();
    second
        .announce(&announcement("https://two.example", "regtest"))
        .await
        .unwrap();
    // Of another network: left out.
    let other = Announcer::new(&[9; 32], Role::Swaps, &relays, &Route::Direct)
        .await
        .unwrap();
    other
        .announce(&announcement("https://three.example", "bitcoin"))
        .await
        .unwrap();
    let waited = std::time::Instant::now();
    while watch.kept().len() < 2 && waited.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let kept = watch.kept();
    assert_eq!(kept.len(), 2, "{kept:?}");
    let mut urls: Vec<&str> = kept
        .iter()
        .map(|f| f.announcement.urls[0].as_str())
        .collect();
    urls.sort_unstable();
    assert_eq!(urls, ["https://one.example", "https://two.example"]);
    assert_eq!(watch.found().await.unwrap(), kept);
}
