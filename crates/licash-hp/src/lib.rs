//! The Hub's statement for a Licash recovery on Lichen's hub-proves exit:
//! "the claim the holder's publication carries is owed at most `b′`".
//!
//! What Bitcoin shows of the exit (her `request` of `b` and `E`, her
//! publication of the claim, the Hub's `respond` of `b′` and its
//! disclosure) is `licash_statement`'s to read ([`licash_statement::read`]).
//! What the claim is owed is Licash's: [`licash::recovery::owed`] of the
//! publication's bytes (a [`licash::recovery::Filing`]) under the request
//! `(b, E)`, against the Hub's evidence, on the Licash chain from Base's
//! checkpoint through the publication's block ([`chain`]). The statement
//! holds if that is at most `b′`, and the response disclosed what the
//! evidence needs (the unlocks and preimages it cites).
//!
//! The statement is fixed when the contract opens ([`contract`]): its
//! journal is `licash_statement`'s, `c ‖ word ‖ H(O_D ‖ checkpoint ‖ W)`,
//! with Licash's word `H("licash/hp/word" ‖ Base digest)`. Base fixes the
//! keyset, the limits, the checkpoint and the work, and its digest is what
//! every claim's context binds, so a proof for one contract is useless for
//! any other.
//!
//! Under a v4 or v5 Base the statement also checks her request's one-time
//! signatures on `b` and `E` against the keys Base binds
//! ([`RequestKeys`]): only she can sign a request, so no proof can be made
//! of an exit she didn't start, on any chain, however long before.
//!
//! The holder garbles for this journal at opening (BABE with the roles
//! swapped); the Hub evaluates [`check`] natively to decide its response
//! (the same chain, the same evidence), and proves it in `r0/hp-guest`
//! only when it contests the claim, for the assertion that follows its
//! response.

use bitcoin::hashes::{hash160, Hash};
use licash::{
    base::Base,
    chain::Chain,
    crypto::tagged_hash,
    disclosure::Disclosures,
    recovery::{self, Evidence, Owed},
    request::Request,
};
use licash_statement::{
    btc::{self, Header, OutPoint},
    checksum_digits,
    codec::{R, W},
    script_digit, Checkpoint, Contract, Exit, Read, Reject, JOURNAL_LEN,
};
use sha2::{Digest, Sha256};

/// Licash names a contract by its Base digest (in the word), not by a
/// number: `c` is always this.
pub const C: u32 = 0;

/// Why the Hub can't prove with an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The input doesn't decode.
    Input,
    /// Bitcoin doesn't show the exit, as `licash_statement` reads it.
    Exit(Reject),
    /// The headers through the publication aren't a Licash chain from
    /// Base's checkpoint.
    Chain(String),
    /// The Hub's evidence doesn't prove what it says (licash's reason).
    Evidence(String),
    /// Her request isn't signed under the one-time keys a v4 Base binds,
    /// or the input's keys aren't those.
    Unsigned,
    /// The response didn't disclose what the evidence needs.
    Undisclosed,
    /// The claim is owed more than `b′`.
    Owed,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Input => f.write_str("the statement's input doesn't decode"),
            Refusal::Exit(r) => write!(f, "the exit doesn't read: {r:?}"),
            Refusal::Chain(e) => write!(f, "the chain doesn't verify: {e}"),
            Refusal::Evidence(e) => write!(f, "the evidence doesn't prove: {e}"),
            Refusal::Unsigned => f.write_str("her request isn't signed under the keys Base binds"),
            Refusal::Undisclosed => f.write_str("the response didn't disclose what is needed"),
            Refusal::Owed => f.write_str("the claim is owed more than the response says"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The contract of `base` as its statement fixes it at opening.
pub fn contract(base: &Base) -> Contract {
    let p = base.parts();
    let cp = &p.chain.checkpoint;
    let header = Header(*cp.header());
    let mut word = Sha256::new();
    word.update(b"licash/hp/word");
    word.update(base.digest());
    let mut txid = [0; 32];
    txid.copy_from_slice(&p.deposit[..32]);
    let vout = u32::from_le_bytes(p.deposit[32..].try_into().expect("36-byte outpoint"));
    Contract {
        c: C,
        d: word.finalize().into(),
        deposit: OutPoint { txid, vout },
        checkpoint: Checkpoint {
            height: cp.height(),
            hash: header.hash(),
        },
        work: u128::from(p.chain.work_depth).saturating_mul(btc::work(header.bits())),
    }
}

/// The Licash chain the statement decides on: Base's checkpoint, then
/// `headers` (the exit's, through the publication's block).
pub fn chain(base: &Base, headers: &[[u8; 80]]) -> Result<Chain, Refusal> {
    let terms = &base.parts().chain;
    Chain::verify(
        terms.checkpoint.clone(),
        &terms.checkpoint.commitment(),
        headers.to_vec(),
        terms.max_headers as usize,
    )
    .map_err(|e| Refusal::Chain(e.to_string()))
}

/// The exit's headers through the block of her publication.
pub fn through_publication<'a>(contract: &Contract, exit: &'a Exit, read: &Read) -> &'a [[u8; 80]] {
    let at = read.heights.publish - contract.checkpoint.height;
    &exit.headers[..at as usize]
}

