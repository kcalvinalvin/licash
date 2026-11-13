//! Owner authorization and coin-producing grants.
//!
//! A grant is the proposal's ordinary blind issuance, sealed. Each packet is
//! `(k_d B, DLEQ)` XORed with a pad hashed from one 32-byte unlock `t`, and
//! the grant commits to `H(t)`. Before `t` is released the packets are
//! independent of the issuer's keys; releasing `t` is exactly one blind
//! issuance per packet. The payer cannot check a sealed packet, so whoever
//! relies on a released unlock (a recipient, or the counter that retires the
//! payer's inputs) checks the opening: `Grant::verify_unlock` opens every
//! packet.
//!
//! This module is stateless: no source is admitted or recorded spent here.

use k256::{ProjectivePoint, Scalar};

use crate::{
    crypto::{self, Dleq, DleqCommit, Point, ScalarBytes, Signature},
    ec::Pt,
    note::Evidence,
    note::{non_identity, Derivation, IssuerKeys, Keyset, Note},
    spend::{Proposal, ProposalId},
    Error,
};

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// A grant's unlock `t`: 32 random bytes, a hash preimage like a swap secret.
pub type Unlock = [u8; 32];

/// Sealed bytes of one packet: the blind signature (33) and its DLEQ (64).
pub const SEALED_BYTES: usize = 33 + 64;

/// `H(t)`, bound to the proposal so one unlock answers one grant.
pub fn unlock_hash(proposal: &ProposalId, unlock: &Unlock) -> [u8; 32] {
    crypto::tagged_hash("licash/grant-lock/v2", &[proposal.as_bytes(), unlock])
}

/// The pad of packet `index`. It commits the position and denomination, so no
/// two packets of any grant share pad bytes.
fn pad(proposal: &ProposalId, unlock: &Unlock, index: usize, denom: u64) -> [u8; SEALED_BYTES] {
    let mut out = [0; SEALED_BYTES];
    let index = u32::try_from(index)
        .expect("bounded packet index")
        .to_be_bytes();
    for (block, chunk) in out.chunks_mut(32).enumerate() {
        let block = crypto::tagged_hash(
            "licash/grant-pad/v2",
            &[
                proposal.as_bytes(),
                unlock,
                &index,
                &denom.to_be_bytes(),
                &[block as u8],
            ],
        );
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
    out
}

/// Untrusted public packet. Every field is committed by issuer and owners;
/// verification is mandatory after any mutation or external reconstruction.
/// `sealed` is opaque until the unlock is known.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Packet {
    pub denom: u64,
    pub blinded: Point,
    pub sealed: Vec<u8>,
    pub commit: DleqCommit,
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantPacket(..)")
    }
}

/// Public offer, not an admitted source or owner authorization. The issuer
/// signature covers the complete offer.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub keyset: [u8; 32],
    pub proposal: ProposalId,
    /// `unlock_hash(proposal, t)`.
    pub lock: [u8; 32],
    pub packets: Vec<Packet>,
    pub sig: Signature,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant(..)")
    }
}

impl Grant {
    fn body(&self) -> Result<Vec<u8>, Error> {
        let count =
            u32::try_from(self.packets.len()).map_err(|_| bad("grant packet count overflow"))?;
        if count == 0 {
            return Err(bad("empty grant"));
        }
        let mut bytes = b"LICASH-GR\x02".to_vec();
        bytes.extend(self.keyset);
        bytes.extend(self.proposal.as_bytes());
        bytes.extend(self.lock);
        bytes.extend(count.to_be_bytes());
        for packet in &self.packets {
            if packet.sealed.len() != SEALED_BYTES {
                return Err(bad("grant packet seal length"));
            }
            bytes.extend(packet.denom.to_be_bytes());
            bytes.extend(packet.blinded.0);
            bytes.extend(&packet.sealed);
            bytes.extend(packet.commit.r1.0);
            bytes.extend(packet.commit.r2.0);
        }
        Ok(bytes)
    }

