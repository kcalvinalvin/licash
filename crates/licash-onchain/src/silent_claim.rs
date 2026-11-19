//! A payout's claim paying a silent payment address (BIP352).
//!
//! The claim spends one input counted for silent payments: the payout's
//! taproot output, of internal key `P = a_c X_c + X_r` (BIP327's `KeyAgg`
//! of the claim key, then the refund key, see [`Htlc::key_agg`]) and
//! output key `Q = g_P P + t G`, `g_P` negating an odd `P`, `t` its
//! TapTweak. Its secret, `q = g_Q (g_P (a_c x_c + x_r) + t)`, no one knows
//! whole: the wallet knows `x_c`, the provider `x_r`. The payee's output
//! needs `D = q B_scan` only, which the wallet makes of
//! `S = x_r B_scan`, the provider's share:
//! `D = g_Q (g_P (a_c (x_c B_scan) + S) + t B_scan)`.
//!
//! The provider never learns the payee's scan key: the wallet sends it
//! blinded, `B' = B_scan + rho G`, and takes `S = S' - rho X_r` of the
//! provider's `S' = x_r B'`, which a proof shows is of the refund key's
//! secret ([`Share`]). Here every key is that of even y of its x
//! coordinate, its secret negated if need be.

use bitcoin::{
    hashes::{sha256, Hash, HashEngine},
    key::Secp256k1,
    secp256k1::{All, Parity, PublicKey, Scalar, SecretKey, XOnlyPublicKey},
    OutPoint,
};
use std::sync::OnceLock;

use crate::{
    deposit::Htlc,
    silent::{output_of_shared, Error, SpAddress},
};

fn secp() -> &'static Secp256k1<All> {
    static SECP: OnceLock<Secp256k1<All>> = OnceLock::new();
    SECP.get_or_init(Secp256k1::new)
}

fn tagged(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let tag = sha256::Hash::hash(tag.as_bytes());
    let mut engine = sha256::Hash::engine();
    engine.input(tag.as_ref());
    engine.input(tag.as_ref());
    for part in parts {
        engine.input(part);
    }
    sha256::Hash::from_engine(engine).to_byte_array()
}

/// `secret`, negated if its key's y is odd: the secret of its x-only key.
fn even(secret: &SecretKey) -> SecretKey {
    match secret.x_only_public_key(secp()).1 {
        Parity::Odd => secret.negate(),
        Parity::Even => *secret,
    }
}

fn mul(point: &PublicKey, by: &Scalar) -> Result<PublicKey, Error> {
    point.mul_tweak(secp(), by).map_err(|_| Error::Key)
}

fn add(a: &PublicKey, b: &PublicKey) -> Result<PublicKey, Error> {
    a.combine(b).map_err(|_| Error::Key)
}

/// `point`, negated if `parity` is odd.
fn signed(point: PublicKey, parity: Parity) -> PublicKey {
    match parity {
        Parity::Odd => point.negate(secp()),
        Parity::Even => point,
    }
}

/// A hash as a nonzero scalar below the group's order.
fn scalar(hash: [u8; 32]) -> Result<SecretKey, Error> {
    SecretKey::from_slice(&hash).map_err(|_| Error::Key)
}

/// The scan key `scan` blinded by `rho`: `scan + rho G`, what the provider
/// is asked its share of.
pub fn blind(scan: &PublicKey, rho: &SecretKey) -> Result<PublicKey, Error> {
    add(scan, &rho.public_key(secp()))
}

/// A provider's share of a blinded scan key `B'`: `S' = x_r B'`, `x_r` the
/// secret of its refund key, and the proof of it (Chaum-Pedersen, a
/// discrete logarithm's equality): `s G = R1 + e X_r`, `s B' = R2 + e S'`,
/// `e` the hash of `R1, R2, X_r, B', S'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    pub point: PublicKey,
    pub e: [u8; 32],
    pub s: [u8; 32],
}

fn challenge(
    r1: &PublicKey,
    r2: &PublicKey,
    key: &XOnlyPublicKey,
    blinded: &PublicKey,
    share: &PublicKey,
) -> Result<SecretKey, Error> {
    scalar(tagged(
        "licash/payout-share",
        &[
            &r1.serialize(),
            &r2.serialize(),
            &key.serialize(),
            &blinded.serialize(),
            &share.serialize(),
        ],
    ))
}

