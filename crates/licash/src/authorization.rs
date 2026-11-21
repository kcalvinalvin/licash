//! Owner authorization and coin-producing grants.
//!
//! A grant is the proposal's ordinary blind issuance, sealed under one
//! secret scalar `t`, the unlock, whose key `T = tG` the grant names. Each
//! packet is `E = k_d B + t U` with a proof of it ([`crypto::Seal`]), `U` a
//! point hashed from the proposal and the packet's position, of which
//! nobody knows a discrete logarithm. Before `t` is released a packet says
//! nothing of `k_d B`; releasing `t` is exactly one blind issuance per
//! packet.
//!
//! The payer verifies every packet before she signs ([`Grant::verify`],
//! which [`SignedProposal::sign`] does): what she authorizes is a grant
//! that opens, whole, under the secret of `T`. So whoever relies on a
//! released unlock (the counter that retires her inputs, a statement that
//! cites her spend) checks one equation, `tG = T` ([`Grant::verify_unlock`],
//! [`GrantHead::verify_unlock`]), whatever the number of packets, and a
//! recipient checks the packets it opens.
//!
//! This module is stateless: no source is admitted or recorded spent here.

use k256::{
    elliptic_curve::hash2curve::{ExpandMsgXmd, GroupDigest},
    ProjectivePoint, Scalar, Secp256k1,
};
use sha2::Sha256;

use crate::{
    codec::{put_blob, put_u32, Reader},
    crypto::{self, DleqCommit, Point, ScalarBytes, Seal, Signature, SEAL_BYTES},
    ec::Pt,
    note::Evidence,
    note::{non_identity, Derivation, IssuerKeys, Keyset, Note},
    spend::{Proposal, ProposalId},
    unlink, Error,
};

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// A grant's unlock `t`: a nonzero scalar, the secret of the key the grant
/// names ([`Grant::unlock_key`]).
pub type Unlock = [u8; 32];

const GRANT_TAG: &[u8] = b"LICASH-GR\x03";
const SIGNED_TAG: &[u8] = b"LICASH-SS\x02";
const MASK_DST: &[u8] = b"LICASH-GRANT-MASK-V1-secp256k1_XMD:SHA-256_SSWU_RO_";
/// A packet's denomination, target, seal, its proof and unlink commitment.
const PACKET_BYTES: usize = 8 + 33 + 33 + SEAL_BYTES + 33 + 33;

/// The mask base `U` of packet `index` of the grant of `proposal`: hashed
/// to the curve, so that nobody knows how it relates to the generator, to
/// a blinded point or to another packet's.
fn mask(proposal: &ProposalId, index: usize) -> Result<ProjectivePoint, Error> {
    let index = u32::try_from(index)
        .map_err(|_| bad("grant packet index"))?
        .to_be_bytes();
    let point = Secp256k1::hash_from_bytes::<ExpandMsgXmd<Sha256>>(
        &[proposal.as_bytes(), &index],
        &[MASK_DST],
    )
    .map_err(|_| bad("grant mask hash-to-curve"))?;
    if point == ProjectivePoint::IDENTITY {
        return Err(Error::InvalidPoint);
    }
    Ok(point)
}

/// The key of an unlock, `tG`, if it is a nonzero scalar.
fn unlock_key(unlock: &Unlock) -> Result<Point, Error> {
    let t = ScalarBytes(*unlock).to_nonzero()?;
    Ok(Point::from_projective(&crypto::generator_mul(&t)))
}

/// Untrusted public packet. Every field is committed by the owners;
/// verification is mandatory after any mutation or external reconstruction.
/// `sealed` is `k_d B + t U`, which `seal` proves.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Packet {
    pub denom: u64,
    pub blinded: Point,
    pub sealed: Point,
    pub seal: Seal,
    pub commit: DleqCommit,
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantPacket(..)")
    }
}

/// Public offer, not an admitted source or owner authorization. Nothing
/// signs it: each packet's proof is of knowledge of its denomination's key,
/// which only the issuer has.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub keyset: [u8; 32],
    pub proposal: ProposalId,
    /// `T = tG`, the key of the unlock.
    pub unlock_key: Point,
    pub packets: Vec<Packet>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant(..)")
    }
}