    pub fn commitment(&self) -> Result<[u8; 32], Error> {
        let mut bytes = self.body()?;
        bytes.extend(self.sig.0);
        Ok(crypto::tagged_hash("licash/grant-commitment/v2", &[&bytes]))
    }

    /// Keyset must be authenticated by the caller. This verifies the public
    /// offer, not input issuance/ownership, spentness, or any funding policy.
    /// Sealed packets are not opened here: see `verify_unlock`.
    pub fn verify(&self, keyset: &Keyset, proposal: &Proposal) -> Result<(), Error> {
        if &self.keyset != keyset.id()
            || proposal.keyset_id() != keyset.id()
            || self.proposal != proposal.id()
            || proposal.lock().is_some()
            || self.packets.len() != proposal.outputs().len()
        {
            return Err(bad("grant proposal context"));
        }
        let digest = crypto::tagged_hash("licash/grant-offer/v2", &[&self.body()?]);
        crypto::schnorr_verify(keyset.issuer(), &digest, &self.sig)?;
        for (packet, output) in self.packets.iter().zip(proposal.outputs()) {
            if packet.denom != output.denomination() || packet.blinded != output.blinded() {
                return Err(bad("grant packet order or target"));
            }
            keyset.key(packet.denom)?;
            non_identity(&packet.blinded)?;
            non_identity(&packet.commit.r1)?;
            non_identity(&packet.commit.r2)?;
        }
        Ok(())
    }

    /// Open one packet: the blind signature `k_d B` and its DLEQ, checked
    /// under the plain denomination key.
    fn unseal(
        &self,
        keyset: &Keyset,
        index: usize,
        unlock: &Unlock,
    ) -> Result<(ProjectivePoint, Dleq), Error> {
        if unlock_hash(&self.proposal, unlock) != self.lock {
            return Err(bad("grant unlock mismatch"));
        }
        let packet = self
            .packets
            .get(index)
            .ok_or_else(|| bad("grant output index"))?;
        if packet.sealed.len() != SEALED_BYTES {
            return Err(bad("grant packet seal length"));
        }
        let mut open = pad(&self.proposal, unlock, index, packet.denom);
        for (byte, sealed) in open.iter_mut().zip(&packet.sealed) {
            *byte ^= sealed;
        }
        let blind_sig = Point(open[..33].try_into().expect("33 bytes"));
        let dleq = Dleq {
            e: ScalarBytes(open[33..65].try_into().expect("32 bytes")),
            s: ScalarBytes(open[65..].try_into().expect("32 bytes")),
        };
        let point = non_identity(&blind_sig)?;
        crypto::dleq_verify_blinded(
            &dleq,
            &Pt::from_point(&keyset.key(packet.denom)?)?,
            &Pt::from_point(&packet.blinded)?,
            &Pt::from_point(&blind_sig)?,
        )?;
        Ok((point, dleq))
    }

    /// The unlock opens every packet to a valid issuance. A counter citing a
    /// granted spend must pass this, so a grant that does not open can never
    /// retire its payer's inputs.
    pub fn verify_unlock(&self, keyset: &Keyset, unlock: &Unlock) -> Result<(), Error> {
        if &self.keyset != keyset.id() {
            return Err(bad("grant keyset"));
        }
        for index in 0..self.packets.len() {
            self.unseal(keyset, index, unlock)?;
        }
        Ok(())
    }

    /// This grant's unlock among published disclosure slots, by its hash.
    pub fn unlock_in(&self, slots: &[[u8; 32]]) -> Option<Unlock> {
        slots
            .iter()
            .copied()
            .find(|slot| unlock_hash(&self.proposal, slot) == self.lock)
    }