impl Share {
    /// The share of the refund key's secret `refund` in `blinded`, its
    /// proof's nonce derived of both: the same proof each time asked.
    pub fn new(refund: &SecretKey, blinded: &PublicKey) -> Result<Self, Error> {
        let x = even(refund);
        let key = x.x_only_public_key(secp()).0;
        let point = mul(blinded, &Scalar::from(x))?;
        let r = scalar(tagged(
            "licash/payout-share-nonce",
            &[&x.secret_bytes(), &blinded.serialize()],
        ))?;
        let r1 = r.public_key(secp());
        let r2 = mul(blinded, &Scalar::from(r))?;
        let e = challenge(&r1, &r2, &key, blinded, &point)?;
        let s = x
            .mul_tweak(&Scalar::from(e))
            .and_then(|ex| ex.add_tweak(&Scalar::from(r)))
            .map_err(|_| Error::Key)?;
        Ok(Self {
            point,
            e: e.secret_bytes(),
            s: s.secret_bytes(),
        })
    }

    /// Whether it is the share of `blinded` by the secret of `refund`.
    pub fn verify(&self, refund: &XOnlyPublicKey, blinded: &PublicKey) -> bool {
        let check = || -> Result<bool, Error> {
            let (e, s) = (scalar(self.e)?, scalar(self.s)?);
            let key = refund.public_key(Parity::Even);
            let minus_e = Scalar::from(e.negate());
            // R1 = s G - e X_r, R2 = s B' - e S'.
            let r1 = add(&s.public_key(secp()), &mul(&key, &minus_e)?)?;
            let r2 = add(&mul(blinded, &Scalar::from(s))?, &mul(&self.point, &minus_e)?)?;
            Ok(challenge(&r1, &r2, refund, blinded, &self.point)? == e)
        };
        check().unwrap_or(false)
    }

    /// `S = S' - rho X_r`: the share of the scan key unblinded, `rho` its
    /// blinding, `refund` the key it is of.
    pub fn unblind(&self, rho: &SecretKey, refund: &XOnlyPublicKey) -> Result<PublicKey, Error> {
        let key = refund.public_key(Parity::Even);
        let minus_rho = Scalar::from(rho.negate());
        add(&self.point, &mul(&key, &minus_rho)?)
    }
}

