//! Canonical Base: the contract's terms, keys and program, fixed before
//! the holder garbles for its statement, and committed by its digest
//! ([`Base::digest`]): what every claim's context binds, and the word of
//! the Hub's statement (`licash-hp`).
//!
//! The dispute is Lichen's hub-proves graph: the holder requests `b`, the
//! Hub responds with what she is owed, `b′`, and proves it. Its payout
//! rounds `b′` down to whole `2^low_bits` units (the bits below have no
//! output): a holder recovering through a dispute loses less than
//! `2^low_bits` satoshis to rounding, as she pays her own exit's fees.

use crate::{
    chain::{Checkpoint, OutPoint},
    crypto,
    note::Keyset,
    Error,
};

/// The tags of the Base's versions: each names its own statement rules.
const BASE_TAG_V3: &[u8] = b"LICASH-BASE\x03";
const BASE_TAG_V4: &[u8] = b"LICASH-BASE\x04";
const BASE_TAG_V5: &[u8] = b"LICASH-BASE\x05";
pub const MAX_BASE_BYTES: usize = 128_000;
const MAX_MONEY: u64 = 2_100_000_000_000_000;
/// The one graph a Base can name: Lichen's hub-proves graph.
pub const GRAPH_VERSION: u32 = 2;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// Independent public party identities, holder first and Hub second. Payout
/// destinations are selected scripts, never Base-derived compiled graph roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Party {
    pub funding: [u8; 32],
    pub payment: [u8; 32],
    pub delayed: [u8; 32],
    pub payout_script: Vec<u8>,
}

/// The graph's public parameters (Lichen's `Terms`, `ChannelParams` and
/// `Windows`). Absolute deadlines are block heights; windows are relative,
/// in blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphTerms {
    pub id: u32,
    pub unit_sats: u64,
    pub bits: u8,
    pub low_bits: u8,
    pub deposit_units: u32,
    pub close_height: u32,
    pub reserve_alice: u64,
    pub reserve_hub: u64,
    pub to_self_delay: u16,
    pub commit_fee: u64,
    pub presign_fee: u64,
    pub dust: u64,
    pub delta: u16,
    pub delta_prime: u16,
    pub deadline_margin: u32,
    /// The latest height of her request (`t_a`): the windows after it end
    /// before `close_height`.
    pub request_height: u32,
    /// `w_H`: the Hub's response after her request.
    pub response_window: u16,
    /// `k`: her claim's publication after her request; then the Hub may
    /// reclaim the slot.
    pub publication_window: u16,
    /// `w_A`: the Hub's assertion after its response.
    pub assert_window: u16,
    /// `Δ2`: her challenge of the assertion.
    pub challenge_window: u16,
    /// `Δ1`: the Hub's answer to her challenge.
    pub answer_window: u16,
    /// `w_X`: her contest of the Hub's response, after it. Left alone, the
    /// response pays her what it says, and nobody proves anything.
    pub contest_window: u16,
}

impl GraphTerms {
    pub fn maximum_amount(&self) -> Result<u32, Error> {
        if !(1..=32).contains(&self.bits) || self.low_bits >= self.bits {
            return Err(bad("native graph amount dimensions"));
        }
        Ok(((1u64 << self.bits) - 1) as u32)
    }

    /// The windows after her request, summed: none ends after
    /// `close_height`.
    fn dispute_blocks(&self) -> u32 {
        [
            self.response_window,
            self.contest_window,
            self.assert_window,
            self.challenge_window,
            self.answer_window,
        ]
        .into_iter()
        .map(u32::from)
        .sum()
    }

    pub fn graph_value(&self) -> Result<u64, Error> {
        u64::from(self.maximum_amount()?)
            .checked_add(self.reserve_alice)
            .and_then(|sum| sum.checked_add(self.reserve_hub))
            .filter(|sum| *sum <= MAX_MONEY)
            .ok_or_else(|| bad("native graph monetary range"))
    }
}