    fn open(
        &self,
        keyset: &Keyset,
        index: usize,
        output: &Derivation,
        unlock: &Unlock,
    ) -> Result<Note, Error> {
        let (blind_sig, dleq) = self.unseal(keyset, index, unlock)?;
        let packet = &self.packets[index];
        if packet.denom != output.message().denomination() || packet.blinded != output.blinded()? {
            return Err(bad("grant output derivation"));
        }
        let signature = crypto::unblind(
            &blind_sig,
            &output.blinding().to_scalar()?,
            &keyset.key(packet.denom)?.to_projective()?,
        );
        if signature == ProjectivePoint::IDENTITY {
            return Err(Error::InvalidPoint);
        }
        // An opened packet is an ordinary issuance: linked until unlinked.
        Note::from_parts(
            keyset,
            self.keyset,
            output.message().clone(),
            Point::from_projective(&signature),
            Evidence::Linked {
                dleq,
                blinding: *output.blinding(),
                commit: packet.commit,
            },
        )
    }
}

#[derive(Clone)]
struct PacketRecord {
    denom: u64,
    blinded: Point,
    sealed: Vec<u8>,
    unlink_nonce: Scalar,
}

impl PacketRecord {
    fn packet(&self) -> Packet {
        Packet {
            denom: self.denom,
            blinded: self.blinded,
            sealed: self.sealed.clone(),
            commit: crypto::dleq_commit(
                &self.unlink_nonce,
                &self
                    .blinded
                    .to_projective()
                    .expect("validated grant target"),
            ),
        }
    }
}

/// In-memory retained offer and secrets. Clones replay exactly, but do not
/// establish exclusive or durable authority. There is no unlink-answer API.
#[derive(Clone)]
pub struct PreparedGrant {
    keyset: [u8; 32],
    proposal: ProposalId,
    unlock: Unlock,
    records: Vec<PacketRecord>,
    sig: Signature,
}

impl std::fmt::Debug for PreparedGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedGrant(..)")
    }
}

impl PreparedGrant {
    /// Curve/signing primitive only. A service must separately authenticate
    /// inputs, authorize admission and retain durable intent before replying.
    pub fn new(keys: &IssuerKeys, proposal: &Proposal) -> Result<Self, Error> {
        let keyset = keys.keyset()?;
        if proposal.keyset_id() != keyset.id() || proposal.lock().is_some() {
            return Err(bad("unsupported grant proposal"));
        }
        let id = proposal.id();
        let unlock: Unlock = ScalarBytes::from_scalar(&crypto::random_scalar()).0;
        let mut records = Vec::new();
        for (index, output) in proposal.outputs().iter().enumerate() {
            let secret = keys.denomination_secret(output.denomination())?;
            let blinded = non_identity(&output.blinded())?;
            let blind_sig = crypto::blind_sign(secret, &blinded);
            let dleq = crypto::dleq_prove(secret, &blinded, &blind_sig);
            let mut sealed = Point::from_projective(&blind_sig).0.to_vec();
            sealed.extend(dleq.e.0);
            sealed.extend(dleq.s.0);
            for (byte, pad) in
                sealed
                    .iter_mut()
                    .zip(pad(&id, &unlock, index, output.denomination()))
            {
                *byte ^= pad;
            }
            records.push(PacketRecord {
                denom: output.denomination(),
                blinded: output.blinded(),
                sealed,
                unlink_nonce: crypto::random_scalar(),
            });
        }
        let mut offer = Grant {
            keyset: *keyset.id(),
            proposal: id,
            lock: unlock_hash(&id, &unlock),
            packets: records.iter().map(PacketRecord::packet).collect(),
            sig: Signature([0; 64]),
        };
        offer.sig = crypto::schnorr_sign(
            keys.issuer_secret(),
            &crypto::tagged_hash("licash/grant-offer/v2", &[&offer.body()?]),
        )?;
        Ok(Self {
            keyset: offer.keyset,
            proposal: offer.proposal,
            unlock,
            records,
            sig: offer.sig,
        })
    }

    pub fn offer(&self) -> Grant {
        Grant {
            keyset: self.keyset,
            proposal: self.proposal,
            lock: unlock_hash(&self.proposal, &self.unlock),
            packets: self.records.iter().map(PacketRecord::packet).collect(),
            sig: self.sig,
        }
    }

