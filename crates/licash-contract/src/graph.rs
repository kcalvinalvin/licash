//! The graph both parties build: Lichen's hub-proves graph
//! (`lichen_babe::hp`) of Base's terms and parties, the holder's one-time
//! keys and the hub's prover keys, and the kept instances' hashlocks.
//!
//! The contract output (F's output [`FUNDING_VOUT`]) depends only on
//! Base and the one-time keys for `b` and `E`, not on the hub's prover
//! keys or the hashlocks: F can be funded before the instances are in.
//!
//! The statement is fixed by Base alone ([`journal`], [`kind`]): the
//! holder garbles for it as soon as she has Base.

use anyhow::{ensure, Result};
use licash::base::{Base, GraphTerms, Party};
use lichen_babe::hp::{HubProves, Spec, Windows};
use lichen_babe::reveal::LamportPub;
use lichen_babe::setup::{Hash, DIGITS, VALUES};
use lichen_babe::statement::Kind;
use lichen_btc::bitcoin::secp256k1::XOnlyPublicKey;
use lichen_btc::bitcoin::{Amount, ScriptBuf, TxOut};
use lichen_btc::winternitz::{WotsParams, WotsPublic};
use lichen_contract::payout::Terms;
use lichen_contract::{ChannelParams, PartyPubKeys};
use lichen_statement::JOURNAL_LEN;

/// F's output the graph spends, as the statement reads it.
pub const FUNDING_VOUT: u32 = 0;

/// The holder's one-time keys: for `b`, which her payout's bits read,
/// and for `E`, her claim's digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderPub {
    pub b_key: WotsPublic,
    pub e_key: WotsPublic,
}

/// The hub's keys as the prover: its one-time key for `b′`, its Lamport
/// key for π1 and its adaptor keys for her challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubPub {
    pub b_key: WotsPublic,
    pub lamport: LamportPub,
    pub adaptor: Vec<[XOnlyPublicKey; VALUES]>,
}

fn wots_of(key: &WotsPublic, bytes: u32, what: &str) -> Result<()> {
    let params = WotsParams::for_bytes(bytes);
    ensure!(
        key.params == params && key.digits.len() == params.total_digits() as usize,
        "{what} of other parameters"
    );
    Ok(())
}

impl HolderPub {
    pub fn check(&self) -> Result<()> {
        wots_of(&self.b_key, 4, "a key for b")?;
        wots_of(&self.e_key, 32, "a key for E")
    }
}

impl HubPub {
    pub fn check(&self) -> Result<()> {
        wots_of(&self.b_key, 4, "a key for b′")?;
        ensure!(
            self.lamport.0.len() == DIGITS && self.adaptor.len() == DIGITS,
            "a Lamport and an adaptor key set per digit of π1"
        );
        Ok(())
    }
}

/// What the instances are garbled for: a genuine proof by Base's program
/// (the hub's statement's guest), or, under mock trust, anyone's.
pub fn kind(base: &Base, mock: bool) -> Kind {
    match mock {
        true => Kind::Mock,
        false => Kind::Risc0 {
            image: base.parts().recipe.program,
        },
    }
}

/// The statement's journal, fixed by Base.
pub fn journal(base: &Base) -> [u8; JOURNAL_LEN] {
    licash_hp::contract(base).journal()
}

pub fn party(keys: &PartyPubKeys) -> Party {
    Party {
        funding: keys.funding.serialize(),
        payment: keys.payment.serialize(),
        delayed: keys.delayed.serialize(),
        payout_script: keys.payout_spk.to_bytes(),
    }
}

pub fn party_pub(p: &Party) -> Result<PartyPubKeys> {
    Ok(PartyPubKeys {
        funding: XOnlyPublicKey::from_slice(&p.funding)?,
        payment: XOnlyPublicKey::from_slice(&p.payment)?,
        delayed: XOnlyPublicKey::from_slice(&p.delayed)?,
        payout_spk: ScriptBuf::from_bytes(p.payout_script.clone()),
    })
}

