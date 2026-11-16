//! The holder's side of a force close the hub answered: R spent by its
//! `respond`, her proof of her claim is needed. Her statement's input
//! from what she keeps and the chain shows; her `assert` of the proof;
//! `success` once the hub's chance to challenge it passed; and, the hub
//! challenging anyway, her answer from a kept instance, which the proof
//! opens.
//!
//! As in [`crate::close`], what is hers to sign is signed here, and the
//! driver chooses the fee coins and when each transaction goes out.

use anyhow::{anyhow, ensure, Context, Result};
use ark_bn254::{Bn254, Fq, Fq2, G1Affine, G2Affine};
use ark_ff::{BigInteger, PrimeField};
use ark_groth16::{Groth16, Proof};
use licash::recovery::HolderWitness;
use licash::statement::{HubResponse, Observed, StatementInput};
use licash::wots::Hashes;
use lichen_babe::garble::evaluate;
use lichen_babe::reveal::extract;
use lichen_babe::vsss::interpolate;
use lichen_babe::we::decrypt;
use lngap_btc::bitcoin::{OutPoint, Transaction, TxOut};
use lngap_btc::witness::witness_args_consumption_order;
use lngap_lamport::winternitz::{WotsParams, WotsSig};
use lngap_sessions::skeleton::sig_name;

use crate::close::{finished, presign, terminal, B_ITEMS, E_ITEMS};
use crate::graph::Babe;
use crate::keys::{HolderKeys, HubKeys};
use crate::opening::{journal, HubOpening, Opening, Setup, Trust};

/// The hub's answers to R: its plain one and its counter, of one txid.
pub const RESPOND: &str = "respond";
pub const RESPOND_COUNTER: &str = "respond_counter";
/// The hub's terminal once her deadline to assert passed.
pub const RESPOND_TIMEOUT: &str = "respond_timeout";
/// Her assertion of the full claim's proof, and its terminal paying her.
pub const ASSERT: &str = "assert";
pub const SUCCESS: &str = "success";
/// The hub's challenge of her assertion, and its terminal paying it.
pub const CHALLENGE: &str = "challenge_assert";
pub const FAILURE: &str = "failure";

/// A proof of her claim: Groth16's, on BN254.
pub type ClaimProof = Proof<Bn254>;

/// What the hub answered R with, read from the chain.
pub enum Response {
    /// `respond`: her proof of her whole claim decides.
    Plain(HubResponse),
    /// `respond_counter`: the hub says some of her notes are spent. Not
    /// read here yet.
    Counter,
}

/// Which presigned link of `o` `tx` is, if any of `names`: each is known
/// by its txid alone, every witness of it having the same.
fn is(o: &Opening, tx: &Transaction, name: &str) -> bool {
    presign(o, name).is_ok_and(|p| p.tx.compute_txid() == tx.compute_txid())
}

/// A link's script arguments, in consumption order, below both
/// signatures.
fn args(tx: &Transaction) -> Vec<Vec<u8>> {
    let all = witness_args_consumption_order(&tx.input[0].witness);
    all.get(2..).map(<[_]>::to_vec).unwrap_or_default()
}

/// The hub's answer `tx`, a spend of R's output: its `respond` (or
/// `respond_counter`, by the leaf it spends), else an error.
pub fn response(o: &Opening, tx: &Transaction) -> Result<Response> {
    ensure!(is(o, tx, RESPOND), "not the hub's answer to her request");
    let leaf = tx.input[0].witness.taproot_leaf_script();
    let script = leaf.context("no script")?.script;
    let leaf = |name: &str| presign(o, name).map(|p| p.leaf.as_script() == script);
    if leaf(RESPOND_COUNTER).unwrap_or(false) {
        return Ok(Response::Counter);
    }
    ensure!(leaf(RESPOND)?, "an answer of no leaf of hers");
    let exit = Babe::read_respond_exit(&args(tx), false)?;
    Ok(Response::Plain(HubResponse::Plain {
        e_sig: Hashes(exit.hub_e),
    }))
}

