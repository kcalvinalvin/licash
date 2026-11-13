//! Guest input codecs for the holder and counter programs.
//!
//! Inputs travel as plain data (bincode). Decoding rebuilds every value
//! through its validating constructor, so a guest trusts no decoded
//! structure: notes and authorizations are verified again, F and R are
//! reparsed, the request leaf is recompiled from Base, and header chains
//! are checked from Base's checkpoint. Inclusion and activation proofs are
//! plain data that the predicates verify against that chain.

use serde::{Deserialize, Serialize};

use crate::{
    authorization::{Authorization, SignedProposal},
    base::Base,
    chain::{Activation, Chain, HeaderBytes, Inclusion, Output, Transaction, WitnessInclusion},
    crypto::{Point, Signature},
    hold::{Context, Record},
    note,
    note::{Keyset, Message, Note, Ownership},
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    recovery::{
        prove_counter, CounterJournal, Evidence, HolderWitness, ItemWitness, Publication,
        SourceWitness,
    },
    request::{PublicKeys, Request},
    spend::{Proposal, ProposalId},
    statement::{HubResponse, Observed, StatementInput},
    Error,
};

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    bincode::serialize(value).map_err(|e| Error::Protocol(format!("guest input: {e}")))
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    bincode::deserialize(bytes).map_err(|e| Error::Protocol(format!("guest input: {e}")))
}

fn headers_to_bytes(headers: &[HeaderBytes]) -> Vec<u8> {
    headers.concat()
}

fn headers_from_bytes(bytes: &[u8]) -> Result<Vec<HeaderBytes>, Error> {
    if !bytes.len().is_multiple_of(80) {
        return Err(bad("guest header framing"));
    }
    Ok(bytes
        .chunks(80)
        .map(|h| h.try_into().expect("80-byte chunks"))
        .collect())
}

/// A header chain from Base's checkpoint, checked on decode.
fn chain(base: &Base, headers: &[u8]) -> Result<Chain, Error> {
    let terms = &base.parts().chain;
    Chain::verify(
        terms.checkpoint.clone(),
        &terms.checkpoint.commitment(),
        headers_from_bytes(headers)?,
        terms.max_headers as usize,
    )
}

#[derive(Serialize, Deserialize)]
struct WireOutput {
    index: u32,
    value: u64,
    script: Vec<u8>,
}

