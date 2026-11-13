//! A Lightning-compatible HTLC on Bitcoin, for interoperability tests: a
//! taproot output with an unspendable (NUMS) internal key and two leaves,
//! `claim`, `OP_SIZE 32 OP_EQUALVERIFY OP_SHA256 <hash> OP_EQUALVERIFY
//! <claim> OP_CHECKSIG`, and `refund`, `<timeout> OP_CLTV OP_DROP <refund>
//! OP_CHECKSIG`. The size check keeps the preimage 32 bytes.

use anyhow::{Context, Result};
use bitcoincore_rpc::json::AddressType;
use bitcoincore_rpc::RpcApi;
use licash::spend::lock_hash;
use lngap_btc::bitcoin::opcodes::all::{
    OP_CHECKSIG, OP_CLTV, OP_DROP, OP_EQUALVERIFY, OP_SHA256, OP_SIZE,
};
use lngap_btc::bitcoin::script::Builder;
use lngap_btc::bitcoin::{Amount, OutPoint, Transaction, TxOut, XOnlyPublicKey};
use lngap_btc::keys::nums_point;
use lngap_btc::regtest::Regtest;
use lngap_btc::secp256k1::Keypair;
use lngap_btc::sighash::sign_tapscript;
use lngap_btc::taptree::{Leaf, TapTree};
use lngap_btc::tx::{build_spend, Timelock};
use lngap_btc::witness::tapscript_witness;

/// `claim` takes the coin with the preimage of `hash`, `refund` from height
/// `timeout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Htlc {
    pub hash: [u8; 32],
    pub claim: [u8; 32],
    pub refund: [u8; 32],
    pub timeout: u32,
    pub sats: u64,
}

/// The HTLC's taproot tree.
pub fn tree(h: &Htlc) -> Result<TapTree> {
    let key = |k: &[u8; 32]| XOnlyPublicKey::from_slice(k).context("an x-only key");
    let claim = Builder::new()
        .push_opcode(OP_SIZE)
        .push_int(32)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SHA256)
        .push_slice(h.hash)
        .push_opcode(OP_EQUALVERIFY)
        .push_x_only_key(&key(&h.claim)?)
        .push_opcode(OP_CHECKSIG)
        .into_script();
    let refund = Builder::new()
        .push_int(i64::from(h.timeout))
        .push_opcode(OP_CLTV)
        .push_opcode(OP_DROP)
        .push_x_only_key(&key(&h.refund)?)
        .push_opcode(OP_CHECKSIG)
        .into_script();
    TapTree::with_internal_key(
        nums_point(),
        vec![
            Leaf::new("claim", claim, Timelock::NONE),
            Leaf::new("refund", refund, Timelock::cltv(h.timeout)),
        ],
    )
}

/// A funded HTLC: its terms (as funded) and coin.
#[derive(Clone, Debug)]
pub struct Funded {
    pub htlc: Htlc,
    pub outpoint: OutPoint,
    pub txout: TxOut,
}

/// Fund `h` from the node's wallet and confirm it.
pub fn fund(rt: &Regtest, h: &Htlc) -> Result<Funded> {
    let (outpoint, txout) = rt.fund(&tree(h)?.script_pubkey(), Amount::from_sat(h.sats))?;
    Ok(Funded {
        htlc: h.clone(),
        outpoint,
        txout,
    })
}

/// What is confirmed at `outpoint`, read as an HTLC of `terms`' script:
/// Spend `f` through `leaf` to a fresh wallet address, signed by `kp`,
/// `args` before the signature on the stack, at 2 sat/vB.
fn spend(
    rt: &Regtest,
    f: &Funded,
    leaf: &str,
    kp: &Keypair,
    args: &[Vec<u8>],
) -> Result<Transaction> {
    let tree = tree(&f.htlc)?;
    let l = tree.leaf(leaf)?;
    let control = tree.control_block(leaf)?;
    let to = rt
        .rpc
        .get_new_address(None, Some(AddressType::Bech32m))?
        .assume_checked()
        .script_pubkey();
    let build = |fee: u64| -> Result<Transaction> {
        let out = TxOut {
            value: f
                .txout
                .value
                .checked_sub(Amount::from_sat(fee))
                .context("the HTLC can't pay the fee")?,
            script_pubkey: to.clone(),
        };
        let mut tx = build_spend(f.outpoint, &l.timelock, vec![out]);
        let sig = sign_tapscript(kp, &tx, 0, std::slice::from_ref(&f.txout), &l.script)?;
        let mut w = vec![sig.as_ref().to_vec()];
        w.extend_from_slice(args);
        tx.input[0].witness = tapscript_witness(&w, &l.script, &control);
        Ok(tx)
    };
    let vsize = build(0)?.vsize() as u64;
    build(2 * (vsize + 10))
}

/// The claim: the preimage, then the claim key's signature.
pub fn claim_tx(
    rt: &Regtest,
    f: &Funded,
    kp: &Keypair,
    preimage: &[u8; 32],
) -> Result<Transaction> {
    spend(rt, f, "claim", kp, &[preimage.to_vec()])
}

/// The refund, final from the timeout.
pub fn refund_tx(rt: &Regtest, f: &Funded, kp: &Keypair) -> Result<Transaction> {
    spend(rt, f, "refund", kp, &[])
}

/// The preimage a claim of `h` shows in its witness, as the other party
/// reads it from the chain.
pub fn preimage_of(tx: &Transaction, h: &Htlc) -> Option<[u8; 32]> {
    let tree = tree(h).ok()?;
    let w = &tx.input.first()?.witness;
    if tree.identify_leaf(w)?.name != "claim" || w.len() != 4 {
        return None;
    }
    let s: [u8; 32] = w.nth(1)?.try_into().ok()?;
    (lock_hash(&s) == h.hash).then_some(s)
}

// -- scenarios ---------------------------------------------------------------
