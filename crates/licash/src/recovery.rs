//! Recovery: signed publication P, holder and Corrected counter predicates
//! and settlement arithmetic.
//!
//! These host functions are the statements the guests evaluate; the
//! Bitcoin graph learns only their journals. A journal binds final D and E, so
//! a proof for one opening or claim is useless for any other. Settlement keeps
//! the agreed Corrected policy: what is owed is owed (no penalty mode).

use crate::{
    authorization::{Authorization, SignedProposal, Unlock},
    base::{Base, FinalDigest},
    chain::{Activation, ActivationRef, Chain, Transaction},
    claim::{self, ActivationRecord, Branch, Item, Source, UnsignedClaim, MAX_ITEMS},
    crypto::{self, generator_mul, Point, ScalarBytes, Signature},
    disclosure::{Disclosures, SLOTS},
    hold::{self, Envelope, Record},
    note::{Keyset, Message, Note, Ownership, PUBLIC_FIELDS_BYTES},
    request::Request,
    source::Certificate,
    spend::{Proposal, ProposalId, TimedTerms, TIMED_TERMS_BYTES},
    Error,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const P_MAGIC: &[u8] = b"LICASH-P\x02";
/// One PUSHDATA2 OP_RETURN body; Core 30 relays it under default policy.
pub const MAX_PUBLICATION_BYTES: usize = 64_000;
const MAX_CERTIFICATE_BYTES: usize = 8_192;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

fn put_count(bytes: &mut Vec<u8>, n: usize) {
    bytes.extend(u32::try_from(n).expect("bounded count").to_be_bytes());
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.0.len() {
            return Err(bad("truncated native publication"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("fixed field"))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }
    fn count(&mut self, limit: usize) -> Result<usize, Error> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(bad("native publication component limit"));
        }
        Ok(n)
    }
}

/// One OP_RETURN with a single PUSHDATA2 of `body`.
fn push_script(body: Vec<u8>) -> Result<Vec<u8>, Error> {
    if body.len() > MAX_PUBLICATION_BYTES {
        return Err(bad("native publication exceeds its byte limit"));
    }
    let mut script = vec![0x6a, 0x4d];
    script.extend((body.len() as u16).to_le_bytes());
    script.extend(body);
    Ok(script)
}

/// The body a [`push_script`] pushes.
fn pushed(script: &[u8]) -> Result<&[u8], Error> {
    if script.len() < 4 || script[..2] != [0x6a, 0x4d] {
        return Err(bad("native publication script"));
    }
    let n = u16::from_le_bytes([script[2], script[3]]) as usize;
    if script.len() != n + 4 || n < 76 {
        return Err(bad("native publication push"));
    }
    Ok(&script[4..])
}

/// The script of `tx`'s unique output pushing a body that starts with
/// `magic`.
fn unique_push<'a>(tx: &'a Transaction, magic: &[u8]) -> Result<&'a [u8], Error> {
    let mut found = tx.outputs().iter().filter(|output| {
        output.script.len() > 4 + magic.len()
            && output.script[..2] == [0x6a, 0x4d]
            && output.script[4..].starts_with(magic)
    });
    let output = found
        .next()
        .ok_or_else(|| bad("no native publication output"))?;
    if found.next().is_some() {
        return Err(bad("ambiguous native publication outputs"));
    }
    Ok(&output.script)
}

/// Signed claim publication P: every counted field of E plus every Q
/// signature. It is posted in a non-witness transaction after R; decoding
/// recomputes E, so P is authority only for the E that R actually signed.
#[derive(Clone)]
pub struct Publication {
    claim: UnsignedClaim,
    envelope: Envelope,
}

impl std::fmt::Debug for Publication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Publication(..)")
    }
}

impl Publication {
    /// Sign every required Q authorization (items plus OwnInputs inputs).
    pub fn sign(
        claim: UnsignedClaim,
        hold_keys: &[(Message, ScalarBytes)],
        limit: usize,
    ) -> Result<Self, Error> {
        let mut entries = Vec::with_capacity(hold_keys.len());
        for (message, key) in hold_keys {
            entries.push((message.identifier()?, message.clone(), *key));
        }
        entries.sort_by_key(|(y, _, _)| *y);
        let entries: Vec<_> = entries.into_iter().map(|(_, m, k)| (m, k)).collect();
        let envelope = Envelope::sign(claim.hold_context(), &entries, limit)?;
        envelope.verify(&claim.hold_context(), claim.hold_root(), limit)?;
        Ok(Self { claim, envelope })
    }

