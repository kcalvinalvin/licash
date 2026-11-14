//! The swap provider API, version 1: a provider swaps notes at one hub for
//! notes at another, atomically, with a timed lock at each under one hash.
//!
//! | Path | Body | Answer |
//! |------|------|--------|
//! | `GET /v1/swaps/info` |  | [`Info`] |
//! | `POST /v1/swaps`     | [`Request`] | [`Swap`] |
//! | `GET /v1/swaps/{id}` |  | [`Swap`] |
//!
//! The client draws a preimage and locks, at hub `from`, the amount and the
//! provider's fee to [`Info::address`] under its hash, then asks for the
//! swap. Once the provider checked that lock, it locks the amount at hub
//! `to` to the client under the same hash, expiring [`Info::margin`]
//! blocks earlier at least. The client claims that lock with the preimage,
//! which its hub then shows the provider, who claims the client's lock with
//! it. A lock unclaimed is refunded once expired: if the provider never
//! locks, or the client never claims, each gets its own notes back.
//!
//! Hubs are named by their addresses ([`crate::Info::address`]), as client
//! and provider may reach one hub at different URLs. Anything else is a
//! [`Problem`]; [`crate::Code::Declined`] when the provider takes no such
//! swap.

use licash::{address::Address, spend::ProposalId};
use serde::{Deserialize, Serialize};

use crate::{Fee, Hex, Problem};

pub const SWAPS: &str = "/v1/swaps";
pub const SWAPS_INFO: &str = "/v1/swaps/info";
/// The path of [`swap_state`], as a route.
pub const SWAP_STATE: &str = "/v1/swaps/{id}";

/// Where the swap of the client's lock `id` stands.
pub fn swap_state(id: &ProposalId) -> String {
    format!("{SWAPS}/{}", hex::encode(id.as_bytes()))
}

/// What a provider swaps, and on what terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// The address the client's lock pays.
    pub address: Address,
    /// Its fee on the amount swapped, which the client's lock pays on top.
    pub fee: Fee,
    /// The least and the most it swaps.
    pub min: u64,
    pub max: u64,
    /// Its lock expires this many blocks before the client's, at least.
    pub margin: u32,
    /// The hubs it swaps between.
    pub hubs: Vec<Hub>,
    /// The key it announces itself under on Nostr, x-only in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nostr: Option<String>,
}

impl Info {
    /// The hub at `address`, if the provider swaps there.
    pub fn hub(&self, address: &Address) -> Option<&Hub> {
        self.hubs.iter().find(|h| h.address == *address)
    }
}

/// A hub a provider swaps at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hub {
    pub address: Address,
    /// The most it locks to a client there now.
    pub max: u64,
}

/// Swap the client's `lock` at hub `from`, which pays the provider, for a
/// lock of `amount` to `address` at hub `to` under the same hash. The same
/// request again answers where the swap stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub from: Address,
    pub lock: Hex,
    pub to: Address,
    pub address: Address,
    pub amount: u64,
}

impl Request {
    pub fn new(
        from: &Address,
        lock: &ProposalId,
        to: &Address,
        address: &Address,
        amount: u64,
    ) -> Self {
        Self {
            from: *from,
            lock: Hex(lock.as_bytes().to_vec()),
            to: *to,
            address: *address,
            amount,
        }
    }

    pub fn lock(&self) -> Result<ProposalId, Problem> {
        Ok(ProposalId::from_bytes(self.lock.array("lock")?))
    }
}

/// Where a swap stands, by the client's lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    /// The client's lock at `from`.
    pub id: Hex,
    pub state: State,
    pub amount: u64,
    /// The provider's lock at `to`, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Hex>,
    /// Its expiry.
    pub expiry: u32,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Swap {
    /// The provider's lock, decoded.
    pub fn lock(&self) -> Result<Option<ProposalId>, Problem> {
        self.lock
            .as_ref()
            .map(|l| Ok(ProposalId::from_bytes(l.array("lock")?)))
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Taken: the provider is locking at `to`.
    Accepted,
    /// The provider's lock is certified: the client claims it.
    Locked,
    /// The client claimed it: the provider claims the client's lock.
    Paid,
    /// Both locks paid.
    Done,
    /// The client never claimed: the provider's lock is refunded, and the
    /// client's will be once expired.
    Refunded,
    /// The provider did not lock: the client's lock is refunded once
    /// expired.
    Failed,
}
