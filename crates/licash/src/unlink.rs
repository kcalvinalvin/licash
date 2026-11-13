//! Unlinking: replacing a note's issuance-linked evidence with a DLEQ on
//! `(Y, C)` itself, in a blinded Chaum-Pedersen session with the issuer.
//!
//! At issuance the issuer draws a nonce `w` for the output and sends
//! `(R1, R2) = (wG, wB)` with the note ([`Nonce`]). The holder blinds them
//! ([`Session`]) and sends the blinded challenge `e` with the output's
//! unlink capability ([`Request`]). The issuer answers `s = w + e k_d`
//! ([`Nonce::answer`]), and `(e', s + alpha)` is the unlinked proof.
//!
//! Two answers under one nonce reveal `k_d`. The issuer therefore keeps what
//! it answered ([`Answered`]) in place of the nonce, durably before
//! releasing the answer, and repeats it only for the identical request. A
//! store restored from before an answer holds the nonce again: answering
//! from it is unsafe, and so is a nonce derived again ([`Nonce::derive`])
//! from a secret that outlived its answers. The holder keeps its session durably before sending
//! the request, so that a retry repeats the challenge; a session that is
//! lost leaves the note with its linked evidence and its value.

use serde::{Deserialize, Serialize};

use crate::{
    codec::Reader,
    crypto::{
        dleq_answer, dleq_blind, dleq_commit, dleq_unblind, random_scalar, scalar_from_hash,
        tagged_hash, DleqCommit, Point, ScalarBytes,
    },
    ec::Pt,
    note::{
        non_identity, unlink_commitment_of, Derivation, Evidence, IssuerKeys, Keyset, Message,
        MintResponse, Note,
    },
    spend::Output,
    Error,
};

const REQUEST_TAG: &[u8] = b"LICASH-UL\x02";
/// Tag, keyset, denomination, blinded point, capability, challenge.
pub const REQUEST_BYTES: usize = REQUEST_TAG.len() + 32 + 8 + 33 + 32 + 32;
/// The request and the answer.
pub const ANSWERED_BYTES: usize = REQUEST_BYTES + 32;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// The holder's request to unlink the note issued on `blinded`: the
/// output's unlink capability, whose commitment the output carries, and the
/// blinded challenge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Request {
    keyset: [u8; 32],
    denom: u64,
    blinded: Point,
    capability: [u8; 32],
    challenge: ScalarBytes,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnlinkRequest(..)")
    }
}

impl Request {
    pub fn keyset(&self) -> &[u8; 32] {
        &self.keyset
    }
    pub fn denomination(&self) -> u64 {
        self.denom
    }
    /// The issuance's blinded point `B`, which names its nonce.
    pub fn blinded(&self) -> Point {
        self.blinded
    }

    /// The unlink commitment of the request's capability: an issuer can
    /// match it to the output before it tells the requester anything.
    pub fn unlink_commitment(&self) -> [u8; 32] {
        unlink_commitment_of(&self.capability)
    }

    /// `LICASH-UL\x02 ‖ keyset ‖ denomination ‖ B ‖ capability ‖ e`.
    pub fn to_bytes(&self) -> [u8; REQUEST_BYTES] {
        let mut bytes = [0; REQUEST_BYTES];
        let mut at = 0;
        for part in [
            REQUEST_TAG,
            &self.keyset,
            &self.denom.to_be_bytes(),
            &self.blinded.0,
            &self.capability,
            &self.challenge.0,
        ] {
            bytes[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        bytes
    }

    /// Structural decoding: a valid blinded point and a nonzero challenge.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes, "unlink request", REQUEST_TAG)?;
        let request = Self {
            keyset: r.array()?,
            denom: r.u64()?,
            blinded: Point(r.array()?),
            capability: r.array()?,
            challenge: ScalarBytes(r.array()?),
        };
        r.finish()?;
        non_identity(&request.blinded)?;
        request.challenge.to_nonzero()?;
        Ok(request)
    }
}

impl Serialize for Request {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::crypto::serialize_fixed(&self.to_bytes(), s)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes = crate::crypto::deserialize_fixed::<D, REQUEST_BYTES>(d, "unlink request")?;
        Self::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

/// The issuer's answer `s = w + e k_d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer(ScalarBytes);

impl Answer {
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0 .0
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, Error> {
        ScalarBytes(bytes).to_scalar()?;
        Ok(Self(ScalarBytes(bytes)))
    }
}

/// The issuer's secret nonce for unlinking one issued output, and the
/// output it was drawn for. It answers one request, ever.
#[derive(Clone)]
pub struct Nonce {
    keyset: [u8; 32],
    denom: u64,
    blinded: Point,
    commitment: [u8; 32],
    w: ScalarBytes,
}

impl std::fmt::Debug for Nonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnlinkNonce(..)")
    }
}