/// What the owners' signatures say of the grant they authorize, in the
/// fewest bytes: the key of its unlock, and a hash of all of it. Whoever
/// holds a signature over it and the secret of `unlock_key` knows that the
/// grant its signer verified is open, without reading a packet
/// ([`crate::recovery::Evidence::Signed`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GrantHead {
    pub unlock_key: Point,
    /// The hash of the grant's bytes.
    pub rest: [u8; 32],
}

impl std::fmt::Debug for GrantHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantHead(..)")
    }
}

/// A [`GrantHead`]'s bytes: its key and its hash.
pub const GRANT_HEAD_BYTES: usize = 33 + 32;

impl GrantHead {
    pub fn to_bytes(&self) -> [u8; GRANT_HEAD_BYTES] {
        let mut bytes = [0; GRANT_HEAD_BYTES];
        bytes[..33].copy_from_slice(&self.unlock_key.0);
        bytes[33..].copy_from_slice(&self.rest);
        bytes
    }

    pub fn from_bytes(bytes: &[u8; GRANT_HEAD_BYTES]) -> Self {
        Self {
            unlock_key: Point(bytes[..33].try_into().expect("33 bytes")),
            rest: bytes[33..].try_into().expect("32 bytes"),
        }
    }

    /// What the owners sign with the proposal's id ([`granted_digest`]).
    pub fn commitment(&self) -> [u8; 32] {
        crypto::tagged_hash("licash/grant-commitment/v3", &[&self.to_bytes()])
    }

    /// `unlock` is the grant's: a nonzero scalar, the secret of its key.
    pub fn verify_unlock(&self, unlock: &Unlock) -> Result<(), Error> {
        if unlock_key(unlock)? != self.unlock_key {
            return Err(bad("grant unlock mismatch"));
        }
        Ok(())
    }
}

/// What the owners of the inputs of the proposal `id` sign to authorize it
/// plainly, with no grant.
pub fn plain_digest(id: &ProposalId) -> [u8; 32] {
    crypto::tagged_hash("licash/authorize-plain/v2", &[id.as_bytes()])
}

/// What the owners of the inputs of the proposal `id` sign to authorize it
/// with the grant of head `head`.
pub fn granted_digest(id: &ProposalId, head: &GrantHead) -> [u8; 32] {
    crypto::tagged_hash(
        "licash/authorize-grant/v3",
        &[id.as_bytes(), &head.commitment()],
    )
}

