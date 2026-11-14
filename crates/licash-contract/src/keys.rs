//! Each party's secrets for one contract: its own only. The holder's sign
//! her side of the graph and the challenge's adaptors; the hub's answer a
//! request and deal the instances' input secrets.
//!
//! Each side derives all of them from one secret of the opening's, under
//! [`DERIVATION`]: the holder from her seed and the opening's id, the hub
//! from a secret it draws for that opening alone. A resumed opening derives
//! them again, a hub its dealings and instances too, so the derivation is
//! a stored format: a known-answer test pins it.

use licash::crypto::tagged_hash;
use lichen_babe::reveal::{AdaptorKeys, LamportKey};
use lichen_babe::setup::DIGITS;
use lichen_babe::vsss::{deal, Dealer, Dealing};
use lngap_btc::keys::Seed;
use lngap_channel::{PartyKeys, Role};
use lngap_lamport::winternitz::{Wots256Secret, WotsParams, WotsSecret};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;

use crate::{wire, INSTANCES, KEPT};

/// The version of the derivations below. Another derivation is another
/// version: a stored opening says which it was derived under.
pub const DERIVATION: u8 = 1;

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

impl HolderKeys {
    /// The holder's keys of opening `id`, from her seed: the same on
    /// resuming it, another opening's keys under another id.
    pub fn derive(seed: &[u8; 32], id: &[u8; 32]) -> Self {
        let root = Seed::new(tagged_hash(
            "licash/contract/holder",
            &[&[DERIVATION], seed, id],
        ));
        let full = root.child("full");
        let corrected = root.child("corrected");
        Self {
            party: PartyKeys::from_seed(Role::User, root.child("party")),
            lamport: LamportKey::new(&full, DIGITS),
            adaptor: AdaptorKeys::new(&full, DIGITS),
            corrected: Assertion {
                b_key: WotsSecret::from_entropy(
                    WotsParams::for_bytes(4),
                    corrected.derive_bytes("b"),
                ),
                lamport: LamportKey::new(&corrected, DIGITS),
                adaptor: AdaptorKeys::new(&corrected, DIGITS),
            },
        }
    }

    /// The keys she sends the hub, with her request keys.
    pub fn public(&self, request: licash::request::PublicKeys) -> wire::Keys {
        wire::Keys {
            request,
            lamport: [self.lamport.public(), self.corrected.lamport.public()],
            adaptor: [self.adaptor.public(), self.corrected.adaptor.public()],
            corrected_b: self.corrected.b_key.public().digits,
        }
    }
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

impl HubKeys {
    /// The hub's keys of one opening, from a secret drawn for it alone: two
    /// openings under one secret would deal the same polynomials, and the
    /// shares two choices open would reveal the holder's kept instances.
    pub fn from_secret(secret: &[u8; 32]) -> Self {
        let h = Seed::new(tagged_hash("licash/contract/hub", &[&[DERIVATION], secret]));
        let wots = |name: &str, bytes: usize| {
            Wots256Secret::from_entropy(WotsParams::for_bytes(bytes as u32), h.derive_bytes(name))
        };
        let disclosure = |i: usize| format!("disclosure-{i}");
        let dealt = |stage: &str| {
            let rng = &mut ChaCha20Rng::from_seed(h.derive_bytes(&format!("{stage}/dealing")));
            let (dealer, dealing) = deal(DIGITS, INSTANCES - KEPT, rng);
            let seeds = (1..=INSTANCES)
                .map(|i| h.derive_bytes(&format!("{stage}/instance-{i}")))
                .collect();
            Dealt {
                dealer,
                dealing,
                seeds,
            }
        };
        Self {
            party: PartyKeys::from_seed(Role::Hub, h.child("party")),
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
            full: dealt("full"),
            corrected: dealt("corrected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;
    use lngap_btc::bitcoin::hashes::{sha256, Hash as _};

    fn hex(bytes: &[u8]) -> String {
        sha256::Hash::hash(bytes).to_string()
    }

    /// A resumed opening derives its keys again, a hub its dealings and
    /// instances: what a secret derives must never change.
    #[test]
    fn a_secret_derives_the_same_keys_always() {
        let a = HolderKeys::derive(&[1; 32], &[2; 32]);
        let mut w = Writer::default();
        w.party(&a.party.public());
        let request = licash::request::Signer::derive(&[1; 32], &[3; 32]).public();
        w.bytes(&a.public(request).encode());
        let holder = hex(&w.0);

        let h = HubKeys::from_secret(&[4; 32]);
        let mut w = Writer::default();
        w.party(&h.party.public());
        let wots = [&h.b, &h.e_plain, &h.e_counter, &h.k, &h.b_corr];
        for key in wots.into_iter().chain(&h.disclosure) {
            w.hashes(&key.public().digits);
        }
        for dealt in [&h.full, &h.corrected] {
            w.dealing(&dealt.dealing);
            w.hashes(&dealt.seeds);
        }
        let hub = hex(&w.0);
        assert_eq!(
            [holder.as_str(), hub.as_str()],
            [
                "05223ef11cf943327ac18b51720e209c772f6428ec49ce441bd995a79735b934",
                "fdbec8f3ed5e94a1a5cde5f5b3cf230a9e7cfc5c39c779aa8ff8748dc0805f80",
            ],
            "derivation {DERIVATION} changed"
        );
    }

    /// One opening's keys are no other's: the holder's under another id,
    /// the hub's under another secret.
    #[test]
    fn each_opening_has_keys_of_its_own() {
        let party = |k: &HolderKeys| k.party.public().funding;
        let a = HolderKeys::derive(&[1; 32], &[2; 32]);
        assert_eq!(party(&a), party(&HolderKeys::derive(&[1; 32], &[2; 32])));
        assert_ne!(party(&a), party(&HolderKeys::derive(&[1; 32], &[3; 32])));
        assert_ne!(party(&a), party(&HolderKeys::derive(&[3; 32], &[2; 32])));
        let (h, other) = (
            HubKeys::from_secret(&[4; 32]),
            HubKeys::from_secret(&[5; 32]),
        );
        assert_ne!(h.full.dealing, other.full.dealing);
        assert_ne!(h.full.dealing, h.corrected.dealing);
        assert_ne!(h.full.seeds, h.corrected.seeds);
        assert_ne!(h.party.public().funding, other.party.public().funding);
    }

    /// The statement's response keys are the keys the hub signs with.
    #[test]
    fn the_sealed_keys_are_the_hubs_wots_keys() {
        let h = HubKeys::from_secret(&[5; 32]);
        let sealed = h.sealed.public();
        let digits = |k: &Wots256Secret| k.public().digits;
        assert_eq!(sealed.b.0, digits(&h.b));
        assert_eq!(sealed.e_plain.0, digits(&h.e_plain));
        assert_eq!(sealed.e_counter.0, digits(&h.e_counter));
        assert_eq!(sealed.k.0, digits(&h.k));
        assert_eq!(sealed.b_corr.0, digits(&h.b_corr));
        for (s, k) in sealed.disclosure.iter().zip(&h.disclosure) {
            assert_eq!(s.0, digits(k));
        }
    }
}
