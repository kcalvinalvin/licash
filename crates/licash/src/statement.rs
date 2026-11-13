//! Holder statement: what the BABE assertion's proof establishes.
//!
//! The holder guest evaluates [`StatementInput::run`]. Its journal is the one
//! fixed when the instances were garbled, `c ‖ D ‖ H(pk_H ‖ O_D)`, so a proof
//! opens only this graph's instances, and only for one assertion: `c = 0`
//! pays `b` (a Plain response, or a counter whose proof does not verify),
//! `c = 1` pays a proven Corrected counter's owed value, which the Hub signed
//! as `b_corr` and Script ties to the holder's corrected payout.
//!
//! Everything the statement reads is either private holder evidence or public
//! chain data authenticated from the opening's checkpoint: R's witness gives
//! `b` and `E`, and P must be confirmed early enough for the Hub to counter.

use crate::{
    base::Base,
    chain::{Chain, HeaderBytes, Inclusion, WitnessInclusion},
    disclosure::{self, SignedDisclosures},
    opening::FrozenPrefix,
    recovery::{prove_holder, CounterJournal, HolderWitness, Publication},
    wots::{self, Hash, Hashes, B_DIGITS, MSG_DIGITS},
    Error,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `K = mask ‖ owed ‖ 0^24 ‖ seal`: the Hub signs its counter's journal
/// values and proof seal with its one-time `K` key in `respond_counter`. The
/// journal's disclosure digest is not in `K`: it is recomputed from the
/// chunks the same transaction publishes.
pub const COUNTER_HEAD: usize = 32;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// The Hub's `K` for a counter journal and its proof seal.
pub fn counter_k(journal: &CounterJournal, seal: &[u8]) -> Result<Vec<u8>, Error> {
    if COUNTER_HEAD + seal.len() != K_LEN {
        return Err(bad("counter seal length"));
    }
    let mut k = journal.mask.to_be_bytes().to_vec();
    k.extend(journal.owed.to_be_bytes());
    k.resize(COUNTER_HEAD, 0);
    k.extend(seal);
    Ok(k)
}

/// Rebuild the counter journal `K` claims for this opening and request.
pub fn counter_journal(
    final_digest: &[u8; 32],
    claim_digest: &[u8; 32],
    k: &[u8],
    disclosure: [u8; 32],
) -> Result<CounterJournal, Error> {
    if k.len() != K_LEN || k[8..COUNTER_HEAD].iter().any(|byte| *byte != 0) {
        return Err(bad("counter K layout"));
    }
    Ok(CounterJournal {
        final_digest: *final_digest,
        claim_digest: *claim_digest,
        mask: u32::from_be_bytes(k[..4].try_into().expect("four bytes")),
        owed: u32::from_be_bytes(k[4..8].try_into().expect("four bytes")),
        disclosure,
    })
}

/// The Hub's response as `respond` or `respond_counter` carried it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum HubResponse {
    Plain {
        e_sig: Hashes,
    },
    Counter {
        e_sig: Hashes,
        k: Vec<u8>,
        k_sig: Hashes,
        b_corr: u32,
        corr_sig: Hashes,
        /// The disclosure chunks `respond_counter` published, as signed.
        publication: Box<SignedDisclosures>,
    },
}

/// Public chain evidence from the opening's checkpoint: headers to the tip,
/// R's witness inclusion and P's inclusion.
#[derive(Clone)]
pub struct Observed {
    pub headers: Vec<HeaderBytes>,
    pub request_height: u32,
    pub request: WitnessInclusion,
    pub publication_height: u32,
    pub publication: Inclusion,
}

#[derive(Clone)]
pub struct StatementInput {
    pub c: u32,
    pub base: Base,
    pub prefix: FrozenPrefix,
    pub observed: Observed,
    pub response: HubResponse,
    pub holder: HolderWitness,
}