impl Grant {
    /// The bytes the owners commit to ([`Grant::head`]): the keyset, the
    /// proposal, the unlock's key and every packet.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let count =
            u32::try_from(self.packets.len()).map_err(|_| bad("grant packet count overflow"))?;
        if count == 0 {
            return Err(bad("empty grant"));
        }
        let mut bytes = GRANT_TAG.to_vec();
        bytes.extend(self.keyset);
        bytes.extend(self.proposal.as_bytes());
        bytes.extend(self.unlock_key.0);
        bytes.extend(count.to_be_bytes());
        for packet in &self.packets {
            bytes.extend(packet.denom.to_be_bytes());
            bytes.extend(packet.blinded.0);
            bytes.extend(packet.sealed.0);
            bytes.extend(packet.seal.to_bytes());
            bytes.extend(packet.commit.r1.0);
            bytes.extend(packet.commit.r2.0);
        }
        Ok(bytes)
    }

    /// What the owners sign of it ([`GrantHead`]).
    pub fn head(&self) -> Result<GrantHead, Error> {
        Ok(GrantHead {
            unlock_key: self.unlock_key,
            rest: crypto::tagged_hash("licash/grant/v3", &[&self.to_bytes()?]),
        })
    }

    pub fn commitment(&self) -> Result<[u8; 32], Error> {
        Ok(self.head()?.commitment())
    }

    /// Structural decoding of `to_bytes`. Nothing is verified: call
    /// `verify` with the expected keyset and the proposal.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes, "grant", GRANT_TAG)?;
        let keyset = r.array()?;
        let proposal = ProposalId::from_bytes(r.array()?);
        let unlock_key = Point(r.array()?);
        let count = r.count(PACKET_BYTES)?;
        if count == 0 {
            return Err(bad("empty grant"));
        }
        let mut packets = Vec::with_capacity(count);
        for _ in 0..count {
            packets.push(Packet {
                denom: r.u64()?,
                blinded: Point(r.array()?),
                sealed: Point(r.array()?),
                seal: Seal::from_bytes(&r.array()?),
                commit: DleqCommit {
                    r1: Point(r.array()?),
                    r2: Point(r.array()?),
                },
            });
        }
        r.finish()?;
        Ok(Self {
            keyset,
            proposal,
            unlock_key,
            packets,
        })
    }

    /// The grant is of `proposal` under `keyset`, a packet for each of its
    /// outputs in order: what is read of it without a curve operation.
    /// Nothing of a packet is verified ([`Grant::verify`]).
    fn names(&self, keyset: &Keyset, proposal: &Proposal) -> Result<(), Error> {
        if &self.keyset != keyset.id()
            || proposal.keyset_id() != keyset.id()
            || self.proposal != proposal.id()
            || proposal.lock().is_some()
            || self.packets.len() != proposal.outputs().len()
        {
            return Err(bad("grant proposal context"));
        }
        for (packet, output) in self.packets.iter().zip(proposal.outputs()) {
            if packet.denom != output.denomination() || packet.blinded != output.blinded() {
                return Err(bad("grant packet order or target"));
            }
        }
        Ok(())
    }

    /// What a payer checks before she signs, and all she need check: the
    /// grant is of `proposal`, and every packet is the issuance its output
    /// asks for, sealed under the secret of [`Grant::unlock_key`]. Whoever
    /// releases that secret then releases every output, and no other
    /// secret is the grant's. Keyset must be authenticated by the caller.
    /// This verifies the public offer, not input issuance/ownership,
    /// spentness, or any funding policy.
    pub fn verify(&self, keyset: &Keyset, proposal: &Proposal) -> Result<(), Error> {
        self.names(keyset, proposal)?;
        non_identity(&self.unlock_key)?;
        let unlock_key = Pt::from_point(&self.unlock_key)?;
        for (index, packet) in self.packets.iter().enumerate() {
            non_identity(&packet.blinded)?;
            non_identity(&packet.sealed)?;
            non_identity(&packet.commit.r1)?;
            non_identity(&packet.commit.r2)?;
            crypto::seal_verify(
                &packet.seal,
                &crypto::Sealing {
                    issuer_pub: &Pt::from_point(&keyset.key(packet.denom)?)?,
                    unlock_key: &unlock_key,
                    blinded: &Pt::from_point(&packet.blinded)?,
                    mask: &Pt::from_projective(&mask(&self.proposal, index)?),
                    sealed: &Pt::from_point(&packet.sealed)?,
                    bound: &packet.commit.to_bytes(),
                },
            )?;
        }
        Ok(())
    }

    /// `unlock` is this grant's: the secret of its key. Every packet a
    /// payer verified opens under it ([`Grant::verify`]), so a counter
    /// citing a granted spend checks this and no packet.
    pub fn verify_unlock(&self, keyset: &Keyset, unlock: &Unlock) -> Result<(), Error> {
        if &self.keyset != keyset.id() {
            return Err(bad("grant keyset"));
        }
        if unlock_key(unlock)? != self.unlock_key {
            return Err(bad("grant unlock mismatch"));
        }
        Ok(())
    }

    /// `unlock` may be released: the owners of `notes` signed `signed`, which
    /// authorizes exactly this offer, and `unlock` opens it. The same
    /// limits as [`PreparedGrant::release`] apply.
    pub fn verify_release(
        &self,
        keyset: &Keyset,
        notes: &[Note],
        signed: &SignedProposal,
        unlock: &Unlock,
    ) -> Result<(), Error> {
        signed.verify(keyset, notes)?;
        if !matches!(signed.authorization(), Authorization::Granted(grant) if **grant == *self) {
            return Err(bad("unlock release requires the exact authorized offer"));
        }
        self.verify_unlock(keyset, unlock)
    }

    /// This grant's unlock among published disclosure slots: the one that
    /// is the secret of its key. A slot that is no scalar is another's.
    pub fn unlock_in(&self, slots: &[[u8; 32]]) -> Option<Unlock> {
        slots
            .iter()
            .copied()
            .find(|slot| unlock_key(slot).is_ok_and(|key| key == self.unlock_key))
    }

    /// Open one packet into its note, verified: the packet's proof, under
    /// the key of `unlock`, is the note's evidence.
    fn open(
        &self,
        keyset: &Keyset,
        index: usize,
        output: &Derivation,
        unlock: &Unlock,
    ) -> Result<Note, Error> {
        self.verify_unlock(keyset, unlock)?;
        let packet = self
            .packets
            .get(index)
            .ok_or_else(|| bad("grant output index"))?;
        if packet.denom != output.message().denomination() || packet.blinded != output.blinded()? {
            return Err(bad("grant output derivation"));
        }
        let t = ScalarBytes(*unlock).to_scalar()?;
        let mask = mask(&self.proposal, index)?;
        let signature = crypto::unblind(
            &(non_identity(&packet.sealed)? - mask * t),
            &output.blinding().to_scalar()?,
            &keyset.key(packet.denom)?.to_projective()?,
        );
        if signature == ProjectivePoint::IDENTITY {
            return Err(Error::InvalidPoint);
        }
        // An opened packet is an issuance its holder can show anyone:
        // linked to the grant until unlinked.
        Note::from_parts(
            keyset,
            self.keyset,
            output.message().clone(),
            Point::from_projective(&signature),
            Evidence::Granted {
                seal: packet.seal,
                blinding: *output.blinding(),
                commit: packet.commit,
                unlock: ScalarBytes(*unlock),
                mask: Point::from_projective(&mask),
            },
        )
    }
}

