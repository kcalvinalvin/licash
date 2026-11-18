//! Recovery: the signed publication P, the claim as filed on Bitcoin
//! ([`Filing`]), and what it is owed ([`owed`]).
//!
//! The holder files P with a public eligibility witness and proves nothing.
//! The Hub decides what the claim is owed from the filing and its own
//! evidence, and proves that decision in its statement (`licash-hp`, whose
//! guest evaluates [`owed`]). Notes are designated by their hold keys alone,
//! not owned through any root key (see [`Filing`] for what that changes).
//! A claim binds its opening's Base digest and E, so a proof for one opening
//! or claim is useless for any other. What is owed is owed: there is no
//! penalty mode.

use crate::{
    authorization::{Authorization, SignedProposal, Unlock},
    base::{Base, Version},
    chain::{Activation, ActivationRef, Chain, Checkpoint, Inclusion},
    claim::{self, ActivationRecord, Branch, Item, Source, UnsignedClaim, MAX_ITEMS},
    crypto::{self, generator_mul, Point, ScalarBytes, Signature},
    disclosure::{Disclosures, SLOTS},
    hold::{self, Envelope, Record},
    note::{Keyset, Message, Note, PUBLIC_FIELDS_BYTES},
    request::Request,
    source::Certificate,
    spend::{Proposal, ProposalId, TimedTerms, TIMED_TERMS_BYTES},
    Error,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const P_MAGIC: &[u8] = b"LICASH-P\x02";
/// A filing's magic: its activations unanchored (v3's), or each with the
/// lock checkpoint it is anchored to (v4's).
const F_MAGIC: &[u8] = b"LICASH-F\x01";
const F_MAGIC_ANCHORED: &[u8] = b"LICASH-F\x02";
/// A filing's limit: its bytes fit Lichen's claim carrier
/// (`lichen_statement::claim_script`), which Core relays under default
/// policy.
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
    fn blob(&mut self) -> Result<&'a [u8], Error> {
        let n = self.count(MAX_PUBLICATION_BYTES)?;
        self.take(n)
    }
}

fn put_blob(bytes: &mut Vec<u8>, blob: &[u8]) {
    put_count(bytes, blob.len());
    bytes.extend(blob);
}

