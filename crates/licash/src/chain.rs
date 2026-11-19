//! Checkpoint-bound static Bitcoin headers and detached inclusion proofs.
//!
//! Raw-byte interfaces intentionally hide the Bitcoin backend. Static
//! checks establish no best-chain membership, current clock or full block/UTXO
//! validity. A checkpoint tuple is trusted only through an authenticated opening.

use bitcoin::{
    block::Header,
    blockdata::constants::genesis_block,
    consensus::{deserialize, serialize},
    hashes::Hash,
    pow::{CompactTarget, Target},
    Block, BlockHash, Network,
};
use sha2::{Digest, Sha256};

use crate::{
    crypto::tagged_hash,
    spend::{ChainProfile, TimedTerms},
    Error,
};

pub type HeaderBytes = [u8; 80];
const CHECKPOINT_TAG: &[u8] = b"LICASH-CP\x02";
const ACTIVATION_MAGIC: &[u8] = b"LICASH-ACT\x01";

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

pub fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}

/// The network whose genesis block and retarget rules the profile shares.
fn network(profile: ChainProfile) -> Network {
    match profile {
        ChainProfile::Core30Mainnet => Network::Bitcoin,
        ChainProfile::Core30DefaultRegtest => Network::Regtest,
        ChainProfile::Core30Signet | ChainProfile::Mutinynet => Network::Signet,
    }
}

fn pow_limit(profile: ChainProfile) -> Target {
    let mut bytes = [0xff; 32];
    match profile {
        ChainProfile::Core30Mainnet => bytes[..4].fill(0),
        ChainProfile::Core30DefaultRegtest => bytes[0] = 0x7f,
        ChainProfile::Core30Signet | ChainProfile::Mutinynet => {
            bytes = [0; 32];
            bytes[2..5].copy_from_slice(&[0x03, 0x77, 0xae]);
        }
    }
    Target::from_be_bytes(bytes)
}

/// Block 1 of the default signet and of Mutinynet, which share its genesis.
const SIGNET_FIRST: &str = "00000086d6b2636cb2a392d45edc4ec544a10024d30141c9adf4bfd9de533b53";
const MUTINYNET_FIRST: &str = "000002855893a0a9b24eaffc5efc770558a326fee4fc10c9da22fc19cd2954f9";

impl ChainProfile {
    /// The profile of the chain whose genesis block hashes to `genesis` and
    /// block 1 to `first`, hashes in their internal byte order. The signets
    /// share a genesis block, so they need block 1 to tell apart.
    pub fn identify(genesis: &[u8; 32], first: Option<&[u8; 32]>) -> Option<Self> {
        let hash = |hex: &str| {
            let hash: bitcoin::BlockHash = hex.parse().expect("a block hash");
            hash.to_byte_array()
        };
        let found = Self::ALL
            .into_iter()
            .filter(|p| genesis_block(network(*p)).block_hash().to_byte_array() == *genesis);
        let mut found = found.filter(|p| match p {
            Self::Core30Signet => first == Some(&hash(SIGNET_FIRST)),
            Self::Mutinynet => first == Some(&hash(MUTINYNET_FIRST)),
            _ => true,
        });
        found.next()
    }
}

fn version_floor(profile: ChainProfile, height: u32) -> i32 {
    let [bip34, bip66, bip65] = match profile {
        ChainProfile::Core30Mainnet => [227_931, 363_725, 388_381],
        ChainProfile::Core30DefaultRegtest
        | ChainProfile::Core30Signet
        | ChainProfile::Mutinynet => [1, 1, 1],
    };
    if height >= bip65 {
        4
    } else if height >= bip66 {
        3
    } else if height >= bip34 {
        2
    } else {
        i32::MIN
    }
}

fn strict_target(profile: ChainProfile, bits: CompactTarget) -> Result<Target, Error> {
    let bits_raw = bits.to_consensus();
    let size = bits_raw >> 24;
    let mut word = bits_raw & 0x007f_ffff;
    if size <= 3 {
        word >>= 8 * (3 - size);
    }
    if word == 0
        || bits_raw & 0x0080_0000 != 0
        || size > 34
        || (word > 0xff && size > 33)
        || (word > 0xffff && size > 32)
    {
        return Err(bad("invalid compact target"));
    }
    let target = Target::from_compact(bits);
    if target == Target::ZERO || target > pow_limit(profile) {
        return Err(bad("target exceeds profile"));
    }
    Ok(target)
}