#[derive(Clone)]
struct PacketRecord {
    sealed: Point,
    seal: Seal,
    nonce: unlink::Nonce,
}

impl PacketRecord {
    fn packet(&self) -> Packet {
        Packet {
            denom: self.nonce.denomination(),
            blinded: self.nonce.blinded(),
            sealed: self.sealed,
            seal: self.seal,
            commit: self.nonce.commit().expect("validated grant target"),
        }
    }
}

/// In-memory retained offer and secrets. Clones replay exactly, but do not
/// establish exclusive or durable authority.
#[derive(Clone)]
pub struct PreparedGrant {
    keyset: [u8; 32],
    proposal: ProposalId,
    unlock: Unlock,
    records: Vec<PacketRecord>,
}

impl std::fmt::Debug for PreparedGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedGrant(..)")
    }
}

impl PreparedGrant {
    /// Curve primitive only. A service must separately authenticate
    /// inputs, authorize admission and retain durable intent before replying.
    pub fn new(keys: &IssuerKeys, proposal: &Proposal) -> Result<Self, Error> {
        let unlock: Unlock = ScalarBytes::from_scalar(&crypto::random_scalar()).0;
        let nonce_secret = ScalarBytes::from_scalar(&crypto::random_scalar()).0;
        Self::derive(keys, proposal, unlock, &nonce_secret)
    }

    /// The grant for `proposal` under issuer-chosen secrets, so an issuer
    /// that keeps the offer it sent can derive its unlock and unlink nonces
    /// again instead of storing them. `unlock` must be a nonzero scalar,
    /// unpredictable to everyone else and of this grant alone; the nonces
    /// follow [`unlink::Nonce::derive`]. Each packet's proof is drawn
    /// afresh: two derivations seal alike and prove differently.
    pub fn derive(
        keys: &IssuerKeys,
        proposal: &Proposal,
        unlock: Unlock,
        nonce_secret: &[u8; 32],
    ) -> Result<Self, Error> {
        let keyset = keys.keyset()?;
        if proposal.keyset_id() != keyset.id() || proposal.lock().is_some() {
            return Err(bad("unsupported grant proposal"));
        }
        let t: Scalar = *ScalarBytes(unlock).to_nonzero()?;
        let id = proposal.id();
        let mut records = Vec::new();
        for (index, output) in proposal.outputs().iter().enumerate() {
            let secret = keys.denomination_secret(output.denomination())?;
            let blinded = non_identity(&output.blinded())?;
            let nonce = unlink::Nonce::derive(nonce_secret, &keyset, output)?;
            let commit = nonce.commit()?;
            let (sealed, seal) =
                crypto::seal_prove(secret, &t, &blinded, &mask(&id, index)?, &commit.to_bytes());
            if sealed == ProjectivePoint::IDENTITY {
                return Err(Error::InvalidPoint);
            }
            records.push(PacketRecord {
                sealed: Point::from_projective(&sealed),
                seal,
                nonce,
            });
        }
        Ok(Self {
            keyset: *keyset.id(),
            proposal: id,
            unlock,
            records,
        })
    }