/// Signed claim publication P: every counted field of E plus every Q
/// signature, filed with its witness ([`Filing`]) after R. Decoding
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
        bytes.extend(context.base);
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
            base: r.array()?,
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

    /// P is authority only for the request R signed under this opening.
    pub fn bind(&self, base: &Base, request: &Request) -> Result<(), Error> {
        let parts = base.parts();
        let context = self.claim.context();
        if context.base != base.digest()
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

fn opens(
    output: &crate::spend::Output,
    message: &Message,
    blinding: &ScalarBytes,
) -> Result<bool, Error> {
    let blinded = message.identifier()?.to_projective()? + generator_mul(&blinding.to_scalar()?);
    Ok(output.denomination() == message.denomination()
        && output.blinded() == Point::from_projective(&blinded))
}

/// The Hub's evidence against a claim. Each evidence must void at least one
/// item (or value a Plain source's fallbacks); voided items are excluded and
/// everything else stays owed.
#[derive(Clone)]
pub enum Evidence {
    /// An owner-signed spend. It voids items it consumes (own Y) and pending
    /// outputs of another source whose inputs it double-spends, except an
    /// activated certified Pay. Executing a Plain source values its fallbacks.
    /// A granted spend is cited with its unlock, which the Hub publishes:
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
    /// The Hub publishes the preimage.
    PayPreimage {
        source: ProposalId,
        preimage: [u8; 32],
    },
    /// The source expired at depth with no activation referenced by the claim:
    /// its Pay items are void (the refund branch holds). v3 only: under v4 a
    /// lock's Pay is owed only with an activation, so expiry voids nothing.
    Expired { source: ProposalId },
}

/// Most evidence a statement's input carries: one per item it voids, and
/// one per source.
pub const MAX_EVIDENCE: usize = 2 * MAX_ITEMS;

/// The Hub's evidence, canonically encoded for its statement's input.
pub fn evidence_to_bytes(evidence: &[Evidence]) -> Vec<u8> {
    let mut bytes = Vec::new();
    put_count(&mut bytes, evidence.len());
    for e in evidence {
        match e {
            Evidence::Spend {
                signed,
                notes,
                unlock,
            } => {
                bytes.push(0);
                put_count(&mut bytes, notes.len());
                for note in notes {
                    put_blob(&mut bytes, &note.to_bytes());
                }
                put_blob(&mut bytes, &signed.to_bytes());
                match unlock {
                    None => bytes.push(0),
                    Some(unlock) => {
                        bytes.push(1);
                        bytes.extend(unlock);
                    }
                }
            }
            Evidence::SecondClaim { context, record } => {
                bytes.push(1);
                bytes.extend(context.to_bytes());
                bytes.extend(record.message.to_bytes());
                bytes.extend(record.sig.0);
            }
            Evidence::PayPreimage { source, preimage } => {
                bytes.push(2);
                bytes.extend(source.as_bytes());
                bytes.extend(preimage);
            }
            Evidence::Expired { source } => {
                bytes.push(3);
                bytes.extend(source.as_bytes());
            }
        }
    }
    bytes
}

/// Strict canonical decoding of [`evidence_to_bytes`]: every note verified
/// under `keyset`, every signed spend against its notes. Whether the
/// evidence voids anything is [`owed`]'s to decide.
pub fn evidence_from_bytes(
    keyset: &Keyset,
    limit: usize,
    bytes: &[u8],
) -> Result<Vec<Evidence>, Error> {
    let mut r = Reader(bytes);
    let mut evidence = Vec::new();
    for _ in 0..r.count(MAX_EVIDENCE)? {
        evidence.push(match r.byte()? {
            0 => {
                let notes = (0..r.count(limit)?)
                    .map(|_| Note::from_bytes(keyset, r.blob()?))
                    .collect::<Result<Vec<_>, _>>()?;
                let signed = SignedProposal::from_bytes(keyset, &notes, r.blob()?)?;
                let unlock = match r.byte()? {
                    0 => None,
                    1 => Some(r.array()?),
                    _ => return Err(bad("evidence unlock tag")),
                };
                Evidence::Spend {
                    signed,
                    notes,
                    unlock,
                }
            }
            1 => Evidence::SecondClaim {
                context: hold::Context::from_bytes(&r.array()?),
                record: Record {
                    message: Message::from_bytes(r.take(crate::note::MESSAGE_BYTES)?)?,
                    sig: Signature(r.array()?),
                },
            },
            2 => Evidence::PayPreimage {
                source: ProposalId::from_bytes(r.array()?),
                preimage: r.array()?,
            },
            3 => Evidence::Expired {
                source: ProposalId::from_bytes(r.array()?),
            },
            _ => return Err(bad("evidence tag")),
        });
    }
    if !r.0.is_empty() || evidence_to_bytes(&evidence) != bytes {
        return Err(bad("noncanonical evidence"));
    }
    Ok(evidence)
}

/// The secrets the Hub publishes with its response: the unlock of every
/// granted spend it cites and every Pay preimage, one slot per source in
/// source-id order, zero padded. Its statement holds only if its response
/// disclosed these exact bytes (when any slot is set).
pub fn disclosures(evidence: &[Evidence]) -> Result<Disclosures, Error> {
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
        return Err(bad("disclosures exceed their slots"));
    }
    let mut disclosures = Disclosures::empty();
    for (slot, value) in disclosures.values.iter_mut().zip(by_source.values()) {
        *slot = *value;
    }
    Ok(disclosures)
}

/// The items `evidence` voids (bit i for item i) and the sources it shows
/// executed. Each evidence must void an item, or show a Plain source
/// executed, which values its fallbacks. `chain` must be verified from the
/// source terms' checkpoint; it is needed only for `Expired`, which only
/// v3 admits.
fn void(
    version: Version,
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
                if version != Version::V3 {
                    return Err(bad("expiry voids nothing under v4"));
                }
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
            return Err(bad("evidence voids nothing"));
        }
    }
    Ok((mask, executed))
}