    /// Release only against verified owners' authorization of this exact offer.
    /// This establishes no timing, source recording, funding or durable release
    /// policy. A service must commit spent/admission state before releasing.
    pub fn release(
        &self,
        keyset: &Keyset,
        notes: &[Note],
        signed: &SignedProposal,
    ) -> Result<Unlock, Error> {
        signed.verify(keyset, notes)?;
        if !matches!(signed.authorization(), Authorization::Granted(grant) if **grant == self.offer())
        {
            return Err(bad("unlock release requires the exact authorized offer"));
        }
        Ok(self.unlock)
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Authorization {
    Plain,
    Granted(Box<Grant>),
}

impl std::fmt::Debug for Authorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authorization(..)")
    }
}

impl Authorization {
    fn digest(&self, keyset: &Keyset, proposal: &Proposal) -> Result<[u8; 32], Error> {
        let id = proposal.id();
        match self {
            Self::Plain => Ok(crypto::tagged_hash(
                "licash/authorize-plain/v2",
                &[id.as_bytes()],
            )),
            Self::Granted(grant) => {
                grant.verify(keyset, proposal)?;
                Ok(crypto::tagged_hash(
                    "licash/authorize-grant/v2",
                    &[id.as_bytes(), &grant.commitment()?],
                ))
            }
        }
    }
}

/// Signature-only internal primitive. Callers separately establish cash
/// issuance or the certified debtor-admission boundary; this returns no cash.
pub(crate) fn verify_owner_signatures(
    keyset: &Keyset,
    proposal: &Proposal,
    authorization: &Authorization,
    signatures: &[Signature],
) -> Result<(), Error> {
    if signatures.len() != proposal.inputs().len() {
        return Err(bad("owner signature count"));
    }
    let digest = authorization.digest(keyset, proposal)?;
    for (message, sig) in proposal.inputs().iter().zip(signatures) {
        crypto::schnorr_verify(message.owner(), &digest, sig)?;
    }
    Ok(())
}

/// Owner-authorized intent, not admitted/spent state or recovery allocation.
/// Every verification path checks both issuance and owner signatures anew.
#[derive(Clone)]
pub struct SignedProposal {
    proposal: Proposal,
    authorization: Authorization,
    signatures: Vec<Signature>,
}

impl std::fmt::Debug for SignedProposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedProposal(..)")
    }
}

impl SignedProposal {
    pub fn sign(
        keyset: &Keyset,
        proposal: Proposal,
        notes: &[Note],
        authorization: Authorization,
        owners: &[Scalar],
    ) -> Result<Self, Error> {
        proposal.verify(keyset, notes)?;
        if owners.len() != proposal.inputs().len() {
            return Err(bad("owner key count"));
        }
        let digest = authorization.digest(keyset, &proposal)?;
        let mut signatures = Vec::new();
        for (message, secret) in proposal.inputs().iter().zip(owners) {
            if *secret == Scalar::ZERO || &crypto::x_only_pub(secret) != message.owner() {
                return Err(bad("wrong proposal owner"));
            }
            signatures.push(crypto::schnorr_sign(secret, &digest)?);
        }
        Ok(Self {
            proposal,
            authorization,
            signatures,
        })
    }

    pub fn from_parts(
        keyset: &Keyset,
        proposal: Proposal,
        notes: &[Note],
        authorization: Authorization,
        signatures: Vec<Signature>,
    ) -> Result<Self, Error> {
        let signed = Self {
            proposal,
            authorization,
            signatures,
        };
        signed.verify(keyset, notes)?;
        Ok(signed)
    }

    pub fn proposal(&self) -> &Proposal {
        &self.proposal
    }
    pub fn authorization(&self) -> &Authorization {
        &self.authorization
    }
    pub fn signatures(&self) -> &[Signature] {
        &self.signatures
    }

    /// Expected keyset is authenticated context, never trusted from the offer.
    /// The Note evidence path is for third parties, not a private issuer API.
    pub fn verify(&self, keyset: &Keyset, notes: &[Note]) -> Result<Vec<Point>, Error> {
        let inputs = self.proposal.verify(keyset, notes)?;
        verify_owner_signatures(
            keyset,
            &self.proposal,
            &self.authorization,
            &self.signatures,
        )?;
        Ok(inputs)
    }