    pub fn claim(&self) -> &UnsignedClaim {
        &self.claim
    }
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let claim = &self.claim;
        let context = claim.context();
        let parts = claim.parts();
        let mut bytes = P_MAGIC.to_vec();
        bytes.extend(context.final_digest);
        bytes.extend(context.deposit);
        bytes.extend(context.cap.to_be_bytes());
        bytes.extend(claim.keyset_id());
        bytes.extend(claim.amount().to_be_bytes());
        put_count(&mut bytes, parts.items.len());
        for item in &parts.items {
            match item {
                Item::Settled(message) => {
                    bytes.push(0);
                    bytes.extend(message.public_fields());
                }
                Item::Pending {
                    message,
                    source,
                    branch,
                    index,
                    blinding,
                    preimage,
                } => {
                    bytes.push(1);
                    bytes.extend(message.public_fields());
                    bytes.extend(source.as_bytes());
                    bytes.push(u8::from(*branch == Branch::Refund));
                    bytes.extend(index.to_be_bytes());
                    bytes.extend(blinding.0);
                    match preimage {
                        None => bytes.push(0),
                        Some(z) => {
                            bytes.push(1);
                            bytes.extend(z);
                        }
                    }
                }
            }
        }
        put_count(&mut bytes, parts.sources.len());
        for source in &parts.sources {
            match source {
                Source::Plain { id, inputs } | Source::OwnInputs { id, inputs, .. } => {
                    bytes.push(u8::from(matches!(source, Source::OwnInputs { .. })));
                    bytes.extend(id.as_bytes());
                    put_count(&mut bytes, inputs.len());
                    for input in inputs {
                        bytes.extend(input.0);
                    }
                    if let Source::OwnInputs { terms, .. } = source {
                        bytes.extend(terms.to_bytes());
                    }
                }
                Source::Admitted(cert) => {
                    bytes.push(2);
                    let cert = cert.to_bytes();
                    put_count(&mut bytes, cert.len());
                    bytes.extend(cert);
                }
            }
        }
        put_count(&mut bytes, parts.activations.len());
        for activation in &parts.activations {
            bytes.extend(activation.source.as_bytes());
            bytes.extend(activation.reference.to_bytes());
        }
        let records = self.envelope.records();
        put_count(&mut bytes, records.len());
        for record in records {
            bytes.extend(record.message.public_fields());
            bytes.extend(record.sig.0);
        }
        bytes
    }

    /// Strict canonical decoding. The keyset and authorization limit are the
    /// opening's authenticated terms, never taken from the payload.
    pub fn from_bytes(keyset: &Keyset, limit: usize, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_PUBLICATION_BYTES || !bytes.starts_with(P_MAGIC) {
            return Err(bad("native publication framing"));
        }
        let mut r = Reader(&bytes[P_MAGIC.len()..]);
        let context = claim::Context {
            final_digest: r.array()?,
            deposit: r.array()?,
            cap: r.u32()?,
        };
        if &r.array::<32>()? != keyset.id() {
            return Err(bad("native publication keyset"));
        }
        let amount = r.u32()?;
        let mut items = Vec::new();
        for _ in 0..r.count(MAX_ITEMS)? {
            let tag = r.byte()?;
            let message = Message::from_public_fields(r.take(PUBLIC_FIELDS_BYTES)?)?;
            items.push(match tag {
                0 => Item::Settled(message),
                1 => {
                    let source = ProposalId::from_bytes(r.array()?);
                    let branch = match r.byte()? {
                        0 => Branch::Pay,
                        1 => Branch::Refund,
                        _ => return Err(bad("native publication branch")),
                    };
                    let index = r.u32()?;
                    let blinding = ScalarBytes(r.array()?);
                    let preimage = match r.byte()? {
                        0 => None,
                        1 => Some(r.array()?),
                        _ => return Err(bad("native publication preimage flag")),
                    };
                    Item::Pending {
                        message,
                        source,
                        branch,
                        index,
                        blinding,
                        preimage,
                    }
                }
                _ => return Err(bad("native publication item tag")),
            });
        }
        let mut sources = Vec::new();
        for _ in 0..r.count(MAX_ITEMS)? {
            let tag = r.byte()?;
            sources.push(match tag {
                0 | 1 => {
                    let id = ProposalId::from_bytes(r.array()?);
                    let mut inputs = Vec::new();
                    for _ in 0..r.count(limit)? {
                        inputs.push(Point(r.array()?));
                    }
                    if tag == 0 {
                        Source::Plain { id, inputs }
                    } else {
                        let terms = TimedTerms::from_bytes(r.take(TIMED_TERMS_BYTES)?)?;
                        Source::OwnInputs { id, inputs, terms }
                    }
                }
                2 => {
                    let n = r.count(MAX_CERTIFICATE_BYTES)?;
                    Source::Admitted(Certificate::from_bytes(keyset, r.take(n)?)?)
                }
                _ => return Err(bad("native publication source tag")),
            });
        }
        let mut activations = Vec::new();
        for _ in 0..r.count(MAX_ITEMS)? {
            let source = ProposalId::from_bytes(r.array()?);
            let reference = ActivationRef {
                height: r.u32()?,
                block_hash: r.array()?,
                txid: r.array()?,
                output: r.u32()?,
            };
            activations.push(ActivationRecord { source, reference });
        }
        let mut records = Vec::new();
        for _ in 0..r.count(limit)? {
            let message = Message::from_public_fields(r.take(PUBLIC_FIELDS_BYTES)?)?;
            records.push(Record {
                message,
                sig: Signature(r.array()?),
            });
        }
        if !r.0.is_empty() {
            return Err(bad("trailing native publication bytes"));
        }
        let authorizations = records
            .iter()
            .map(|record| record.message.clone())
            .collect();
        let claim = UnsignedClaim::new(
            context,
            keyset,
            amount,
            claim::Parts {
                items,
                sources,
                activations,
                authorizations,
            },
            limit,
        )?;
        let envelope =
            Envelope::from_records(claim.hold_context(), claim.hold_root(), records, limit)?;
        let publication = Self { claim, envelope };
        if publication.to_bytes() != bytes {
            return Err(bad("noncanonical native publication"));
        }
        Ok(publication)
    }

    /// One OP_RETURN with a single PUSHDATA2 of the canonical body.
    pub fn script(&self) -> Result<Vec<u8>, Error> {
        push_script(self.to_bytes())
    }

    pub fn from_script(keyset: &Keyset, limit: usize, script: &[u8]) -> Result<Self, Error> {
        Self::from_bytes(keyset, limit, pushed(script)?)
    }

    /// The unique P output of a publication transaction. Inclusion, depth and
    /// timing are the observer's checks against the opening's chain profile.
    pub fn from_transaction(
        keyset: &Keyset,
        limit: usize,
        tx: &Transaction,
    ) -> Result<Self, Error> {
        Self::from_script(keyset, limit, unique_push(tx, P_MAGIC)?)
    }

    /// P is authority only for the request R signed under this opening.
    pub fn bind(&self, base: &Base, d: &FinalDigest, request: &Request) -> Result<(), Error> {
        let parts = base.parts();
        let context = self.claim.context();
        if &context.final_digest != d.as_bytes()
            || context.deposit != parts.deposit
            || context.cap != parts.cap
            || self.claim.keyset_id() != parts.keyset.id()
            || request.amount != self.claim.amount()
            || &request.claim_digest != self.claim.digest()
        {
            return Err(bad("publication does not match the opening request"));
        }
        Ok(())
    }
}