/// Her request's one-time public keys, each digit's chain end (HASH160
/// chains, as `lichen_wots`'s `WotsPublic`): `b`'s 8 message digits and 2
/// checksum digits, `E`'s 64 and 3. A v4 Base binds their digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestKeys {
    b: Vec<[u8; 20]>,
    e: Vec<[u8; 20]>,
}

/// The digits of a signature on `bytes` bytes: its message's and checksum's.
fn digits(bytes: usize) -> usize {
    2 * bytes + checksum_digits(bytes)
}

impl RequestKeys {
    pub fn new(b: Vec<[u8; 20]>, e: Vec<[u8; 20]>) -> Option<Self> {
        (b.len() == digits(4) && e.len() == digits(32)).then_some(Self { b, e })
    }

    /// What a v4 or v5 Base binds of them (`Version::holder_wots`).
    pub fn digest(&self) -> [u8; 32] {
        tagged_hash(
            "licash/hp/request-keys",
            &[&self.b.concat(), &self.e.concat()],
        )
    }

    fn write(&self, w: &mut W) {
        for key in self.b.iter().chain(&self.e) {
            w.put(key);
        }
    }

    fn read(r: &mut R) -> Option<Self> {
        let mut key = || r.take::<20>().ok();
        let b = (0..digits(4)).map(|_| key()).collect::<Option<_>>()?;
        let e = (0..digits(32)).map(|_| key()).collect::<Option<_>>()?;
        Self::new(b, e)
    }

    /// Whether her request's witness (bottom first: her signature on `E`,
    /// then on `b`, then the 2-of-2's) is signed under these keys, as the
    /// request's script checks it: each digit read as the script reads it
    /// ([`script_digit`]), its item hashed up its chain to the key's end,
    /// and the checksum digits those of the message's.
    pub fn signed(&self, witness: &[&[u8]]) -> bool {
        let e_items = 2 * self.e.len();
        witness.len() >= e_items + 2 * self.b.len()
            && signs(&witness[..e_items], &self.e, 32)
            && signs(&witness[e_items..], &self.b, 4)
    }
}

/// Whether `items` (bottom first: `hash_0, digit_0, hash_1, …`) are a
/// Winternitz signature on `bytes` bytes under the chain ends `keys`.
fn signs(items: &[&[u8]], keys: &[[u8; 20]], bytes: usize) -> bool {
    let mut read = Vec::with_capacity(keys.len());
    for (i, end) in keys.iter().enumerate() {
        let (Some(item), Some(digit)) = (items.get(2 * i), items.get(2 * i + 1)) else {
            return false;
        };
        let Some(digit) = script_digit(digit) else {
            return false;
        };
        let mut h = item.to_vec();
        for _ in digit..15 {
            h = hash160::Hash::hash(&h).to_byte_array().to_vec();
        }
        if h != end {
            return false;
        }
        read.push(u32::from(digit));
    }
    let (message, checksum) = read.split_at(2 * bytes);
    let sum: u32 = message.iter().map(|d| 15 - d).sum();
    checksum.iter().fold(0, |value, d| value * 16 + d) == sum
}

/// The statement's input: Base, the exit as the Hub shows it, its
/// evidence against the claim and, under a v4 Base, her request's keys.
#[derive(Clone)]
pub struct HubInput {
    pub base: Base,
    pub exit: Exit,
    pub evidence: Vec<Evidence>,
    pub keys: Option<RequestKeys>,
}

impl HubInput {
    /// Under v3, the bytes v3's program reads: the keys aren't written.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.bytes(&self.base.to_bytes());
        self.exit.write(&mut w);
        w.bytes(&recovery::evidence_to_bytes(&self.evidence));
        if let (Some(_), Some(keys)) = (self.base.version().holder_wots(), &self.keys) {
            keys.write(&mut w);
        }
        w.0
    }

    /// Strict decoding: Base validated, every note and signed spend in the
    /// evidence verified under its keyset.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Refusal> {
        let mut r = R(bytes);
        let base = Base::from_bytes(&r.bytes().map_err(|_| Refusal::Input)?)
            .map_err(|_| Refusal::Input)?;
        let exit = Exit::read(&mut r).map_err(|_| Refusal::Input)?;
        let p = base.parts();
        let evidence = recovery::evidence_from_bytes(
            &p.keyset,
            p.limits.max_authorizations as usize,
            &r.bytes().map_err(|_| Refusal::Input)?,
        )
        .map_err(|_| Refusal::Input)?;
        let keys = match base.version().holder_wots() {
            None => None,
            Some(_) => Some(RequestKeys::read(&mut r).ok_or(Refusal::Input)?),
        };
        r.end().map_err(|_| Refusal::Input)?;
        Ok(Self {
            base,
            exit,
            evidence,
            keys,
        })
    }
}