fn expected_bits(
    profile: ChainProfile,
    previous_height: u32,
    previous: &Header,
    period_start: Option<u32>,
) -> Result<CompactTarget, Error> {
    let height = previous_height
        .checked_add(1)
        .ok_or_else(|| bad("header height overflow"))?;
    let Some(interval) = profile.retarget_interval() else {
        return Ok(CompactTarget::from_consensus(0x207f_ffff));
    };
    if !height.is_multiple_of(interval) {
        return Ok(previous.bits);
    }
    let first = period_start.ok_or_else(|| bad("missing retarget context"))?;
    // Every retargeting profile aims for two weeks a period.
    let elapsed = (i64::from(previous.time) - i64::from(first)).clamp(302_400, 4_838_400) as u64;
    Ok(CompactTarget::from_next_work_required(
        previous.bits,
        elapsed,
        network(profile),
    ))
}

fn header(bytes: &HeaderBytes) -> Result<Header, Error> {
    deserialize(bytes).map_err(|_| bad("header encoding"))
}

/// Authenticated anchor context, including timestamp/retarget history. Local
/// shape checks do not authenticate that history; both parties must do so.
#[derive(Clone)]
pub struct Checkpoint {
    profile: ChainProfile,
    height: u32,
    header: HeaderBytes,
    /// `header` decoded and hashed once, for the headers that follow it.
    decoded: Header,
    hash: [u8; 32],
    times: Vec<u32>,
    period_start: Option<u32>,
}

impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Checkpoint(..)")
    }
}

impl Checkpoint {
    pub fn new(
        profile: ChainProfile,
        height: u32,
        bytes: HeaderBytes,
        times: Vec<u32>,
        period_start: Option<u32>,
    ) -> Result<Self, Error> {
        let h = header(&bytes)?;
        if times.len() != (u64::from(height) + 1).min(11) as usize
            || times.last() != Some(&h.time)
            || h.version.to_consensus() < version_floor(profile, height)
        {
            return Err(bad("checkpoint context"));
        }
        h.validate_pow(strict_target(profile, h.bits)?)
            .map_err(|_| bad("checkpoint proof of work"))?;
        if height == 0 && h != genesis_block(network(profile)).header {
            return Err(bad("wrong checkpoint genesis"));
        }
        match profile.retarget_interval() {
            Some(interval) => {
                if period_start.is_none()
                    || (height.is_multiple_of(interval) && period_start != Some(h.time))
                {
                    return Err(bad("checkpoint retarget context"));
                }
            }
            None => {
                if period_start.is_some() || h.bits.to_consensus() != 0x207f_ffff {
                    return Err(bad("checkpoint regtest context"));
                }
            }
        }
        Ok(Self {
            profile,
            height,
            header: bytes,
            decoded: h,
            hash: sha256d(&bytes),
            times,
            period_start,
        })
    }

    pub fn genesis(profile: ChainProfile) -> Result<Self, Error> {
        let h = genesis_block(network(profile)).header;
        Self::new(
            profile,
            0,
            serialize(&h).try_into().expect("80-byte header"),
            vec![h.time],
            profile.retarget_interval().map(|_| h.time),
        )
    }
    pub fn profile(&self) -> ChainProfile {
        self.profile
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn header(&self) -> &HeaderBytes {
        &self.header
    }

    pub fn commitment(&self) -> [u8; 32] {
        tagged_hash("licash/header-checkpoint/v2", &[&self.context_bytes()])
    }

    fn context_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![self.profile.tag()];
        bytes.extend(self.height.to_be_bytes());
        bytes.extend(self.header);
        bytes.extend((self.times.len() as u32).to_be_bytes());
        for time in &self.times {
            bytes.extend(time.to_be_bytes());
        }
        match self.period_start {
            Some(time) => {
                bytes.push(1);
                bytes.extend(time.to_be_bytes());
            }
            None => bytes.push(0),
        }
        bytes
    }