/// What no evidence voided: settled and timed items, and the fallbacks of
/// the Plain sources shown executed, capped at the claim's amount. Under v4
/// a timed lock's Pay item counts only if the claim advertises an
/// activation of the lock (which [`eligible`] verified on Bitcoin): the
/// statement can't see an expiry, so a Pay without one may have been
/// refunded. And its Refund item counts only once the chain judged, to
/// `judged`, reaches its expiry at depth: any activation is buried by
/// then, so the Hub holds its preimage, and can't be owed the Pay later.
fn live(
    version: Version,
    claim: &UnsignedClaim,
    mask: u32,
    executed: &BTreeSet<ProposalId>,
    judged: u32,
) -> u32 {
    let sources: BTreeMap<_, _> = claim.parts().sources.iter().map(|s| (s.id(), s)).collect();
    let activated: BTreeSet<_> = claim.parts().activations.iter().map(|a| a.source).collect();
    let mut live = 0u128;
    for (i, item) in claim.parts().items.iter().enumerate() {
        if mask & (1 << i) != 0 {
            continue;
        }
        let valued = match item {
            Item::Settled(_) => true,
            Item::Pending { source, branch, .. } => match sources[source].terms() {
                None => executed.contains(source),
                Some(terms) => match (version, branch) {
                    (Version::V4 { .. }, Branch::Pay) => activated.contains(source),
                    (Version::V4 { .. }, Branch::Refund) => terms
                        .expiry()
                        .checked_add(terms.depth())
                        .is_some_and(|settled| judged >= settled),
                    (Version::V3, _) => true,
                },
            },
        };
        if valued {
            live += u128::from(item.message().denomination());
        }
    }
    u32::try_from(live.min(u128::from(claim.amount()))).expect("bounded by amount")
}

/// The public evidence that a claim's items exist, filed with P under the
/// hub-proves model, so that the Hub, not the holder, can evaluate
/// eligibility.
#[derive(Clone)]
pub struct Witness {
    /// The issued note of each settled item, in item order.
    pub notes: Vec<Note>,
    /// The body of each claimed source, in P's (source-id) order.
    pub sources: Vec<SourceWitness>,
    /// The evidence of each advertised activation, in P's order.
    pub activations: Vec<ActivationWitness>,
}

/// An advertised activation's public evidence: its transaction's inclusion
/// and, under a v4 Base, the lock checkpoint it is anchored to (the one the
/// lock's terms name), which must be a block of the chain the statement
/// reads. Under v3 the lock's checkpoint must be Base's, and none is filed.
#[derive(Clone)]
pub struct ActivationWitness {
    pub inclusion: Inclusion,
    pub anchor: Option<Checkpoint>,
}

/// A claim as filed on Bitcoin under the hub-proves model: P and its public
/// witness W, `LICASH-F\x01 ‖ P ‖ W`, which the holder's publication
/// carries after R (Lichen's claim slot, `lichen_statement::claim_script`).
/// Its activations anchored, `LICASH-F\x02 ‖ P ‖ W`, each activation's
/// inclusion followed by its anchor.
///
/// **Recovery authority.** There is no ownership check: an item is
/// designated by its hold key Q signing this claim's context (P's
/// envelope), and that is the whole of the authority to recover it. So a
/// note can be claimed through any contract, not only one whose Base's spend
/// root derives its owner key, and the claim names no root. Whoever holds a
/// note's Q secret can recover it: the recipient, or anyone given its hold
/// root (exporting it delegates recovery as well as holds). A payer can't: it
/// knows the outputs it made, not their Q secrets. A Q that signs a second
/// claim, anywhere, voids its note in both ([`Evidence::SecondClaim`]).
///
/// **Privacy.** W is public. Beyond P (each item's public fields, its
/// sources and blindings), it shows each settled note's issuer signature and
/// evidence, each source's body (the payer's proposal, owner signatures and
/// input notes, or the issuer's certificate) and each activation's
/// inclusion: the Hub knew them all, and an observer learns the in-flight
/// payments a claim rests on. No root key, spend root or ownership seed is
/// published.
#[derive(Clone)]
pub struct Filing {
    publication: Publication,
    witness: Witness,
}