impl Nonce {
    /// A fresh nonce for issuing `output` under `keyset`.
    pub fn new(keyset: &Keyset, output: &Output) -> Result<Self, Error> {
        Self::with(keyset, output, random_scalar())
    }

    /// The nonce for issuing `output` under `keyset` derived from `secret`.
    /// A secret must not outlive the issuer's record of what it answered:
    /// losing that record would re-arm the same nonce.
    pub fn derive(secret: &[u8; 32], keyset: &Keyset, output: &Output) -> Result<Self, Error> {
        let w = scalar_from_hash(&tagged_hash(
            "licash/unlink-nonce/v2",
            &[
                secret,
                keyset.id(),
                &output.denomination().to_be_bytes(),
                &output.blinded().0,
                output.unlink_commitment(),
            ],
        ));
        Self::with(keyset, output, w)
    }

    fn with(keyset: &Keyset, output: &Output, w: k256::Scalar) -> Result<Self, Error> {
        keyset.key(output.denomination())?;
        non_identity(&output.blinded())?;
        if w == k256::Scalar::ZERO {
            return Err(Error::InvalidScalar);
        }
        Ok(Self {
            keyset: *keyset.id(),
            denom: output.denomination(),
            blinded: output.blinded(),
            commitment: *output.unlink_commitment(),
            w: ScalarBytes::from_scalar(&w),
        })
    }

    pub fn keyset(&self) -> &[u8; 32] {
        &self.keyset
    }
    pub fn denomination(&self) -> u64 {
        self.denom
    }
    /// The issuance's blinded point `B`, which names this nonce.
    pub fn blinded(&self) -> Point {
        self.blinded
    }

    /// `(R1, R2) = (wG, wB)`, sent with the issued note.
    pub fn commit(&self) -> Result<DleqCommit, Error> {
        Ok(dleq_commit(
            &self.w.to_scalar()?,
            &non_identity(&self.blinded)?,
        ))
    }

    /// The linked issuance of this nonce's output under `keys`: its blind
    /// signature, and [`Nonce::commit`] for unlinking it later.
    pub fn issue(&self, keys: &IssuerKeys) -> Result<MintResponse, Error> {
        if keys.id() != &self.keyset {
            return Err(bad("issuance keyset mismatch"));
        }
        let proof = keys.prove_blinded(self.denom, self.blinded)?;
        Ok(MintResponse::new(proof, self.commit()?))
    }

    /// Answer `request` for this nonce's output, under `keys`. The caller
    /// must durably replace its stored nonce by the result before releasing
    /// the answer. On an error nothing was answered.
    pub fn answer(self, keys: &IssuerKeys, request: &Request) -> Result<Answered, Error> {
        if request.keyset != self.keyset || keys.id() != &self.keyset {
            return Err(bad("unlink keyset mismatch"));
        }
        if request.denom != self.denom || request.blinded != self.blinded {
            return Err(bad("unlink request for another issuance"));
        }
        if unlink_commitment_of(&request.capability) != self.commitment {
            return Err(bad("unlink capability does not match the output"));
        }
        let e = *request.challenge.to_nonzero()?;
        let s = dleq_answer(
            &self.w.to_scalar()?,
            &e,
            keys.denomination_secret(self.denom)?,
        );
        Ok(Answered {
            request: *request,
            answer: Answer(ScalarBytes::from_scalar(&s)),
        })
    }
}

/// What the issuer answered under a nonce, kept in its place.
#[derive(Clone, PartialEq, Eq)]
pub struct Answered {
    request: Request,
    answer: Answer,
}

