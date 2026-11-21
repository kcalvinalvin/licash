//! Curve primitives: BDHKE blind signatures with DLEQ proofs,
//! BIP340 Schnorr signatures, and serializable point and scalar newtypes.
//!
//! Everything is secp256k1 over SHA-256. Domain strings are licash's own so
//! that notes are never valid Cashu proofs or vice versa.

use k256::elliptic_curve::group::GroupEncoding;
use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::elliptic_curve::PrimeField;
use k256::{AffinePoint, NonZeroScalar, ProjectivePoint, Scalar, U256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ec::{gen_mul, msm, Pt};
use crate::Error;

/// A compressed secp256k1 point (33 bytes), serialized as hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point(pub [u8; 33]);

impl Point {
    pub fn from_projective(p: &ProjectivePoint) -> Point {
        let mut out = [0u8; 33];
        out.copy_from_slice(p.to_affine().to_encoded_point(true).as_bytes());
        Point(out)
    }

    pub fn to_projective(&self) -> Result<ProjectivePoint, Error> {
        self.to_affine().map(ProjectivePoint::from)
    }

    /// Decode the canonical encodings only: compressed SEC1 (tag 2 or 3),
    /// or 33 zero bytes for the identity (as `ec::Pt::to_point` writes it).
    /// `k256` also decodes SEC1's compact form (tag 5), a second encoding
    /// of a point that byte comparisons and hashes would take for another.
    pub(crate) fn to_affine(self) -> Result<AffinePoint, Error> {
        if !matches!(self.0[0], 0x00 | 0x02 | 0x03) {
            return Err(Error::InvalidPoint);
        }
        let affine: Option<AffinePoint> = AffinePoint::from_bytes((&self.0).into()).into();
        affine.ok_or(Error::InvalidPoint)
    }

    /// The x-only key (BIP340) of this point.
    pub fn x_only(&self) -> [u8; 32] {
        let mut x = [0u8; 32];
        x.copy_from_slice(&self.0[1..]);
        x
    }
}

impl std::fmt::Debug for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Point({})", hex::encode(self.0))
    }
}

/// Fixed-size byte strings: hex in human-readable formats (JSON), raw
/// bytes in binary ones (bincode, the guest's input), where hex costs
/// cycles.
pub(crate) fn serialize_fixed<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    if s.is_human_readable() {
        s.serialize_str(&hex::encode(bytes))
    } else {
        s.serialize_bytes(bytes)
    }
}

pub(crate) fn deserialize_fixed<'de, D: serde::Deserializer<'de>, const N: usize>(
    d: D,
    what: &'static str,
) -> Result<[u8; N], D::Error> {
    let v: Vec<u8> = if d.is_human_readable() {
        let s = String::deserialize(d)?;
        // Its width first: a longer string is never decoded.
        if s.len() != 2 * N {
            return Err(serde::de::Error::custom(format!(
                "{what} must be {N} bytes"
            )));
        }
        hex::decode(s).map_err(serde::de::Error::custom)?
    } else {
        serde_bytes::ByteBuf::deserialize(d)?.into_vec()
    };
    v.try_into()
        .map_err(|_| serde::de::Error::custom(format!("{what} must be {N} bytes")))
}

impl Serialize for Point {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_fixed(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for Point {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_fixed::<D, 33>(d, "point").map(Point)
    }
}

/// A scalar in the group order, serialized as 32 big-endian bytes in hex.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScalarBytes(pub [u8; 32]);

impl ScalarBytes {
    pub fn from_scalar(s: &Scalar) -> ScalarBytes {
        ScalarBytes(s.to_bytes().into())
    }

    pub fn to_scalar(&self) -> Result<Scalar, Error> {
        let s: Option<Scalar> = Scalar::from_repr(k256::FieldBytes::from(self.0)).into();
        s.ok_or(Error::InvalidScalar)
    }

    pub fn to_nonzero(&self) -> Result<NonZeroScalar, Error> {
        let s = self.to_scalar()?;
        let nz: Option<NonZeroScalar> = NonZeroScalar::new(s).into();
        nz.ok_or(Error::InvalidScalar)
    }
}

impl std::fmt::Debug for ScalarBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scalar(..)")
    }
}

impl Serialize for ScalarBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_fixed(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for ScalarBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_fixed::<D, 32>(d, "scalar").map(ScalarBytes)
    }
}