/// Holder's private evidence for one source: the complete owner-signed body.
/// Plain/OwnInputs need input issuance evidence; certified (Admitted) sources
/// use the issuer's admission signature instead of payer note evidence.
#[derive(Clone)]
pub enum SourceWitness {
    Signed {
        signed: SignedProposal,
        notes: Vec<Note>,
    },
    Certified {
        proposal: Proposal,
        signatures: Vec<Signature>,
    },
}

impl SourceWitness {
    fn proposal(&self) -> &Proposal {
        match self {
            Self::Signed { signed, .. } => signed.proposal(),
            Self::Certified { proposal, .. } => proposal,
        }
    }

    /// Authenticate against the claim's public source metadata.
    fn authenticate(&self, keyset: &Keyset, source: &Source) -> Result<(), Error> {
        let lock_terms = self.proposal().lock().map(|lock| lock.terms());
        match (source, self) {
            (
                Source::Admitted(cert),
                Self::Certified {
                    proposal,
                    signatures,
                },
            ) => cert.authenticate_source(keyset, proposal, signatures),
            (Source::Admitted(cert), Self::Signed { signed, notes }) => {
                cert.verify_source(keyset, signed, notes)
            }
            (Source::Plain { .. } | Source::OwnInputs { .. }, Self::Signed { signed, notes }) => {
                let mut inputs = signed.verify(keyset, notes)?;
                inputs.sort_unstable();
                if inputs != source.inputs() || lock_terms != source.terms() {
                    return Err(bad("source body differs from claimed metadata"));
                }
                Ok(())
            }
            _ => Err(bad("source witness class")),
        }
    }
}

