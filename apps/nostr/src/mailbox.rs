//! Mailboxes: a wallet's key on Nostr, to which payers send the payments
//! they make it, as private messages are (NIP-17): a rumor of kind
//! [`PAYMENT`] holding the payment, sealed by a key of the payer's own for
//! that message and gift wrapped to the mailbox (NIP-59). Relays see only
//! that something is addressed to the mailbox, at a time up to two days
//! off; its payee alone opens it.
//!
//! Nothing says a payment arrived: a payer sends it again until its payee
//! has it, or hands it over otherwise, and the payee takes each once.

use std::time::Duration;

use anyhow::Context;
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr_sdk::prelude::{
    Event, EventBuilder, Filter, FinalizeEvent, FinalizeUnsignedEvent, Keys, Kind, PublicKey,
};

use crate::{client, connected, keys, Route};

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
    client.connect().and_wait(route.timeout()).await;
    let sent = client.send_event(&wrap).await;
    client.shutdown().await;
    let sent = sent.context("send to the mailbox")?;
    if sent.success.is_empty() {
        let failed: Vec<String> = sent
            .failed
            .iter()
            .map(|(relay, e)| format!("{relay}: {e}"))
            .collect();
        anyhow::bail!("no relay took the payment: {}", failed.join("; "));
    }
    Ok(sent.success.len())
}

/// The payments in the mailbox of `secret` on `relays`, asked through
/// `route`, oldest first: each once, whatever relays hold it. Relays that
/// do not answer in time are skipped; fails only if none answered.
pub async fn fetch(
    secret: &[u8; 32],
    relays: &[String],
    route: &Route,
) -> anyhow::Result<Vec<Mail>> {
    let keys = keys(secret)?;
    // Relays may serve gift wraps only to their addressee (NIP-42).
    let client = client(relays, route, Some(keys.clone())).await?;
    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(keys.public_key())
        .limit(MAX_MAIL);
    let events = match connected(&client, route).await {
        Ok(relays) => {
            let targets: Vec<_> = relays
                .into_iter()
                .map(|r| (r, vec![filter.clone()]))
                .collect();
            client.fetch_events(targets).timeout(route.timeout()).await
        }
        Err(e) => {
            client.shutdown().await;
            return Err(e);
        }
    };
    client.shutdown().await;
    let events = events.context("ask the Nostr relays")?;
    let mut mail: Vec<Mail> = events.into_iter().filter_map(|e| open(&keys, &e)).collect();
    mail.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    Ok(mail)
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