/// The hub's plain `respond` to `r`, her request in contract `o`: her
/// signatures of `b` and `E`, read from R, signed back under its one-time
/// keys of the plain leaf, both parties' signatures below, R's
/// publication coins spent with it. Her proof decides then. Refused for
/// a transaction not the contract's request, or one that misreads it.
pub fn respond(o: &HubOpening, keys: &HubKeys, r: &Transaction) -> Result<Transaction> {
    let link = |name: &str| {
        o.set
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("no {name} in the contract's graph"))
    };
    ensure!(
        r.compute_txid() == link("request")?.tx.compute_txid(),
        "not this contract's request"
    );
    let items: Vec<Vec<u8>> = r.input[0].witness.iter().map(<[u8]>::to_vec).collect();
    ensure!(
        items.len() > E_ITEMS + B_ITEMS,
        "a request of too few items"
    );
    let leaf = o.prefix.leaf();
    let request = leaf
        .keys()
        .decode_witness(&items, leaf.script(), leaf.control())?;
    let order = |items: &[Vec<u8>]| items.iter().rev().cloned().collect::<Vec<_>>();
    let e = WotsSig::from_consumption_order(WotsParams::for_bytes(32), &order(&items[..E_ITEMS]))?;
    let b = &items[E_ITEMS..E_ITEMS + B_ITEMS];
    let b = WotsSig::from_consumption_order(WotsParams::for_bytes(4), &order(b))?;
    ensure!(
        b.message() == request.amount.to_be_bytes() && e.message() == request.claim_digest,
        "her signatures read another request than R's"
    );
    let hub_b = keys.b.sign(&b.message())?;
    let hub_e = keys.e_plain.sign(&e.message())?;
    let wire = Babe::respond_exit_wire(&hub_b, &b, &e, &hub_e, None);
    let p = link(RESPOND)?;
    // Each publication coin's input carries the E part alone: the main
    // input's wire but its `b` signatures, at its bottom.
    let n = 4 * WotsParams::for_bytes(4).total_digits() as usize;
    let e_wire = wire[..wire.len() - n].to_vec();
    let mut wires = vec![wire];
    wires.extend((0..p.extra.len()).map(|_| e_wire.clone()));
    let sigs = |of: &std::collections::HashMap<String, Vec<u8>>, whose: &str| {
        (0..p.inputs())
            .map(|i| {
                let name = sig_name(RESPOND, i);
                of.get(&name)
                    .cloned()
                    .with_context(|| format!("no signature of {whose} of {name}"))
            })
            .collect::<Result<Vec<_>>>()
    };
    let hub = sigs(&o.sigs, "the hub")?;
    let alice = sigs(&o.consent.sigs, "hers")?;
    Ok(p.finish_multi(wires, &hub, &alice))
}

/// Her statement's input proving her whole claim (`c` = 0): `observed`,
/// the chain's evidence of R and P, `response` the hub's, `holder` her
/// notes and their ownership. Private: it shows her notes.
pub fn statement(
    o: &Opening,
    observed: Observed,
    response: HubResponse,
    holder: HolderWitness,
) -> StatementInput {
    StatementInput {
        c: 0,
        base: o.base.clone(),
        prefix: o.prefix.clone(),
        observed,
        response,
        holder,
    }
}

/// The journal her proof must commit: fixed at opening.
pub fn expected(o: &Opening) -> licash::statement::Journal {
    journal(&o.base, &o.d, 0)
}

/// The last block her `assert` may be in: past it, `respond_timeout`
/// pays the hub.
pub fn deadline(o: &Opening) -> u32 {
    o.babe.spec.windows.t_a
}

/// The blocks after her `assert`'s that `success` waits: the hub's
/// chance to challenge it.
pub fn success_delay(o: &Opening) -> u32 {
    u32::from(o.babe.spec.windows.delta2)
}

/// The blocks after the hub's challenge that `failure` waits: her chance
/// to answer it.
pub fn answer_delay(o: &Opening) -> u32 {
    u32::from(o.babe.spec.windows.delta1)
}