/// The statement's program and the cut-and-choose: the holder garbles
/// `instances` for it, of which the Hub keeps `kept`. The program is the
/// image id of the Hub's statement's guest; each party checks it is one it
/// trusts before garbling or funding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub graph_version: u32,
    pub program: [u8; 32],
    pub instances: u16,
    pub kept: u16,
}

#[derive(Clone, Debug)]
pub struct ChainTerms {
    pub checkpoint: Checkpoint,
    pub funding_depth: u32,
    /// The work `W` the statement wants on top of the exit's last event,
    /// in blocks at the checkpoint's difficulty.
    pub work_depth: u32,
    /// The statement's header budget from the checkpoint.
    pub max_headers: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_transaction_bytes: u32,
    pub max_publication_bytes: u32,
    pub max_authorizations: u32,
    pub max_source_inputs: u32,
}

/// Fee terms: the quote's rate ceiling and the opening's fee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fees {
    pub max_rate_sat_per_kvb: u64,
    pub opening_fee_quote: u64,
}

/// A Base's version, and with it the rules its statement judges by. A
/// contract keeps its version's rules for its life: v3's for those opened
/// under it, v4's for every new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// The first rules: a claim's chain evidence is judged only up to its
    /// publication, and her request's keys are not bound.
    V3,
    /// Her request's one-time keys are bound (`holder_wots`, the digest of
    /// her keys on `b` and `E`), a lock's chain evidence is anchored to
    /// its lock checkpoint, and a pending Pay counts only with an
    /// activation.
    V4 { holder_wots: [u8; 32] },
    /// As v4, and the holder funds part of the contract herself: her
    /// `deposit_units` of it, owed to her as notes the Hub issues for her
    /// blinded outputs, whose ordered list `funded` commits to
    /// ([`crate::spend::funded_root`]). She may claim those of them it
    /// never issued her ([`crate::claim::Item::Funded`]).
    V5 {
        holder_wots: [u8; 32],
        funded: [u8; 32],
    },
}

impl Version {
    /// Her request's one-time keys, bound from v4 on.
    pub fn holder_wots(&self) -> Option<[u8; 32]> {
        match *self {
            Self::V3 => None,
            Self::V4 { holder_wots } | Self::V5 { holder_wots, .. } => Some(holder_wots),
        }
    }

    /// The commitment to her funded outputs, of v5.
    pub fn funded(&self) -> Option<[u8; 32]> {
        match *self {
            Self::V5 { funded, .. } => Some(funded),
            _ => None,
        }
    }

    /// Whether v4's rules hold: a lock anchored to its own checkpoint, a
    /// Pay owed only with an activation. Of v4 and v5.
    pub fn anchored(&self) -> bool {
        !matches!(self, Self::V3)
    }
}

#[derive(Clone)]
pub struct Parts {
    pub version: Version,
    pub nonce: [u8; 32],
    /// The holder's key authorizing her claim's publication.
    pub claim_key: [u8; 32],
    pub keyset: Keyset,
    /// Bitcoin outpoint bytes: raw txid followed by little-endian vout.
    pub deposit: [u8; 36],
    pub cap: u32,
    pub parties: [Party; 2],
    pub graph: GraphTerms,
    pub recipe: Recipe,
    pub chain: ChainTerms,
    pub limits: Limits,
    pub fees: Fees,
}

/// An unsigned canonical preimage, NOT an accepted/prepared opening, a trusted
/// checkpoint, input reservation, or permission to release funding signatures.
#[derive(Clone)]
pub struct Base(Parts);

impl std::fmt::Debug for Base {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Base(..)")
    }
}

impl Base {
    /// Generate before key derivation. Validation cannot establish randomness
    /// or freshness of caller-supplied bytes; neither is inferred from absence.
    pub fn fresh_nonce() -> [u8; 32] {
        use rand_core::RngCore;
        let mut nonce = [0; 32];
        rand_core::OsRng.fill_bytes(&mut nonce);
        nonce
    }