impl WireOutput {
    fn of(g: &GraphOutput) -> Self {
        Self {
            index: g.index,
            value: g.output.value,
            script: g.output.script.clone(),
        }
    }
    fn rebuild(self) -> GraphOutput {
        GraphOutput {
            index: self.index,
            output: Output {
                value: self.value,
                script: self.script,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WirePrefix {
    funding: Vec<u8>,
    request: Vec<u8>,
    funding_graph: WireOutput,
    request_graph: WireOutput,
    keys: Vec<u8>,
    alice: [u8; 32],
    hub: [u8; 32],
    delay: u16,
    control: Vec<u8>,
}

impl WirePrefix {
    fn of(prefix: &FrozenPrefix) -> Self {
        let leaf = prefix.leaf();
        Self {
            funding: prefix.funding().bytes().to_vec(),
            request: prefix.request().bytes().to_vec(),
            funding_graph: WireOutput::of(prefix.funding_graph()),
            request_graph: WireOutput::of(prefix.request_graph()),
            keys: leaf.keys().to_bytes(),
            alice: *leaf.alice(),
            hub: *leaf.hub(),
            delay: leaf.delay(),
            control: leaf.control().to_vec(),
        }
    }

    /// F and R reparsed, the request leaf recompiled for this Base.
    fn rebuild(self, base: &Base) -> Result<FrozenPrefix, Error> {
        let max = base.parts().limits.max_transaction_bytes as usize;
        FrozenPrefix::new(
            Transaction::parse(&self.funding, max)?,
            Transaction::parse(&self.request, max)?,
            self.funding_graph.rebuild(),
            self.request_graph.rebuild(),
            RequestLeaf::for_base(
                base.digest(),
                PublicKeys::from_bytes(&self.keys)?,
                self.alice,
                self.hub,
                self.delay,
                self.control,
            )?,
        )
    }
}

#[derive(Serialize, Deserialize)]
struct WireNote {
    keyset: [u8; 32],
    message: Vec<u8>,
    signature: Point,
    evidence: note::Evidence,
}

impl WireNote {
    fn of(n: &Note) -> Self {
        Self {
            keyset: *n.keyset_id(),
            message: n.message().to_bytes().to_vec(),
            signature: n.signature(),
            evidence: *n.evidence(),
        }
    }
    fn rebuild(self, keyset: &Keyset) -> Result<Note, Error> {
        Note::from_parts(
            keyset,
            self.keyset,
            Message::from_bytes(&self.message)?,
            self.signature,
            self.evidence,
        )
    }
}

fn notes_of(notes: &[Note]) -> Vec<WireNote> {
    notes.iter().map(WireNote::of).collect()
}

fn rebuild_notes(notes: Vec<WireNote>, keyset: &Keyset) -> Result<Vec<Note>, Error> {
    notes.into_iter().map(|n| n.rebuild(keyset)).collect()
}

/// An owner-signed spend and the notes it consumes, verified on rebuild.
#[derive(Serialize, Deserialize)]
struct WireSigned {
    proposal: Vec<u8>,
    authorization: Authorization,
    signatures: Vec<Signature>,
    notes: Vec<WireNote>,
}

impl WireSigned {
    fn of(signed: &SignedProposal, notes: &[Note]) -> Self {
        Self {
            proposal: signed.proposal().commitment_bytes(),
            authorization: signed.authorization().clone(),
            signatures: signed.signatures().to_vec(),
            notes: notes_of(notes),
        }
    }
    fn rebuild(self, keyset: &Keyset) -> Result<(SignedProposal, Vec<Note>), Error> {
        let notes = rebuild_notes(self.notes, keyset)?;
        let signed = SignedProposal::from_parts(
            keyset,
            Proposal::from_commitment_bytes(&self.proposal)?,
            &notes,
            self.authorization,
            self.signatures,
        )?;
        Ok((signed, notes))
    }
}

#[derive(Serialize, Deserialize)]
enum WireSource {
    Signed(WireSigned),
    Certified {
        proposal: Vec<u8>,
        signatures: Vec<Signature>,
    },
}

#[derive(Serialize, Deserialize)]
enum WireItem {
    Settled { note: Box<WireNote>, seed: [u8; 32] },
    Pending { seed: [u8; 32] },
}

#[derive(Serialize, Deserialize)]
struct WireHolder {
    items: Vec<WireItem>,
    sources: Vec<WireSource>,
    chain: Option<Vec<u8>>,
    activations: Vec<Activation>,
}

impl WireHolder {
    fn of(w: &HolderWitness) -> Self {
        Self {
            items: w
                .items
                .iter()
                .map(|item| match item {
                    ItemWitness::Settled { note, ownership } => WireItem::Settled {
                        note: Box::new(WireNote::of(note)),
                        seed: ownership.seed(),
                    },
                    ItemWitness::Pending { ownership } => WireItem::Pending {
                        seed: ownership.seed(),
                    },
                })
                .collect(),
            sources: w
                .sources
                .iter()
                .map(|source| match source {
                    SourceWitness::Signed { signed, notes } => {
                        WireSource::Signed(WireSigned::of(signed, notes))
                    }
                    SourceWitness::Certified {
                        proposal,
                        signatures,
                    } => WireSource::Certified {
                        proposal: proposal.commitment_bytes(),
                        signatures: signatures.clone(),
                    },
                })
                .collect(),
            chain: w.chain.as_ref().map(|c| headers_to_bytes(c.headers())),
            activations: w.activations.clone(),
        }
    }

    fn rebuild(self, base: &Base) -> Result<HolderWitness, Error> {
        let keyset = &base.parts().keyset;
        let items = self
            .items
            .into_iter()
            .map(|item| {
                Ok(match item {
                    WireItem::Settled { note, seed } => ItemWitness::Settled {
                        note: Box::new(note.rebuild(keyset)?),
                        ownership: Ownership::from_seed(seed),
                    },
                    WireItem::Pending { seed } => ItemWitness::Pending {
                        ownership: Ownership::from_seed(seed),
                    },
                })
            })
            .collect::<Result<_, Error>>()?;
        let sources = self
            .sources
            .into_iter()
            .map(|source| {
                Ok(match source {
                    WireSource::Signed(signed) => {
                        let (signed, notes) = signed.rebuild(keyset)?;
                        SourceWitness::Signed { signed, notes }
                    }
                    WireSource::Certified {
                        proposal,
                        signatures,
                    } => SourceWitness::Certified {
                        proposal: Proposal::from_commitment_bytes(&proposal)?,
                        signatures,
                    },
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(HolderWitness {
            items,
            sources,
            chain: self.chain.map(|h| chain(base, &h)).transpose()?,
            activations: self.activations,
        })
    }
}

#[derive(Serialize, Deserialize)]
enum WireEvidence {
    Spend(WireSigned, Option<[u8; 32]>),
    SecondClaim {
        final_digest: [u8; 32],
        deposit: Vec<u8>,
        amount: u32,
        claim_digest: [u8; 32],
        message: Vec<u8>,
        sig: Signature,
    },
    PayPreimage {
        source: ProposalId,
        preimage: [u8; 32],
    },
    Expired {
        source: ProposalId,
    },
}

impl WireEvidence {
    fn of(e: &Evidence) -> Self {
        match e {
            Evidence::Spend {
                signed,
                notes,
                unlock,
            } => Self::Spend(WireSigned::of(signed, notes), *unlock),
            Evidence::SecondClaim { context, record } => Self::SecondClaim {
                final_digest: context.final_digest,
                deposit: context.deposit.to_vec(),
                amount: context.amount,
                claim_digest: context.claim_digest,
                message: record.message.to_bytes().to_vec(),
                sig: record.sig,
            },
            Evidence::PayPreimage { source, preimage } => Self::PayPreimage {
                source: *source,
                preimage: *preimage,
            },
            Evidence::Expired { source } => Self::Expired { source: *source },
        }
    }

    fn rebuild(self, keyset: &Keyset) -> Result<Evidence, Error> {
        Ok(match self {
            Self::Spend(signed, unlock) => {
                let (signed, notes) = signed.rebuild(keyset)?;
                Evidence::Spend {
                    signed,
                    notes,
                    unlock,
                }
            }
            Self::SecondClaim {
                final_digest,
                deposit,
                amount,
                claim_digest,
                message,
                sig,
            } => Evidence::SecondClaim {
                context: Context {
                    final_digest,
                    deposit: deposit
                        .try_into()
                        .map_err(|_| bad("guest second-claim deposit"))?,
                    amount,
                    claim_digest,
                },
                record: Record {
                    message: Message::from_bytes(&message)?,
                    sig,
                },
            },
            Self::PayPreimage { source, preimage } => Evidence::PayPreimage { source, preimage },
            Self::Expired { source } => Evidence::Expired { source },
        })
    }
}

#[derive(Serialize, Deserialize)]
struct WireObserved {
    headers: Vec<u8>,
    request_height: u32,
    request: WitnessInclusion,
    publication_height: u32,
    publication: Inclusion,
}

#[derive(Serialize, Deserialize)]
struct WireStatement {
    c: u32,
    base: Vec<u8>,
    prefix: WirePrefix,
    observed: WireObserved,
    response: HubResponse,
    holder: WireHolder,
}

impl StatementInput {
    /// The holder guest's private input.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let o = &self.observed;
        encode(&WireStatement {
            c: self.c,
            base: self.base.to_bytes(),
            prefix: WirePrefix::of(&self.prefix),
            observed: WireObserved {
                headers: headers_to_bytes(&o.headers),
                request_height: o.request_height,
                request: o.request.clone(),
                publication_height: o.publication_height,
                publication: o.publication.clone(),
            },
            response: self.response.clone(),
            holder: WireHolder::of(&self.holder),
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let w: WireStatement = decode(bytes)?;
        let base = Base::from_bytes(&w.base)?;
        let o = w.observed;
        Ok(Self {
            c: w.c,
            prefix: w.prefix.rebuild(&base)?,
            observed: Observed {
                headers: headers_from_bytes(&o.headers)?,
                request_height: o.request_height,
                request: o.request,
                publication_height: o.publication_height,
                publication: o.publication,
            },
            response: w.response,
            holder: w.holder.rebuild(&base)?,
            base,
        })
    }
}

/// The counter guest's input: the Hub's evidence against one publication.
/// The request is P's own (amount, E), so the journal binds this exact
/// claim; the holder statement then requires that E equal R's.
#[derive(Clone)]
pub struct CounterInput {
    pub base: Base,
    pub prefix: FrozenPrefix,
    pub publication: Publication,
    pub evidence: Vec<Evidence>,
    /// Headers from Base's checkpoint, for evidence that reads the chain.
    pub chain: Option<Chain>,
}

#[derive(Serialize, Deserialize)]
struct WireCounter {
    base: Vec<u8>,
    prefix: WirePrefix,
    publication: Vec<u8>,
    evidence: Vec<WireEvidence>,
    chain: Option<Vec<u8>>,
}

impl CounterInput {
    pub fn run(&self) -> Result<CounterJournal, Error> {
        let d = self.base.final_digest(&self.prefix)?;
        let claim = self.publication.claim();
        let request = Request {
            amount: claim.amount(),
            claim_digest: *claim.digest(),
        };
        prove_counter(
            &self.base,
            &d,
            &request,
            &self.publication,
            &self.evidence,
            self.chain.as_ref(),
        )
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        encode(&WireCounter {
            base: self.base.to_bytes(),
            prefix: WirePrefix::of(&self.prefix),
            publication: self.publication.to_bytes(),
            evidence: self.evidence.iter().map(WireEvidence::of).collect(),
            chain: self.chain.as_ref().map(|c| headers_to_bytes(c.headers())),
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let w: WireCounter = decode(bytes)?;
        let base = Base::from_bytes(&w.base)?;
        let parts = base.parts();
        let limit = parts.limits.max_authorizations as usize;
        Ok(Self {
            prefix: w.prefix.rebuild(&base)?,
            publication: Publication::from_bytes(&parts.keyset, limit, &w.publication)?,
            evidence: w
                .evidence
                .into_iter()
                .map(|e| e.rebuild(&parts.keyset))
                .collect::<Result<_, _>>()?,
            chain: w.chain.map(|h| chain(&base, &h)).transpose()?,
            base,
        })
    }
}
