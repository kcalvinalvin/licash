//! Canonical counter secrets and their five setup-bound WOTS publications.
//! Raw bytes are authenticated, not merely supplied beside a signed digest.

use serde::{Deserialize, Serialize};

use crate::crypto::tagged_hash;
use crate::spend::lock_hash;
use crate::wots::{self, Hashes, SecretKey};
use crate::Error;

pub const SLOTS: usize = 32;
pub const BYTES: usize = SLOTS * 32;
pub const CHUNK_BYTES: [usize; 5] = [224, 224, 224, 224, 128];
pub const CHUNK_DIGITS: [usize; 5] = [452, 452, 452, 452, 259];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disclosures {
    /// Raw secrets, not scalars. Hash preimages may be zero or noncanonical
    /// scalars, so consumers must inspect all slots, including padding.
    pub values: [[u8; 32]; SLOTS],
}

impl Disclosures {
    pub fn empty() -> Self {
        Self {
            values: [[0; 32]; SLOTS],
        }
    }

    /// Decoding is intentionally total for authenticated garbage. A malformed
    /// vector must remain available to prove that the counter failed. No
    /// scalar decoding is performed, even for genuine hash preimages.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != BYTES {
            return Err(Error::Protocol("counter disclosure vector length".into()));
        }
        let mut out = Self::empty();
        for (slot, bytes) in out.values.iter_mut().zip(bytes.as_chunks::<32>().0) {
            *slot = *bytes;
        }
        Ok(out)
    }

    pub fn to_bytes(&self) -> [u8; BYTES] {
        let mut bytes = [0; BYTES];
        for (slot, value) in bytes.as_chunks_mut::<32>().0.iter_mut().zip(&self.values) {
            *slot = *value;
        }
        bytes
    }

    pub fn digest(&self) -> [u8; 32] {
        tagged_hash("licash/counter-disclosures/v1", &[&self.to_bytes()])
    }

    /// Match all 32 raw slots. Zero is also a valid preimage, not a sentinel.
    /// This does not authenticate a publication; verify SignedDisclosures
    /// against the trusted opening keys before treating its bytes as posted.
    pub fn preimage(&self, hash: &[u8; 32]) -> Option<[u8; 32]> {
        self.values
            .iter()
            .copied()
            .find(|value| lock_hash(value) == *hash)
    }
}

pub fn keys_well_formed(keys: &[Hashes; 5]) -> bool {
    keys.iter()
        .zip(CHUNK_DIGITS)
        .all(|(key, digits)| key.len() == digits)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDisclosures {
    pub disclosures: Disclosures,
    pub signatures: [Hashes; 5],
}

impl SignedDisclosures {
    pub fn sign(disclosures: Disclosures, keys: &[SecretKey; 5]) -> Result<Self, Error> {
        let public = std::array::from_fn(|i| keys[i].public());
        if !keys_well_formed(&public) {
            return Err(Error::Protocol("counter disclosure key shape".into()));
        }
        let bytes = disclosures.to_bytes();
        let mut offset = 0;
        let signatures = std::array::from_fn(|i| {
            let end = offset + CHUNK_BYTES[i];
            let signature = keys[i].sign_msg(&bytes[offset..end]);
            offset = end;
            signature
        });
        Ok(Self {
            disclosures,
            signatures,
        })
    }

    pub fn verify(&self, keys: &[Hashes; 5]) -> bool {
        if !keys_well_formed(keys) {
            return false;
        }
        let bytes = self.disclosures.to_bytes();
        let mut offset = 0;
        keys.iter()
            .zip(&self.signatures)
            .zip(CHUNK_BYTES)
            .all(|((key, signature), size)| {
                let end = offset + size;
                let valid = wots::verify_msg(key, signature, &bytes[offset..end]);
                offset = end;
                valid
            })
    }
}
