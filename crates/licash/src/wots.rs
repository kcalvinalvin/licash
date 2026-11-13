//! One-time Winternitz signatures as Lichen's BABE graph checks them in
//! script (`lichen-withdraw`'s `fixed.rs`, ln-gap's `WotsParams::for_bytes`):
//! 4-bit digits, SHA-256 chains of at most 15, a checksum of `Σ (15 - d)`
//! in as few digits as hold it. The hub signs the amount `b` and the claim
//! digest `E` in `respond`; script checks these signatures, and the
//! statement checks them again to learn which leaf ran. The same code
//! serves both, so the two cannot disagree on a digit.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

/// A key or a signature: one hash per digit. Serialized flat (a guest
/// deserializing hash by hash through serde spent more cycles on that
/// than on the chains).
#[derive(Clone, PartialEq, Eq)]
pub struct Hashes(pub Vec<Hash>);

impl Hashes {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Hashes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hashes({} digits)", self.0.len())
    }
}

impl Serialize for Hashes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let flat: Vec<u8> = self.0.iter().flatten().copied().collect();
        serde_bytes::Bytes::new(&flat).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Hashes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Hashes, D::Error> {
        let flat = serde_bytes::ByteBuf::deserialize(d)?.into_vec();
        if flat.len() % 32 != 0 {
            return Err(serde::de::Error::custom("hash list length"));
        }
        Ok(Hashes(flat.as_chunks::<32>().0.to_vec()))
    }
}

/// The longest chain: a digit's key is its secret hashed this often.
pub const MAX_DIGIT: u8 = 15;

/// Digits of `b`: 8 of the amount, 2 of checksum.
pub const B_DIGITS: usize = 10;

/// Digits of a 32-byte message: 64, and 3 of checksum.
pub const MSG_DIGITS: usize = 67;

/// `b`'s digits: big-endian nibbles, then the checksum big-endian in two.
pub fn digits_b(b: u32) -> Vec<u8> {
    let mut d: Vec<u8> = b
        .to_be_bytes()
        .iter()
        .flat_map(|x| [x >> 4, x & 0xf])
        .collect();
    let sum: u8 = d.iter().map(|x| MAX_DIGIT - x).sum();
    d.push(sum >> 4);
    d.push(sum & 0xf);
    d
}

/// `msg`'s digits: big-endian nibbles, then the checksum `Σ (15 - d)`
/// big-endian in as few nibbles as hold `15 * digits`.
pub fn digits_of(msg: &[u8]) -> Vec<u8> {
    let mut d: Vec<u8> = msg.iter().flat_map(|b| [b >> 4, b & 0xf]).collect();
    let most = u32::from(MAX_DIGIT) * d.len() as u32;
    let mut sum: u32 = d.iter().map(|x| u32::from(MAX_DIGIT - x)).sum();
    let n = (1..).find(|n| 16u64.pow(*n) > u64::from(most)).unwrap();
    let at = d.len();
    d.resize(at + n as usize, 0);
    for x in d[at..].iter_mut().rev() {
        *x = (sum % 16) as u8;
        sum /= 16;
    }
    d
}

/// `h` hashed `n` times.
pub fn chain(mut h: Hash, n: u8) -> Hash {
    for _ in 0..n {
        h = Sha256::digest(h).into();
    }
    h
}

fn verify_digits(key: &Hashes, sig: &Hashes, d: &[u8]) -> bool {
    key.len() == d.len()
        && sig.len() == d.len()
        && d.iter()
            .zip(&sig.0)
            .zip(&key.0)
            .all(|((d, s), k)| chain(*s, MAX_DIGIT - d) == *k)
}

/// Whether `sig` is a signature on the amount `b` under `key`.
pub fn verify_b(key: &Hashes, sig: &Hashes, b: u32) -> bool {
    verify_digits(key, sig, &digits_b(b))
}

/// Whether `sig` is a signature on `msg` under `key`.
pub fn verify_msg(key: &Hashes, sig: &Hashes, msg: &[u8]) -> bool {
    verify_digits(key, sig, &digits_of(msg))
}