/// The output paying `to` of a claim of `htlc`'s output `funding` by the
/// secret `claim` of its claim key, `share` the provider's of `to`'s scan
/// key, unblinded ([`Share::unblind`]). The claim must spend that output
/// alone, and pay `to` no other output.
pub fn claim_output(
    htlc: &Htlc,
    claim: &SecretKey,
    share: &PublicKey,
    funding: OutPoint,
    to: &SpAddress,
) -> Result<XOnlyPublicKey, Error> {
    let x_c = even(claim);
    if x_c.x_only_public_key(secp()).0 != htlc.claim {
        return Err(Error::Key);
    }
    let agg = htlc.key_agg();
    let a_c = agg.coefficients[0];
    let p_parity = agg.point.x_only_public_key().1;
    let info = htlc.spend_info();
    let t = info.tap_tweak().to_scalar();
    let q_parity = info.output_key_parity();
    let scan = to.scan;
    // a_c (x_c B_scan) + S
    let inner = add(&mul(&mul(&scan, &Scalar::from(x_c))?, &a_c)?, share)?;
    // g_Q (g_P inner + t B_scan)
    let d = signed(add(&signed(inner, p_parity), &mul(&scan, &t)?)?, q_parity);
    let key = info.output_key().to_x_only_public_key().public_key(Parity::Even);
    output_of_shared(&d, &key, funding, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silent::{outputs, Keys};
    use bitcoin::{hashes::sha256, NetworkKind, Txid};

    fn secret(n: u32) -> SecretKey {
        SecretKey::from_slice(&tagged("test", &[&n.to_be_bytes()])).unwrap()
    }

    /// What the claim pays `to`, the way a sender knowing the output's
    /// secret whole would (BIP352's own derivation), against the one of
    /// shares, for every parity of each key and of `P` and `Q`.
    #[test]
    fn a_claim_pays_the_payee_as_bip352_derives_it_of_every_parity() {
        let payee = Keys::new(secret(1000), secret(1001), NetworkKind::Test);
        let to = payee.address();
        let funding = OutPoint {
            txid: Txid::from_byte_array([5; 32]),
            vout: 2,
        };
        let mut seen = std::collections::HashSet::new();
        let mut n = 0;
        while seen.len() < 16 {
            n += 1;
            assert!(n < 2000, "parities seen: {seen:?}");
            let (x_c, x_r) = (secret(2 * n), secret(2 * n + 1));
            let htlc = Htlc {
                hash: sha256::Hash::hash(&n.to_be_bytes()).to_byte_array(),
                claim: x_c.x_only_public_key(secp()).0,
                refund: x_r.x_only_public_key(secp()).0,
                timeout: 100 + n,
            };
            let agg = htlc.key_agg();
            let info = htlc.spend_info();
            let parities = (
                x_c.x_only_public_key(secp()).1,
                x_r.x_only_public_key(secp()).1,
                agg.point.x_only_public_key().1,
                info.output_key_parity(),
            );
            if !seen.insert(format!("{parities:?}")) {
                continue;
            }
            // The output's secret whole: q, of the output key as is.
            let p = even(&x_c)
                .mul_tweak(&agg.coefficients[0])
                .unwrap()
                .add_tweak(&Scalar::from(even(&x_r)))
                .unwrap();
            let p = match parities.2 {
                Parity::Odd => p.negate(),
                Parity::Even => p,
            };
            let q = p.add_tweak(&info.tap_tweak().to_scalar()).unwrap();
            assert_eq!(
                q.x_only_public_key(secp()).0,
                info.output_key().to_x_only_public_key()
            );
            let expected = outputs(&[(q, true)], &[funding], &[to]).unwrap()[0];

            // Of shares, the scan key blinded.
            let rho = secret(5000 + n);
            let blinded = blind(&to.scan, &rho).unwrap();
            let share = Share::new(&x_r, &blinded).unwrap();
            assert!(share.verify(&htlc.refund, &blinded));
            let s = share.unblind(&rho, &htlc.refund).unwrap();
            let got = claim_output(&htlc, &x_c, &s, funding, &to).unwrap();
            assert_eq!(got, expected, "{parities:?}");

            // The payee finds it, of the claim's tweak.
            let key = info.output_key().to_x_only_public_key().public_key(Parity::Even);
            let tweak = {
                let hash = tagged(
                    "BIP0352/Inputs",
                    &[&bitcoin::consensus::serialize(&funding), &key.serialize()],
                );
                key.mul_tweak(secp(), &Scalar::from_be_bytes(hash).unwrap())
                    .unwrap()
            };
            let found = payee.scanner([]).scan(&tweak, &[got]);
            assert_eq!(found.len(), 1, "{parities:?}");
        }
    }

    #[test]
    fn a_share_proves_only_its_own_key_and_blinded_point() {
        let (x_r, other) = (secret(1), secret(2));
        let refund = x_r.x_only_public_key(secp()).0;
        let blinded = blind(&secret(3).public_key(secp()), &secret(4)).unwrap();
        let share = Share::new(&x_r, &blinded).unwrap();
        assert!(share.verify(&refund, &blinded));
        assert_eq!(Share::new(&x_r, &blinded).unwrap(), share);
        // Of another key, another point, or altered: refused.
        assert!(!share.verify(&other.x_only_public_key(secp()).0, &blinded));
        let elsewhere = blind(&secret(5).public_key(secp()), &secret(4)).unwrap();
        assert!(!share.verify(&refund, &elsewhere));
        let forged = Share {
            point: share.point.negate(secp()),
            ..share
        };
        assert!(!forged.verify(&refund, &blinded));
        let wrong = Share::new(&other, &blinded).unwrap();
        assert!(!wrong.verify(&refund, &blinded));
        // Its proof's scalars, not scalars: refused, not a panic.
        let zero = Share { e: [0; 32], ..share };
        assert!(!zero.verify(&refund, &blinded));
    }

    #[test]
    fn a_claim_by_another_key_is_refused() {
        let (x_c, x_r) = (secret(1), secret(2));
        let htlc = Htlc {
            hash: [1; 32],
            claim: x_c.x_only_public_key(secp()).0,
            refund: x_r.x_only_public_key(secp()).0,
            timeout: 1,
        };
        let to = Keys::new(secret(3), secret(4), NetworkKind::Test).address();
        let funding = OutPoint::null();
        let s = secret(9).public_key(secp());
        assert!(claim_output(&htlc, &secret(7), &s, funding, &to).is_err());
        assert!(claim_output(&htlc, &x_c.negate(), &s, funding, &to).is_ok());
    }
}