/// SHA-256 of a domain tag followed by the given parts.
pub fn tagged_hash(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Reduce 32 bytes to a scalar (mod n).
pub fn scalar_from_hash(bytes: &[u8; 32]) -> Scalar {
    <Scalar as Reduce<U256>>::reduce_bytes(bytes.into())
}

/// Random nonzero scalar from the OS.
pub fn random_scalar() -> Scalar {
    let mut rng = rand_core::OsRng;
    *NonZeroScalar::random(&mut rng)
}

pub fn generator_mul(s: &Scalar) -> ProjectivePoint {
    ProjectivePoint::GENERATOR * s
}

// ---------------------------------------------------------------------------
// BDHKE (blind Diffie-Hellman key exchange) with DLEQ proofs
// ---------------------------------------------------------------------------

/// The issuer's blind signature `C_ = k B_`.
pub fn blind_sign(k: &Scalar, blinded: &ProjectivePoint) -> ProjectivePoint {
    *blinded * k
}

/// Unblind: `C = C_ - r K`.
pub fn unblind(
    blind_sig: &ProjectivePoint,
    r: &Scalar,
    issuer_pub: &ProjectivePoint,
) -> ProjectivePoint {
    *blind_sig - *issuer_pub * r
}

/// A proof that `C_ = k B_` for the `k` behind the issuer's public key `K = k G`.
/// Chaum-Pedersen as in Cashu NUT-12, but with the base in the challenge:
/// `e = H(R1 || R2 || K || B_ || C_)`, `s = r + e k` ([`dleq_challenge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dleq {
    pub e: ScalarBytes,
    pub s: ScalarBytes,
}

pub fn dleq_prove(k: &Scalar, blinded: &ProjectivePoint, blind_sig: &ProjectivePoint) -> Dleq {
    let r = random_scalar();
    let r1 = ProjectivePoint::GENERATOR * r;
    let r2 = *blinded * r;
    let issuer_pub = ProjectivePoint::GENERATOR * k;
    let e = dleq_challenge(
        &Pt::from_projective(&r1),
        &Pt::from_projective(&r2),
        &Pt::from_projective(&issuer_pub),
        &Pt::from_projective(blinded),
        &Pt::from_projective(blind_sig),
    );
    let s = r + e * k;
    Dleq {
        e: ScalarBytes::from_scalar(&e),
        s: ScalarBytes::from_scalar(&s),
    }
}

/// The challenge hashes the whole statement, `K = kG` and `sig = k base`,
/// with the commitments: `G` is fixed, but `base` varies from proof to
/// proof (`B_` at issuance, `Y` once unlinked). Left out, a prover who
/// knows `k` could pick the base after the challenge, and so the blinding
/// `r` of a linked proof, and prove a `C` that is not `kY`.
fn dleq_challenge(r1: &Pt, r2: &Pt, issuer_pub: &Pt, base: &Pt, blind_sig: &Pt) -> Scalar {
    let h = tagged_hash(
        "licash/dleq/v2",
        &[
            &r1.to_point().0,
            &r2.to_point().0,
            &issuer_pub.to_point().0,
            &base.to_point().0,
            &blind_sig.to_point().0,
        ],
    );
    scalar_from_hash(&h)
}

/// Verify a DLEQ on blinded values: `s G == R1 + e K`, `s B_ == R2 + e C_`.
/// Two two-term MSMs.
pub fn dleq_verify_blinded(
    proof: &Dleq,
    issuer_pub: &Pt,
    blinded: &Pt,
    blind_sig: &Pt,
) -> Result<(), Error> {
    let e = proof.e.to_scalar()?;
    let s = proof.s.to_scalar()?;
    let neg_e = -e;
    let r1 = msm(&[(Pt::generator(), s), (*issuer_pub, neg_e)]);
    let r2 = msm(&[(*blinded, s), (*blind_sig, neg_e)]);
    if dleq_challenge(&r1, &r2, issuer_pub, blinded, blind_sig) == e {
        Ok(())
    } else {
        Err(Error::BadDleq)
    }
}

/// The issuer's first move of a blinded DLEQ for `C_ = k B_`, with a
/// secret nonce `w` it answers one challenge with, ever: `R1 = wG`,
/// `R2 = wB_`. A second challenge answered with the same `w` reveals `k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DleqCommit {
    pub r1: Point,
    pub r2: Point,
}

pub fn dleq_commit(w: &Scalar, blinded: &ProjectivePoint) -> DleqCommit {
    DleqCommit {
        r1: Point::from_projective(&(ProjectivePoint::GENERATOR * w)),
        r2: Point::from_projective(&(*blinded * w)),
    }
}