    /// The unlink nonce of each packet, in order, for the issuer to keep
    /// once the grant is released ([`unlink::Nonce`]). Each answers one
    /// request, so take them into the issuer's store once.
    pub fn unlink_nonces(&self) -> Vec<unlink::Nonce> {
        self.records.iter().map(|r| r.nonce.clone()).collect()
    }

    pub fn offer(&self) -> Grant {
        Grant {
            keyset: self.keyset,
            proposal: self.proposal,
            unlock_key: unlock_key(&self.unlock).expect("a nonzero scalar"),
            packets: self.records.iter().map(PacketRecord::packet).collect(),
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
        self.offer()
            .verify_release(keyset, notes, signed, &self.unlock)?;
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
    /// What the owners of `proposal`'s inputs sign. A grant is of the
    /// proposal, a packet for each output in order ([`Grant::names`]);
    /// whether its packets are what they say is for whoever signs to have
    /// verified ([`SignedProposal::sign`]) and for whoever opens one.
    fn digest(&self, keyset: &Keyset, proposal: &Proposal) -> Result<[u8; 32], Error> {
        let id = proposal.id();
        match self {
            Self::Plain => Ok(plain_digest(&id)),
            Self::Granted(grant) => {
                grant.names(keyset, proposal)?;
                Ok(granted_digest(&id, &grant.head()?))
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
/// Of a grant it checks what the owners signed, not its packets: they
/// verified every one before signing ([`SignedProposal::sign`]), and their
/// word is what a third party takes.
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
        // Her signature is her word that the grant opens, whole, under the
        // secret of its key: nobody checks it for her afterwards.
        if let Authorization::Granted(grant) = &authorization {
            grant.verify(keyset, &proposal)?;
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

    /// `LICASH-SS\x02 ‖ proposal ‖ authorization ‖ signatures`: the
    /// proposal's commitment bytes and the grant's bytes length-prefixed,
    /// the authorization a tag (0 Plain, 1 Granted).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = SIGNED_TAG.to_vec();
        put_blob(&mut bytes, &self.proposal.commitment_bytes()).expect("bounded proposal");
        match &self.authorization {
            Authorization::Plain => bytes.push(0),
            Authorization::Granted(grant) => {
                bytes.push(1);
                // Signing and decoding verified the grant, so it encodes.
                let grant = grant.to_bytes().expect("verified grant");
                put_blob(&mut bytes, &grant).expect("bounded grant");
            }
        }
        put_u32(&mut bytes, self.signatures.len()).expect("bounded signatures");
        for sig in &self.signatures {
            bytes.extend(sig.0);
        }
        bytes
    }

    /// Decode `to_bytes` and verify it, as `from_parts` does, against the
    /// expected keyset and the notes it spends.
    pub fn from_bytes(keyset: &Keyset, notes: &[Note], bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes, "signed proposal", SIGNED_TAG)?;
        let proposal = Proposal::from_commitment_bytes(r.blob()?)?;
        let authorization = match r.byte()? {
            0 => Authorization::Plain,
            1 => Authorization::Granted(Box::new(Grant::from_bytes(r.blob()?)?)),
            _ => return Err(bad("unknown authorization")),
        };
        let count = r.count(64)?;
        let signatures = (0..count)
            .map(|_| r.array().map(Signature))
            .collect::<Result<_, _>>()?;
        r.finish()?;
        Self::from_parts(keyset, proposal, notes, authorization, signatures)
    }

    /// The owner signatures of a Plain proposal's `to_bytes`, unverified:
    /// for whoever holds its certificate to check them against it
    /// ([`crate::source::Certificate::authenticate_source`]) without the
    /// notes it spends. Empty for a Granted one.
    pub fn plain_signatures(bytes: &[u8]) -> Result<Vec<Signature>, Error> {
        let mut r = Reader::new(bytes, "signed proposal", SIGNED_TAG)?;
        r.blob()?;
        if r.byte()? != 0 {
            return Ok(vec![]);
        }
        let count = r.count(64)?;
        let signatures = (0..count)
            .map(|_| r.array().map(Signature))
            .collect::<Result<_, _>>()?;
        r.finish()?;
        Ok(signatures)
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

    /// The identifiers of the inputs this spend consumes, of `notes` under
    /// `keyset`, without verifying it again: whatever made it verified it,
    /// its input notes and its owners' signatures ([`Self::sign`],
    /// [`Self::from_parts`], [`Self::from_bytes`]), under the keyset its
    /// proposal names, so this checks only that `keyset` is that one and
    /// `notes` are of its inputs, in order.
    pub fn inputs_of(&self, keyset: &Keyset, notes: &[Note]) -> Result<Vec<Point>, Error> {
        let inputs = self.proposal.inputs();
        if self.proposal.keyset_id() != keyset.id()
            || notes.len() != inputs.len()
            || notes
                .iter()
                .zip(inputs)
                .any(|(note, m)| note.message() != m)
        {
            return Err(bad("signed proposal input witness context"));
        }
        inputs
            .iter()
            .map(crate::note::Message::identifier)
            .collect()
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
    fn a_payer_refuses_a_packet_that_does_not_open_before_she_signs() {
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
            std::slice::from_ref(&note),
            spend,
            vec![Output::from_derivation(&output).unwrap()],
            None,
        )
        .unwrap();
        let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
        let original = prepared.offer();
        original.verify(&keyset, &proposal).unwrap();
        original.verify_unlock(&keyset, &prepared.unlock).unwrap();
        original
            .open(&keyset, 0, &output, &prepared.unlock)
            .unwrap()
            .verify(&keyset)
            .unwrap();
        let owner = hold_owner(&input);
        let sign = |grant: &Grant| {
            SignedProposal::sign(
                &keyset,
                proposal.clone(),
                std::slice::from_ref(&note),
                Authorization::Granted(Box::new(grant.clone())),
                &[owner],
            )
        };
        sign(&original).unwrap();
        // The payer tells a sealed packet from garbage before she signs: no
        // packet she authorizes fails to open under the unlock released.
        let mut garbage = original.clone();
        garbage.packets[0].sealed = Point::from_projective(
            &(garbage.packets[0].sealed.to_projective().unwrap() + ProjectivePoint::GENERATOR),
        );
        assert!(garbage.verify(&keyset, &proposal).is_err());
        assert!(sign(&garbage).is_err());
        // It still never opens, whoever is asked to.
        assert!(garbage.open(&keyset, 0, &output, &prepared.unlock).is_err());
        // A packet proven under another unlock is not this grant's.
        let other = PreparedGrant::new(&keys, &proposal).unwrap().offer();
        let mut mixed = original.clone();
        mixed.packets[0] = other.packets[0].clone();
        assert!(mixed.verify(&keyset, &proposal).is_err());
        let mut rekeyed = original.clone();
        rekeyed.unlock_key = other.unlock_key;
        assert!(rekeyed.verify(&keyset, &proposal).is_err());
        rekeyed.unlock_key = Point([0; 33]);
        assert!(rekeyed.verify(&keyset, &proposal).is_err());
        // Malformed or retargeted packets.
        for field in 0..6 {
            let mut grant = original.clone();
            match field {
                0 => grant.packets[0].commit.r1 = Point([0; 33]),
                1 => grant.packets[0].commit.r2 = Point([0xff; 33]),
                2 => grant.packets[0].denom = 2,
                3 => grant.packets[0].seal.s_k.0[31] ^= 1,
                4 => grant.packets[0].seal.e.0[31] ^= 1,
                _ => grant.packets[0].blinded = spend,
            }
            assert!(grant.verify(&keyset, &proposal).is_err(), "field {field}");
            assert!(sign(&grant).is_err(), "field {field}");
        }
    }

    /// The owner secret of a note derived from `derivation` under the
    /// spend key 17 G, as the fixture derives it.
    fn hold_owner(derivation: &Derivation) -> Scalar {
        derivation
            .ownership()
            .owner_secret(&Scalar::from(17u64))
            .unwrap()
    }
}
