//! The holder's transactions of a force close on a silent hub, from the
//! contract she keeps ([`Opening`]) and her keys ([`HolderKeys`]): R, her
//! one-time request in the presigned `request`; the fee inputs she adds,
//! of coins at her payment key (R's anchor among them), to R's child P and
//! to `request_timeout`; and her sweep of the payout once `delta` passed.
//!
//! What is hers to sign here is signed here. Which coins pay the fees,
//! when each transaction goes out and what the chain shows of them are
//! the driver's.

use anyhow::{ensure, Context, Result};
use licash::request::{Request, SignedRequest};
use lngap_btc::bitcoin::absolute::LockTime;
use lngap_btc::bitcoin::{
    relative, Amount, FeeRate, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
};
use lngap_btc::sighash::sign_tapscript;
use lngap_btc::taptree::TapTree;
use lngap_btc::tx::build_tx;
use lngap_btc::witness::tapscript_witness;
use lngap_lamport::winternitz::{WotsParams, WotsSig};
use lngap_sessions::session::{bit_tree, reserve_tree};
use lngap_sessions::skeleton::{anchor_tree_for, sig_name, Presign};

use crate::graph::Babe;
use crate::keys::HolderKeys;
use crate::opening::Opening;

/// The presigned `request` link.
pub const REQUEST: &str = "request";
/// The terminal paying her if the hub lets its window pass.
pub const TIMEOUT: &str = "request_timeout";

/// R's witness: her signatures of `E` (67 digits) then of `b` (10), two
/// items a digit, below both parties' signatures.
const E_ITEMS: usize = 134;
const B_ITEMS: usize = 20;

pub(crate) fn presign<'a>(o: &'a Opening, name: &str) -> Result<&'a Presign> {
    o.set
        .iter()
        .find(|p| p.name == name)
        .with_context(|| format!("no {name} in the contract's graph"))
}

/// Presigned `name` of `o`, `wire` below both parties' signatures.
pub(crate) fn finished(o: &Opening, name: &str, wire: Vec<Vec<u8>>) -> Result<Transaction> {
    let p = presign(o, name)?;
    let n = sig_name(name, 0);
    let hub = o
        .signed
        .hub
        .get(&n)
        .with_context(|| format!("no hub signature of {name}"))?;
    let alice = o
        .signed
        .alice
        .get(&n)
        .or_else(|| o.kept.get(&n))
        .with_context(|| format!("no signature of hers of {name}"))?;
    Ok(p.finish(wire, hub, alice))
}

/// The blocks `sequence` waits, a relative lock in blocks.
pub(crate) fn blocks(sequence: Sequence) -> Result<u32> {
    match sequence.to_relative_lock_time() {
        Some(relative::LockTime::Blocks(h)) => Ok(u32::from(h.value())),
        None => Ok(0),
        Some(other) => anyhow::bail!("a relative lock of {other}, not in blocks"),
    }
}

/// R's id: the same signed or not.
pub fn request_txid(o: &Opening) -> Result<Txid> {
    Ok(presign(o, REQUEST)?.tx.compute_txid())
}

/// The coin R spends, F's contract output, and the blocks after F's
/// that R waits.
pub fn request_input(o: &Opening) -> Result<(OutPoint, TxOut, u32)> {
    let p = presign(o, REQUEST)?;
    let input = &p.tx.input[0];
    Ok((
        input.previous_output,
        p.prevout.clone(),
        blocks(input.sequence)?,
    ))
}

/// R: `signed`, her one-time signature of her request, in the presigned
/// `request` link.
pub fn request(o: &Opening, signed: &SignedRequest) -> Result<Transaction> {
    finished(o, REQUEST, signed.pairs().to_vec())
}