    /// Complete canonical checkpoint preimage for Base. The codec tag is not
    /// part of the checkpoint commitment bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = CHECKPOINT_TAG.to_vec();
        bytes.extend(self.context_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let at = CHECKPOINT_TAG.len();
        // Profile, height, header, count, at least one time and presence flag.
        if !bytes.starts_with(CHECKPOINT_TAG)
            || bytes.len() < at + 1 + 4 + 80 + 4 + 4 + 1
            || bytes.len() > at + 1 + 4 + 80 + 4 + 11 * 4 + 1 + 4
        {
            return Err(bad("checkpoint framing"));
        }
        let profile =
            ChainProfile::from_tag(bytes[at]).ok_or_else(|| bad("unknown checkpoint profile"))?;
        let height = u32::from_be_bytes(bytes[at + 1..at + 5].try_into().expect("fixed height"));
        let header = bytes[at + 5..at + 85].try_into().expect("fixed header");
        let count =
            u32::from_be_bytes(bytes[at + 85..at + 89].try_into().expect("fixed count")) as usize;
        if !(1..=11).contains(&count) {
            return Err(bad("checkpoint timestamp count"));
        }
        let end = at + 89 + count * 4;
        let presence = bytes
            .get(end)
            .copied()
            .ok_or_else(|| bad("truncated checkpoint times"))?;
        let period_start = match presence {
            0 if bytes.len() == end + 1 => None,
            1 if bytes.len() == end + 5 => Some(u32::from_be_bytes(
                bytes[end + 1..end + 5]
                    .try_into()
                    .expect("fixed retarget time"),
            )),
            _ => return Err(bad("checkpoint retarget framing")),
        };
        let times = bytes[at + 89..end]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|time| u32::from_be_bytes(*time))
            .collect();
        Self::new(profile, height, header, times, period_start)
    }

    fn push(&mut self, bytes: HeaderBytes) -> Result<(), Error> {
        let previous = self.decoded;
        let h = header(&bytes)?;
        let hash = sha256d(&bytes);
        let height = self
            .height
            .checked_add(1)
            .ok_or_else(|| bad("header height overflow"))?;
        if h.prev_blockhash.to_byte_array() != self.hash
            || h.bits != expected_bits(self.profile, self.height, &previous, self.period_start)?
        {
            return Err(bad("header ancestry or difficulty"));
        }
        // `Header::validate_pow` of the strict target, on the hash made
        // once: Bitcoin's own SHA-256 costs the statement's prover several
        // times what the accelerated one does.
        if !strict_target(self.profile, h.bits)?.is_met_by(BlockHash::from_byte_array(hash)) {
            return Err(bad("header proof of work"));
        }
        let mut sorted = self.times.clone();
        sorted.sort_unstable();
        if h.time <= sorted[sorted.len() / 2]
            || h.version.to_consensus() < version_floor(self.profile, height)
        {
            return Err(bad("header MTP or version"));
        }
        if let Some(interval) = self.profile.retarget_interval() {
            if height.is_multiple_of(interval) {
                self.period_start = Some(h.time);
            }
        }
        self.times.push(h.time);
        if self.times.len() > 11 {
            self.times.remove(0);
        }
        self.header = bytes;
        self.decoded = h;
        self.hash = hash;
        self.height = height;
        Ok(())
    }
}

/// One immutable checked ancestry, not an active-chain or current-time oracle.
#[derive(Clone)]
pub struct Chain {
    checkpoint: Checkpoint,
    headers: Vec<HeaderBytes>,
}

