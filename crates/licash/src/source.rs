//! Timed source metadata and issuer certification for counter-visible carriers.
//!
//! Public verification does not prove private source consistency, activation,
//! ownership of a claimed output, or a durable spent record. The holder matches
//! the actual authorized source separately. authenticate_source permits that
//! match without payer Note evidence only under the debtor's explicit monetary
//! admission signature. It never yields issuance evidence or input-hold rights.

use std::collections::BTreeMap;

use crate::{
    authorization::{verify_owner_signatures, Authorization, SignedProposal},
    crypto::{self, Point, Signature},
    note::{non_identity, IssuerKeys, Keyset, Note},
    spend::{Proposal, ProposalId, TimedTerms, TIMED_TERMS_BYTES},
    Error,
};

const TAG: &[u8] = b"LICASH-SRC\x02";
const FIXED_BYTES: usize = TAG.len() + 32 + 32 + 4 + TIMED_TERMS_BYTES + 64;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// Counter-visible metadata only: source inputs' private messages, blinding,
/// root secrets and issuer evidence are deliberately absent.
#[derive(Clone, PartialEq, Eq)]
pub struct Metadata {
    keyset: [u8; 32],
    source: ProposalId,
    inputs: Vec<Point>,
    terms: TimedTerms,
}

impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceMetadata(..)")
    }
}

impl Metadata {
    pub fn new(
        keyset: [u8; 32],
        source: ProposalId,
        inputs: Vec<Point>,
        terms: TimedTerms,
    ) -> Result<Self, Error> {
        input_root(&inputs)?;
        Ok(Self {
            keyset,
            source,
            inputs,
            terms,
        })
    }

    /// Private holder/native consistency check, including owner authorization.
    /// This returns sorted Ys without reordering the signed source itself.
    pub fn from_source(
        keyset: &Keyset,
        signed: &SignedProposal,
        notes: &[Note],
    ) -> Result<Self, Error> {
        let inputs = signed.verify(keyset, notes)?;
        Self::from_proposal(keyset, signed.proposal(), inputs)
    }

    fn from_proposal(
        keyset: &Keyset,
        proposal: &Proposal,
        mut inputs: Vec<Point>,
    ) -> Result<Self, Error> {
        let lock = proposal
            .lock()
            .ok_or_else(|| bad("timed certificate needs a timed source"))?;
        inputs.sort_unstable();
        Self::new(*keyset.id(), proposal.id(), inputs, lock.terms().clone())
    }

    pub fn keyset_id(&self) -> &[u8; 32] {
        &self.keyset
    }
    pub fn source_id(&self) -> ProposalId {
        self.source
    }
    pub fn inputs(&self) -> &[Point] {
        &self.inputs
    }
    pub fn terms(&self) -> &TimedTerms {
        &self.terms
    }

    /// Counted, unsigned claim-digest component. This is not a complete claim
    /// or proof of publication; every exact publication must carry signatures.
    pub fn unsigned_bytes(&self) -> Vec<u8> {
        let mut bytes = self.keyset.to_vec();
        bytes.extend(self.source.as_bytes());
        bytes.extend(
            u32::try_from(self.inputs.len())
                .expect("validated input count")
                .to_be_bytes(),
        );
        for input in &self.inputs {
            bytes.extend(input.0);
        }
        bytes.extend(self.terms.to_bytes());
        bytes
    }

    fn certificate_message(&self) -> Result<[u8; 32], Error> {
        Ok(crypto::tagged_hash(
            "licash/timed-lock-admission/v1",
            &[
                &self.keyset,
                self.source.as_bytes(),
                &input_root(&self.inputs)?,
                &self.terms.root(),
            ],
        ))
    }
}

/// The nonempty counted, strictly increasing Y root.
/// Denomination is bound inside Y, not appended as an uncommitted annotation.
pub fn input_root(inputs: &[Point]) -> Result<[u8; 32], Error> {
    if inputs.is_empty() || inputs.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(bad("noncanonical source inputs"));
    }
    let n = u32::try_from(inputs.len()).map_err(|_| bad("source input count overflow"))?;
    let mut bytes = n.to_be_bytes().to_vec();
    for input in inputs {
        non_identity(input)?;
        bytes.extend(input.0);
    }
    Ok(crypto::tagged_hash("licash/source-inputs/v2", &[&bytes]))
}