/// Blind the issuer's commitment for `(B_, C_)` to the unblinded `(Y, C)`
/// (`B_ = Y + rG`, `C = C_ - rK`) with the holder's secret `alpha`,
/// `beta`. Since `wY = R2 - rR1`, the commitment for `(Y, C)` is
/// `R1' = R1 + alpha G + beta K`, `R2' = wY + alpha Y + beta C`, with
/// challenge `e' = H(R1', R2', K, Y, C)`. Returns `(e' + beta, e')`: the
/// first goes to the issuer, whose answer `s = w + (e' + beta) k` makes
/// `(e', s + alpha)` a DLEQ on `(Y, C)` (`dleq_unblind`). Neither
/// `e' + beta` nor `s` says anything of `(e', s + alpha)` without `alpha`
/// and `beta`.
pub fn dleq_blind(
    commit: &DleqCommit,
    issuer_pub: &Pt,
    y: &Pt,
    c: &Pt,
    r: &Scalar,
    alpha: &Scalar,
    beta: &Scalar,
) -> Result<(Scalar, Scalar), Error> {
    let r1 = Pt::from_point(&commit.r1)?;
    let r2 = Pt::from_point(&commit.r2)?;
    let r1b = msm(&[
        (r1, Scalar::ONE),
        (Pt::generator(), *alpha),
        (*issuer_pub, *beta),
    ]);
    let r2b = msm(&[(r2, Scalar::ONE), (r1, -*r), (*y, *alpha), (*c, *beta)]);
    let e = dleq_challenge(&r1b, &r2b, issuer_pub, y, c);
    Ok((e + beta, e))
}

/// The issuer's answer to a blinded challenge: `s = w + e k`.
pub fn dleq_answer(w: &Scalar, e: &Scalar, k: &Scalar) -> Scalar {
    w + e * k
}

/// The DLEQ on `(Y, C)` from the issuer's answer `s` to `dleq_blind`'s
/// challenge, checked: `(e', s + alpha)`.
pub fn dleq_unblind(
    e: &Scalar,
    s: &Scalar,
    alpha: &Scalar,
    issuer_pub: &Pt,
    y: &Pt,
    c: &Pt,
) -> Result<Dleq, Error> {
    let d = Dleq {
        e: ScalarBytes::from_scalar(e),
        s: ScalarBytes::from_scalar(&(s + alpha)),
    };
    dleq_verify_blinded(&d, issuer_pub, y, c)?;
    Ok(d)
}

/// Verify a DLEQ on unblinded values, as a third party with `(Y, C, r)`
/// does. With `B_ = Y + r G` and `C_ = C + r K`, `R2 = s B_ - e C_ = s Y -
/// e C + r R1`: MSMs of two, one and three terms instead of six scalar
/// multiplications, and `r G` for the challenge's `B_`.
pub fn dleq_verify_unblinded(
    proof: &Dleq,
    issuer_pub: &Pt,
    y: &Pt,
    c: &Pt,
    r: &Scalar,
) -> Result<(), Error> {
    let e = proof.e.to_scalar()?;
    let s = proof.s.to_scalar()?;
    let neg_e = -e;
    let r1 = msm(&[(Pt::generator(), s), (*issuer_pub, neg_e)]);
    let blinded = y.add(&gen_mul(r));
    let blind_sig = c.add(&msm(&[(*issuer_pub, *r)]));
    let r2 = msm(&[(*y, s), (*c, neg_e), (r1, *r)]);
    if dleq_challenge(&r1, &r2, issuer_pub, &blinded, &blind_sig) == e {
        Ok(())
    } else {
        Err(Error::BadDleq)
    }
}

// ---------------------------------------------------------------------------
// BIP340 Schnorr over 32-byte digests
// ---------------------------------------------------------------------------

/// A BIP340 signature (64 bytes), hex in JSON.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature(pub [u8; 64]);

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signature({}..)", hex::encode(&self.0[..8]))
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_fixed(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_fixed::<D, 64>(d, "signature").map(Signature)
    }
}

/// Sign a 32-byte digest with the secret scalar `sk` (negated internally if
/// its point has odd y, as BIP340 requires).
pub fn schnorr_sign(sk: &Scalar, digest: &[u8; 32]) -> Result<Signature, Error> {
    let nz: Option<NonZeroScalar> = NonZeroScalar::new(*sk).into();
    let key = k256::schnorr::SigningKey::from(nz.ok_or(Error::InvalidScalar)?);
    let mut aux = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut aux);
    let sig = key
        .sign_prehash_with_aux_rand(digest, &aux)
        .map_err(|_| Error::InvalidScalar)?;
    Ok(Signature(sig.to_bytes()))
}