impl std::fmt::Debug for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnlinkAnswered(..)")
    }
}

impl Answered {
    /// The issuance's blinded point `B`, which named the nonce.
    pub fn blinded(&self) -> Point {
        self.request.blinded
    }

    pub fn answer(&self) -> Answer {
        self.answer
    }

    /// `request ‖ answer`.
    pub fn to_bytes(&self) -> [u8; ANSWERED_BYTES] {
        let mut bytes = [0; ANSWERED_BYTES];
        bytes[..REQUEST_BYTES].copy_from_slice(&self.request.to_bytes());
        bytes[REQUEST_BYTES..].copy_from_slice(&self.answer.to_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != ANSWERED_BYTES {
            return Err(bad("unlink answer record length"));
        }
        let answer: [u8; 32] = bytes[REQUEST_BYTES..].try_into().expect("checked length");
        Ok(Self {
            request: Request::from_bytes(&bytes[..REQUEST_BYTES])?,
            answer: Answer::from_bytes(answer)?,
        })
    }

    /// The answer again for the identical request, and nothing for any
    /// other: a second challenge under the nonce would reveal the key.
    pub fn replay(&self, request: &Request) -> Result<Answer, Error> {
        if *request != self.request {
            return Err(bad("unlink nonce already answered another request"));
        }
        Ok(self.answer)
    }
}

/// The holder's side of one unlink session for one note.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    keyset: [u8; 32],
    message: Message,
    signature: Point,
    request: Request,
    /// `e'`, the unblinded challenge.
    challenge: ScalarBytes,
    alpha: ScalarBytes,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnlinkSession(..)")
    }
}

fn points(keyset: &Keyset, message: &Message, signature: &Point) -> Result<[Pt; 3], Error> {
    Ok([
        Pt::from_point(&keyset.key(message.denomination())?)?,
        Pt::from_point(&message.identifier()?)?,
        Pt::from_point(signature)?,
    ])
}

impl Session {
    /// Blind the issuer's commitment of `note`, an issuance-linked note under
    /// `keyset`, which `output` derived.
    pub fn start(keyset: &Keyset, note: &Note, output: &Derivation) -> Result<Self, Error> {
        if note.keyset_id() != keyset.id() {
            return Err(bad("note keyset mismatch"));
        }
        if note.message() != output.message() {
            return Err(bad("derivation of another note"));
        }
        let Evidence::Linked {
            blinding, commit, ..
        } = note.evidence()
        else {
            return Err(bad("only issuance-linked notes unlink"));
        };
        if blinding != output.blinding() {
            return Err(bad("derivation of another issuance"));
        }
        let [key, y, c] = points(keyset, note.message(), &note.signature())?;
        let alpha = random_scalar();
        let (e, unblinded) = dleq_blind(
            commit,
            &key,
            &y,
            &c,
            &blinding.to_scalar()?,
            &alpha,
            &random_scalar(),
        )?;
        let request = Request {
            keyset: *keyset.id(),
            denom: note.message().denomination(),
            blinded: output.blinded()?,
            capability: output.unlink_capability(),
            challenge: ScalarBytes::from_scalar(&e),
        };
        request.challenge.to_nonzero()?;
        Ok(Self {
            keyset: request.keyset,
            message: note.message().clone(),
            signature: note.signature(),
            request,
            challenge: ScalarBytes::from_scalar(&unblinded),
            alpha: ScalarBytes::from_scalar(&alpha),
        })
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    /// The note with unlinked evidence, if `answer` completes the session.
    pub fn finish(&self, keyset: &Keyset, answer: &Answer) -> Result<Note, Error> {
        if keyset.id() != &self.keyset {
            return Err(bad("note keyset mismatch"));
        }
        let [key, y, c] = points(keyset, &self.message, &self.signature)?;
        let dleq = dleq_unblind(
            &self.challenge.to_scalar()?,
            &answer.0.to_scalar()?,
            &self.alpha.to_scalar()?,
            &key,
            &y,
            &c,
        )?;
        Note::from_parts(
            keyset,
            self.keyset,
            self.message.clone(),
            self.signature,
            Evidence::Unlinked(dleq),
        )
    }
}