/// A proof from its points as RISC Zero's wrap is exported (`licash-r0
/// native-wrap`, ln-gap's convention: A, B, C, each x little-endian, the
/// top bit of its last byte set iff y is odd; B as x.c0 then x.c1, the
/// flag from y.c0): 128 bytes.
pub fn proof_from_points(points: &[u8]) -> Result<Proof<Bn254>> {
    ensure!(
        points.len() == 128,
        "a proof's points are 128 bytes, not {}",
        points.len()
    );
    Ok(Proof {
        a: g1(&points[..32])?,
        b: g2(&points[32..96])?,
        c: g1(&points[96..])?,
    })
}

/// A proof's points as [`proof_from_points`] reads them.
pub fn proof_to_points(proof: &Proof<Bn254>) -> Vec<u8> {
    let le = |x: Fq| {
        let mut b = x.into_bigint().to_bytes_le();
        b.resize(32, 0);
        b
    };
    let flagged = |mut b: Vec<u8>, odd: bool| {
        if odd {
            b[31] |= 0x80;
        }
        b
    };
    let g1 = |p: &G1Affine| flagged(le(p.x), odd(p.y));
    let mut points = g1(&proof.a);
    points.extend(le(proof.b.x.c0));
    points.extend(flagged(le(proof.b.x.c1), odd(proof.b.y.c0)));
    points.extend(g1(&proof.c));
    points
}

/// A coordinate from 32 little-endian bytes, below the modulus.
fn fq_le(b: &[u8]) -> Result<Fq> {
    let x = Fq::from_le_bytes_mod_order(b);
    ensure!(
        x.into_bigint().to_bytes_le() == b,
        "a coordinate beyond the field"
    );
    Ok(x)
}

fn odd(y: Fq) -> bool {
    y.into_bigint().is_odd()
}

fn g1(b: &[u8]) -> Result<G1Affine> {
    let mut x = b.to_vec();
    let flag = x[31] & 0x80 != 0;
    x[31] &= 0x7f;
    let x = fq_le(&x)?;
    let (y, minus) = G1Affine::get_ys_from_x_unchecked(x).context("a G1 x off the curve")?;
    let p = G1Affine::new_unchecked(x, if odd(y) == flag { y } else { minus });
    ensure!(
        p.is_on_curve() && p.is_in_correct_subgroup_assuming_on_curve(),
        "not a G1 point"
    );
    Ok(p)
}

fn g2(b: &[u8]) -> Result<G2Affine> {
    let mut c1 = b[32..].to_vec();
    let flag = c1[31] & 0x80 != 0;
    c1[31] &= 0x7f;
    let x = Fq2::new(fq_le(&b[..32])?, fq_le(&c1)?);
    let (y, minus) = G2Affine::get_ys_from_x_unchecked(x).context("a G2 x off the curve")?;
    let p = G2Affine::new_unchecked(x, if odd(y.c0) == flag { y } else { minus });
    ensure!(
        p.is_on_curve() && p.is_in_correct_subgroup_assuming_on_curve(),
        "not a G2 point"
    );
    Ok(p)
}

/// A proof as a recovery keeps it: its points compressed, 128 bytes.
pub fn proof_bytes(proof: &Proof<Bn254>) -> Vec<u8> {
    lichen_babe::statement::proof_to_bytes(proof)
}

/// [`proof_bytes`] read back.
pub fn proof_of_bytes(bytes: &[u8]) -> Result<Proof<Bn254>> {
    lichen_babe::statement::proof_from_bytes(bytes).context("not a proof's bytes")
}

/// The program her proof must be of: the holder program her opening
/// names, by its image id.
pub fn holder_image(o: &Opening) -> [u8; 32] {
    o.base.parts().recipe.holder_program
}

/// Whether `proof` proves her claim in full under `trust`: the statement
/// her kept instances are garbled under, of the journal fixed at
/// opening. A proof that does not opens none of them.
pub fn proves(o: &Opening, trust: &Trust, proof: &Proof<Bn254>) -> bool {
    let statement = (trust.statement)(&expected(o));
    <Groth16<Bn254> as ark_snark::SNARK<_>>::verify(&statement.vk, &statement.public, proof)
        .unwrap_or(false)
}