/// Base's graph terms as Lichen's payout reads them.
pub fn terms(g: &GraphTerms) -> Terms {
    Terms {
        id: g.id,
        unit: Amount::from_sat(g.unit_sats),
        bits: g.bits.into(),
        low_bits: g.low_bits.into(),
        deposit: g.deposit_units,
        t_close: g.close_height,
        reserve_alice: Amount::from_sat(g.reserve_alice),
        reserve_hub: Amount::from_sat(g.reserve_hub),
    }
}

/// The windows of Base's terms: `w_H` the hub's response, `k` her
/// publication, `w_A` its assertion, `Δ2` her challenge, `Δ1` its answer.
pub fn windows(g: &GraphTerms) -> Windows {
    Windows {
        w_h: g.response_window,
        k: g.publication_window,
        w_a: g.assert_window,
        t_a: g.request_height,
        delta2: g.challenge_window,
        delta1: g.answer_window,
    }
}

/// The graph of `base` with the holder's and the hub's keys and the kept
/// instances' hashlocks, in the order of the instances kept.
pub fn build(base: &Base, holder: &HolderPub, hub: &HubPub, hmsg: Vec<Hash>) -> Result<HubProves> {
    let p = base.parts();
    let g = &p.graph;
    HubProves::new(Spec {
        terms: terms(g),
        // The graph reads neither the channel's funding amount nor its
        // deadline margin (Base's, for the request's latest height).
        params: ChannelParams {
            funding_amount: Amount::ZERO,
            to_self_delay: g.to_self_delay,
            commit_fee: Amount::from_sat(g.commit_fee),
            presign_fee: Amount::from_sat(g.presign_fee),
            dust: Amount::from_sat(g.dust),
            delta: g.delta,
            delta_prime: g.delta_prime,
            deadline_margin: 0,
        },
        pubs: [party_pub(&p.parties[0])?, party_pub(&p.parties[1])?],
        b_key: holder.b_key.clone(),
        e_key: holder.e_key.clone(),
        claim_key: XOnlyPublicKey::from_slice(&p.claim_key)?,
        hub_b: hub.b_key.clone(),
        lamport: hub.lamport.clone(),
        adaptor: hub.adaptor.clone(),
        hmsg,
        windows: windows(g),
    })
}

/// Hashlocks for a graph built before the instances are kept, one per
/// kept instance (the leaves must differ): its contract output is the
/// graph's.
pub fn draft_hmsg(kept: u16) -> Vec<Hash> {
    (0..kept).map(|i| [i as u8 ^ 0xa5; 32]).collect()
}

/// F's output the graph spends.
pub fn contract_output(graph: &HubProves) -> Result<TxOut> {
    Ok(TxOut {
        value: graph.value(),
        script_pubkey: graph.tree()?.script_pubkey(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{HolderKeys, HubKeys};

    /// F's output stands before the instances are in: neither the hub's
    /// prover keys nor the hashlocks change it.
    #[test]
    fn the_contract_output_is_of_base_and_her_one_time_keys_only() {
        let base = crate::opening::testing::base(&[3; 32]);
        let holder = HolderKeys::derive(&[1; 32], &[3; 32]).public();
        let hub = HubKeys::from_secret(&[2; 32]).public();
        let other = HubKeys::from_secret(&[9; 32]).public();
        let out = |hub: &HubPub, hmsg| {
            contract_output(&build(&base, &holder, hub, hmsg).unwrap()).unwrap()
        };
        let draft = out(&hub, draft_hmsg(2));
        assert_eq!(draft, out(&other, vec![[7; 32], [8; 32]]));
        assert_eq!(
            draft.value.to_sat(),
            base.parts().graph.graph_value().unwrap()
        );
        let mut theirs = holder.clone();
        theirs.b_key = HolderKeys::derive(&[1; 32], &[4; 32]).public().b_key;
        assert_ne!(
            draft,
            contract_output(&build(&base, &theirs, &hub, draft_hmsg(2)).unwrap()).unwrap()
        );
    }
}