/// What R, a spend of F's contract output by `request`, signed, read
/// under the frozen request keys, and her signature of `b` as the
/// payout's bits read it.
pub fn read_request(o: &Opening, r: &Transaction) -> Result<(Request, WotsSig)> {
    let (coin, _, _) = request_input(o)?;
    ensure!(
        r.compute_txid() == request_txid(o)? && r.input[0].previous_output == coin,
        "not this contract's request"
    );
    let items: Vec<Vec<u8>> = r.input[0].witness.iter().map(<[u8]>::to_vec).collect();
    let leaf = o.prefix.leaf();
    let request = leaf
        .keys()
        .decode_witness(&items, leaf.script(), leaf.control())?;
    let b: Vec<Vec<u8>> = items[E_ITEMS..E_ITEMS + B_ITEMS]
        .iter()
        .rev()
        .cloned()
        .collect();
    let b = WotsSig::from_consumption_order(WotsParams::for_bytes(4), &b)?;
    ensure!(
        b.message() == request.amount.to_be_bytes(),
        "her signature of b reads another amount"
    );
    Ok((request, b))
}

/// Her fee coins' output, R's anchor's too: her payment key's.
pub fn fee_script(keys: &HolderKeys) -> Result<ScriptBuf> {
    Ok(fee_tree(keys)?.script_pubkey())
}

fn fee_tree(keys: &HolderKeys) -> Result<TapTree> {
    anchor_tree_for(&keys.party.payment.x_only_public_key().0)
}

/// R's anchor, hers to spend at once: its coin and output.
pub fn anchor(r: &Transaction, keys: &HolderKeys) -> Result<(OutPoint, TxOut)> {
    let script = fee_script(keys)?;
    let vout = r
        .output
        .iter()
        .position(|o| o.script_pubkey == script)
        .context("R has no anchor of hers")?;
    let coin = OutPoint {
        txid: r.compute_txid(),
        vout: vout as u32,
    };
    Ok((coin, r.output[vout].clone()))
}

/// A fee input of `coin`, at her [`fee_script`], unsigned: replaceable.
pub fn fee_input(coin: OutPoint) -> TxIn {
    TxIn {
        previous_output: coin,
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        ..Default::default()
    }
}

/// Sign input `index` of `tx`, a coin at her [`fee_script`]: `prevouts`
/// the output each input spends.
pub fn sign_fee_input(
    keys: &HolderKeys,
    tx: &mut Transaction,
    index: usize,
    prevouts: &[TxOut],
) -> Result<()> {
    let tree = fee_tree(keys)?;
    ensure!(
        prevouts.len() == tx.input.len(),
        "{} coins for {} inputs",
        prevouts.len(),
        tx.input.len()
    );
    ensure!(
        prevouts.get(index).map(|o| &o.script_pubkey) == Some(&tree.script_pubkey()),
        "input {index} is no coin at her payment key"
    );
    let leaf = tree.leaf("fee")?;
    let signature = sign_tapscript(&keys.party.payment, tx, index, prevouts, &leaf.script)?;
    tx.input[index].witness = tapscript_witness(
        &[signature.as_ref().to_vec()],
        &leaf.script,
        &tree.control_block("fee")?,
    );
    Ok(())
}

/// `request_timeout`, the presigned terminal paying her once the hub let
/// its window pass, with `fee`, a coin at her [`fee_script`], all of it
/// its fee: the presigned outputs are fixed (ALL|ANYONECANPAY).
pub fn timeout(o: &Opening, keys: &HolderKeys, fee: (OutPoint, TxOut)) -> Result<Transaction> {
    terminal(o, keys, TIMEOUT, vec![], fee)
}

/// Presigned terminal `name` of `o`, `wire` below both parties'
/// signatures, with `fee`, a coin at her [`fee_script`], all of it its
/// fee.
pub(crate) fn terminal(
    o: &Opening,
    keys: &HolderKeys,
    name: &str,
    wire: Vec<Vec<u8>>,
    fee: (OutPoint, TxOut),
) -> Result<Transaction> {
    let p = presign(o, name)?;
    let mut tx = finished(o, name, wire)?;
    tx.input.push(fee_input(fee.0));
    sign_fee_input(keys, &mut tx, 1, &[p.prevout.clone(), fee.1])?;
    Ok(tx)
}