/// What the statement decides of `exit` under `base` against `evidence`:
/// what the claim is owed, and what Bitcoin shows of the exit. Under a v4
/// Base, her request must be signed under `keys`, the keys Base binds.
pub fn decide(
    base: &Base,
    keys: Option<&RequestKeys>,
    exit: &Exit,
    evidence: &[Evidence],
) -> Result<(Owed, Read), Refusal> {
    decide_with(base, keys, exit, evidence, &mut |_| {})
}

/// [`decide`], calling `mark` with the name of each step as it ends: the
/// guest's profile build counts the cycles between them.
pub fn decide_with(
    base: &Base,
    keys: Option<&RequestKeys>,
    exit: &Exit,
    evidence: &[Evidence],
    mark: &mut dyn FnMut(&'static str),
) -> Result<(Owed, Read), Refusal> {
    let contract = contract(base);
    let read = licash_statement::read(&contract, exit).map_err(Refusal::Exit)?;
    mark("the exit read: its headers' work, its transactions");
    if let Some(holder_wots) = base.version().holder_wots() {
        let keys = keys
            .filter(|k| k.digest() == holder_wots)
            .ok_or(Refusal::Unsigned)?;
        let witness = exit
            .request
            .spending(&contract.deposit)
            .ok_or(Refusal::Exit(Reject::Posted))?;
        if !keys.signed(&witness) {
            return Err(Refusal::Unsigned);
        }
    }
    mark("her request's one-time signatures");
    let chain = chain(base, through_publication(&contract, exit, &read))?;
    mark("the Licash header chain through her publication");
    let request = Request {
        amount: read.b,
        claim_digest: read.e,
    };
    let owed = recovery::owed(base, &request, &read.claim, evidence, &chain)
        .map_err(|e| Refusal::Evidence(e.to_string()))?;
    mark("her filing decoded and judged against the evidence");
    Ok((owed, read))
}

/// The Hub's statement: it holds if the claim is owed at most the
/// response's `b′`, and the response disclosed what the evidence needs.
/// On success, the journal fixed at opening.
pub fn check(input: &HubInput) -> Result<[u8; JOURNAL_LEN], Refusal> {
    check_with(input, &mut |_| {})
}

/// [`check`], calling `mark` as [`decide_with`] does.
pub fn check_with(
    input: &HubInput,
    mark: &mut dyn FnMut(&'static str),
) -> Result<[u8; JOURNAL_LEN], Refusal> {
    let (owed, read) = decide_with(
        &input.base,
        input.keys.as_ref(),
        &input.exit,
        &input.evidence,
        mark,
    )?;
    verdict(&owed, read.b2, read.disclosed.as_deref())?;
    licash_statement::holds(&contract(&input.base), &read, owed.owed).map_err(|_| Refusal::Owed)
}

/// Whether a response of `b2` that disclosed `disclosed` bears out what the
/// claim is owed: the response disclosed what its evidence uses, and the
/// claim is owed at most `b2`. The statement's own test of a response, which
/// a holder makes too before she lets one stand ([`bears_out`]).
pub fn verdict(owed: &Owed, b2: u32, disclosed: Option<&[u8]>) -> Result<(), Refusal> {
    if owed.disclosures != Disclosures::empty()
        && disclosed != Some(&owed.disclosures.to_bytes()[..])
    {
        return Err(Refusal::Undisclosed);
    }
    match owed.owed > b2 {
        true => Err(Refusal::Owed),
        false => Ok(()),
    }
}

/// What a holder makes of the Hub's response to her claim, as its statement
/// would: whether the claim `filed` (her filing's bytes) under `request` is
/// owed at most `b2` against `evidence`, on the headers from Base's
/// checkpoint through her publication's block, and the response disclosed
/// what the evidence uses (`disclosed`, the bytes of its disclosure, if it
/// made one). The statement is a deterministic function of these, so a
/// holder for whom this holds may leave the response uncontested: nothing
/// the Hub could prove would say otherwise.
pub fn bears_out(
    base: &Base,
    request: &Request,
    filed: &[u8],
    evidence: &[Evidence],
    headers: &[[u8; 80]],
    b2: u32,
    disclosed: Option<&[u8]>,
) -> Result<(), Refusal> {
    let chain = chain(base, headers)?;
    let owed = recovery::owed(base, request, filed, evidence, &chain)
        .map_err(|e| Refusal::Evidence(e.to_string()))?;
    verdict(&owed, b2, disclosed)
}