/// Verify a BIP340 signature by an x-only key over a 32-byte digest.
/// BIP340 verification, written out so that `R = s G - e P` is one MSM:
/// `k256`'s verifier multiplies the two terms separately.
pub fn schnorr_verify(x_only: &[u8; 32], digest: &[u8; 32], sig: &Signature) -> Result<(), Error> {
    let mut enc = [0u8; 33];
    enc[0] = 0x02;
    enc[1..].copy_from_slice(x_only);
    let p = Pt::from_point(&Point(enc))?;
    let r_x: [u8; 32] = sig.0[..32].try_into().unwrap();
    let s = ScalarBytes(sig.0[32..].try_into().unwrap()).to_scalar()?;
    let tag: [u8; 32] = Sha256::digest(b"BIP0340/challenge").into();
    let mut h = Sha256::new();
    h.update(tag);
    h.update(tag);
    h.update(r_x);
    h.update(x_only);
    h.update(digest);
    let e = scalar_from_hash(&h.finalize().into());
    let r = msm(&[(Pt::generator(), s), (p, -e)]).to_point();
    if r.0[0] == 0x02 && r.0[1..] == r_x {
        Ok(())
    } else {
        Err(Error::BadSignature)
    }
}

/// The x-only public key of a secret scalar.
pub fn x_only_pub(sk: &Scalar) -> [u8; 32] {
    Point::from_projective(&(ProjectivePoint::GENERATOR * sk)).x_only()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::gen_mul;

    /// A string of the wrong width is refused before it is decoded: no
    /// matter its length, nothing but the honest width is allocated.
    #[test]
    fn a_hex_string_of_the_wrong_width_is_refused_undecoded() {
        let point = Point([2; 33]);
        let honest = serde_json::to_string(&point).unwrap();
        assert_eq!(serde_json::from_str::<Point>(&honest).unwrap(), point);
        let long = format!("\"{}\"", "ab".repeat(1 << 20));
        let e = serde_json::from_str::<Point>(&long)
            .unwrap_err()
            .to_string();
        assert!(e.contains("must be"), "{e}");
        let short = format!("\"{}\"", "ab".repeat(32));
        assert!(serde_json::from_str::<Point>(&short).is_err());
    }

    #[test]
    fn bdhke_roundtrip_and_dleq() {
        let k = random_scalar();
        let kpub = ProjectivePoint::GENERATOR * k;
        let kpt = Pt::from_projective(&kpub);
        let y_point = generator_mul(&random_scalar());
        let y = Pt::from_projective(&y_point);
        let r = random_scalar();
        let b_ = y_point + generator_mul(&r);
        let c_ = blind_sign(&k, &b_);
        let proof = dleq_prove(&k, &b_, &c_);
        dleq_verify_blinded(
            &proof,
            &kpt,
            &Pt::from_projective(&b_),
            &Pt::from_projective(&c_),
        )
        .unwrap();
        let c = unblind(&c_, &r, &kpub);
        let cpt = Pt::from_projective(&c);
        assert_eq!(y_point * k, c);
        dleq_verify_unblinded(&proof, &kpt, &y, &cpt, &r).unwrap();
        // wrong key fails
        let k2 = random_scalar();
        assert_ne!(y_point * k2, c);
        assert!(dleq_verify_unblinded(&proof, &gen_mul(&k2), &y, &cpt, &r).is_err());
        // forged C with a valid-looking DLEQ fails
        let forged = gen_mul(&random_scalar());
        assert!(dleq_verify_blinded(&proof, &kpt, &Pt::from_projective(&b_), &forged).is_err());
    }

    #[test]
    fn a_linked_proof_binds_its_blinding() {
        // Knowing k, a prover picks R1 = rho G, R2 = rho Y and C_ = kY + G,
        // and r = e / s after the challenge: then s G - e K = R1 and
        // s Y - e C + r R1 = R2 for C = C_ - rK, which is not kY.
        let k = random_scalar();
        let kpt = gen_mul(&k);
        let y = gen_mul(&random_scalar());
        let rho = random_scalar();
        let (r1, r2) = (gen_mul(&rho), msm(&[(y, rho)]));
        let c_ = msm(&[(y, k)]).add(&Pt::generator());
        let forge = |e: Scalar| {
            let s = rho + e * k;
            let r = e * s.invert().unwrap();
            let c = c_.add(&msm(&[(kpt, -r)]));
            let proof = Dleq {
                e: ScalarBytes::from_scalar(&e),
                s: ScalarBytes::from_scalar(&s),
            };
            (proof, c, r)
        };
        // Under a challenge without the base, as before, it verified.
        let unbound = |r1: &Pt, r2: &Pt| {
            let parts = [r1, r2, &kpt, &c_].map(|p| p.to_point().0);
            scalar_from_hash(&tagged_hash(
                "licash/dleq",
                &parts.each_ref().map(|p| &p[..]),
            ))
        };
        let (proof, c, r) = forge(unbound(&r1, &r2));
        assert_ne!(c.to_point(), msm(&[(y, k)]).to_point());
        let e = proof.e.to_scalar().unwrap();
        let s = proof.s.to_scalar().unwrap();
        let r1v = msm(&[(Pt::generator(), s), (kpt, -e)]);
        let r2v = msm(&[(y, s), (c, -e), (r1v, r)]);
        assert_eq!(unbound(&r1v, &r2v), e);
        // With the base in the challenge, no choice of it made before r is
        // the base after it.
        for base in [y, y.add(&Pt::generator())] {
            let e = dleq_challenge(&r1, &r2, &kpt, &base, &c_);
            let (proof, c, r) = forge(e);
            assert!(dleq_verify_unblinded(&proof, &kpt, &y, &c, &r).is_err());
        }
    }

    #[test]
    fn blinded_dleq_is_on_the_unblinded_pair_and_shares_nothing() {
        let k = random_scalar();
        let kpub = ProjectivePoint::GENERATOR * k;
        let kpt = Pt::from_projective(&kpub);
        let y_point = generator_mul(&random_scalar());
        let y = Pt::from_projective(&y_point);
        let r = random_scalar();
        let b_ = y_point + generator_mul(&r);
        let c_ = blind_sign(&k, &b_);
        let c = Pt::from_projective(&unblind(&c_, &r, &kpub));
        let w = random_scalar();
        let commit = dleq_commit(&w, &b_);
        let (alpha, beta) = (random_scalar(), random_scalar());
        let (sent, e) = dleq_blind(&commit, &kpt, &y, &c, &r, &alpha, &beta).unwrap();
        let s = dleq_answer(&w, &sent, &k);
        let proof = dleq_unblind(&e, &s, &alpha, &kpt, &y, &c).unwrap();
        // A plain DLEQ on (Y, C): nothing of the issuer's transcript.
        dleq_verify_blinded(&proof, &kpt, &y, &c).unwrap();
        assert_ne!(proof.e.to_scalar().unwrap(), sent);
        assert_ne!(proof.s.to_scalar().unwrap(), s);
        // A wrong answer, or another holder's blinding, gives no proof.
        let bad = dleq_answer(&random_scalar(), &sent, &k);
        assert!(dleq_unblind(&e, &bad, &alpha, &kpt, &y, &c).is_err());
        assert!(dleq_unblind(&e, &s, &random_scalar(), &kpt, &y, &c).is_err());
        // Two challenges answered with one nonce reveal the key: the
        // issuer answers one per commitment, ever.
        let (other, _) = dleq_blind(&commit, &kpt, &y, &c, &r, &alpha, &random_scalar()).unwrap();
        let s2 = dleq_answer(&w, &other, &k);
        let leaked = (s - s2) * (sent - other).invert().unwrap();
        assert_eq!(leaked, k);
    }

    #[test]
    fn schnorr_roundtrip() {
        let sk = random_scalar();
        let pk = x_only_pub(&sk);
        let digest = tagged_hash("test", &[b"msg"]);
        let sig = schnorr_sign(&sk, &digest).unwrap();
        schnorr_verify(&pk, &digest, &sig).unwrap();
        let other = tagged_hash("test", &[b"other"]);
        assert!(schnorr_verify(&pk, &other, &sig).is_err());
    }

    #[test]
    fn point_serde() {
        let p = Point::from_projective(&ProjectivePoint::GENERATOR);
        let s = serde_json::to_string(&p).unwrap();
        let back: Point = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn only_canonical_encodings_decode() {
        for k in [1u64, 2, 17, 19] {
            let p = Point::from_projective(&generator_mul(&Scalar::from(k)));
            assert_eq!(Point::from_projective(&p.to_projective().unwrap()), p);
            assert_eq!(Pt::from_point(&p).unwrap().to_point(), p);
            // Every other tag, SEC1's compact form (5) included, which
            // `k256` decodes to a point whose canonical encoding differs.
            for tag in (0u8..=255).filter(|t| ![0x02, 0x03].contains(t)) {
                let mut other = p;
                other.0[0] = tag;
                assert_eq!(other.to_projective(), Err(Error::InvalidPoint));
                assert!(Pt::from_point(&other).is_err());
            }
        }
        let identity = Point([0; 33]);
        assert_eq!(identity.to_projective(), Ok(ProjectivePoint::IDENTITY));
        assert!(Pt::from_point(&identity).unwrap().is_identity());
    }
}