/// Issuer attestation of exact public input/lock metadata. In the target
/// protocol this commits the issuer to source admission. The signing primitive
/// below does not write spent state; a service must commit that state first.
/// The Schnorr issuer key is monetary admission authority, not a metadata-only
/// key: its signature can acknowledge capped Admitted pending liabilities
/// without denomination secrets. Protect it with equivalent custody. A cert
/// is the debtor's acknowledgment, never evidence that input coins were issued.
#[derive(Clone, PartialEq, Eq)]
pub struct Certificate {
    metadata: Metadata,
    sig: Signature,
}

impl std::fmt::Debug for Certificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceCertificate(..)")
    }
}

impl Certificate {
    /// Stateless signing only. Never use this as a replacement for recording
    /// the exact consumed input set and admission/replay policy durably.
    pub fn attest(
        keys: &IssuerKeys,
        signed: &SignedProposal,
        notes: &[Note],
    ) -> Result<Self, Error> {
        let metadata = Metadata::from_source(&keys.keyset()?, signed, notes)?;
        let sig = crypto::schnorr_sign(keys.issuer_secret(), &metadata.certificate_message()?)?;
        Ok(Self { metadata, sig })
    }

    pub fn from_parts(keyset: &Keyset, metadata: Metadata, sig: Signature) -> Result<Self, Error> {
        let certificate = Self { metadata, sig };
        certificate.verify(keyset)?;
        Ok(certificate)
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub fn signature(&self) -> Signature {
        self.sig
    }

    /// The expected keyset/issuer must be authenticated external context.
    /// This public check cannot establish source-body consistency by itself.
    pub fn verify(&self, keyset: &Keyset) -> Result<(), Error> {
        if self.metadata.keyset_id() != keyset.id() {
            return Err(bad("source certificate keyset"));
        }
        crypto::schnorr_verify(
            keyset.issuer(),
            &self.metadata.certificate_message()?,
            &self.sig,
        )
    }

    pub fn verify_source(
        &self,
        keyset: &Keyset,
        signed: &SignedProposal,
        notes: &[Note],
    ) -> Result<(), Error> {
        self.verify(keyset)?;
        if self.metadata != Metadata::from_source(keyset, signed, notes)? {
            return Err(bad("source certificate private-body mismatch"));
        }
        Ok(())
    }

    /// Match certified timed admission to the exact plain-authorized body,
    /// without disclosing payer C/issuance evidence (Linked includes blinding).
    /// The expected issuer must be the debtor or its monetary admission key.
    /// This does not construct a SignedProposal, Note, C or issuance Evidence.
    ///
    /// The certified holder/public paths must keep this same boundary: require
    /// matching metadata and owner signatures, then separately check the claimed
    /// output opening/ownership, preimage and advertised activation. This is not
    /// valid for OwnInputs, Plain cash validation, grants or release. It proves
    /// no durable spent state, coverage allocation or acceptability of terms.
    pub fn authenticate_source(
        &self,
        keyset: &Keyset,
        proposal: &Proposal,
        owner_signatures: &[Signature],
    ) -> Result<(), Error> {
        self.verify(keyset)?;
        proposal.verify_declared_context(keyset)?;
        verify_owner_signatures(keyset, proposal, &Authorization::Plain, owner_signatures)?;
        let inputs = proposal
            .inputs()
            .iter()
            .map(|message| message.identifier())
            .collect::<Result<Vec<_>, _>>()?;
        if self.metadata != Metadata::from_proposal(keyset, proposal, inputs)? {
            return Err(bad("source certificate authorized-body mismatch"));
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = TAG.to_vec();
        bytes.extend(self.metadata.unsigned_bytes());
        bytes.extend(self.sig.0);
        bytes
    }

    /// Bounded exact framing; no input-only certificate fallback.
    pub fn from_bytes(keyset: &Keyset, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < FIXED_BYTES || &bytes[..TAG.len()] != TAG {
            return Err(bad("source certificate framing"));
        }
        let at = TAG.len();
        let count_at = at + 64;
        let n = usize::try_from(u32::from_be_bytes(
            bytes[count_at..count_at + 4]
                .try_into()
                .expect("checked header"),
        ))
        .map_err(|_| bad("source input count overflow"))?;
        let point_bytes = n
            .checked_mul(33)
            .ok_or_else(|| bad("source input length overflow"))?;
        if n == 0 || FIXED_BYTES.checked_add(point_bytes) != Some(bytes.len()) {
            return Err(bad("source certificate length"));
        }
        let start = count_at + 4;
        let inputs: Vec<_> = bytes[start..start + point_bytes]
            .as_chunks::<33>()
            .0
            .iter()
            .map(|p| Point(*p))
            .collect();
        let metadata = Metadata::new(
            bytes[at..at + 32].try_into().expect("checked header"),
            ProposalId::from_bytes(bytes[at + 32..count_at].try_into().expect("checked header")),
            inputs,
            TimedTerms::from_bytes(
                &bytes[start + point_bytes..start + point_bytes + TIMED_TERMS_BYTES],
            )?,
        )?;
        Self::from_parts(
            keyset,
            metadata,
            Signature(
                bytes[bytes.len() - 64..]
                    .try_into()
                    .expect("checked signature length"),
            ),
        )
    }
}

/// A component for future E, not E itself or authorization to hold inputs.
/// Verify every exact signature, sort by source ID, deduplicate identical
/// metadata, and reject conflicting descriptions even when both are signed.
pub fn metadata_root(keyset: &Keyset, sources: &[Certificate]) -> Result<[u8; 32], Error> {
    let mut unique = BTreeMap::new();
    for source in sources {
        source.verify(keyset)?;
        let metadata = source.metadata();
        if let Some(previous) = unique.insert(metadata.source, metadata) {
            if previous != metadata {
                return Err(bad("conflicting certified source metadata"));
            }
        }
    }
    let n = u32::try_from(unique.len()).map_err(|_| bad("source count overflow"))?;
    let mut bytes = n.to_be_bytes().to_vec();
    for metadata in unique.into_values() {
        bytes.extend(metadata.unsigned_bytes());
    }
    Ok(crypto::tagged_hash("licash/claim-sources/v2", &[&bytes]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend::ChainProfile;
    use k256::Scalar;

    fn metadata(keys: &IssuerKeys) -> Metadata {
        let y = Point::from_projective(&crypto::generator_mul(&Scalar::from(17u64)));
        Metadata::new(
            *keys.keyset().unwrap().id(),
            ProposalId::from_bytes([37; 32]),
            vec![y],
            TimedTerms::new(
                ChainProfile::Core30DefaultRegtest,
                [31; 32],
                1_000,
                3,
                [41; 32],
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn certify(keys: &IssuerKeys, metadata: Metadata) -> Certificate {
        let sig = crypto::schnorr_sign(
            keys.issuer_secret(),
            &metadata.certificate_message().unwrap(),
        )
        .unwrap();
        Certificate::from_parts(&keys.keyset().unwrap(), metadata, sig).unwrap()
    }

    #[test]
    fn valid_signature_variants_leave_unsigned_roots_unchanged_but_exact_bytes_differ() {
        let keys = IssuerKeys::random(2).unwrap();
        let keyset = keys.keyset().unwrap();
        let a = certify(&keys, metadata(&keys));
        let b = certify(&keys, metadata(&keys));
        assert_ne!(a.signature(), b.signature());
        assert_ne!(a.to_bytes(), b.to_bytes());
        assert_eq!(a.metadata().unsigned_bytes(), b.metadata().unsigned_bytes());
        assert_eq!(
            metadata_root(&keyset, std::slice::from_ref(&a)).unwrap(),
            metadata_root(&keyset, &[a.clone(), b]).unwrap()
        );
        assert_eq!(a.clone().to_bytes(), a.to_bytes());
    }

    #[test]
    fn even_signed_conflicting_sources_and_input_only_certificates_are_refused() {
        let keys = IssuerKeys::random(2).unwrap();
        let keyset = keys.keyset().unwrap();
        let original = metadata(&keys);
        let mut changed = original.clone();
        changed.terms = TimedTerms::new(
            ChainProfile::Core30DefaultRegtest,
            [31; 32],
            1_001,
            3,
            [41; 32],
        )
        .unwrap();
        let a = certify(&keys, original.clone());
        let b = certify(&keys, changed);
        a.verify(&keyset).unwrap();
        b.verify(&keyset).unwrap();
        assert!(metadata_root(&keyset, &[a, b]).is_err());
        let old_message = crypto::tagged_hash(
            "licash/lock-admission/v2",
            &[
                keyset.id(),
                original.source_id().as_bytes(),
                &input_root(original.inputs()).unwrap(),
            ],
        );
        let old_sig = crypto::schnorr_sign(keys.issuer_secret(), &old_message).unwrap();
        assert!(Certificate::from_parts(&keyset, original, old_sig).is_err());
    }
}