    pub fn new(parts: Parts) -> Result<Self, Error> {
        let base = Self(parts);
        base.validate()?;
        Ok(base)
    }

    pub fn parts(&self) -> &Parts {
        &self.0
    }

    pub fn deposit_outpoint(&self) -> OutPoint {
        OutPoint {
            txid: self.0.deposit[..32].try_into().expect("fixed txid"),
            index: u32::from_le_bytes(self.0.deposit[32..].try_into().expect("fixed vout")),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        let p = &self.0;
        for key in [&p.claim_key].into_iter().chain(
            p.parties
                .iter()
                .flat_map(|party| [&party.funding, &party.payment, &party.delayed]),
        ) {
            k256::schnorr::VerifyingKey::from_bytes(key).map_err(|_| Error::InvalidPoint)?;
        }
        let funded_bad = match p.version.funded() {
            Some(root) => {
                root == [0; 32] || p.graph.deposit_units == 0 || p.graph.deposit_units > p.cap
            }
            None => false,
        };
        if p.nonce == [0; 32]
            || p.version.holder_wots() == Some([0; 32])
            || funded_bad
            || p.deposit[..32] == [0; 32]
            || p.parties[0].payment == p.parties[1].payment
            || p.cap == 0
            || p.cap > p.graph.maximum_amount()?
        {
            return Err(bad("native Base identity or capacity"));
        }
        for party in &p.parties {
            if party.payout_script.len() < 2 || party.payout_script.len() > 10_000 {
                return Err(bad("native payout destination length"));
            }
        }
        let g = &p.graph;
        g.graph_value()?;
        let c = &p.chain;
        if g.unit_sats != 1
            || g.dust != 330
            || (1u64 << g.low_bits) < g.dust
            || g.reserve_alice < g.dust
            || g.reserve_hub < g.dust
            || g.deposit_units > g.maximum_amount()?
            || g.to_self_delay == 0
            || g.delta == 0
            || g.delta_prime == 0
            || g.deadline_margin == 0
            || g.publication_window <= 1
            || g.response_window <= g.publication_window + 1
            || u32::from(g.assert_window) <= c.work_depth + 1
            || g.contest_window <= 1
            || g.challenge_window <= 1
            || g.answer_window <= 1
            || g.request_height <= c.checkpoint.height()
            || g.close_height >= 500_000_000
            || g.request_height
                .checked_add(g.dispute_blocks())
                .is_none_or(|end| end > g.close_height)
        {
            return Err(bad("native graph profile terms"));
        }
        if [
            g.commit_fee,
            g.presign_fee,
            p.fees.opening_fee_quote,
            p.fees.max_rate_sat_per_kvb,
        ]
        .into_iter()
        .any(|value| value > MAX_MONEY)
            || p.fees.max_rate_sat_per_kvb == 0
        {
            return Err(bad("native fee quote range"));
        }
        // The statement reads Bitcoin as a header chain from the checkpoint
        // through the work on the Hub's response to the latest request. A
        // budget that ends earlier would leave a late recovery unprovable.
        if c.max_headers == 0
            || u64::from(c.checkpoint.height()) + u64::from(c.max_headers)
                < u64::from(g.request_height)
                    + u64::from(g.response_window)
                    + u64::from(c.work_depth)
            || [c.funding_depth, c.work_depth]
                .into_iter()
                .any(|depth| depth == 0 || depth > c.max_headers)
        {
            return Err(bad("native chain confirmation profile"));
        }
        let limits = &p.limits;
        if !(60..=4_000_000).contains(&limits.max_transaction_bytes)
            || limits.max_publication_bytes == 0
            || limits.max_publication_bytes > limits.max_transaction_bytes
            || limits.max_authorizations < 32
            || limits.max_source_inputs == 0
        {
            return Err(bad("native resource limits"));
        }
        let r = &p.recipe;
        if r.graph_version != GRAPH_VERSION
            || r.program == [0; 32]
            || r.kept == 0
            || r.kept >= r.instances
        {
            return Err(bad("native static recipe identity"));
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let p = &self.0;
        let mut bytes = match p.version {
            Version::V3 => BASE_TAG_V3,
            Version::V4 { .. } => BASE_TAG_V4,
            Version::V5 { .. } => BASE_TAG_V5,
        }
        .to_vec();
        bytes.extend(p.nonce);
        bytes.extend(p.claim_key);
        put_blob(&mut bytes, &p.keyset.to_bytes());
        bytes.extend(p.deposit);
        bytes.extend(p.cap.to_be_bytes());
        for party in &p.parties {
            bytes.extend(party.funding);
            bytes.extend(party.payment);
            bytes.extend(party.delayed);
            put_blob(&mut bytes, &party.payout_script);
        }
        let g = &p.graph;
        bytes.extend(g.id.to_be_bytes());
        bytes.extend(g.unit_sats.to_be_bytes());
        bytes.extend([g.bits, g.low_bits]);
        bytes.extend(g.deposit_units.to_be_bytes());
        bytes.extend(g.close_height.to_be_bytes());
        for value in [g.reserve_alice, g.reserve_hub] {
            bytes.extend(value.to_be_bytes());
        }
        bytes.extend(g.to_self_delay.to_be_bytes());
        for value in [g.commit_fee, g.presign_fee, g.dust] {
            bytes.extend(value.to_be_bytes());
        }
        bytes.extend(g.delta.to_be_bytes());
        bytes.extend(g.delta_prime.to_be_bytes());
        bytes.extend(g.deadline_margin.to_be_bytes());
        bytes.extend(g.request_height.to_be_bytes());
        for window in [
            g.response_window,
            g.publication_window,
            g.contest_window,
            g.assert_window,
            g.challenge_window,
            g.answer_window,
        ] {
            bytes.extend(window.to_be_bytes());
        }
        let r = &p.recipe;
        bytes.extend(r.graph_version.to_be_bytes());
        bytes.extend(r.program);
        bytes.extend(r.instances.to_be_bytes());
        bytes.extend(r.kept.to_be_bytes());
        let c = &p.chain;
        put_blob(&mut bytes, &c.checkpoint.to_bytes());
        for value in [
            c.funding_depth,
            c.work_depth,
            c.max_headers,
            p.limits.max_transaction_bytes,
            p.limits.max_publication_bytes,
            p.limits.max_authorizations,
            p.limits.max_source_inputs,
        ] {
            bytes.extend(value.to_be_bytes());
        }
        for value in [p.fees.max_rate_sat_per_kvb, p.fees.opening_fee_quote] {
            bytes.extend(value.to_be_bytes());
        }
        if let Some(holder_wots) = p.version.holder_wots() {
            bytes.extend(holder_wots);
        }
        if let Some(funded) = p.version.funded() {
            bytes.extend(funded);
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BASE_BYTES {
            return Err(bad("native Base byte limit"));
        }
        let mut reader = Reader(bytes);
        let tag = match reader.take(BASE_TAG_V3.len())? {
            tag if tag == BASE_TAG_V3 => 3,
            tag if tag == BASE_TAG_V4 => 4,
            tag if tag == BASE_TAG_V5 => 5,
            _ => return Err(bad("native Base tag")),
        };
        let nonce = reader.array()?;
        let claim_key = reader.array()?;
        let keyset = Keyset::from_bytes(reader.blob(4_096)?)?;
        let deposit = reader.array()?;
        let cap = reader.u32()?;
        let mut parties = Vec::with_capacity(2);
        for _ in 0..2 {
            parties.push(Party {
                funding: reader.array()?,
                payment: reader.array()?,
                delayed: reader.array()?,
                payout_script: reader.blob(10_000)?.to_vec(),
            });
        }
        let graph = GraphTerms {
            id: reader.u32()?,
            unit_sats: reader.u64()?,
            bits: reader.array::<1>()?[0],
            low_bits: reader.array::<1>()?[0],
            deposit_units: reader.u32()?,
            close_height: reader.u32()?,
            reserve_alice: reader.u64()?,
            reserve_hub: reader.u64()?,
            to_self_delay: reader.u16()?,
            commit_fee: reader.u64()?,
            presign_fee: reader.u64()?,
            dust: reader.u64()?,
            delta: reader.u16()?,
            delta_prime: reader.u16()?,
            deadline_margin: reader.u32()?,
            request_height: reader.u32()?,
            response_window: reader.u16()?,
            publication_window: reader.u16()?,
            contest_window: reader.u16()?,
            assert_window: reader.u16()?,
            challenge_window: reader.u16()?,
            answer_window: reader.u16()?,
        };
        let recipe = Recipe {
            graph_version: reader.u32()?,
            program: reader.array()?,
            instances: reader.u16()?,
            kept: reader.u16()?,
        };
        let chain = ChainTerms {
            checkpoint: Checkpoint::from_bytes(reader.blob(256)?)?,
            funding_depth: reader.u32()?,
            work_depth: reader.u32()?,
            max_headers: reader.u32()?,
        };
        let limits = Limits {
            max_transaction_bytes: reader.u32()?,
            max_publication_bytes: reader.u32()?,
            max_authorizations: reader.u32()?,
            max_source_inputs: reader.u32()?,
        };
        let fees = Fees {
            max_rate_sat_per_kvb: reader.u64()?,
            opening_fee_quote: reader.u64()?,
        };
        let version = match tag {
            3 => Version::V3,
            4 => Version::V4 {
                holder_wots: reader.array()?,
            },
            _ => Version::V5 {
                holder_wots: reader.array()?,
                funded: reader.array()?,
            },
        };
        if !reader.0.is_empty() {
            return Err(bad("trailing native Base bytes"));
        }
        Self::new(Parts {
            version,
            nonce,
            claim_key,
            keyset,
            deposit,
            cap,
            parties: parties.try_into().expect("two fixed parties"),
            graph,
            recipe,
            chain,
            limits,
            fees,
        })
    }

    pub fn digest(&self) -> [u8; 32] {
        let domain = match self.0.version {
            Version::V3 => "licash/base/v3",
            Version::V4 { .. } => "licash/base/v4",
            Version::V5 { .. } => "licash/base/v5",
        };
        crypto::tagged_hash(domain, &[&self.to_bytes()])
    }

    pub fn version(&self) -> Version {
        self.0.version
    }

    /// What each party checks of agreed terms before it garbles or funds:
    /// the statement's program is the one it trusts (a program that
    /// accepts anything would let the Hub void any claim), and F funded
    /// at `tip_height` still lets her request in time.
    pub fn check(&self, trusted: &[u8; 32], tip_height: u32) -> Result<(), Error> {
        if &self.0.recipe.program != trusted {
            return Err(bad("opening names a program this party does not trust"));
        }
        if !self.fundable_at(tip_height) {
            return Err(bad(
                "opening cannot complete a recovery before its deadline",
            ));
        }
        Ok(())
    }

    /// Whether F funded at `tip_height` still lets the holder request by
    /// the latest height for it: F buried, then the request's delay, with
    /// the deadline margin to spare.
    pub fn fundable_at(&self, tip_height: u32) -> bool {
        let g = &self.0.graph;
        [
            self.0.chain.funding_depth,
            u32::from(g.to_self_delay),
            g.deadline_margin,
        ]
        .into_iter()
        .try_fold(tip_height, |sum, step| sum.checked_add(step))
        .is_some_and(|height| height <= g.request_height)
    }
}

fn put_blob(bytes: &mut Vec<u8>, blob: &[u8]) {
    bytes.extend((blob.len() as u32).to_be_bytes());
    bytes.extend(blob);
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        if count > self.0.len() {
            return Err(bad("truncated native Base"));
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("fixed Base field"))
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn blob(&mut self, limit: usize) -> Result<&'a [u8], Error> {
        let count = self.u32()? as usize;
        if count > limit {
            return Err(bad("native Base component limit"));
        }
        self.take(count)
    }
}