/// Private per-item eligibility. Ownership binds the item to Base's spending
/// root; settled items add issuance evidence, pending ones the source output.
#[derive(Clone)]
pub enum ItemWitness {
    Settled {
        note: Box<Note>,
        ownership: Ownership,
    },
    Pending {
        ownership: Ownership,
    },
}

#[derive(Clone)]
pub struct HolderWitness {
    pub items: Vec<ItemWitness>,
    pub sources: Vec<SourceWitness>,
    /// Positive witnesses for every advertised activation reference.
    pub chain: Option<Chain>,
    pub activations: Vec<Activation>,
}

/// The holder statement's journal: this exact opening, request and amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HolderJournal {
    pub final_digest: [u8; 32],
    pub claim_digest: [u8; 32],
    pub amount: u32,
}

fn opens(
    output: &crate::spend::Output,
    message: &Message,
    blinding: &ScalarBytes,
) -> Result<bool, Error> {
    let blinded = message.identifier()?.to_projective()? + generator_mul(&blinding.to_scalar()?);
    Ok(output.denomination() == message.denomination()
        && output.blinded() == Point::from_projective(&blinded))
}

/// Holder recovery predicate: every claimed item is eligible, owned by Base's
/// spending root and Q-authorized in P; every advertised activation is real.
///
/// The Pay outputs of a timed lock are eligible only under a Base opened on
/// the checkpoint that the lock's terms name. Both programs read Bitcoin from
/// Base's checkpoint, so under any other the Hub could never show that the
/// lock expired, and an expired, refunded lock would be paid a second time.
pub fn prove_holder(
    base: &Base,
    d: &FinalDigest,
    request: &Request,
    publication: &Publication,
    witness: &HolderWitness,
) -> Result<HolderJournal, Error> {
    publication.bind(base, d, request)?;
    let parts = base.parts();
    let keyset = &parts.keyset;
    let claim = publication.claim();
    let items = &claim.parts().items;
    if witness.items.len() != items.len() {
        return Err(bad("holder witness item count"));
    }
    let claimed: BTreeMap<_, _> = claim.parts().sources.iter().map(|s| (s.id(), s)).collect();
    let mut proposals = BTreeMap::new();
    for source in &witness.sources {
        let id = source.proposal().id();
        let metadata = claimed
            .get(&id)
            .ok_or_else(|| bad("unclaimed source witness"))?;
        source.authenticate(keyset, metadata)?;
        proposals.insert(id, source.proposal());
    }
    if proposals.len() != claimed.len() {
        return Err(bad("missing source witness"));
    }
    let checkpoint = &parts.chain.checkpoint;
    let agreed = (checkpoint.profile(), checkpoint.commitment());
    for (item, w) in items.iter().zip(&witness.items) {
        match (item, w) {
            (Item::Settled(message), ItemWitness::Settled { note, ownership }) => {
                if note.message() != message {
                    return Err(bad("settled witness message"));
                }
                note.verify(keyset)?;
                ownership.verify(&parts.spend_root, message)?;
            }
            (
                Item::Pending {
                    message,
                    source,
                    branch,
                    index,
                    blinding,
                    ..
                },
                ItemWitness::Pending { ownership },
            ) => {
                let proposal = proposals[source];
                let timed_elsewhere = claimed[source]
                    .terms()
                    .is_some_and(|t| (t.profile(), *t.checkpoint()) != agreed);
                if *branch == Branch::Pay && timed_elsewhere {
                    return Err(bad("pay output of a lock timed on another checkpoint"));
                }
                let outputs = match branch {
                    Branch::Pay => proposal.outputs(),
                    Branch::Refund => proposal
                        .lock()
                        .ok_or_else(|| bad("refund of an untimed source"))?
                        .refund(),
                };
                let output = outputs
                    .get(*index as usize)
                    .ok_or_else(|| bad("claimed source output index"))?;
                if !opens(output, message, blinding)?
                    || output.unlink_commitment() != &ownership.unlink_commitment()
                {
                    return Err(bad("pending item does not open its source output"));
                }
                ownership.verify(&parts.spend_root, message)?;
            }
            _ => return Err(bad("holder witness item class")),
        }
    }
    for record in &claim.parts().activations {
        let chain = witness
            .chain
            .as_ref()
            .ok_or_else(|| bad("activation needs a chain"))?;
        let activation = witness
            .activations
            .iter()
            .find(|a| a.reference == record.reference)
            .ok_or_else(|| bad("advertised activation lacks a witness"))?;
        let terms = claimed[&record.source]
            .terms()
            .expect("admitted source is timed");
        activation.verify(chain, terms, parts.limits.max_transaction_bytes as usize)?;
    }
    Ok(HolderJournal {
        final_digest: *d.as_bytes(),
        claim_digest: *claim.digest(),
        amount: claim.amount(),
    })
}

