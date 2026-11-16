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
//! regtest only, and say so; on signet they are under the genuine programs
//! kept in `r0/programs/`: [`trust`].
//!
//! The `node` feature, on by default, adds the hub's side and a node or an
//! Esplora API as the holder's chain: a wallet bringing its own chain, as
//! a phone's light client, goes without.

use anyhow::{bail, Result};
use hex_literal::hex;
use licash::base::Programs;
use licash::spend::ChainProfile;
use licash::statement::Journal;
use licash_contract::opening::{Bounds, Quote, Statement, Trust};
use licash_contract::wire::{Reader, Writer};

pub mod holder;
#[cfg(feature = "node")]
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
/// authority on regtest, labeled as such; on signet the genuine programs
/// [`SIGNET`], under RISC Zero's shared Groth16 setup. None elsewhere yet.
///
/// Signet's are a trial: its headers are checked by their work alone, not
/// by the network's signature of each block, so headers that fool the
/// programs cost little to make.
pub fn trust(profile: ChainProfile) -> Result<Trust> {
    match profile {
        ChainProfile::Core30DefaultRegtest => Ok(Trust::mock()),
        ChainProfile::Core30Signet => Ok(Trust {
            programs: SIGNET,
            statement: signet_statement,
            mock: false,
        }),
        _ => bail!("contracts are not opened on {profile} yet"),
    }
}

/// The genuine programs contracts open under on signet, as built by
/// `r0/build-guest.sh` with the toolchains it pins, from the commit
/// `r0/README.md` names. Each is kept as `r0/programs/<image id>.bin`:
/// the contracts opened under one are proven with it alone, so none named
/// here is ever to go. Import verification currently supports only this
/// holder image; preserve it until prior-image verification is implemented.
pub const SIGNET: Programs = Programs {
    holder: hex!("8be30cb2a683e896d251c7c363dddf794dc8ddb9d502922199cfa4d2f73b1390"),
    counter: hex!("25051741d3d6438bc7f78ebf074a4b93f30363729564345bb800efd43ce21129"),
};

/// What a proof by [`SIGNET`]'s holder program answers.
fn signet_statement(journal: &Journal) -> Statement {
    Statement::genuine(&SIGNET.holder, journal)
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
    fn the_holder_image_stays_fixed_until_prior_openings_remain_verifiable() {
        assert_eq!(
            SIGNET.holder,
            hex!("8be30cb2a683e896d251c7c363dddf794dc8ddb9d502922199cfa4d2f73b1390"),
            "changing the holder image strands prior funded openings: full and corrected proofs use the current authority; add prior-image verification and a genuine prior-image proof regression first"
        );
    }

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
        // Signet's are genuine; none elsewhere yet.
        let signet = trust(ChainProfile::Core30Signet).unwrap();
        assert!(!signet.mock);
        assert_eq!(signet.programs, SIGNET);
        for profile in [ChainProfile::Core30Mainnet, ChainProfile::Mutinynet] {
            assert!(trust(profile).is_err());
        }
    }

    #[test]
    fn the_programs_contracts_open_under_are_kept_by_image_id() {
        let kept = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../r0/programs");
        for id in [SIGNET.holder, SIGNET.counter] {
            let program = kept.join(format!("{}.bin", hex::encode(id)));
            let size = std::fs::metadata(&program).map_or(0, |m| m.len());
            assert!(size > 0, "{} is not kept", program.display());
        }
    }

    #[test]
    fn signet_proofs_answer_risc_zeros_key_for_its_holder_program() {
        let journal = Journal::from_bytes(&[5; 68]).unwrap();
        let statement = (trust(ChainProfile::Core30Signet).unwrap().statement)(&journal);
        let holders = Statement::genuine(&SIGNET.holder, &journal);
        assert_eq!(statement.public, holders.public);
        assert_eq!(statement.vk, holders.vk);
        // Not the mock key, nor the counter program's inputs.
        let mock = (Trust::mock().statement)(&journal);
        assert_ne!(statement.vk, mock.vk);
        let counters = Statement::genuine(&SIGNET.counter, &journal);
        assert_ne!(statement.public, counters.public);
    }
}
