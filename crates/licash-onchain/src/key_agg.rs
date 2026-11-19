//! BIP327's `KeyAgg`: the aggregate of a list of keys, each weighed by a
//! coefficient of the whole list, so that no key of it cancels the others.
//! A swap's taproot output takes it of its two parties' keys as internal
//! key: not the point of no known secret, its spends by script count as
//! inputs of silent payments (BIP352), and a payout's claim can pay a
//! silent payment address.

use bitcoin::{
    hashes::{sha256, Hash, HashEngine},
    key::Secp256k1,
    secp256k1::{PublicKey, Scalar},
};

/// The aggregate of a list of keys and each one's coefficient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyAgg {
    /// The sum of each key times its coefficient: never infinity.
    pub point: PublicKey,
    /// Each key's coefficient, in the list's order.
    pub coefficients: Vec<Scalar>,
}

/// SHA256 tagged with `tag`, as BIP340 defines it.
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

/// The order of secp256k1's group.
const N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

/// `bytes`, a 256-bit number, modulo the group's order: at most once
/// past it, as 2^256 < 2n.
fn reduce(bytes: [u8; 32]) -> Scalar {
    if let Ok(scalar) = Scalar::from_be_bytes(bytes) {
        return scalar;
    }
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = i16::from(bytes[i]) - i16::from(N[i]) - borrow;
        borrow = i16::from(d < 0);
        out[i] = (d + 256 * borrow) as u8;
    }
    Scalar::from_be_bytes(out).expect("below the order")
}

/// BIP327's `KeyAgg` of `keys`, in that order: `None` if their weighed
/// sum is infinity (or a coefficient zero, as unlikely).
pub fn key_agg(keys: &[PublicKey]) -> Option<KeyAgg> {
    let serialized: Vec<[u8; 33]> = keys.iter().map(PublicKey::serialize).collect();
    let list: Vec<&[u8]> = serialized.iter().map(|k| k.as_slice()).collect();
    let hash_keys = tagged("KeyAgg list", &list);
    // The first key other than the first: its coefficient is 1.
    let first = serialized.first()?;
    let second = serialized.iter().find(|k| *k != first);
    let secp = Secp256k1::verification_only();
    let mut coefficients = Vec::with_capacity(keys.len());
    let mut weighed = Vec::with_capacity(keys.len());
    for (key, bytes) in keys.iter().zip(&serialized) {
        let coefficient = if Some(bytes) == second {
            Scalar::ONE
        } else {
            reduce(tagged("KeyAgg coefficient", &[&hash_keys, bytes]))
        };
        weighed.push(key.mul_tweak(&secp, &coefficient).ok()?);
        coefficients.push(coefficient);
    }
    let refs: Vec<&PublicKey> = weighed.iter().collect();
    let point = PublicKey::combine_keys(&refs).ok()?;
    Some(KeyAgg {
        point,
        coefficients,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_past_the_order_is_reduced_once() {
        let mut past = N;
        past[31] += 5;
        let mut five = [0u8; 32];
        five[31] = 5;
        assert_eq!(reduce(past), Scalar::from_be_bytes(five).unwrap());
        assert_eq!(reduce(five), Scalar::from_be_bytes(five).unwrap());
        assert_eq!(reduce(N), Scalar::ZERO);
    }
}