/// The mock authority's proof of her claim (regtest only): anyone makes
/// it, at once.
pub fn mock_proof(o: &Opening, trust: &Trust) -> Result<Proof<Bn254>> {
    ensure!(trust.mock, "a mock proof under a genuine authority");
    Ok(lichen_babe::statement::mock_prove(&expected(o).to_bytes()))
}

/// π1's digits as she signs them, π2, π3.
fn pi(proof: &Proof<Bn254>) -> (Vec<bool>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let bits = lichen_babe::gadget::input(&proof.a);
    let (pi2, pi3) = lichen_babe::statement::pi23_to_bytes(proof);
    let digits = lichen_babe::setup::digits(&bits);
    (bits, digits, pi2, pi3)
}

/// `assert` of `proof`: π1 signed digit by digit under her one-time
/// Lamport key, π2 and π3 beside. Her key signs once: a second assert of
/// another proof shows both preimages of a digit, so a driver asserts
/// one proof only, kept before it signs.
pub fn assert(o: &Opening, keys: &HolderKeys, proof: &Proof<Bn254>) -> Result<Transaction> {
    let (_, digits, pi2, pi3) = pi(proof);
    let wire = Babe::assert_wire(&keys.lamport.sign(&digits), &pi2, &pi3);
    finished(o, ASSERT, wire)
}

/// Whether `tx` is her `assert`, any proof's.
pub fn is_assert(o: &Opening, tx: &Transaction) -> bool {
    is(o, tx, ASSERT)
}

/// The proof `tx`, an `assert` of hers, carries: one posted by another
/// copy of her wallet, say, to go on with.
pub fn asserted(o: &Opening, tx: &Transaction) -> Result<Proof<Bn254>> {
    ensure!(is_assert(o, tx), "not her assert");
    let args = args(tx);
    let (signed, pi2, pi3) = Babe::read_assert(&args).context("assert's arguments")?;
    let lamport = &o.babe.spec.lamport;
    let digits = signed
        .iter()
        .enumerate()
        .map(|(d, x)| lamport.read(d, x).context("a preimage of no digit"))
        .collect::<Result<Vec<u8>>>()?;
    let bits: Vec<bool> = digits
        .iter()
        .flat_map(|v| [v & 1 == 1, v & 2 == 2])
        .collect();
    lichen_babe::statement::proof_from_chain(&bits, pi2, pi3).context("no proof")
}

/// `success`, paying her once the hub let its chance to challenge her
/// `assert` pass, with `fee`, a coin at her [`crate::close::fee_script`],
/// all of it its fee.
pub fn success(o: &Opening, keys: &HolderKeys, fee: (OutPoint, TxOut)) -> Result<Transaction> {
    terminal(o, keys, SUCCESS, vec![], fee)
}

/// Whether `tx` is `success`, any fee inputs added.
pub fn is_success(o: &Opening, tx: &Transaction) -> Result<bool> {
    let p = presign(o, SUCCESS)?;
    let at = p.tx.input[0].previous_output;
    Ok(tx.input.iter().any(|i| i.previous_output == at) && tx.output == p.tx.output)
}

/// Whether `tx` is her answer to the hub's challenge, from any kept
/// instance, any fee inputs added.
pub fn is_answer(o: &Opening, tx: &Transaction) -> bool {
    o.set
        .iter()
        .filter(|p| p.name.starts_with("wrongly_challenged_") && !p.name.contains("corrected"))
        .any(|p| {
            let at = p.tx.input[0].previous_output;
            tx.input.iter().any(|i| i.previous_output == at) && tx.output == p.tx.output
        })
}

/// Whether `tx` is the hub's challenge of her `assert`.
pub fn is_challenge(o: &Opening, tx: &Transaction) -> bool {
    is(o, tx, CHALLENGE)
}

