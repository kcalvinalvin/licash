//! Each party's secrets for one contract: its own only. The holder's sign
//! her side of the graph and the challenge's adaptors; the hub's answer a
//! request and deal the instances' input secrets.

use lichen_babe::reveal::{AdaptorKeys, LamportKey};
use lichen_babe::vsss::{Dealer, Dealing};
use lngap_channel::PartyKeys;
use lngap_lamport::winternitz::{Wots256Secret, WotsSecret};

/// The holder's one-time keys for one assertion: her `b` (the corrected
/// payout's bits read it), π1's Lamport key and the challenge's adaptors.
pub struct Assertion {
    pub b_key: WotsSecret,
    pub lamport: LamportKey,
    pub adaptor: AdaptorKeys,
}

/// The holder's secrets for one contract. Her request keys (`b` and `E`
/// of the exit's assertion) come from her request seed and Base instead.
pub struct HolderKeys {
    pub party: PartyKeys,
    pub lamport: LamportKey,
    pub adaptor: AdaptorKeys,
    /// A Corrected contract's second assertion: each key signs once.
    pub corrected: Assertion,
}

/// The hub's dealing of one assertion's instances: its polynomials, their
/// commitments, and each instance's garbling seed (instance `i` is
/// `seeds[i - 1]`).
pub struct Dealt {
    pub dealer: Dealer,
    pub dealing: Dealing,
    pub seeds: Vec<[u8; 32]>,
}

/// The hub's secrets for one contract.
pub struct HubKeys {
    pub party: PartyKeys,
    /// One-time keys for `respond`: `b`, `E` in each leaf, `K`, what a mask
    /// pays (`b_corr`), and the five disclosure chunks.
    pub b: Wots256Secret,
    pub e_plain: Wots256Secret,
    pub e_counter: Wots256Secret,
    pub k: Wots256Secret,
    pub b_corr: Wots256Secret,
    pub disclosure: [Wots256Secret; 5],
    /// The same keys as licash's statement signs with: Base's response keys.
    pub sealed: licash::statement::SecretKeys,
    /// The exit's assertion's dealing, then a Corrected contract's.
    pub full: Dealt,
    pub corrected: Dealt,
}

/// Keys any test derives from a label: never a real party's.
#[cfg(any(test, feature = "fixture"))]
mod fixture {
    use super::*;
    use lichen_babe::setup::DIGITS;
    use lichen_babe::vsss::deal;
    use lngap_btc::bitcoin::hashes::{sha256, Hash as _};
    use lngap_btc::keys::Seed;
    use lngap_channel::Role;
    use lngap_lamport::winternitz::WotsParams;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    use crate::{INSTANCES, KEPT};

    fn rng(label: &str) -> ChaCha20Rng {
        ChaCha20Rng::from_seed(sha256::Hash::hash(label.as_bytes()).to_byte_array())
    }

    fn seeds(tag: &str) -> Vec<[u8; 32]> {
        (1..=INSTANCES as u64)
            .map(|i| {
                sha256::Hash::hash(&[tag.as_bytes(), &i.to_le_bytes()].concat()).to_byte_array()
            })
            .collect()
    }

    impl HolderKeys {
        /// The fixture holder's keys of the contract labelled `label`: anyone
        /// knowing the label knows them.
        pub fn from_label(label: &str) -> Self {
            let a = Seed::from_label(&format!("{label}/alice"));
            let a2 = Seed::from_label(&format!("{label}/alice/corrected"));
            Self {
                party: PartyKeys::from_seed(Role::User, a.clone()),
                lamport: LamportKey::new(&a, DIGITS),
                adaptor: AdaptorKeys::new(&a, DIGITS),
                corrected: Assertion {
                    b_key: WotsSecret::from_entropy(WotsParams::for_bytes(4), a2.derive_bytes("b")),
                    lamport: LamportKey::new(&a2, DIGITS),
                    adaptor: AdaptorKeys::new(&a2, DIGITS),
                },
            }
        }
    }

    impl HubKeys {
        /// The fixture hub's keys of the contract labelled `label`: anyone
        /// knowing the label knows them, its instances' secrets included.
        pub fn from_label(label: &str) -> Self {
            let h = Seed::from_label(&format!("{label}/hub"));
            let wots = |name: &str, bytes: usize| {
                Wots256Secret::from_entropy(
                    WotsParams::for_bytes(bytes as u32),
                    h.derive_bytes(name),
                )
            };
            let disclosure = |i: usize| format!("disclosure-{i}");
            // Each dealing has its own randomness: the first is unchanged by
            // the second.
            let (dealer, dealing) = deal(DIGITS, INSTANCES - KEPT, &mut rng(label));
            let corrected = format!("{label}/corrected");
            let (dealer2, dealing2) = deal(DIGITS, INSTANCES - KEPT, &mut rng(&corrected));
            Self {
                party: PartyKeys::from_seed(Role::Hub, h.clone()),
                b: wots("b", 4),
                e_plain: wots("e_plain", 32),
                e_counter: wots("e_counter", 32),
                k: wots("k", licash::statement::K_LEN),
                b_corr: wots("b_corr", 4),
                disclosure: std::array::from_fn(|i| {
                    wots(&disclosure(i), licash::disclosure::CHUNK_BYTES[i])
                }),
                sealed: licash::statement::SecretKeys::from_entropy(
                    ["b", "e_plain", "e_counter", "k", "b_corr"].map(|n| h.derive_bytes(n)),
                    std::array::from_fn(|i| h.derive_bytes(&disclosure(i))),
                ),
                full: Dealt {
                    dealer,
                    dealing,
                    seeds: seeds(label),
                },
                corrected: Dealt {
                    dealer: dealer2,
                    dealing: dealing2,
                    seeds: seeds(&corrected),
                },
            }
        }
    }
}