    pub fn open_granted_output(
        &self,
        keyset: &Keyset,
        notes: &[Note],
        index: usize,
        output: &Derivation,
        unlock: &Unlock,
    ) -> Result<Note, Error> {
        self.verify(keyset, notes)?;
        let Authorization::Granted(grant) = &self.authorization else {
            return Err(bad("plain proposal has no grant"));
        };
        let intended = self
            .proposal
            .outputs()
            .get(index)
            .ok_or_else(|| bad("grant output index"))?;
        if intended.unlink_commitment() != &output.unlink_commitment() {
            return Err(bad("grant unlink authority mismatch"));
        }
        grant.open(keyset, index, output, unlock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::HoldRoot;
    use crate::spend::Output;

    #[test]
    fn an_issuer_signature_does_not_replace_packet_equation_and_point_checks() {
        let keys = IssuerKeys::random(2).unwrap();
        let keyset = keys.keyset().unwrap();
        let spend = Point::from_projective(&crypto::generator_mul(&Scalar::from(17u64)));
        let hold = HoldRoot::from_seed(&[7; 32]).unwrap();
        let input = Derivation::derive(4, spend, hold.public(), &[23; 32], 0).unwrap();
        let proof = keys.prove_blinded(4, input.blinded().unwrap()).unwrap();
        let note = Note::from_parts(
            &keyset,
            *keyset.id(),
            input.message().clone(),
            proof.open_point(&keyset, &input).unwrap(),
            Evidence::Linked {
                dleq: proof.dleq(),
                blinding: *input.blinding(),
                commit: crypto::dleq_commit(
                    &Scalar::from(71u64),
                    &input.blinded().unwrap().to_projective().unwrap(),
                ),
            },
        )
        .unwrap();
        let output = Derivation::derive(4, spend, hold.public(), &[23; 32], 1).unwrap();
        let proposal = Proposal::new(
            &keyset,
            &[note],
            spend,
            vec![Output::from_derivation(&output).unwrap()],
            None,
        )
        .unwrap();
        let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
        let original = prepared.offer();
        original.verify_unlock(&keyset, &prepared.unlock).unwrap();
        // The payer cannot tell a sealed packet from garbage, but garbage
        // never opens, so it can never be cited against her.
        let mut garbage = original.clone();
        garbage.packets[0].sealed[40] ^= 1;
        garbage.sig = crypto::schnorr_sign(
            keys.issuer_secret(),
            &crypto::tagged_hash("licash/grant-offer/v2", &[&garbage.body().unwrap()]),
        )
        .unwrap();
        garbage.verify(&keyset, &proposal).unwrap();
        assert!(garbage.verify_unlock(&keyset, &prepared.unlock).is_err());
        let mut partial_attestation = original.clone();
        partial_attestation.sig = crypto::schnorr_sign(
            keys.issuer_secret(),
            &crypto::tagged_hash(
                "licash/grant",
                &[proposal.id().as_bytes(), &partial_attestation.lock],
            ),
        )
        .unwrap();
        assert!(partial_attestation.verify(&keyset, &proposal).is_err());
        // A valid issuer signature over a malformed or retargeted offer.
        for field in 0..5 {
            let mut grant = original.clone();
            match field {
                0 => grant.packets[0].commit.r1 = Point([0; 33]),
                1 => grant.packets[0].commit.r2 = Point([0xff; 33]),
                2 => grant.packets[0].denom = 2,
                3 => {
                    grant.packets[0].sealed.pop();
                    assert!(grant.body().is_err());
                    continue;
                }
                _ => grant.packets[0].blinded = spend,
            }
            grant.sig = crypto::schnorr_sign(
                keys.issuer_secret(),
                &crypto::tagged_hash("licash/grant-offer/v2", &[&grant.body().unwrap()]),
            )
            .unwrap();
            assert!(grant.verify(&keyset, &proposal).is_err(), "field {field}");
        }
    }
}
