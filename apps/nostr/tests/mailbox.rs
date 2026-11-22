//! Payments sent to mailboxes through a relay in this process.

use std::time::Duration;

use licash_nostr::{
    mailbox::{self, KEEP},
    Route,
};
use nostr::nips::nip59::GiftWrapBuilder;
use nostr_sdk::prelude::{
    Client, EventBuilder, FinalizeEvent, FinalizeUnsignedEvent, Keys, Kind, MockRelay, SecretKey,
};

fn mailbox(secret: [u8; 32]) -> [u8; 32] {
    let keys = Keys::new(SecretKey::from_slice(&secret).unwrap());
    keys.public_key().to_bytes()
}

#[tokio::test]
async fn payments_sent_to_a_mailbox_are_opened_by_its_owner_alone() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let (alice, bob) = ([1; 32], [2; 32]);
    let to = mailbox(alice);
    let sent = mailbox::send(&to, r#"{"first":1}"#, &relays, &Route::Direct);
    assert_eq!(sent.await.unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let sent = mailbox::send(&to, r#"{"second":2}"#, &relays, &Route::Direct);
    assert_eq!(sent.await.unwrap(), 1);

    let mail = mailbox::fetch(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    let contents: Vec<&str> = mail.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, [r#"{"first":1}"#, r#"{"second":2}"#]);
    assert!(mail[0].at < mail[1].at && mail[0].id != mail[1].id);
    assert!(mailbox::fetch(&bob, &relays, &Route::Direct)
        .await
        .unwrap()
        .is_empty());

    // On the relay, only a gift wrap to Alice, of an unknown key, at a
    // time up to two days early, expiring KEEP after it.
    let client = Client::default();
    client.add_relay(&relays[0]).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let filter = nostr_sdk::prelude::Filter::new().kind(Kind::GiftWrap);
    let wraps = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(wraps.len(), 2);
    for wrap in wraps {
        assert!(wrap
            .tags
            .public_keys()
            .all(|k| k.to_bytes() == mailbox(alice)));
        assert_ne!(wrap.pubkey.to_bytes(), mailbox(alice));
        let at = wrap.created_at.as_secs();
        let expiry = wrap.tags.expiration().unwrap().as_secs();
        assert_eq!(expiry, at + KEEP.as_secs());
        let now = nostr_sdk::prelude::Timestamp::now().as_secs();
        assert!(at <= now && at + 2 * 24 * 60 * 60 >= now - 5, "{at} {now}");
    }
}

#[tokio::test]
async fn what_is_not_a_payment_gift_wrapped_to_the_mailbox_is_skipped() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let alice = [1; 32];
    let to = Keys::new(SecretKey::from_slice(&alice).unwrap()).public_key();
    let client = Client::default();
    client.add_relay(&relays[0]).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    // A private message (NIP-17), and a gift wrap that opens to nothing.
    let sender = Keys::generate();
    let message =
        EventBuilder::new(Kind::PrivateDirectMessage, "hi").finalize_unsigned(sender.public_key());
    let wrap = GiftWrapBuilder::new(to, message).finalize(&sender).unwrap();
    client.send_event(&wrap).await.unwrap();
    let junk = EventBuilder::new(Kind::GiftWrap, "not encrypted")
        .tag(nostr_sdk::prelude::Tag::public_key(to))
        .finalize(&Keys::generate())
        .unwrap();
    client.send_event(&junk).await.unwrap();
    mailbox::send(&mailbox(alice), "{}", &relays, &Route::Direct)
        .await
        .unwrap();
    let mail = mailbox::fetch(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].content, "{}");
}

#[tokio::test]
async fn a_mailbox_no_relay_answers_for_fails() {
    let relays = ["ws://127.0.0.1:1".to_owned()];
    let fetched = mailbox::fetch(&[1; 32], &relays, &Route::Direct).await;
    assert!(fetched
        .unwrap_err()
        .to_string()
        .contains("no Nostr relay answered"));
    let sent = mailbox::send(&mailbox([1; 32]), "{}", &relays, &Route::Direct).await;
    assert!(sent.is_err());
    assert!(mailbox::fetch(&[1; 32], &[], &Route::Direct).await.is_err());
}

/// A relay that takes connections and never answers: the WebSocket
/// handshake waits on it until the route's timeout.
async fn silent_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    url
}

#[tokio::test]
async fn a_relay_that_never_answers_holds_nothing_up() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string(), silent_relay().await];
    let alice = [1; 32];
    // Direct, a relay has 10 s to connect: the one answering is enough,
    // the other given a few seconds once it did.
    let to = mailbox(alice);
    let started = std::time::Instant::now();
    let sent = mailbox::send(&to, r#"{"paid":1}"#, &relays, &Route::Direct);
    assert_eq!(sent.await.unwrap(), 1);
    let mail = mailbox::fetch(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(12), "{took:?}");
}