impl StatementInput {
    /// Returns the journal and the value this assertion pays. Fails when the
    /// other assertion is the one this response and evidence select.
    pub fn run(
        &self,
        counter_verify: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<(Journal, u32), Error> {
        let parts = self.base.parts();
        let d = self.base.final_digest(&self.prefix)?;
        let terms = &parts.chain;
        let chain = Chain::verify(
            terms.checkpoint.clone(),
            &terms.checkpoint.commitment(),
            self.observed.headers.clone(),
            terms.max_headers as usize,
        )?;
        let o = &self.observed;
        let max_tx = parts.limits.max_transaction_bytes as usize;
        let included = o.request.verify(&chain, o.request_height, max_tx)?;
        let request = self.prefix.request_from_included(&included)?;
        let posted = o.publication.verify(&chain, o.publication_height, max_tx)?;
        let publication = Publication::from_transaction(
            &parts.keyset,
            parts.limits.max_authorizations as usize,
            posted.transaction(),
        )?;
        publication.bind(&self.base, &d, &request)?;
        let deep = |height: u32, depth: u32| {
            chain
                .confirmations(height)
                .is_some_and(|n| n >= u64::from(depth))
        };
        // The Hub must observe P at depth, with margin, inside its window.
        let observed_by = u64::from(o.publication_height)
            + u64::from(terms.publication_depth)
            + u64::from(terms.observation_margin);
        if !deep(o.request_height, terms.request_depth)
            || !deep(o.publication_height, terms.publication_depth)
            || o.publication_height < o.request_height
            || observed_by > u64::from(o.request_height) + u64::from(parts.graph.response_window)
        {
            return Err(bad(
                "claim publication was not observable before the response deadline",
            ));
        }
        let keys = parts.response_keys.keys();
        let e = &request.claim_digest;
        let (c, paid) = match &self.response {
            HubResponse::Plain { e_sig } => {
                if !wots::verify_msg(&keys.e_plain, e_sig, e) {
                    return Err(bad("plain response signature"));
                }
                (0, request.amount)
            }
            HubResponse::Counter {
                e_sig,
                k,
                k_sig,
                b_corr,
                corr_sig,
                publication,
            } => {
                if !wots::verify_msg(&keys.e_counter, e_sig, e)
                    || !wots::verify_msg(&keys.k, k_sig, k)
                    || !wots::verify_b(&keys.b_corr, corr_sig, *b_corr)
                    || !publication.verify(&keys.disclosure)
                {
                    return Err(bad("counter response signatures"));
                }
                // A counter that withholds or falsifies a secret it must
                // publish commits another digest, so its seal fails here. A K
                // that is not a journal proves nothing either: were it an
                // error, the Hub could sign one and leave no statement at all.
                let proven = counter_journal(d.as_bytes(), e, k, publication.disclosures.digest())
                    .is_ok_and(|journal| {
                        journal.owed == *b_corr
                            && journal.owed <= request.amount
                            && counter_verify(&k[COUNTER_HEAD..], &journal.to_bytes())
                    });
                // An unproven, misreported or malformed counter is a Plain response.
                if proven {
                    (1, *b_corr)
                } else {
                    (0, request.amount)
                }
            }
        };
        if c != self.c {
            return Err(bad(if c == 1 {
                "countered"
            } else {
                "no proven counter"
            }));
        }
        prove_holder(&self.base, &d, &request, &publication, &self.holder)?;
        Ok((
            Journal {
                c,
                d: *d.as_bytes(),
                keys: keys.digest(&parts.deposit),
            },
            paid,
        ))
    }
}

// The hub's one-time response keys and the journal fixed at opening.

pub const SEAL_LEN: usize = 128;
pub const K_LEN: usize = 32 + SEAL_LEN;
// 320 message digits and four checksum digits (320 * 15 = 4800).
pub const K_DIGITS: usize = 324;

/// The hub's one-time keys for `respond`, fixed at opening. `b_corr`
/// signs what a mask pays (a Corrected contract's `respond_counter`); every
/// opening has it, so the digest has one layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubKeys {
    pub b: Hashes,
    pub e_plain: Hashes,
    pub e_counter: Hashes,
    pub k: Hashes,
    pub b_corr: Hashes,
    pub disclosure: [Hashes; 5],
}

impl HubKeys {
    /// `H(pk_H ‖ O_D)` as the journal commits it (Lichen's tag, so that the
    /// journal is the one its setup expects).
    pub fn digest(&self, deposit: &[u8]) -> Hash {
        let mut h = Sha256::new();
        h.update(b"lichen/babe/pk_H/exit");
        for k in self
            .b
            .0
            .iter()
            .chain(&self.e_plain.0)
            .chain(&self.e_counter.0)
            .chain(&self.k.0)
            .chain(&self.b_corr.0)
            .chain(self.disclosure.iter().flat_map(|key| &key.0))
        {
            h.update(k);
        }
        h.update(deposit);
        h.finalize().into()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SecretKeys {
    b: wots::SecretKey,
    plain: wots::SecretKey,
    counter: wots::SecretKey,
    k: wots::SecretKey,
    b_corr: wots::SecretKey,
    disclosure: [wots::SecretKey; 5],
}

impl SecretKeys {
    pub fn random() -> Self {
        Self {
            b: wots::SecretKey::random(B_DIGITS),
            plain: wots::SecretKey::random(MSG_DIGITS),
            counter: wots::SecretKey::random(MSG_DIGITS),
            k: wots::SecretKey::random(K_DIGITS),
            b_corr: wots::SecretKey::random(B_DIGITS),
            disclosure: std::array::from_fn(|i| {
                wots::SecretKey::random(disclosure::CHUNK_DIGITS[i])
            }),
        }
    }

    /// Entropy per key: `b`, plain `E`, counter `E`, `K`, `b_corr`.
    pub fn from_entropy(entropy: [[u8; 32]; 5], disclosure_entropy: [[u8; 32]; 5]) -> Self {
        let [b, plain, counter, k, b_corr] = entropy;
        Self {
            b: wots::SecretKey::from_entropy(B_DIGITS, b),
            plain: wots::SecretKey::from_entropy(MSG_DIGITS, plain),
            counter: wots::SecretKey::from_entropy(MSG_DIGITS, counter),
            k: wots::SecretKey::from_entropy(K_DIGITS, k),
            b_corr: wots::SecretKey::from_entropy(B_DIGITS, b_corr),
            disclosure: std::array::from_fn(|i| {
                wots::SecretKey::from_entropy(disclosure::CHUNK_DIGITS[i], disclosure_entropy[i])
            }),
        }
    }

    pub fn public(&self) -> HubKeys {
        HubKeys {
            b: self.b.public(),
            e_plain: self.plain.public(),
            e_counter: self.counter.public(),
            k: self.k.public(),
            b_corr: self.b_corr.public(),
            disclosure: std::array::from_fn(|i| self.disclosure[i].public()),
        }
    }
}

/// `c ‖ D ‖ H(pk_H ‖ O_D)`, 68 bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub c: u32,
    pub d: Hash,
    pub keys: Hash,
}

impl Journal {
    pub const LEN: usize = 68;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.c.to_le_bytes());
        out.extend_from_slice(&self.d);
        out.extend_from_slice(&self.keys);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Journal, Error> {
        if b.len() != Self::LEN {
            return Err(Error::Protocol("journal length".into()));
        }
        Ok(Journal {
            c: u32::from_le_bytes(b[..4].try_into().unwrap()),
            d: b[4..36].try_into().unwrap(),
            keys: b[36..].try_into().unwrap(),
        })
    }
}
