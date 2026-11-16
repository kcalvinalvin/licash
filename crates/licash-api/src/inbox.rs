//! A hub's inbox: the payments made at the hub that it keeps for their
//! payees, as their payers ask on releasing them, so that a payee collects
//! them there, and tells its payer so, rather than through its mailbox on
//! Nostr.
//!
//! | Path | Body | Answer |
//! |------|------|--------|
//! | `POST /v1/release` | [`Release`] | [`Released`] |
//! | `GET /v1/inbox?after=` | [`Query`] | [`Feed`] |
//! | `POST /v1/inbox/take` | [`Take`] | [`Taken`] |
//! | `POST /v1/inbox/ack` | [`Acknowledge`] | [`Acknowledged`] |
//! | `POST /v1/inbox/acks` | [`AskAcks`] | [`Acks`] |
//!
//! Everything about a kept payment derives from the receipt secret its
//! payer and payee share (`licash::address::Ephemeral::shared`, and
//! `ScanKey::shared` of the proposal's ephemeral key), which the hub does
//! not know: the [`tag`] its payee finds it by in the feed, the [`key`] it
//! takes it with, and the [`ack`] it tells its payer it has it with. The
//! hub holds only the ack's [`commitment`]: it cannot make one, and learns
//! one only once the payee sent it.
//!
//! The feed is public: the sequence number, ephemeral key and tag of each
//! payment kept, none of which tells whom it pays. Its numbers only grow,
//! across restarts and losses of the hub's file. A hub keeps payments for
//! [`KEPT_FOR`]. One that keeps none answers [`Released`] without `kept`,
//! and these paths not at all.

use licash::{crypto::tagged_hash, spend::ProposalId};
use serde::{Deserialize, Serialize};

use crate::{Hex, Payment, Problem};

pub const INBOX: &str = "/v1/inbox";
pub const TAKE: &str = "/v1/inbox/take";
pub const ACK: &str = "/v1/inbox/ack";
pub const ACKS: &str = "/v1/inbox/acks";

/// How long a hub keeps a payment, in seconds: as long as relays keep a
/// payment mailed.
pub const KEPT_FOR: i64 = 14 * 24 * 60 * 60;

/// The most entries of the feed one answer holds.
pub const MAX_ENTRIES: usize = 1000;

/// The most payments one [`Take`], [`Acknowledge`] or [`AskAcks`] names.
pub const MAX_ASKED: usize = 100;

/// The bytes of a [`tag`].
pub const TAG: usize = 4;

/// What the payee of a payment finds it by in the feed, of the receipt
/// secret `shared`.
pub fn tag(shared: &[u8; 32]) -> [u8; TAG] {
    let hash = tagged_hash("licash/inbox/tag", &[shared]);
    let mut tag = [0; TAG];
    tag.copy_from_slice(&hash[..TAG]);
    tag
}

/// What the payee of a payment takes it with, of the receipt secret
/// `shared`.
pub fn key(shared: &[u8; 32]) -> [u8; 32] {
    tagged_hash("licash/inbox/key", &[shared])
}

/// What the payee of payment `id` tells its payer it has it with, of the
/// receipt secret `shared`.
pub fn ack(shared: &[u8; 32], id: &ProposalId) -> [u8; 32] {
    tagged_hash("licash/inbox/ack", &[shared, id.as_bytes()])
}

/// What the hub holds of `ack` to tell it.
pub fn commitment(ack: &[u8; 32]) -> [u8; 32] {
    tagged_hash("licash/inbox/commitment", &[ack])
}

/// The tag, key and commitment of a [`Keep`].
pub type Keeping = ([u8; TAG], [u8; 32], [u8; 32]);

/// Keep the payment for its payee, under `tag`, taken with `key`, and
/// acknowledged with the preimage of `commitment` under [`commitment`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keep {
    pub tag: Hex,
    pub key: Hex,
    pub commitment: Hex,
}

impl Keep {
    /// Of the payment `id`, with the receipt secret `shared`: and the ack
    /// its payee will tell.
    pub fn new(shared: &[u8; 32], id: &ProposalId) -> (Self, [u8; 32]) {
        let ack = ack(shared, id);
        let keep = Self {
            tag: Hex(tag(shared).to_vec()),
            key: Hex(key(shared).to_vec()),
            commitment: Hex(commitment(&ack).to_vec()),
        };
        (keep, ack)
    }

    /// Its tag, key and commitment.
    pub fn decode(&self) -> Result<Keeping, Problem> {
        Ok((
            self.tag.array("tag")?,
            self.key.array("key")?,
            self.commitment.array("commitment")?,
        ))
    }
}

/// A [`crate::Release`], and the payment it makes to keep for its payee,
/// at a hub that keeps them. Others take it as a plain release.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    #[serde(flatten)]
    pub release: crate::Release,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Keep>,
}

/// A [`crate::Released`], and whether the hub keeps the payment, as asked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Released {
    #[serde(flatten)]
    pub released: crate::Released,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept: bool,
}

/// The entries of the feed after `after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub after: u64,
}

/// The path of the entries of the feed after `after`.
pub fn feed(after: u64) -> String {
    format!("{INBOX}?after={after}")
}

/// Payments kept, in the order kept: at most [`MAX_ENTRIES`], `more`
/// after them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub entries: Vec<Entry>,
    pub more: bool,
}

/// A payment kept: its proposal's ephemeral key and its [`tag`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub ephemeral: Hex,
    pub tag: Hex,
}

impl Entry {
    /// Its ephemeral key, a compressed point, and tag.
    pub fn decode(&self) -> Result<([u8; 33], [u8; TAG]), Problem> {
        Ok((self.ephemeral.array("ephemeral")?, self.tag.array("tag")?))
    }
}

/// Take the payments of these entries, each with its [`key`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Take {
    pub payments: Vec<Kept>,
}

/// A payment kept, by its entry's sequence number, and its [`key`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub seq: u64,
    pub key: Hex,
}

/// The payments asked for, in the order asked: none for one not kept
/// under that key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Taken {
    pub payments: Vec<Option<Receipt>>,
}

/// A payment as a hub keeps it: without the hub's URL, which its payee
/// knows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    #[serde(flatten)]
    pub release: crate::Release,
    pub unlock: Hex,
}

impl Receipt {
    /// The payment, at the hub at `hub`.
    pub fn at(self, hub: &str) -> Payment {
        Payment {
            hub: hub.to_owned(),
            release: self.release,
            unlock: self.unlock,
        }
    }
}

/// Tell the payers of payments their payee has them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acknowledge {
    pub acks: Vec<Acknowledgment>,
}

/// The [`ack`] of the payment kept under [`key`] `key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgment {
    pub key: Hex,
    pub ack: Hex,
}

/// Whether each ack told is that of a payment kept, in the order told.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acknowledged {
    pub acked: Vec<bool>,
}

/// The acks the payees of the payments kept under these keys told.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AskAcks {
    pub keys: Vec<Hex>,
}

/// The acks asked for, in the order asked: none for a payment not
/// acknowledged, or not kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acks {
    pub acks: Vec<Option<Hex>>,
}

/// Each of `hexes` of `N` bytes, at most [`MAX_ASKED`] of them.
pub fn arrays<'h, const N: usize>(
    hexes: impl ExactSizeIterator<Item = &'h Hex>,
    what: &str,
) -> Result<Vec<[u8; N]>, Problem> {
    if hexes.len() > MAX_ASKED {
        return Err(Problem::invalid(format!("at most {MAX_ASKED} {what}s")));
    }
    hexes.map(|h| h.array(what)).collect()
}