/// Hub counter evidence under the Corrected policy. Each evidence must void at
/// least one item; voided items are excluded and everything else stays owed.
#[derive(Clone)]
pub enum Evidence {
    /// An owner-signed spend. It voids items it consumes (own Y) and pending
    /// outputs of another source whose inputs it double-spends, except an
    /// activated certified Pay. Executing a Plain source values its fallbacks.
    /// A granted spend is cited with its unlock, which the counter publishes:
    /// retiring the payer's inputs releases the payee's outputs.
    Spend {
        signed: SignedProposal,
        notes: Vec<Note>,
        unlock: Option<Unlock>,
    },
    /// A Q key that authorized this claim authorized a different claim too. It
    /// voids that key's item and, for a lock of the holder's own notes
    /// (`Source::OwnInputs`, whose inputs' keys must authorize the claim), the
    /// outputs of the lock that the key's note funds: the Hub may never have
    /// seen that lock, and the note may have been paid in the other claim.
    SecondClaim {
        context: hold::Context,
        record: Record,
    },
    /// The Pay preimage of a timed source is known: its Refund items are void.
    /// The counter publishes the preimage.
    PayPreimage {
        source: ProposalId,
        preimage: [u8; 32],
    },
    /// The source expired at depth with no activation referenced by the claim:
    /// its Pay items are void (the refund branch holds).
    Expired { source: ProposalId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterJournal {
    pub final_digest: [u8; 32],
    pub claim_digest: [u8; 32],
    /// Bit i voids claim item i (item positions are mask positions).
    pub mask: u32,
    pub owed: u32,
    /// Digest of the 32 disclosure slots the response must publish.
    pub disclosure: [u8; 32],
}

impl CounterJournal {
    pub const LEN: usize = 104;

    /// The counter guest's public output: `D ‖ E ‖ mask ‖ owed ‖ H(slots)`.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut bytes = [0; Self::LEN];
        bytes[..32].copy_from_slice(&self.final_digest);
        bytes[32..64].copy_from_slice(&self.claim_digest);
        bytes[64..68].copy_from_slice(&self.mask.to_be_bytes());
        bytes[68..72].copy_from_slice(&self.owed.to_be_bytes());
        bytes[72..].copy_from_slice(&self.disclosure);
        bytes
    }
}

/// The secrets a counter publishes with its response: the unlock of every
/// granted spend it cites and every Pay preimage, one slot per source in
/// source-id order, zero padded. `prove_counter` commits their digest, and the
/// holder statement accepts the counter only with these exact posted bytes.
pub fn counter_disclosures(evidence: &[Evidence]) -> Result<Disclosures, Error> {
    let mut by_source = BTreeMap::new();
    for e in evidence {
        match e {
            Evidence::Spend {
                signed,
                unlock: Some(unlock),
                ..
            } => {
                by_source.insert(signed.proposal().id(), *unlock);
            }
            Evidence::PayPreimage { source, preimage } => {
                by_source.insert(*source, *preimage);
            }
            _ => {}
        }
    }
    if by_source.len() > SLOTS {
        return Err(bad("counter disclosures exceed the publication"));
    }
    let mut disclosures = Disclosures::empty();
    for (slot, value) in disclosures.values.iter_mut().zip(by_source.values()) {
        *slot = *value;
    }
    Ok(disclosures)
}

/// Corrected counter predicate. `chain` must be verified from the source
/// terms' checkpoint; it is needed only for `Expired`.
pub fn prove_counter(
    base: &Base,
    d: &FinalDigest,
    request: &Request,
    publication: &Publication,
    evidence: &[Evidence],
    chain: Option<&Chain>,
) -> Result<CounterJournal, Error> {
    publication.bind(base, d, request)?;
    let claim = publication.claim();
    let (mask, executed) = void(&base.parts().keyset, claim, evidence, chain)?;
    if mask == 0 {
        return Err(bad("empty counter"));
    }
    Ok(CounterJournal {
        final_digest: *d.as_bytes(),
        claim_digest: *claim.digest(),
        mask,
        owed: live(claim, mask, &executed),
        disclosure: counter_disclosures(evidence)?.digest(),
    })
}

/// The items `evidence` voids (bit i for item i) and the sources it shows
/// executed. Each evidence must void an item, or show a Plain source
/// executed, which values its fallbacks. `chain` must be verified from the
/// source terms' checkpoint; it is needed only for `Expired`.
fn void(
    keyset: &Keyset,
    claim: &UnsignedClaim,
    evidence: &[Evidence],
    chain: Option<&Chain>,
) -> Result<(u32, BTreeSet<ProposalId>), Error> {
    let items = &claim.parts().items;
    let sources: BTreeMap<_, _> = claim.parts().sources.iter().map(|s| (s.id(), s)).collect();
    let activated: BTreeSet<_> = claim.parts().activations.iter().map(|a| a.source).collect();
    let immune = |source: &ProposalId, branch: &Branch| {
        *branch == Branch::Pay
            && activated.contains(source)
            && matches!(sources.get(source), Some(Source::Admitted(_)))
    };
    let mut mask = 0u32;
    let mut executed = BTreeSet::new();
    for e in evidence {
        let before = mask;
        let mut values_fallback = false;
        match e {
            Evidence::Spend {
                signed,
                notes,
                unlock,
            } => {
                let spent: BTreeSet<_> = signed.verify(keyset, notes)?.into_iter().collect();
                // The owners committed to the grant, so a granted spend cannot
                // be cited as a plain one to keep its unlock private.
                match (signed.authorization(), unlock) {
                    (Authorization::Plain, None) => {}
                    (Authorization::Granted(grant), Some(unlock)) => {
                        grant.verify_unlock(keyset, unlock)?;
                    }
                    _ => return Err(bad("a granted spend is cited with its unlock")),
                }
                let id = signed.proposal().id();
                values_fallback = matches!(sources.get(&id), Some(Source::Plain { .. }));
                executed.insert(id);
                for (i, item) in items.iter().enumerate() {
                    let own = spent.contains(&item.message().identifier()?);
                    let shared = match item {
                        Item::Pending { source, branch, .. } => {
                            *source != id
                                && !immune(source, branch)
                                && sources[source].inputs().iter().any(|y| spent.contains(y))
                        }
                        Item::Settled(_) => false,
                    };
                    if own || shared {
                        mask |= 1 << i;
                    }
                }
            }
            Evidence::SecondClaim { context, record } => {
                if *context == claim.hold_context() {
                    return Err(bad("second claim must be a different claim"));
                }
                crypto::schnorr_verify(
                    record.message.hold(),
                    &context.signing_digest(&record.message),
                    &record.sig,
                )?;
                let claimed = record.message.identifier()?;
                for (i, item) in items.iter().enumerate() {
                    let funded = match item {
                        Item::Pending { source, .. } => matches!(
                            &sources[source],
                            Source::OwnInputs { inputs, .. } if inputs.contains(&claimed)
                        ),
                        Item::Settled(_) => false,
                    };
                    if funded || item.message() == &record.message {
                        mask |= 1 << i;
                    }
                }
            }
            Evidence::PayPreimage { source, preimage } => {
                let terms = sources
                    .get(source)
                    .and_then(|s| s.terms())
                    .ok_or_else(|| bad("preimage for an untimed or unclaimed source"))?;
                if <[u8; 32]>::from(Sha256::digest(preimage)) != *terms.hash() {
                    return Err(bad("pay preimage hash"));
                }
                for (i, item) in items.iter().enumerate() {
                    if matches!(item, Item::Pending { source: s, branch: Branch::Refund, .. } if s == source)
                    {
                        mask |= 1 << i;
                    }
                }
            }
            Evidence::Expired { source } => {
                let terms = sources
                    .get(source)
                    .and_then(|s| s.terms())
                    .ok_or_else(|| bad("expiry of an untimed or unclaimed source"))?;
                let chain = chain.ok_or_else(|| bad("expiry needs a chain"))?;
                let mature = terms
                    .expiry()
                    .checked_add(terms.depth())
                    .ok_or_else(|| bad("expiry horizon"))?;
                if chain.checkpoint().commitment() != *terms.checkpoint()
                    || chain.tip_height() < mature
                {
                    return Err(bad("source expiry is not mature on the agreed chain"));
                }
                for (i, item) in items.iter().enumerate() {
                    if let Item::Pending {
                        source: s,
                        branch: Branch::Pay,
                        ..
                    } = item
                    {
                        if s == source && !immune(s, &Branch::Pay) {
                            mask |= 1 << i;
                        }
                    }
                }
            }
        }
        if mask == before && !values_fallback {
            return Err(bad("counter evidence voids nothing"));
        }
    }
    Ok((mask, executed))
}

/// What no evidence voided: settled and timed items, and the fallbacks of
/// the Plain sources shown executed, capped at the claim's amount.
fn live(claim: &UnsignedClaim, mask: u32, executed: &BTreeSet<ProposalId>) -> u32 {
    let sources: BTreeMap<_, _> = claim.parts().sources.iter().map(|s| (s.id(), s)).collect();
    let mut live = 0u128;
    for (i, item) in claim.parts().items.iter().enumerate() {
        if mask & (1 << i) != 0 {
            continue;
        }
        let valued = match item {
            Item::Settled(_) => true,
            Item::Pending { source, .. } => {
                sources[source].terms().is_some() || executed.contains(source)
            }
        };
        if valued {
            live += u128::from(item.message().denomination());
        }
    }
    u32::try_from(live.min(u128::from(claim.amount()))).expect("bounded by amount")
}

/// What the Hub did after R.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// No response within the window: the request timeout pays b.
    Silent,
    /// Plain response: the holder must prove the full claim.
    Plain,
    /// A proven Corrected counter; an unproven one is treated as Plain.
    Counter(CounterJournal),
}

/// Settled value in satoshis: principal through winning bits, the remainder
/// from the holder terminal's reserve surplus (checked by RecoveryBudget).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub owed: u32,
    pub principal: u64,
    pub remainder: u64,
}

pub fn settle(
    base: &Base,
    d: &FinalDigest,
    request: &Request,
    response: Response,
    holder: Option<&HolderJournal>,
) -> Result<Settlement, Error> {
    let proven = holder.is_some_and(|j| {
        j.final_digest == *d.as_bytes()
            && j.claim_digest == request.claim_digest
            && j.amount == request.amount
    });
    let owed = match response {
        Response::Silent => request.amount,
        Response::Plain => {
            if proven {
                request.amount
            } else {
                0
            }
        }
        Response::Counter(c) => {
            if c.final_digest != *d.as_bytes() || c.claim_digest != request.claim_digest {
                return Err(bad("counter journal for another claim"));
            }
            if proven {
                c.owed
            } else {
                0
            }
        }
    };
    let low = base.recovery_budget()?.low_bits();
    let principal = u64::from(owed) & !((1u64 << low) - 1);
    Ok(Settlement {
        owed,
        principal,
        remainder: u64::from(owed) - principal,
    })
}