/// A one-time secret key: one 32-byte secret per digit. Signing twice with
/// different messages reveals enough to forge, as with any WOTS key.
#[derive(Clone, Serialize, Deserialize)]
pub struct SecretKey {
    secrets: Hashes,
}

impl SecretKey {
    /// Import ln-gap's SHA-256 WOTS key derivation, for independent graph
    /// builders and signers to agree on exactly the same one-time keys.
    pub fn from_entropy(digits: usize, entropy: [u8; 32]) -> SecretKey {
        let secrets = (0..digits)
            .map(|i| {
                let mut data = entropy.to_vec();
                let mut index = i;
                while index > 0 {
                    data.push((index & 255) as u8);
                    index >>= 8;
                }
                Sha256::digest(data).into()
            })
            .collect();
        SecretKey {
            secrets: Hashes(secrets),
        }
    }

    /// A fresh key for `digits` digits.
    pub fn random(digits: usize) -> SecretKey {
        let secrets = (0..digits)
            .map(|_| crate::crypto::random_scalar().to_bytes().into())
            .collect();
        SecretKey {
            secrets: Hashes(secrets),
        }
    }

    /// Each digit's secret hashed `MAX_DIGIT` times.
    pub fn public(&self) -> Hashes {
        Hashes(
            self.secrets
                .0
                .iter()
                .map(|s| chain(*s, MAX_DIGIT))
                .collect(),
        )
    }

    fn sign_digits(&self, d: &[u8]) -> Hashes {
        assert_eq!(d.len(), self.secrets.len(), "key size for this message");
        Hashes(
            self.secrets
                .0
                .iter()
                .zip(d)
                .map(|(s, d)| chain(*s, *d))
                .collect(),
        )
    }

    pub fn sign_b(&self, b: u32) -> Hashes {
        self.sign_digits(&digits_b(b))
    }

    pub fn sign_msg(&self, msg: &[u8]) -> Hashes {
        self.sign_digits(&digits_of(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let kb = SecretKey::random(B_DIGITS);
        let pb = kb.public();
        assert!(verify_b(&pb, &kb.sign_b(200_000), 200_000));
        assert!(!verify_b(&pb, &kb.sign_b(200_000), 200_001));
        let km = SecretKey::random(MSG_DIGITS);
        let pm = km.public();
        let e = [7u8; 32];
        assert!(verify_msg(&pm, &km.sign_msg(&e), &e));
        assert!(!verify_msg(&pm, &km.sign_msg(&e), &[8u8; 32]));
        assert!(!verify_msg(&pb, &km.sign_msg(&e), &e));
        let bytes = bincode::serialize(&pm).unwrap();
        assert_eq!(bytes.len(), 8 + 32 * MSG_DIGITS);
        assert_eq!(bincode::deserialize::<Hashes>(&bytes).unwrap(), pm);
    }

    /// Lichen's `digits` and `digits_of` on the same inputs (generated with
    /// `lichen-withdraw` at `d01bb5b`), so that a signature the hub makes
    /// for Lichen's script verifies here.
    #[test]
    fn digits_match_lichen() {
        assert_eq!(digits_b(0x0001_86a0), [0, 0, 0, 1, 8, 6, 10, 0, 5, 15]);
        assert_eq!(digits_b(0), [0, 0, 0, 0, 0, 0, 0, 0, 7, 8]);
        assert_eq!(
            digits_b(u32::MAX),
            [15; 8].iter().chain(&[0, 0]).copied().collect::<Vec<_>>()
        );
        let d = digits_of(&[0xffu8; 32]);
        assert_eq!(d.len(), MSG_DIGITS);
        assert_eq!(&d[64..], &[0, 0, 0]);
        let d = digits_of(&[0u8; 32]);
        assert_eq!(&d[64..], &[3, 12, 0]); // 64 * 15 = 960 = 0x3c0
        let msg: Vec<u8> = (0..32u8).collect();
        let d = digits_of(&msg);
        assert_eq!(&d[..6], &[0, 0, 0, 1, 0, 2]);
        assert_eq!(&d[62..], &[1, 15, 2, 12, 0]);
    }
}