/// The blocks after R's that `request_timeout` waits: the hub's window.
pub fn timeout_delay(o: &Opening) -> Result<u32> {
    blocks(presign(o, TIMEOUT)?.tx.input[0].sequence)
}

/// Whether `spend`, a spend of R's output, is `request_timeout` (any fee
/// inputs added): else the hub answered.
pub fn is_timeout(o: &Opening, spend: &Transaction) -> Result<bool> {
    let p = presign(o, TIMEOUT)?;
    let r = p.tx.input[0].previous_output;
    Ok(spend.input.iter().any(|i| i.previous_output == r) && spend.output == p.tx.output)
}

/// The blocks after the payout's that her bits and reserve wait: the
/// hub's chance to show she signed another `b`.
pub fn sweep_delay(o: &Opening) -> u32 {
    u32::from(o.babe.spec.params.delta)
}

/// Her sweep of `payout`, a transaction paying her withdrawal of
/// `amount` (`request_timeout`), to `to`: her bits set in `amount` and her
/// reserve, `b` her signature of `amount` the bits read, the fee at
/// `rate`. Valid [`sweep_delay`] blocks after the payout's.
pub fn sweep(
    o: &Opening,
    keys: &HolderKeys,
    payout: &Transaction,
    amount: u32,
    b: &WotsSig,
    to: ScriptBuf,
    rate: FeeRate,
) -> Result<Transaction> {
    ensure!(
        b.message() == amount.to_be_bytes(),
        "her signature of b reads another amount"
    );
    let spec = &o.babe.spec;
    let (t, b_key) = (&spec.terms, &spec.b_key);
    let ctx = o.babe.core().ctx();
    let bit_wire = Babe::request_wire(b);
    let mut ins: Vec<(TapTree, String, Vec<Vec<u8>>)> = (t.low_bits..t.bits)
        .filter(|i| (amount >> i) & 1 == 1)
        .map(|i| {
            Ok((
                bit_tree(&ctx, i, b_key)?,
                format!("alice_bit_{i}"),
                bit_wire.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    ins.push((
        reserve_tree(&ctx, t, b_key)?,
        "alice_reserve".into(),
        vec![],
    ));
    let txid = payout.compute_txid();
    let mut coins = Vec::with_capacity(ins.len());
    for (tree, name, _) in &ins {
        let script = tree.script_pubkey();
        let vout = payout
            .output
            .iter()
            .position(|o| o.script_pubkey == script)
            .with_context(|| format!("the payout has no output of {name}"))?;
        let coin = OutPoint {
            txid,
            vout: vout as u32,
        };
        coins.push((coin, tree.leaf(name)?.timelock.sequence()));
    }
    let prevouts: Vec<TxOut> = coins
        .iter()
        .map(|(c, _)| payout.output[c.vout as usize].clone())
        .collect();
    let total: Amount = prevouts.iter().map(|o| o.value).sum();
    let build = |fee: Amount| -> Result<Transaction> {
        let value = total
            .checked_sub(fee)
            .context("her bits cannot pay the fee")?;
        let out = TxOut {
            value,
            script_pubkey: to.clone(),
        };
        let mut tx = build_tx(&coins, vec![out], LockTime::ZERO);
        for (n, (tree, name, wire)) in ins.iter().enumerate() {
            let leaf = tree.leaf(name)?;
            let sig = sign_tapscript(&keys.party.payment, &tx, n, &prevouts, &leaf.script)?;
            let mut w = wire.clone();
            w.push(sig.as_ref().to_vec());
            tx.input[n].witness = tapscript_witness(&w, &leaf.script, &tree.control_block(name)?);
        }
        Ok(tx)
    };
    let vsize = build(Amount::ZERO)?.vsize() as u64;
    let fee = rate.fee_vb(vsize).context("the fee")?;
    let tx = build(fee)?;
    ensure!(
        tx.output[0].value >= to.minimal_non_dust(),
        "her bits less the fee are dust"
    );
    Ok(tx)
}
