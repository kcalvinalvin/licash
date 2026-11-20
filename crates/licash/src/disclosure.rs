//! The secrets the Hub discloses with its response: the unlocks of the
//! granted spends and the Pay preimages its evidence cites, in the slot
//! its `respond` spends. The statement authenticates the bytes by their
//! block and the work on it.

use serde::{Deserialize, Serialize};

use crate::crypto::tagged_hash;
use crate::spend::lock_hash;
use crate::Error;

pub const SLOTS: usize = 32;
pub const BYTES: usize = SLOTS * 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disclosures {
    /// Raw secrets, not scalars. Hash preimages may be zero or noncanonical
    /// scalars, so consumers must inspect all slots, including padding.
    pub values: [[u8; 32]; SLOTS],
    /// How many of the slots, from the first, hold a secret: what a lean
    /// format writes ([`Disclosures::encode`]), where the rest is padding.
    pub used: usize,
}

impl Disclosures {
    pub fn empty() -> Self {
        Self {
            values: [[0; 32]; SLOTS],
            used: 0,
        }
    }

    /// Any 32 slots decode: no scalar decoding is performed, even for
    /// genuine hash preimages.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != BYTES {
            return Err(Error::Protocol("disclosure vector length".into()));
        }
        let mut out = Self::empty();
        out.used = SLOTS;
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

    /// What the response's disclosure carries: all 32 slots, padded
    /// ([`Disclosures::to_bytes`]), or, if `lean`, only the secrets
    /// disclosed, as many 32-byte slots as there are.
    pub fn encode(&self, lean: bool) -> Vec<u8> {
        match lean {
            false => self.to_bytes().to_vec(),
            // `used` is a field anyone deserializing sets: never past the
            // slots there are.
            true => self.values[..self.used.min(SLOTS)].concat(),
        }
    }

    pub fn digest(&self) -> [u8; 32] {
        tagged_hash("licash/counter-disclosures/v1", &[&self.to_bytes()])
    }

    /// Match all 32 raw slots. Zero is also a valid preimage, not a sentinel.
    pub fn preimage(&self, hash: &[u8; 32]) -> Option<[u8; 32]> {
        self.values
            .iter()
            .copied()
            .find(|value| lock_hash(value) == *hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `used` is a field anyone deserializing sets: past the slots, a lean
    /// encoding is all of them, never a panic.
    #[test]
    fn a_used_count_past_the_slots_encodes_them_all() {
        let mut d = Disclosures::empty();
        d.values[0] = [7; 32];
        d.used = 1;
        assert_eq!(d.encode(true), vec![7; 32]);
        d.used = SLOTS + 1;
        assert_eq!(d.encode(true).len(), BYTES);
        let wild: Disclosures =
            serde_json::from_value(serde_json::json!({"values": d.values, "used": usize::MAX}))
                .unwrap();
        assert_eq!(wild.encode(true).len(), BYTES);
    }
}