impl std::fmt::Debug for Filing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Filing(..)")
    }
}

impl Filing {
    /// P with its witness, checked to line up with it: the note of each
    /// settled item, the body of each source, an inclusion for each advertised
    /// activation. Whether they prove anything is [`owed`]'s to decide.
    pub fn new(publication: Publication, witness: Witness) -> Result<Self, Error> {
        let parts = publication.claim().parts();
        let settled: Vec<_> = parts
            .items
            .iter()
            .filter(|item| matches!(item, Item::Settled(_)))
            .map(Item::message)
            .collect();
        if witness.notes.len() != settled.len()
            || witness
                .notes
                .iter()
                .zip(settled)
                .any(|(note, message)| note.message() != message)
            || witness.sources.len() != parts.sources.len()
            || witness
                .sources
                .iter()
                .zip(&parts.sources)
                .any(|(w, source)| w.proposal().id() != source.id())
            || witness.activations.len() != parts.activations.len()
            || witness.activations.iter().any(|a| a.anchor.is_some())
                && witness.activations.iter().any(|a| a.anchor.is_none())
        {
            return Err(bad("the witness does not line up with P"));
        }
        Ok(Self {
            publication,
            witness,
        })
    }

    pub fn publication(&self) -> &Publication {
        &self.publication
    }
    pub fn witness(&self) -> &Witness {
        &self.witness
    }

    fn anchored(&self) -> bool {
        self.witness.activations.iter().any(|a| a.anchor.is_some())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let w = &self.witness;
        let magic = match self.anchored() {
            false => F_MAGIC,
            true => F_MAGIC_ANCHORED,
        };
        let mut bytes = magic.to_vec();
        put_blob(&mut bytes, &self.publication.to_bytes());
        put_count(&mut bytes, w.notes.len());
        for note in &w.notes {
            put_blob(&mut bytes, &note.to_bytes());
        }
        put_count(&mut bytes, w.sources.len());
        for source in &w.sources {
            match source {
                SourceWitness::Signed { signed, notes } => {
                    bytes.push(0);
                    put_count(&mut bytes, notes.len());
                    for note in notes {
                        put_blob(&mut bytes, &note.to_bytes());
                    }
                    put_blob(&mut bytes, &signed.to_bytes());
                }
                SourceWitness::Certified {
                    proposal,
                    signatures,
                } => {
                    bytes.push(1);
                    put_blob(&mut bytes, &proposal.commitment_bytes());
                    put_count(&mut bytes, signatures.len());
                    for sig in signatures {
                        bytes.extend(sig.0);
                    }
                }
            }
        }
        put_count(&mut bytes, w.activations.len());
        for ActivationWitness { inclusion, anchor } in &w.activations {
            put_blob(&mut bytes, &inclusion.transaction);
            bytes.extend(inclusion.index.to_be_bytes());
            put_count(&mut bytes, inclusion.siblings.len());
            for sibling in &inclusion.siblings {
                bytes.extend(sibling);
            }
            put_blob(&mut bytes, &inclusion.coinbase);
            put_count(&mut bytes, inclusion.coinbase_siblings.len());
            for sibling in &inclusion.coinbase_siblings {
                bytes.extend(sibling);
            }
            if let Some(anchor) = anchor {
                put_blob(&mut bytes, &anchor.to_bytes());
            }
        }
        bytes
    }