/// Her answer to `challenge`, the hub's of her `assert` of `proof`: her
/// kept instances of the full assertion (`setup`) evaluated on π1's
/// secrets the challenge reveals, the first whose message `proof` opens
/// paying her through its `wrongly_challenged`, with `fee`. None if none
/// opens: the proof is false, or the hub cheated at setup, which the
/// instances opened at setup would have shown.
pub fn answer(
    o: &Opening,
    keys: &HolderKeys,
    setup: &Setup,
    challenge: &Transaction,
    proof: &Proof<Bn254>,
    fee: (OutPoint, TxOut),
) -> Result<Option<Transaction>> {
    ensure!(is_challenge(o, challenge), "not the hub's challenge");
    let (bits, digits, _, _) = pi(proof);
    let lamport = &o.babe.spec.lamport;
    let (posted, revealed) = extract(lamport, &setup.presigs, &args(challenge))
        .ok_or_else(|| anyhow!("the challenge's secrets"))?;
    ensure!(
        posted == digits,
        "the challenge reveals other digits than she asserted"
    );
    let of_digits: Vec<(u64, Vec<[u8; 32]>)> = setup
        .opened
        .iter()
        .map(|(i, shares)| {
            let secrets = shares.iter().zip(&posted).map(|(s, v)| s[*v as usize]);
            (*i, secrets.collect())
        })
        .collect();
    for (k, (i, instance)) in setup.instances.iter().enumerate() {
        let Some(secrets) = interpolate(*i, &of_digits, &revealed) else {
            continue;
        };
        let r_pi1 = evaluate(&instance.gc, &bits, &instance.labels(&bits, &secrets));
        let msg = decrypt(&instance.ct, &r_pi1, proof);
        if instance.is_msg(&msg) {
            let name = format!("wrongly_challenged_{k}");
            return terminal(o, keys, &name, Babe::wrongly_wire(&msg), fee).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A genuine proof as `licash-r0 native-wrap` exports it (a holder's
    /// claim on regtest, under the native holder program): its points
    /// decode to a proof RISC Zero's Groth16 key accepts for its image
    /// and journal, and to none once a bit of them flips.
    #[test]
    fn a_genuine_proof_decodes_from_its_points() {
        let image: [u8; 32] = hex(IMAGE).try_into().unwrap();
        let journal = hex(JOURNAL);
        let points = hex(PROOF);
        let vk = lichen_babe::risc0::verifying_key();
        let public = lichen_babe::risc0::public_inputs(&image, &journal);
        let verify = |p: &Proof<Bn254>| {
            <Groth16<Bn254> as ark_snark::SNARK<_>>::verify(&vk, &public, p).unwrap_or(false)
        };
        let proof = proof_from_points(&points).unwrap();
        assert!(verify(&proof));
        assert_eq!(proof_to_points(&proof), points);
        assert_eq!(proof_of_bytes(&proof_bytes(&proof)).unwrap(), proof);
        let mut flipped = points.clone();
        flipped[0] ^= 1;
        assert!(proof_from_points(&flipped).map_or(true, |p| !verify(&p)));
        assert!(proof_from_points(&points[..127]).is_err());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const IMAGE: &str = "8be30cb2a683e896d251c7c363dddf794dc8ddb9d502922199cfa4d2f73b1390";
    const JOURNAL: &str = concat!(
        "010000004cbf911de13902ea9f0e72101a66629ff95841320a0294b07046a56b8c93",
        "11e3e77f53b54307609ce218e6bb0a83500d2f811c073a183beb55428fb7d5938c57"
    );
    const PROOF: &str = concat!(
        "3be6654db05941dfc23968096a37fed2e26f25e1ef1d2ade7c263a2c8ec7080e",
        "6bf06462184c8b2693afd3d30d0119b8f530e16d7b1b8cecf8aa39252389d11b",
        "72b28f2691c9fc3f8a620f2a12ff88189468bd6bd553a5966eb100b2a024e800",
        "80026264ebe486c5c6f3c00fd48189aa9c67eb4f607a9660e0b011e0aa7d6f2a"
    );
}
