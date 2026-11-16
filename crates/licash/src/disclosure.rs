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
}

impl Disclosures {
    pub fn empty() -> Self {
        Self {
            values: [[0; 32]; SLOTS],
        }
    }

    /// Any 32 slots decode: no scalar decoding is performed, even for
    /// genuine hash preimages.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != BYTES {
            return Err(Error::Protocol("disclosure vector length".into()));
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
    pub fn preimage(&self, hash: &[u8; 32]) -> Option<[u8; 32]> {
        self.values
            .iter()
            .copied()
            .find(|value| lock_hash(value) == *hash)
    }
}
