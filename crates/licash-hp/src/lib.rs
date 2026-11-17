//! The Hub's statement for a Licash recovery on Lichen's hub-proves exit:
//! "the claim the holder's publication carries is owed at most `b′`".
//!
//! What Bitcoin shows of the exit (her `request` of `b` and `E`, her
//! publication of the claim, the Hub's `respond` of `b′` and its
//! disclosure) is `lichen_statement`'s to read ([`lichen_statement::read`]).
//! What the claim is owed is Licash's: [`licash::recovery::owed`] of the
//! publication's bytes (a [`licash::recovery::Filing`]) under the request
//! `(b, E)`, against the Hub's evidence, on the Licash chain from Base's
//! checkpoint through the publication's block ([`chain`]). The statement
//! holds if that is at most `b′`, and the response disclosed what the
//! evidence needs (the unlocks and preimages it cites).
//!
//! The statement is fixed when the contract opens ([`contract`]): its
//! journal is `lichen_statement`'s, `c ‖ word ‖ H(O_D ‖ checkpoint ‖ W)`,
//! with Licash's word `H("licash/hp/word" ‖ Base digest)`. Base fixes the
//! keyset, the limits, the checkpoint and the work, and its digest is what
//! every claim's context binds, so a proof for one contract is useless for
//! any other.
//!
//! The holder garbles for this journal at opening (BABE with the roles
//! swapped); the Hub evaluates [`check`] natively to decide its response
//! (the same chain, the same evidence), and proves it in `r0/hp-guest`
//! only when it contests the claim, for the assertion that follows its
//! response.

use licash::{
    base::Base,
    chain::Chain,
    disclosure::Disclosures,
    recovery::{self, Evidence, Owed},
    request::Request,
};
use lichen_statement::{
    btc::{self, Header, OutPoint},
    codec::{R, W},
    Checkpoint, Contract, Exit, Read, Reject, JOURNAL_LEN,
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
    /// Bitcoin doesn't show the exit, as `lichen_statement` reads it.
    Exit(Reject),
    /// The headers through the publication aren't a Licash chain from
    /// Base's checkpoint.
    Chain(String),
    /// The Hub's evidence doesn't prove what it says (licash's reason).
    Evidence(String),
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

/// The statement's input: Base, the exit as the Hub shows it, and its
/// evidence against the claim.
#[derive(Clone)]
pub struct HubInput {
    pub base: Base,
    pub exit: Exit,
    pub evidence: Vec<Evidence>,
}

impl HubInput {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.bytes(&self.base.to_bytes());
        self.exit.write(&mut w);
        w.bytes(&recovery::evidence_to_bytes(&self.evidence));
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
        r.end().map_err(|_| Refusal::Input)?;
        Ok(Self {
            base,
            exit,
            evidence,
        })
    }
}

/// What the statement decides of `exit` under `base` against `evidence`:
/// what the claim is owed, and what Bitcoin shows of the exit.
pub fn decide(base: &Base, exit: &Exit, evidence: &[Evidence]) -> Result<(Owed, Read), Refusal> {
    let contract = contract(base);
    let read = lichen_statement::read(&contract, exit).map_err(Refusal::Exit)?;
    let chain = chain(base, through_publication(&contract, exit, &read))?;
    let request = Request {
        amount: read.b,
        claim_digest: read.e,
    };
    let owed = recovery::owed(base, &request, &read.claim, evidence, &chain)
        .map_err(|e| Refusal::Evidence(e.to_string()))?;
    Ok((owed, read))
}

/// The Hub's statement: it holds if the claim is owed at most the
/// response's `b′`, and the response disclosed what the evidence needs.
/// On success, the journal fixed at opening.
pub fn check(input: &HubInput) -> Result<[u8; JOURNAL_LEN], Refusal> {
    let (owed, read) = decide(&input.base, &input.exit, &input.evidence)?;
    if owed.disclosures != Disclosures::empty()
        && read.disclosed.as_deref() != Some(&owed.disclosures.to_bytes()[..])
    {
        return Err(Refusal::Undisclosed);
    }
    lichen_statement::holds(&contract(&input.base), &read, owed.owed).map_err(|_| Refusal::Owed)
}