    /// Strict canonical decoding: P as [`Publication::from_bytes`] decodes
    /// it, every note verified under `keyset` and every signed source against
    /// its notes. The keyset and authorization limit are the opening's.
    pub fn from_bytes(keyset: &Keyset, limit: usize, bytes: &[u8]) -> Result<Self, Error> {
        let anchored = bytes.starts_with(F_MAGIC_ANCHORED);
        if bytes.len() > MAX_PUBLICATION_BYTES || !(anchored || bytes.starts_with(F_MAGIC)) {
            return Err(bad("filing framing"));
        }
        let mut r = Reader(&bytes[F_MAGIC.len()..]);
        let publication = Publication::from_bytes(keyset, limit, r.blob()?)?;
        let notes = |r: &mut Reader, limit| -> Result<Vec<Note>, Error> {
            (0..r.count(limit)?)
                .map(|_| Note::from_bytes(keyset, r.blob()?))
                .collect()
        };
        let settled = notes(&mut r, MAX_ITEMS)?;
        let mut sources = Vec::new();
        for _ in 0..r.count(MAX_ITEMS)? {
            sources.push(match r.byte()? {
                0 => {
                    let notes = notes(&mut r, limit)?;
                    let signed = SignedProposal::from_bytes(keyset, &notes, r.blob()?)?;
                    SourceWitness::Signed { signed, notes }
                }
                1 => SourceWitness::Certified {
                    proposal: Proposal::from_commitment_bytes(r.blob()?)?,
                    signatures: (0..r.count(limit)?)
                        .map(|_| r.array().map(Signature))
                        .collect::<Result<_, _>>()?,
                },
                _ => return Err(bad("filing source tag")),
            });
        }
        let siblings = |r: &mut Reader| -> Result<Vec<[u8; 32]>, Error> {
            (0..r.count(32)?).map(|_| r.array()).collect()
        };
        let mut activations = Vec::new();
        for _ in 0..r.count(MAX_ITEMS)? {
            let inclusion = Inclusion {
                transaction: r.blob()?.to_vec(),
                index: r.u32()?,
                siblings: siblings(&mut r)?,
                coinbase: r.blob()?.to_vec(),
                coinbase_siblings: siblings(&mut r)?,
            };
            let anchor = match anchored {
                true => Some(Checkpoint::from_bytes(r.blob()?)?),
                false => None,
            };
            activations.push(ActivationWitness { inclusion, anchor });
        }
        if !r.0.is_empty() {
            return Err(bad("trailing filing bytes"));
        }
        let filing = Self::new(
            publication,
            Witness {
                notes: settled,
                sources,
                activations,
            },
        )?;
        if filing.to_bytes() != bytes {
            return Err(bad("noncanonical filing"));
        }
        Ok(filing)
    }
}

/// Whether a decoded filing is eligible under this opening and request: an
/// error is the holder's, and leaves the claim owed nothing.
///
/// Under v3, the Pay outputs of a timed lock are eligible only under a Base
/// opened on the checkpoint that the lock's terms name: the statement reads
/// Bitcoin from Base's checkpoint, so under any other the Hub could never
/// show that the lock expired, and an expired, refunded lock would be paid
/// twice. Under v4 a lock may name any checkpoint on the chain the
/// statement reads, its activation anchored to it, and a Pay is owed only
/// with an activation ([`live`]).
fn eligible(filing: &Filing, base: &Base, request: &Request, chain: &Chain) -> Result<(), Error> {
    let publication = &filing.publication;
    publication.bind(base, request)?;
    let parts = base.parts();
    let claim = publication.claim();
    let sources = &claim.parts().sources;
    for (source, w) in sources.iter().zip(&filing.witness.sources) {
        w.authenticate(&parts.keyset, source)?;
    }
    let metadata: BTreeMap<_, _> = sources.iter().map(|s| (s.id(), s)).collect();
    let bodies: BTreeMap<_, _> = filing
        .witness
        .sources
        .iter()
        .map(|w| (w.proposal().id(), w.proposal()))
        .collect();
    let checkpoint = &parts.chain.checkpoint;
    let agreed = (checkpoint.profile(), checkpoint.commitment());
    for item in &claim.parts().items {
        let Item::Pending {
            message,
            source,
            branch,
            index,
            blinding,
            ..
        } = item
        else {
            continue;
        };
        let timed_elsewhere = metadata[source]
            .terms()
            .is_some_and(|t| (t.profile(), *t.checkpoint()) != agreed);
        if *branch == Branch::Pay && timed_elsewhere && base.version() == Version::V3 {
            return Err(bad("pay output of a lock timed on another checkpoint"));
        }
        let outputs = match branch {
            Branch::Pay => bodies[source].outputs(),
            Branch::Refund => bodies[source]
                .lock()
                .ok_or_else(|| bad("refund of an untimed source"))?
                .refund(),
        };
        let output = outputs
            .get(*index as usize)
            .ok_or_else(|| bad("claimed source output index"))?;
        if !opens(output, message, blinding)? {
            return Err(bad("pending item does not open its source output"));
        }
    }
    let max = parts.limits.max_transaction_bytes as usize;
    for (record, w) in claim
        .parts()
        .activations
        .iter()
        .zip(&filing.witness.activations)
    {
        let terms = metadata[&record.source]
            .terms()
            .expect("admitted source is timed");
        let anchor = match (base.version(), &w.anchor) {
            (Version::V3, None) => chain.checkpoint(),
            (Version::V4 { .. }, Some(anchor)) => anchor,
            _ => return Err(bad("activation anchor of another Base version")),
        };
        Activation {
            reference: record.reference,
            inclusion: w.inclusion.clone(),
        }
        .verify(chain, anchor, terms, max)?;
    }
    Ok(())
}