#[tokio::test]
async fn a_payment_too_large_for_the_relays_is_said_too_large_to_mail() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let to = mailbox([1; 32]);
    // Some 40 KB of payment, wrapped in some 77 KB: more than the relay
    // takes, as most do.
    let payment = format!(r#"{{"signed":"{}"}}"#, "ab".repeat(20_000));
    let sent = mailbox::send(&to, &payment, &relays, &Route::Direct).await;
    let why = format!("{:#}", sent.unwrap_err());
    assert!(
        why.starts_with("too large to mail, 7")
            && why.contains(" KB where relays commonly take 64 KB at most: "),
        "{why}"
    );
    assert!(why.contains("exceeds maximum allowed size"), "{why}");
    assert!(mailbox::fetch(&[1; 32], &relays, &Route::Direct)
        .await
        .unwrap()
        .is_empty());

    // A payment that fits is mailed as ever.
    let payment = format!(r#"{{"signed":"{}"}}"#, "ab".repeat(10_000));
    let sent = mailbox::send(&to, &payment, &relays, &Route::Direct).await;
    assert_eq!(sent.unwrap(), 1);
}

#[tokio::test]
async fn no_relay_answering_fails_in_the_routes_time() {
    let relays = [silent_relay().await];
    let to = mailbox([1; 32]);
    let started = std::time::Instant::now();
    let sent = mailbox::send(&to, "{}", &relays, &Route::Direct).await;
    let why = format!("{:#}", sent.unwrap_err());
    assert!(why.contains("no Nostr relay answered"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[tokio::test]
async fn a_look_waits_on_a_relay_never_answering_about_as_long_as_the_first_took() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string(), silent_relay().await];
    let alice = [1; 32];
    let to = mailbox(alice);
    let sent = mailbox::send(&to, r#"{"paid":1}"#, &relays, &Route::Direct);
    assert_eq!(sent.await.unwrap(), 1);
    // The relay answering does so at once: the other is given a second.
    let started = std::time::Instant::now();
    let mail = mailbox::fetch(&alice, &relays, &Route::Direct);
    assert_eq!(mail.await.unwrap().len(), 1);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[tokio::test]
async fn a_mailbox_kept_open_finds_the_payments_sent_since() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string(), silent_relay().await];
    let (alice, bob) = ([1; 32], [2; 32]);
    let kept = mailbox::Mailbox::open(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    assert!(kept.is(&alice, &relays));
    assert!(!kept.is(&bob, &relays));
    assert!(!kept.is(&alice, &relays[..1]));
    assert!(kept.fetch().await.unwrap().is_empty());
    let to = mailbox(alice);
    for paid in 1..=2 {
        let content = format!(r#"{{"paid":{paid}}}"#);
        mailbox::send(&to, &content, &relays, &Route::Direct)
            .await
            .unwrap();
        // Connected already, the other still trying: answered at once.
        let started = std::time::Instant::now();
        let mail = kept.fetch().await.unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert_eq!(mail.len(), paid);
        assert_eq!(mail[paid - 1].content, content);
    }
    kept.close().await;
}

#[tokio::test]
async fn mail_done_with_is_not_returned_again_by_the_mailbox_kept() {
    let relay = MockRelay::run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let alice = [1; 32];
    let to = mailbox(alice);
    for paid in 1..=2 {
        let content = format!(r#"{{"paid":{paid}}}"#);
        mailbox::send(&to, &content, &relays, &Route::Direct)
            .await
            .unwrap();
    }
    let kept = mailbox::Mailbox::open(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    let mail = kept.fetch().await.unwrap();
    assert_eq!(mail.len(), 2);
    kept.done(&mail[0].id);
    kept.done("not an identifier");
    assert_eq!(kept.fetch().await.unwrap(), [mail[1].clone()]);
    // Another look, apart, sees it all.
    let all = mailbox::fetch(&alice, &relays, &Route::Direct).await;
    assert_eq!(all.unwrap(), mail);
    kept.close().await;
}

/// A relay whose answer is all a look asks for is said full: newer mail,
/// junk anyone may gift wrap, may hide an older payment there. One holding
/// less is not.
#[tokio::test]
async fn a_relay_answering_all_the_mail_a_look_asks_for_is_said_full() {
    use nostr_sdk::prelude::{LocalRelay, RateLimit, Tag};
    let relay = LocalRelay::builder()
        .rate_limit(RateLimit {
            max_reqs: 500,
            notes_per_minute: 10_000,
        })
        .build();
    relay.run().await.unwrap();
    let relays = [relay.url().await.to_string()];
    let alice = [1; 32];
    let to = mailbox(alice);
    mailbox::send(&to, r#"{"paid":1}"#, &relays, &Route::Direct)
        .await
        .unwrap();
    let kept = mailbox::Mailbox::open(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    assert_eq!(kept.fetch().await.unwrap().len(), 1);
    assert!(kept.full().is_empty());
    kept.close().await;

    let client = Client::default();
    client.add_relay(&relays[0]).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let to = Keys::new(SecretKey::from_slice(&alice).unwrap()).public_key();
    for _ in 0..500 {
        let junk = EventBuilder::new(Kind::GiftWrap, "junk")
            .tag(Tag::public_key(to))
            .finalize(&Keys::generate())
            .unwrap();
        client.send_event(&junk).await.unwrap();
    }
    let kept = mailbox::Mailbox::open(&alice, &relays, &Route::Direct)
        .await
        .unwrap();
    // The payment among the newest 500 no longer: hidden, and said so.
    assert!(kept.fetch().await.unwrap().is_empty());
    assert_eq!(kept.full().len(), 1, "{:?}", kept.full());
    kept.close().await;
}
