//! Recovery contracts opened over HTTP between a holder's wallet and a
//! hub: the hub's side and its routes ([`hub`]), the holder's ([`holder`]),
//! and what they send each other.
//!
//! An opening is the exchange of `licash_contract::opening`'s two machines,
//! carried as their envelopes. The holder takes the hub's [`Offer`] at
//! [`QUOTE`], opens on it at [`OPEN`], then POSTs each of her messages to
//! [`STEP`]. The hub answers each with its own messages and the instances
//! she is to keep, which she fetches one by one at [`INSTANCE`]. A step of
//! the hub's that takes long (funding, building its instances) answers
//! 202 until done: she sends the same message again, and gets the answer.
//!
//! Openings under the mock authority (anyone "proves" anything) are for
//! regtest only, and say so: [`trust`].

use anyhow::{bail, ensure, Result};
use licash::spend::ChainProfile;
use licash_contract::opening::{Bounds, Quote, Trust};
use licash_contract::wire::{Reader, Writer};

pub mod holder;
pub mod hub;

/// GET: the hub's [`Offer`].
pub const QUOTE: &str = "/v1/contracts/quote";
/// POST an [`Opening`]: [`Frame`]s, or 202 while working on them.
pub const OPEN: &str = "/v1/contracts/open";
/// POST the holder's next message of opening `id` (hex): as [`OPEN`].
pub const STEP: &str = "/v1/contracts/openings/{id}";
/// GET instance `index` of assertion `stage` the hub serves opening `id`.
pub const INSTANCE: &str = "/v1/contracts/openings/{id}/instances/{stage}/{index}";

/// The most bytes a request or an answer of an opening holds.
pub const MAX_BODY: usize = 64 << 20;

/// What a hub opens contracts on: its quote, whose checkpoint is a recent
/// block of its chain, and how far beyond it a holder may ask.
#[derive(Clone)]
pub struct Offer {
    pub quote: Quote,
    pub bounds: Bounds,
}

impl Offer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.quote(&self.quote);
        w.u32(self.bounds.lock);
        w.u16(self.bounds.response_window);
        w.0
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let quote = r.quote()?;
        let bounds = Bounds {
            lock: r.u32()?,
            response_window: r.u16()?,
        };
        r.end()?;
        Ok(Self { quote, bounds })
    }

    /// What opening on it costs the holder, in sat: her reserve and the
    /// hub's fee, from her deposit.
    pub fn price(&self) -> u64 {
        let q = &self.quote;
        q.graph
            .reserve_alice
            .saturating_add(q.fees.opening_fee_quote)
    }
}

/// What a holder POSTs to open: the quote she opens on, and her Open.
pub struct Opening {
    pub quote: Quote,
    pub open: Vec<u8>,
}

impl Opening {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.quote(&self.quote);
        w.blob(&self.open);
        w.0
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let quote = r.quote()?;
        let open = r.blob()?.to_vec();
        r.end()?;
        Ok(Self { quote, open })
    }
}

/// What a hub answers a message with, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// One of its messages.
    Message(Vec<u8>),
    /// An instance the holder keeps, to fetch at [`INSTANCE`].
    Instance { stage: usize, index: usize },
}

impl Frame {
    pub fn encode_all(frames: &[Frame]) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.count(frames.len());
        for frame in frames {
            match frame {
                Frame::Message(bytes) => {
                    w.u8(0);
                    w.blob(bytes);
                }
                Frame::Instance { stage, index } => {
                    w.u8(1);
                    w.u16(*stage as u16);
                    w.u16(*index as u16);
                }
            }
        }
        w.0
    }

    pub fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>> {
        let mut r = Reader(bytes);
        let n = r.count(1)?;
        let mut frames = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            frames.push(match r.u8()? {
                0 => Frame::Message(r.blob()?.to_vec()),
                1 => Frame::Instance {
                    stage: usize::from(r.u16()?),
                    index: usize::from(r.u16()?),
                },
                tag => bail!("a frame of tag {tag}"),
            });
        }
        r.end()?;
        Ok(frames)
    }
}

/// Who a party of an opening on `profile` trusts for proofs: the mock
/// authority on regtest, labeled as such. Elsewhere the genuine programs
/// are needed, which this build does not name yet.
pub fn trust(profile: ChainProfile) -> Result<Trust> {
    ensure!(
        profile == ChainProfile::Core30DefaultRegtest,
        "contracts on {profile} need the genuine proof programs, which this build does not name yet: mock proofs are for regtest only"
    );
    Ok(Trust::mock())
}

/// The SQLite file at `path`, readable by its owner only, made so before
/// anything is written to it.
pub(crate) fn private_db(path: &std::path::Path) -> Result<rusqlite::Connection> {
    if !path.exists() {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        file.open(path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    }
    let c = rusqlite::Connection::open(path)?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "FULL")?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(c)
}

/// Seconds since 1970.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `id` in hex, as paths name an opening.
pub fn hex_id(id: &[u8; 32]) -> String {
    hex::encode(id)
}

/// The opening `hex` names.
pub fn parse_id(hex: &str) -> Result<[u8; 32]> {
    let mut id = [0; 32];
    hex::decode_to_slice(hex, &mut id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_read_back_as_written() {
        let frames = vec![
            Frame::Message(vec![1, 2, 3]),
            Frame::Instance { stage: 1, index: 3 },
            Frame::Message(Vec::new()),
        ];
        let bytes = Frame::encode_all(&frames);
        assert_eq!(Frame::decode_all(&bytes).unwrap(), frames);
        assert!(Frame::decode_all(&bytes[..bytes.len() - 1]).is_err());
        let mut tagged = bytes.clone();
        // The second frame's tag.
        let at = 4 + 1 + 4 + 3;
        tagged[at] = 7;
        assert!(Frame::decode_all(&tagged).is_err());
    }

    #[test]
    fn mock_proofs_are_for_regtest_only() {
        assert!(trust(ChainProfile::Core30DefaultRegtest).unwrap().mock);
        for profile in [ChainProfile::Core30Mainnet, ChainProfile::Core30Signet] {
            assert!(trust(profile).is_err());
        }
    }
}