impl Chain {
    pub fn verify(
        checkpoint: Checkpoint,
        expected_checkpoint: &[u8; 32],
        headers: Vec<HeaderBytes>,
        max_headers: usize,
    ) -> Result<Self, Error> {
        if &checkpoint.commitment() != expected_checkpoint {
            return Err(bad("checkpoint not bound to opening"));
        }
        if headers.len() > max_headers {
            return Err(bad("header budget"));
        }
        let mut state = checkpoint.clone();
        for bytes in &headers {
            state.push(*bytes)?;
        }
        Ok(Self {
            checkpoint,
            headers,
        })
    }
    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }
    pub fn headers(&self) -> &[HeaderBytes] {
        &self.headers
    }
    pub fn tip_height(&self) -> u32 {
        self.checkpoint.height
            + u32::try_from(self.headers.len()).expect("validated header heights")
    }
    pub fn header(&self, height: u32) -> Option<&HeaderBytes> {
        if height == self.checkpoint.height {
            return Some(&self.checkpoint.header);
        }
        let offset = height.checked_sub(self.checkpoint.height)?.checked_sub(1)?;
        self.headers.get(usize::try_from(offset).ok()?)
    }
    pub fn block_hash(&self, height: u32) -> Option<[u8; 32]> {
        self.header(height).map(|h| sha256d(h))
    }
    pub fn confirmations(&self, height: u32) -> Option<u64> {
        self.header(height)?;
        Some(u64::from(self.tip_height()) - u64::from(height) + 1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutPoint {
    pub txid: [u8; 32],
    pub index: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub previous: OutPoint,
    pub sequence: u32,
    pub script_sig: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub value: u64,
    pub script: Vec<u8>,
}

/// Canonical non-witness body view. Parsing alone authenticates no inclusion.
#[derive(Clone)]
pub struct Transaction {
    bytes: Vec<u8>,
    version: i32,
    lock_time: u32,
    inputs: Vec<Input>,
    outputs: Vec<Output>,
    coinbase: bool,
}

impl Transaction {
    pub fn parse(bytes: &[u8], max_bytes: usize) -> Result<Self, Error> {
        if bytes.len() > max_bytes {
            return Err(bad("transaction byte budget"));
        }
        let tx: bitcoin::Transaction =
            deserialize(bytes).map_err(|_| bad("transaction encoding"))?;
        if stripped(&tx) != bytes {
            return Err(bad("transaction must be canonical non-witness bytes"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            version: tx.version.0,
            lock_time: tx.lock_time.to_consensus_u32(),
            inputs: tx
                .input
                .iter()
                .map(|input| Input {
                    previous: OutPoint {
                        txid: input.previous_output.txid.to_byte_array(),
                        index: input.previous_output.vout,
                    },
                    sequence: input.sequence.to_consensus_u32(),
                    script_sig: input.script_sig.as_bytes().to_vec(),
                })
                .collect(),
            outputs: tx
                .output
                .iter()
                .map(|output| Output {
                    value: output.value.to_sat(),
                    script: output.script_pubkey.as_bytes().to_vec(),
                })
                .collect(),
            coinbase: tx.is_coinbase(),
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn txid(&self) -> [u8; 32] {
        sha256d(&self.bytes)
    }
    pub fn version(&self) -> i32 {
        self.version
    }
    pub fn lock_time(&self) -> u32 {
        self.lock_time
    }
    pub fn inputs(&self) -> &[Input] {
        &self.inputs
    }
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }
    pub fn is_coinbase(&self) -> bool {
        self.coinbase
    }
}

/// Only an inclusion verifier can construct this type. Non-witness inclusion
/// never supplies unauthenticated witness data to a request verifier.
#[derive(Clone)]
pub struct IncludedTransaction {
    transaction: Transaction,
    height: u32,
    block_hash: [u8; 32],
    witnesses: Option<Vec<Vec<Vec<u8>>>>,
}

impl IncludedTransaction {
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn block_hash(&self) -> &[u8; 32] {
        &self.block_hash
    }
    pub fn input_witness(&self, index: usize) -> Result<&[Vec<u8>], Error> {
        self.witnesses
            .as_ref()
            .and_then(|w| w.get(index))
            .map(Vec::as_slice)
            .ok_or_else(|| bad("no authenticated input witness"))
    }
}

fn stripped(tx: &bitcoin::Transaction) -> Vec<u8> {
    let mut tx = tx.clone();
    for input in &mut tx.input {
        input.witness.clear();
    }
    serialize(&tx)
}
fn parent(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(left);
    bytes[32..].copy_from_slice(right);
    sha256d(&bytes)
}
fn root(mut hash: [u8; 32], mut index: u32, siblings: &[[u8; 32]]) -> [u8; 32] {
    for sibling in siblings {
        hash = if index & 1 == 0 {
            parent(&hash, sibling)
        } else {
            parent(sibling, &hash)
        };
        index >>= 1;
    }
    hash
}
fn path(mut level: Vec<[u8; 32]>, mut index: usize) -> Vec<[u8; 32]> {
    let mut siblings = Vec::new();
    while level.len() > 1 {
        siblings.push(*level.get(index ^ 1).unwrap_or(&level[index]));
        if !level.len().is_multiple_of(2) {
            level.push(*level.last().expect("nonempty merkle level"));
        }
        level = level
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| parent(&p[0], &p[1]))
            .collect();
        index /= 2;
    }
    siblings
}

/// Detached positive non-witness inclusion. Complete block data is not needed
/// for verification; both paths bind the same height/root and tree depth.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Inclusion {
    pub transaction: Vec<u8>,
    pub index: u32,
    pub siblings: Vec<[u8; 32]>,
    pub coinbase: Vec<u8>,
    pub coinbase_siblings: Vec<[u8; 32]>,
}

impl Inclusion {
    pub fn capture(
        block_bytes: &[u8],
        index: usize,
        max_block_bytes: usize,
    ) -> Result<Self, Error> {
        let block = block(block_bytes, max_block_bytes)?;
        if index == 0 || index >= block.txdata.len() {
            return Err(bad("inclusion target index"));
        }
        let txids: Vec<_> = block
            .txdata
            .iter()
            .map(|tx| sha256d(&stripped(tx)))
            .collect();
        Ok(Self {
            transaction: stripped(&block.txdata[index]),
            index: u32::try_from(index).map_err(|_| bad("inclusion index overflow"))?,
            siblings: path(txids.clone(), index),
            coinbase: stripped(&block.txdata[0]),
            coinbase_siblings: path(txids, 0),
        })
    }

    pub fn verify(
        &self,
        chain: &Chain,
        height: u32,
        max_transaction_bytes: usize,
    ) -> Result<IncludedTransaction, Error> {
        let raw_header = chain
            .header(height)
            .ok_or_else(|| bad("inclusion header missing"))?;
        if self.transaction.len() == 64 || self.coinbase.len() == 64 {
            return Err(bad("ambiguous 64-byte merkle leaf"));
        }
        let tx = Transaction::parse(&self.transaction, max_transaction_bytes)?;
        let coinbase = Transaction::parse(&self.coinbase, max_transaction_bytes)?;
        let depth = self.siblings.len();
        if depth >= 32 || self.coinbase_siblings.len() != depth {
            return Err(bad("inclusion tree depth"));
        }
        if self.index == 0
            || self.index >> depth != 0
            || tx.is_coinbase()
            || !coinbase.is_coinbase()
        {
            return Err(bad("inclusion transaction index or coinbase"));
        }
        let expected = header(raw_header)?.merkle_root.to_byte_array();
        if root(tx.txid(), self.index, &self.siblings) != expected
            || root(coinbase.txid(), 0, &self.coinbase_siblings) != expected
        {
            return Err(bad("transaction merkle inclusion"));
        }
        Ok(IncludedTransaction {
            transaction: tx,
            height,
            block_hash: sha256d(raw_header),
            witnesses: None,
        })
    }
}

fn block(bytes: &[u8], max_bytes: usize) -> Result<Block, Error> {
    if bytes.len() > max_bytes {
        return Err(bad("block capture budget"));
    }
    let block: Block = deserialize(bytes).map_err(|_| bad("block encoding"))?;
    if serialize(&block) != bytes
        || block.txdata.is_empty()
        || !block.txdata[0].is_coinbase()
        || block
            .txdata
            .iter()
            .skip(1)
            .any(bitcoin::Transaction::is_coinbase)
        || !block.check_merkle_root()
    {
        return Err(bad("block capture shape"));
    }
    Ok(block)
}

/// BIP141 witness authentication additionally opens the coinbase's witness
/// commitment. A txid path alone can never authenticate request witness values.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct WitnessInclusion {
    pub non_witness: Inclusion,
    pub full_transaction: Vec<u8>,
    pub witness_siblings: Vec<[u8; 32]>,
    pub coinbase_witness_siblings: Vec<[u8; 32]>,
    pub reserved_value: [u8; 32],
}

impl WitnessInclusion {
    pub fn capture(
        block_bytes: &[u8],
        index: usize,
        max_block_bytes: usize,
    ) -> Result<Self, Error> {
        let block = block(block_bytes, max_block_bytes)?;
        let non_witness = Inclusion::capture(block_bytes, index, max_block_bytes)?;
        let reserved: Vec<_> = block.txdata[0].input[0].witness.iter().collect();
        if reserved.len() != 1 || reserved[0].len() != 32 {
            return Err(bad("coinbase witness reserved value"));
        }
        let mut wtxids: Vec<_> = block
            .txdata
            .iter()
            .map(|tx| sha256d(&serialize(tx)))
            .collect();
        wtxids[0] = [0; 32];
        Ok(Self {
            non_witness,
            full_transaction: serialize(&block.txdata[index]),
            witness_siblings: path(wtxids.clone(), index),
            coinbase_witness_siblings: path(wtxids, 0),
            reserved_value: reserved[0].try_into().expect("checked reserved value"),
        })
    }

    pub fn verify(
        &self,
        chain: &Chain,
        height: u32,
        max_transaction_bytes: usize,
    ) -> Result<IncludedTransaction, Error> {
        if chain.checkpoint().profile() == ChainProfile::Core30Mainnet && height < 481_824 {
            return Err(bad("witness commitment predates segwit"));
        }
        let mut included = self
            .non_witness
            .verify(chain, height, max_transaction_bytes)?;
        if self.full_transaction.len() > max_transaction_bytes {
            return Err(bad("witness transaction byte budget"));
        }
        let tx: bitcoin::Transaction =
            deserialize(&self.full_transaction).map_err(|_| bad("witness transaction encoding"))?;
        if serialize(&tx) != self.full_transaction || stripped(&tx) != self.non_witness.transaction
        {
            return Err(bad("witness transaction body mismatch"));
        }
        let depth = self.non_witness.siblings.len();
        if self.witness_siblings.len() != depth || self.coinbase_witness_siblings.len() != depth {
            return Err(bad("witness tree depth"));
        }
        let witness_root = root(
            sha256d(&self.full_transaction),
            self.non_witness.index,
            &self.witness_siblings,
        );
        if root([0; 32], 0, &self.coinbase_witness_siblings) != witness_root {
            return Err(bad("coinbase witness inclusion"));
        }
        let coinbase = Transaction::parse(&self.non_witness.coinbase, max_transaction_bytes)?;
        let commitment = coinbase
            .outputs()
            .iter()
            .rev()
            .find_map(|output| {
                let script = &output.script;
                (script.len() >= 38 && script[..6] == [0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed]).then(
                    || <[u8; 32]>::try_from(&script[6..38]).expect("checked commitment width"),
                )
            })
            .ok_or_else(|| bad("coinbase witness commitment missing"))?;
        if parent(&witness_root, &self.reserved_value) != commitment {
            return Err(bad("witness commitment mismatch"));
        }
        included.witnesses = Some(
            tx.input
                .iter()
                .map(|input| input.witness.to_vec())
                .collect(),
        );
        Ok(included)
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ActivationRef {
    pub height: u32,
    pub block_hash: [u8; 32],
    pub txid: [u8; 32],
    pub output: u32,
}

impl ActivationRef {
    pub fn to_bytes(&self) -> [u8; 72] {
        let mut bytes = [0; 72];
        bytes[..4].copy_from_slice(&self.height.to_be_bytes());
        bytes[4..36].copy_from_slice(&self.block_hash);
        bytes[36..68].copy_from_slice(&self.txid);
        bytes[68..].copy_from_slice(&self.output.to_be_bytes());
        bytes
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Activation {
    pub reference: ActivationRef,
    pub inclusion: Inclusion,
}

impl Activation {
    /// The lock's secret, if this activation of it is on `chain`: in a
    /// block after `anchor`, the checkpoint the lock's terms name, and
    /// before the lock's expiry, buried at the terms' depth. `anchor` must
    /// be a block of `chain`: its own checkpoint, or one of its headers.
    ///
    /// A missing, shallow, late or context-mismatched positive witness is an
    /// error, not evidence of nonactivation. No absence proof is claimed here.
    pub fn verify(
        &self,
        chain: &Chain,
        anchor: &Checkpoint,
        terms: &TimedTerms,
        max_transaction_bytes: usize,
    ) -> Result<[u8; 32], Error> {
        let reference = &self.reference;
        if anchor.profile() != chain.checkpoint().profile()
            || chain.block_hash(anchor.height()) != Some(sha256d(anchor.header()))
        {
            return Err(bad("activation anchor is not on the chain"));
        }
        if terms.profile() != anchor.profile()
            || terms.checkpoint() != &anchor.commitment()
            || terms.expiry() <= anchor.height()
            || reference.height <= anchor.height()
            || reference.height >= terms.expiry()
            || chain.block_hash(reference.height) != Some(reference.block_hash)
            || chain
                .confirmations(reference.height)
                .is_none_or(|k| k < u64::from(terms.depth()))
        {
            return Err(bad("activation context, height or depth"));
        }
        let included = self
            .inclusion
            .verify(chain, reference.height, max_transaction_bytes)?;
        if included.transaction().txid() != reference.txid {
            return Err(bad("activation transaction reference"));
        }
        let index =
            usize::try_from(reference.output).map_err(|_| bad("activation output index"))?;
        let output = included
            .transaction()
            .outputs()
            .get(index)
            .ok_or_else(|| bad("activation output missing"))?;
        let secret =
            activation_secret(&output.script).ok_or_else(|| bad("activation record encoding"))?;
        if <[u8; 32]>::from(Sha256::digest(secret)) != *terms.hash() {
            return Err(bad("activation preimage hash"));
        }
        Ok(secret)
    }
}

/// The output script that publishes `secret`, activating every lock under
/// its hash that has not expired.
pub fn activation_script(secret: &[u8; 32]) -> Vec<u8> {
    let mut bytes = vec![0x6a, (ACTIVATION_MAGIC.len() + 32) as u8];
    bytes.extend(ACTIVATION_MAGIC);
    bytes.extend(secret);
    bytes
}

/// The secret an output script publishes, if it is an
/// [`activation_script`]: what a watcher reads from each block.
pub fn activation_secret(script: &[u8]) -> Option<[u8; 32]> {
    let payload = ACTIVATION_MAGIC.len() + 32;
    let rest = script.strip_prefix(&[0x6a, payload as u8])?;
    rest.strip_prefix(ACTIVATION_MAGIC)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witness_commitment_shape_before_segwit_activation_is_not_authentication() {
        let cp = Checkpoint::genesis(ChainProfile::Core30Mainnet).unwrap();
        let chain = Chain::verify(cp.clone(), &cp.commitment(), vec![], 0).unwrap();
        let proof = WitnessInclusion {
            non_witness: Inclusion {
                transaction: vec![],
                index: 1,
                siblings: vec![],
                coinbase: vec![],
                coinbase_siblings: vec![],
            },
            full_transaction: vec![],
            witness_siblings: vec![],
            coinbase_witness_siblings: vec![],
            reserved_value: [0; 32],
        };
        assert_eq!(
            proof.verify(&chain, 0, 1_000).err(),
            Some(bad("witness commitment predates segwit"))
        );
    }

    #[test]
    fn core30_targets_and_signed_retargets_match_independent_vectors() {
        for bits in [
            0, 0x01003456, 0x1d80ffff, 0x23010000, 0x22000100, 0x21010000, 0x1d010000,
        ] {
            assert!(strict_target(
                ChainProfile::Core30Mainnet,
                CompactTarget::from_consensus(bits)
            )
            .is_err());
        }
        // Bitcoin Core v30.0 src/test/pow_tests.cpp get_next_work cases.
        for (first, last, bits, expected) in [
            (1261130161, 1262152739, 0x1d00ffff, 0x1d00d86a),
            (1231006505, 1233061996, 0x1d00ffff, 0x1d00ffff),
            (1279008237, 1279297671, 0x1c05a3f4, 0x1c0168fd),
            (1263163443, 1269211443, 0x1c387f6f, 0x1d00e1fd),
            (101, 100, 0x1c05a3f4, 0x1c0168fd),
        ] {
            let mut previous = genesis_block(Network::Bitcoin).header;
            previous.time = last;
            previous.bits = CompactTarget::from_consensus(bits);
            assert_eq!(
                expected_bits(ChainProfile::Core30Mainnet, 32_255, &previous, Some(first))
                    .unwrap()
                    .to_consensus(),
                expected
            );
            assert_eq!(
                expected_bits(ChainProfile::Core30Mainnet, 32_254, &previous, None).unwrap(),
                previous.bits
            );
        }
    }

    #[test]
    fn signet_retargets_match_core_at_each_signets_interval() {
        // Bitcoin Core's CalculateNextWorkRequired, signet's limit and two
        // weeks a period; the first is the real signet retarget at 330,624.
        for (first, last, bits, expected) in [
            (1793327751, 1794548120, 0x1d14cd85, 0x1d14fcee),
            (0, 1_000_000, 0x1d14cd85, 0x1d1132b6),
            (0, 10, 0x1d14cd85, 0x1d053361),
            (0, 10, 0x1e0377ae, 0x1e00ddeb),
            (0, 100_000_000, 0x1e0377ae, 0x1e0377ae),
            (100, 2_500_000, 0x1d0fffff, 0x1d211143),
        ] {
            let mut previous = genesis_block(Network::Signet).header;
            previous.time = last;
            previous.bits = CompactTarget::from_consensus(bits);
            for profile in [ChainProfile::Core30Signet, ChainProfile::Mutinynet] {
                let interval = profile.retarget_interval().unwrap();
                let retarget = expected_bits(profile, 7 * interval - 1, &previous, Some(first));
                assert_eq!(retarget.unwrap().to_consensus(), expected);
                let carried = expected_bits(profile, 7 * interval, &previous, None);
                assert_eq!(carried.unwrap(), previous.bits);
            }
            // Mutinynet does not retarget at the default signet's interval.
            let carried = expected_bits(ChainProfile::Mutinynet, 7 * 2016 - 1, &previous, None);
            assert_eq!(carried.unwrap(), previous.bits);
        }
        for bits in [0x1e0377af, 0x1e037800, 0x1f00ffff] {
            let bits = CompactTarget::from_consensus(bits);
            assert!(strict_target(ChainProfile::Core30Signet, bits).is_err());
            assert!(strict_target(ChainProfile::Mutinynet, bits).is_err());
        }
    }

    #[test]
    fn rolling_mtp_and_regtest_boundaries_are_static_not_a_wall_clock() {
        let mut state = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
        let mine = |previous: &Header, time| {
            let mut h = Header {
                version: bitcoin::block::Version::from_consensus(4),
                prev_blockhash: previous.block_hash(),
                merkle_root: previous.merkle_root,
                time,
                bits: CompactTarget::from_consensus(0x207fffff),
                nonce: 0,
            };
            while h.validate_pow(h.target()).is_err() {
                h.nonce += 1;
            }
            serialize(&h).try_into().unwrap()
        };
        for _ in 0..160 {
            let previous = header(&state.header).unwrap();
            state.push(mine(&previous, previous.time + 600)).unwrap();
        }
        assert_eq!(state.times.len(), 11);
        let mut sorted = state.times.clone();
        sorted.sort_unstable();
        let previous = header(&state.header).unwrap();
        let before = state.commitment();
        assert!(state.push(mine(&previous, sorted[5])).is_err());
        assert_eq!(before, state.commitment());
        assert!(sorted[5] + 1 < previous.time);
        state.push(mine(&previous, sorted[5] + 1)).unwrap();
        let mut genesis = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
        let previous = header(&genesis.header).unwrap();
        genesis.push(mine(&previous, previous.time + 7201)).unwrap();
        for (height, version) in [
            (227930, i32::MIN),
            (227931, 2),
            (363724, 2),
            (363725, 3),
            (388380, 3),
            (388381, 4),
        ] {
            assert_eq!(version_floor(ChainProfile::Core30Mainnet, height), version);
        }
    }
}