/// What the Hub's statement decides of a filed claim ([`owed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owed {
    /// What the claim is owed: the `b′` the Hub's response signs.
    pub owed: u32,
    /// Bit i: item i voided by the Hub's evidence.
    pub mask: u32,
    /// What the Hub's response must disclose ([`disclosures`]): nothing if
    /// every slot is empty.
    pub disclosures: Disclosures,
    /// Why the claim is owed nothing, if the holder filed it wrong.
    pub ineligible: Option<String>,
}

/// The Hub's statement under the hub-proves model: what the claim filed as
/// `filed` (a filing's bytes, [`Filing::to_bytes`]) is owed under request R.
///
/// A claim the holder got wrong is ineligible, and owed 0: bytes that don't
/// decode, a P for another opening or request, a note the issuer didn't
/// sign, a source body that doesn't authenticate or an output it doesn't
/// open, under v3 a Pay of a lock timed on another checkpoint, an activation
/// `chain` doesn't bury at its terms' depth (under v4, after the lock's
/// checkpoint, which must be a block of `chain`). Otherwise the claim is
/// owed what the Hub's evidence leaves live, capped at its amount: with no
/// evidence, its amount, less under v4 any lock's Pay it advertises no
/// activation of and any lock's Refund `chain` doesn't reach the expiry of
/// at depth.
///
/// An error is the Hub's, and it can't prove with these inputs: evidence
/// that doesn't verify or voids nothing, evidence against an ineligible
/// claim, or a chain not from Base's checkpoint. The statement fixes
/// `chain` (from Base's checkpoint through the block of the claim's
/// publication), so the Hub can't shorten it to bury an activation less,
/// and decides at signing what it proves later.
pub fn owed(
    base: &Base,
    request: &Request,
    filed: &[u8],
    evidence: &[Evidence],
    chain: &Chain,
) -> Result<Owed, Error> {
    let parts = base.parts();
    if chain.checkpoint().commitment() != parts.chain.checkpoint.commitment() {
        return Err(bad("the chain is not from Base's checkpoint"));
    }
    let limit = parts.limits.max_authorizations as usize;
    let filing = Filing::from_bytes(&parts.keyset, limit, filed)
        .and_then(|filing| eligible(&filing, base, request, chain).map(|()| filing));
    let filing = match filing {
        Ok(filing) => filing,
        Err(_) if !evidence.is_empty() => {
            return Err(bad("evidence against an ineligible claim"));
        }
        Err(e) => {
            return Ok(Owed {
                owed: 0,
                mask: 0,
                disclosures: Disclosures::empty(),
                ineligible: Some(e.to_string()),
            })
        }
    };
    let claim = filing.publication.claim();
    let (mask, executed) = void(base.version(), &parts.keyset, claim, evidence, Some(chain))?;
    Ok(Owed {
        owed: live(base.version(), claim, mask, &executed, chain.tip_height()),
        mask,
        disclosures: disclosures(evidence)?,
        ineligible: None,
    })
}
