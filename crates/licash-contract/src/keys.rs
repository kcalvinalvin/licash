//! Each party's secrets for one contract: its own only.
//!
//! The holder's sign her side of the graph, her request (`b` and `E`, one
//! time each) and her claim's publication, and garble her instances; the
//! hub's sign its side, its response (`b′`, one time) and its assertion
//! (a Lamport key for π1), and pre-sign her challenge (adaptor keys).
//!
//! Each side derives all of them from one secret of the opening's, under
//! [`DERIVATION`]: the holder from her seed and the opening's id, the hub
//! from a secret it draws for that opening alone. A resumed opening
//! derives them again (her instances too, from her garbler's seed), so the
//! derivation is a stored format: a known-answer test pins it.
//!
//! No key here is any wallet root's: her claim key is the contract's own,
//! so a claim names no note owner beyond the contract.

use licash::crypto::tagged_hash;
use lichen_babe::reveal::{AdaptorKeys, LamportKey};
use lichen_babe::setup::DIGITS;
use lichen_btc::bitcoin::key::Keypair;
use lichen_btc::bitcoin::secp256k1::XOnlyPublicKey;
use lichen_btc::keys::{xonly, Seed};
use lichen_contract::{PartyKeys, Role};
use lichen_wots::{WotsParams, WotsSecret};

use crate::graph::{HolderPub, HubPub};

/// The version of the derivations below. Another derivation is another
/// version: a stored opening says which it was derived under.
pub const DERIVATION: u8 = 2;

/// The holder's secrets for one contract.
pub struct HolderKeys {
    pub party: PartyKeys,
    /// Her one-time keys for `b` (4 bytes: the payout's bits read it) and
    /// `E` (32 bytes, her claim's digest).
    pub b: WotsSecret,
    pub e: WotsSecret,
    /// The key her claim's publication is signed with.
    pub claim: Keypair,
    /// Her instances' seed: her dealing and every instance come from it.
    pub garbler: Seed,
}

impl HolderKeys {
    /// The holder's keys of opening `id`, from her seed: the same on
    /// resuming it, another opening's keys under another id.
    pub fn derive(seed: &[u8; 32], id: &[u8; 32]) -> Self {
        let root = Seed::new(tagged_hash(
            "licash/contract/holder",
            &[&[DERIVATION], seed, id],
        ));
        Self {
            party: PartyKeys::from_seed(Role::User, root.child("party")),
            b: WotsSecret::from_entropy(WotsParams::for_bytes(4), root.derive_bytes("b")),
            e: WotsSecret::from_entropy(WotsParams::for_bytes(32), root.derive_bytes("e")),
            claim: root.keypair("claim"),
            garbler: root.child("garbler"),
        }
    }

    /// Her one-time keys' public halves, which the graph compiles with.
    pub fn public(&self) -> HolderPub {
        HolderPub {
            b_key: self.b.public(),
            e_key: self.e.public(),
        }
    }

    pub fn claim_key(&self) -> XOnlyPublicKey {
        xonly(&self.claim)
    }
}

/// The hub's secrets for one contract.
pub struct HubKeys {
    pub party: PartyKeys,
    /// Its one-time key for `b′`.
    pub b: WotsSecret,
    /// Its Lamport key for π1, in 2-bit digits, and its adaptor keys, one
    /// a digit and value.
    pub lamport: LamportKey,
    pub adaptor: AdaptorKeys,
}

impl HubKeys {
    /// The hub's keys of one opening, from a secret drawn for it alone: a
    /// Lamport key signs once, and two openings under one secret would
    /// share it.
    pub fn from_secret(secret: &[u8; 32]) -> Self {
        let root = Seed::new(tagged_hash("licash/contract/hub", &[&[DERIVATION], secret]));
        let prover = root.child("prover");
        Self {
            party: PartyKeys::from_seed(Role::Hub, root.child("party")),
            b: WotsSecret::from_entropy(WotsParams::for_bytes(4), root.derive_bytes("b")),
            lamport: LamportKey::new(&prover, DIGITS),
            adaptor: AdaptorKeys::new(&prover, DIGITS),
        }
    }

    pub fn public(&self) -> HubPub {
        HubPub {
            b_key: self.b.public(),
            lamport: self.lamport.public(),
            adaptor: self.adaptor.public(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;
    use lichen_btc::bitcoin::hashes::{sha256, Hash as _};

    fn hex(bytes: &[u8]) -> String {
        sha256::Hash::hash(bytes).to_string()
    }

    /// A resumed opening derives its keys again, her instances too: what
    /// a secret derives must never change.
    #[test]
    fn a_secret_derives_the_same_keys_always() {
        let a = HolderKeys::derive(&[1; 32], &[2; 32]);
        let mut w = Writer::default();
        w.party(&a.party.public());
        w.holder_pub(&a.public());
        w.bytes(&a.claim_key().serialize());
        w.bytes(&a.garbler.derive_bytes("probe"));
        let holder = hex(&w.0);

        let h = HubKeys::from_secret(&[4; 32]);
        let mut w = Writer::default();
        w.party(&h.party.public());
        w.hub_pub(&h.public());
        let hub = hex(&w.0);
        assert_eq!(
            [holder.as_str(), hub.as_str()],
            [
                "1782c609cc8b8dd029aca152570717e3de400cc38d8cddddd4e30351e1e64267",
                "1cd67206c983e810b11ffd28b82f1aa26dfb7b45a3a4c623fea26e0b104f269c"
            ],
            "derivation {DERIVATION} changed"
        );
    }

    /// One opening's keys are no other's: the holder's under another id,
    /// the hub's under another secret.
    #[test]
    fn each_opening_has_keys_of_its_own() {
        let claim = |k: &HolderKeys| k.claim_key();
        let a = HolderKeys::derive(&[1; 32], &[2; 32]);
        assert_eq!(claim(&a), claim(&HolderKeys::derive(&[1; 32], &[2; 32])));
        assert_ne!(claim(&a), claim(&HolderKeys::derive(&[1; 32], &[3; 32])));
        assert_ne!(claim(&a), claim(&HolderKeys::derive(&[3; 32], &[2; 32])));
        assert_ne!(claim(&a), a.party.public().payment);
        let (h, other) = (
            HubKeys::from_secret(&[4; 32]),
            HubKeys::from_secret(&[5; 32]),
        );
        assert_ne!(h.public().lamport, other.public().lamport);
        assert_ne!(h.party.public().funding, other.party.public().funding);
    }
}
