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
use licash::disclosure::{Disclosures, SignedDisclosures, CHUNK_BYTES};
use licash::recovery::HolderWitness;
use licash::statement::{HubKeys as ResponseKeys, HubResponse, Observed, StatementInput, K_LEN};
use licash::wots::{self, Hashes};
use lichen_babe::garble::evaluate;
use lichen_babe::reveal::{extract, LamportPub};
use lichen_babe::vsss::interpolate;
use lichen_babe::we::decrypt;
use lngap_btc::bitcoin::{OutPoint, Transaction, TxOut};
use lngap_btc::witness::witness_args_consumption_order;
use lngap_lamport::winternitz::{Wots256Sig, WotsParams, WotsSig};
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
pub const ASSERT_CORRECTED: &str = "assert_corrected";
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
    /// `respond_counter`, with all five signed disclosure publications.
    /// Whether it proves anything is decided by the holder statement.
    Counter(HubResponse),
}

/// Which presigned link of `o` `tx` is, if any of `names`: each is known
/// by its txid alone, every witness of it having the same.
fn is(o: &Opening, tx: &Transaction, name: &str) -> bool {
    presign(o, name).is_ok_and(|p| p.tx.compute_txid() == tx.compute_txid())
}

/// A link's script arguments, in consumption order, below both
/// signatures.
fn args(tx: &Transaction) -> Vec<Vec<u8>> {
    input_args(&tx.input[0].witness)
}

fn input_args(witness: &lngap_btc::bitcoin::Witness) -> Vec<Vec<u8>> {
    let all = witness_args_consumption_order(witness);
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
        let plain = presign(o, RESPOND)?;
        let counter = presign(o, RESPOND_COUNTER)?;
        ensure!(
            plain.extra.len() == 5 && counter.extra.len() == 5 && tx.input.len() == 6,
            "counter publication input count"
        );
        return counter_response(
            o.base.parts().response_keys.keys(),
            tx,
            &std::array::from_fn(|i| plain.extra[i].1.clone()),
            &std::array::from_fn(|i| counter.extra[i].1.clone()),
        );
    }
    ensure!(leaf(RESPOND)?, "an answer of no leaf of hers");
    Ok(Response::Plain(HubResponse::Plain {
        e_sig: Hashes(Babe::read_plain_response_hashes(&args(tx))?),
    }))
}

/// Decode only after the caller identifies the presigned transaction and
/// its main counter leaf. The sidecar scripts come from that same opening.
fn counter_response(
    keys: &ResponseKeys,
    tx: &Transaction,
    plain: &[lngap_btc::bitcoin::ScriptBuf; 5],
    published: &[lngap_btc::bitcoin::ScriptBuf; 5],
) -> Result<Response> {
    ensure!(tx.input.len() == 6, "counter publication input count");
    let mut plain_response = None;
    for (i, input) in tx.input.iter().skip(1).enumerate() {
        let leaf = input.witness.taproot_leaf_script();
        let script = leaf.context("publication has no script")?.script;
        if script == plain[i].as_script() {
            let signature = Hashes(Babe::read_plain_publication_hashes(&input_args(
                &input.witness,
            ))?);
            plain_response = Some(HubResponse::Plain { e_sig: signature });
        } else {
            ensure!(
                script == published[i].as_script(),
                "publication of no response leaf"
            );
        }
    }
    // Any plain publication authenticates E under the plain key, even
    // when the main input uses the counter leaf. No disclosures needed.
    if let Some(response) = plain_response {
        return Ok(Response::Plain(response));
    }
    let publication = read_disclosures(keys, tx)?;
    Ok(Response::Counter(read_counter(
        keys,
        &args(tx),
        publication,
    )?))
}

fn signed_message(key: &Hashes, signature: &Hashes, bytes: usize) -> Result<Vec<u8>> {
    let params = WotsParams::for_bytes(bytes as u32);
    ensure!(
        key.len() == params.total_digits() as usize && signature.len() == key.len(),
        "response key digit count"
    );
    let mut digits = Vec::with_capacity(key.len());
    for (reveal, public) in signature.0.iter().zip(&key.0) {
        let mut hash = *reveal;
        let mut digit = None;
        for distance in 0..=wots::MAX_DIGIT {
            if hash == *public {
                digit = Some(wots::MAX_DIGIT - distance);
                break;
            }
            hash = wots::chain(hash, 1);
        }
        digits.push(digit.context("response reveal of no public-key digit")?);
    }
    let message = digits[..2 * bytes]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|d| (d[0] << 4) | d[1])
        .collect::<Vec<_>>();
    ensure!(
        wots::verify_msg(key, signature, &message),
        "response signature checksum"
    );
    Ok(message)
}

fn read_disclosures(keys: &ResponseKeys, tx: &Transaction) -> Result<SignedDisclosures> {
    ensure!(tx.input.len() == 6, "counter publication input count");
    let mut bytes = Vec::new();
    let mut signatures = Vec::new();
    for (i, size) in CHUNK_BYTES.into_iter().enumerate() {
        let params = WotsParams::for_bytes(size as u32);
        let n = 2 * params.total_digits() as usize;
        let witness = &tx.input[i + 1].witness;
        ensure!(witness.len() == n + 4, "publication witness shape");
        let mut arguments: Vec<_> = witness.iter().take(n).map(<[u8]>::to_vec).collect();
        arguments.reverse();
        let signature = Hashes(Babe::read_wots256_hashes(params, &arguments)?);
        bytes.extend(signed_message(&keys.disclosure[i], &signature, size)?);
        signatures.push(signature);
    }
    Ok(SignedDisclosures {
        disclosures: Disclosures::from_bytes(&bytes)?,
        signatures: signatures.try_into().expect("five publication signatures"),
    })
}

fn read_counter(
    keys: &ResponseKeys,
    arguments: &[Vec<u8>],
    publication: SignedDisclosures,
) -> Result<HubResponse> {
    let hashes =
        Babe::read_corrected_response_hashes(arguments, WotsParams::for_bytes(K_LEN as u32))?;
    let k = Hashes(hashes.k);
    let corrected = Hashes(hashes.b_corr);
    let message = signed_message(&keys.k, &k, K_LEN)?;
    let amount = signed_message(&keys.b_corr, &corrected, 4)?;
    let e_sig = Hashes(hashes.e);
    ensure!(
        publication.verify(&keys.disclosure),
        "counter response signatures"
    );
    Ok(HubResponse::Counter {
        e_sig,
        k: message,
        k_sig: k,
        b_corr: u32::from_be_bytes(amount.try_into().expect("a four-byte corrected amount")),
        corr_sig: corrected,
        publication: Box::new(publication),
    })
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

/// The corrected assertion bound to one opening and the hub's signed amount.
/// Its Lamport and amount keys are separate from the full claim's keys.
pub struct Corrected<'a> {
    opening: &'a Opening,
    amount: u32,
    hub_b: Wots256Sig,
}

impl<'a> Corrected<'a> {
    /// Bind the corrected amount before any holder key is used. The driver
    /// must also validate the counter proof and durably keep its decision.
    pub fn new(opening: &'a Opening, amount: u32, signature: &Hashes) -> Result<Self> {
        ensure!(
            amount <= opening.base.parts().cap,
            "corrected amount exceeds the cap"
        );
        ensure!(
            wots::verify_b(
                &opening.base.parts().response_keys.keys().b_corr,
                signature,
                amount
            ),
            "corrected amount signature"
        );
        let hub_b = Wots256Sig::from_hashes(
            WotsParams::for_bytes(4),
            &amount.to_be_bytes(),
            signature.0.clone(),
        )?;
        Ok(Self {
            opening,
            amount,
            hub_b,
        })
    }

    pub fn amount(&self) -> u32 {
        self.amount
    }

    pub fn expected(&self) -> licash::statement::Journal {
        journal(&self.opening.base, &self.opening.d, 1)
    }

    pub fn proves(&self, trust: &Trust, proof: &ClaimProof) -> bool {
        let statement = (trust.statement)(&self.expected());
        <Groth16<Bn254> as ark_snark::SNARK<_>>::verify(&statement.vk, &statement.public, proof)
            .unwrap_or(false)
    }

    pub fn mock_proof(&self, trust: &Trust) -> Result<ClaimProof> {
        ensure!(trust.mock, "a mock proof under a genuine authority");
        Ok(lichen_babe::statement::mock_prove(
            &self.expected().to_bytes(),
        ))
    }

    /// Sign only a proof and amount already kept by the driver. Both
    /// corrected one-time keys must never sign different data on retry.
    pub fn assert(&self, keys: &HolderKeys, proof: &ClaimProof) -> Result<Transaction> {
        let (_, digits, pi2, pi3) = pi(proof);
        let wire = Babe::assert_corrected_wire(
            &keys.corrected.lamport.sign(&digits),
            &pi2,
            &pi3,
            &self.hub_b,
            &keys.corrected.b_key.sign(&self.amount.to_be_bytes())?,
        );
        finished(self.opening, ASSERT_CORRECTED, wire)
    }

    pub fn is_assert(opening: &Opening, tx: &Transaction) -> bool {
        is(opening, tx, ASSERT_CORRECTED)
    }

    /// Adopt a posted assertion, including its existing holder amount
    /// signature for collection. Reading never emits another signature.
    pub fn asserted(opening: &'a Opening, tx: &Transaction) -> Result<(Self, ClaimProof, WotsSig)> {
        ensure!(Self::is_assert(opening, tx), "not her corrected assert");
        let arguments = args(tx);
        let read = Babe::read_corrected_assertion_hashes(&arguments)?;
        let signature = Hashes(read.hub_b);
        let amount = signed_message(
            &opening.base.parts().response_keys.keys().b_corr,
            &signature,
            4,
        )?;
        let amount = u32::from_be_bytes(amount.try_into().expect("a four-byte corrected amount"));
        let corrected = Self::new(opening, amount, &signature)?;
        let holder_b = WotsSig::from_hashes(
            WotsParams::for_bytes(4),
            &amount.to_be_bytes(),
            read.holder_b,
        )?;
        let spec = opening
            .babe
            .spec
            .corrected
            .as_ref()
            .context("no corrected assertion")?;
        spec.b_key
            .verify(&holder_b)
            .context("holder's corrected amount signature")?;
        let proof = proof_from_reveals(&spec.lamport, read.proof)?;
        Ok((corrected, proof, holder_b))
    }
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
    proof_from_reveals(lamport, (signed, pi2, pi3))
}

fn proof_from_reveals(
    lamport: &LamportPub,
    (signed, pi2, pi3): crate::graph::Asserted<'_>,
) -> Result<ClaimProof> {
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
    use lngap_btc::bitcoin::{absolute, transaction, Transaction, TxIn, Witness};
    use lngap_lamport::winternitz::{Wots256Secret, WotsSecret};

    fn counter_witnesses() -> (ResponseKeys, Transaction, Vec<Vec<u8>>, SignedDisclosures) {
        let key = |bytes, entropy| {
            Wots256Secret::from_entropy(WotsParams::for_bytes(bytes), [entropy; 32])
        };
        let b = key(4, 1);
        let plain = key(32, 2);
        let e = key(32, 3);
        let k = key(K_LEN as u32, 4);
        let corrected = key(4, 5);
        let disclosure: [_; 5] = std::array::from_fn(|i| key(CHUNK_BYTES[i] as u32, i as u8 + 6));
        let public = |key: &Wots256Secret| Hashes(key.public().digits);
        let keys = ResponseKeys {
            b: public(&b),
            e_plain: public(&plain),
            e_counter: public(&e),
            k: public(&k),
            b_corr: public(&corrected),
            disclosure: std::array::from_fn(|i| public(&disclosure[i])),
        };
        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default(); 6],
            output: vec![],
        };
        let mut values = Disclosures::empty();
        values.values[0] = [255; 32];
        let bytes = values.to_bytes();
        let mut at = 0;
        for (i, key) in disclosure.iter().enumerate() {
            let end = at + CHUNK_BYTES[i];
            let sig = key.sign(&bytes[at..end]).unwrap();
            let mut wire = sig.consumption_order();
            wire.reverse();
            // Both transaction signatures, script and control block.
            wire.extend([vec![], vec![], vec![0x52], vec![0xc0; 33]]);
            tx.input[i + 1].witness = Witness::from_slice(&wire);
            at = end;
        }
        let publication = read_disclosures(&keys, &tx).unwrap();
        assert_eq!(publication.disclosures, values);
        let alice_b = WotsSecret::from_entropy(WotsParams::for_bytes(4), [20; 32]);
        let alice_e = WotsSecret::from_entropy(WotsParams::for_bytes(32), [21; 32]);
        let mut digest = [255; 32];
        digest[31] = 240; // E's checksum has both zero and 15 digits.
        let mut counter = [0; K_LEN];
        counter[0] = 255;
        let mut wire = Babe::respond_corrected_wire(
            &b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_e.sign(&digest).unwrap(),
            &e.sign(&digest).unwrap(),
            &k.sign(&counter).unwrap(),
            &corrected.sign(&15u32.to_be_bytes()).unwrap(),
        );
        let mut witness = wire.clone();
        witness.extend([vec![], vec![], vec![0x53], vec![0xc0; 33]]);
        tx.input[0].witness = Witness::from_slice(&witness);
        wire.reverse();
        (keys, tx, wire, publication)
    }

    #[test]
    fn a_counter_is_reconstructed_and_authenticated_without_any_secret_keys() {
        let (keys, _, args, publication) = counter_witnesses();
        let response = read_counter(&keys, &args, publication).unwrap();
        let HubResponse::Counter {
            k,
            b_corr,
            publication,
            ..
        } = response
        else {
            panic!("not a counter")
        };
        assert_eq!(k[0], 255);
        assert!(k[1..].iter().all(|b| *b == 0));
        assert_eq!(b_corr, 15);
        assert!(publication.verify(&keys.disclosure));
        assert_eq!(publication.disclosures.values[0], [255; 32]);
    }

    #[test]
    fn a_complete_counter_response_uses_every_publication_input() {
        let (keys, tx, _, _) = counter_witnesses();
        let plain = std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x51]));
        let published =
            std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x52]));
        assert!(matches!(
            counter_response(&keys, &tx, &plain, &published).unwrap(),
            Response::Counter(_)
        ));
        let mut unknown = tx;
        unknown.input[3].witness =
            Witness::from_slice(&[vec![], vec![], vec![0x54], vec![0xc0; 33]]);
        assert!(counter_response(&keys, &unknown, &plain, &published).is_err());
    }

    fn counter_fields(
        response: Response,
    ) -> (Hashes, Vec<u8>, Hashes, u32, Hashes, Box<SignedDisclosures>) {
        let Response::Counter(HubResponse::Counter {
            e_sig,
            k,
            k_sig,
            b_corr,
            corr_sig,
            publication,
        }) = response
        else {
            panic!("not a complete counter")
        };
        (e_sig, k, k_sig, b_corr, corr_sig, publication)
    }

    fn alias_digit(arguments: &mut [Vec<u8>], range: std::ops::Range<usize>, alias: &[u8]) {
        let mut changed = false;
        for item in &mut arguments[range] {
            if item == &[15] {
                *item = alias.to_vec();
                changed = true;
            }
        }
        assert!(changed, "fixture has no digit 15 in this signature");
    }

    fn replace_arguments(tx: &mut Transaction, i: usize, mut arguments: Vec<Vec<u8>>) {
        let witness: Vec<_> = tx.input[i].witness.iter().map(<[u8]>::to_vec).collect();
        arguments.reverse();
        arguments.extend([
            witness[witness.len() - 4].clone(),
            witness[witness.len() - 3].clone(),
            witness[witness.len() - 2].clone(),
            witness[witness.len() - 1].clone(),
        ]);
        tx.input[i].witness = Witness::from_slice(&arguments);
    }

    #[test]
    fn counter_messages_are_recovered_from_hashes_not_declared_digits() {
        let (keys, tx, _, _) = counter_witnesses();
        let plain = std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x51]));
        let published =
            std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x52]));
        let expected = counter_fields(counter_response(&keys, &tx, &plain, &published).unwrap());
        let b = 2 * WotsParams::for_bytes(4).total_digits() as usize;
        let n = WotsParams::for_bytes(32).total_digits() as usize;
        let e_len = 3 * n + (n - WotsParams::for_bytes(32).message_digits as usize);
        let args = input_args(&tx.input[0].witness);
        let ranges = [
            2 * b..3 * b,
            3 * b..3 * b + 2 * n,
            3 * b + 2 * n..3 * b + 2 * n + 6,
            3 * b + e_len..args.len(),
        ];
        for alias in [vec![16], vec![16, 1]] {
            for range in &ranges {
                let mut changed = tx.clone();
                let mut arguments = args.clone();
                alias_digit(&mut arguments, range.clone(), &alias);
                replace_arguments(&mut changed, 0, arguments);
                assert_eq!(
                    counter_fields(counter_response(&keys, &changed, &plain, &published).unwrap()),
                    expected
                );
            }
            let mut changed = tx.clone();
            let mut arguments = input_args(&changed.input[1].witness);
            let length = arguments.len();
            alias_digit(&mut arguments, 0..length, &alias);
            replace_arguments(&mut changed, 1, arguments);
            assert_eq!(
                counter_fields(counter_response(&keys, &changed, &plain, &published).unwrap()),
                expected
            );
        }
    }

    #[test]
    fn forward_hashing_a_reveal_cannot_forge_a_message_with_the_old_checksum() {
        let (keys, tx, args, _) = counter_witnesses();
        let raw = Babe::read_corrected_response_hashes(&args, WotsParams::for_bytes(K_LEN as u32))
            .unwrap();
        let mut k = Hashes(raw.k);
        k.0[2] = wots::chain(k.0[2], 1);
        assert!(signed_message(&keys.k, &k, K_LEN)
            .unwrap_err()
            .to_string()
            .contains("response signature checksum"));
        let witness = input_args(&tx.input[1].witness);
        let mut chunk = Hashes(
            Babe::read_wots256_hashes(WotsParams::for_bytes(CHUNK_BYTES[0] as u32), &witness)
                .unwrap(),
        );
        chunk.0[64] = wots::chain(chunk.0[64], 1);
        assert!(signed_message(&keys.disclosure[0], &chunk, CHUNK_BYTES[0])
            .unwrap_err()
            .to_string()
            .contains("response signature checksum"));
    }

    #[test]
    fn any_plain_publication_makes_a_mixed_counter_a_full_claim_response() {
        let (keys, tx, _, _) = counter_witnesses();
        let word = WotsParams::for_bytes(4);
        let message = WotsParams::for_bytes(32);
        let b = Wots256Secret::from_entropy(word, [1; 32]);
        let alice_b = WotsSecret::from_entropy(word, [20; 32]);
        let alice_e = WotsSecret::from_entropy(message, [21; 32]);
        let hub_e = Wots256Secret::from_entropy(message, [2; 32]);
        let mut digest = [255; 32];
        digest[31] = 240;
        let wire = Babe::respond_exit_wire(
            &b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_e.sign(&digest).unwrap(),
            &hub_e.sign(&digest).unwrap(),
            None,
        );
        let n = 4 * word.total_digits() as usize;
        let mut e_wire = wire[..wire.len() - n].to_vec();
        e_wire.extend([vec![], vec![], vec![0x51], vec![0xc0; 33]]);
        let plain = std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x51]));
        let published =
            std::array::from_fn(|_| lngap_btc::bitcoin::ScriptBuf::from_bytes(vec![0x52]));
        for i in 1..=5 {
            let mut mixed = tx.clone();
            mixed.input[i].witness = Witness::from_slice(&e_wire);
            // The other inputs' disclosed bytes are unnecessary on this
            // path, but every input must still use an authorized leaf.
            let Response::Plain(HubResponse::Plain { e_sig }) =
                counter_response(&keys, &mixed, &plain, &published).unwrap()
            else {
                panic!("a mixed response did not select the full claim")
            };
            assert!(wots::verify_msg(&keys.e_plain, &e_sig, &digest));
            for alias in [vec![16], vec![16, 1]] {
                let mut aliased = mixed.clone();
                let mut arguments = input_args(&aliased.input[i].witness);
                let n = 2 * message.total_digits() as usize;
                alias_digit(&mut arguments, 0..n, &alias);
                replace_arguments(&mut aliased, i, arguments);
                let Response::Plain(HubResponse::Plain { e_sig }) =
                    counter_response(&keys, &aliased, &plain, &published).unwrap()
                else {
                    panic!("aliased E blocked a mixed response")
                };
                assert!(wots::verify_msg(&keys.e_plain, &e_sig, &digest));
            }
            for zero in [vec![], vec![0], vec![0x80]] {
                let mut aliased = mixed.clone();
                let mut arguments = input_args(&aliased.input[i].witness);
                let n = 2 * message.total_digits() as usize;
                assert_eq!(arguments[n], vec![15]);
                arguments[n] = vec![16];
                arguments[n + 2] = zero.clone();
                arguments[n + 4] = zero;
                replace_arguments(&mut aliased, i, arguments);
                let Response::Plain(HubResponse::Plain { e_sig }) =
                    counter_response(&keys, &aliased, &plain, &published).unwrap()
                else {
                    panic!("tied-checksum encoding blocked a mixed response")
                };
                assert!(wots::verify_msg(&keys.e_plain, &e_sig, &digest));
            }
        }
        let mut mixed = tx;
        for i in 1..=5 {
            mixed.input[i].witness = Witness::from_slice(&e_wire);
            assert!(matches!(
                counter_response(&keys, &mixed, &plain, &published).unwrap(),
                Response::Plain(_)
            ));
        }
    }

    #[test]
    fn missing_or_unauthenticated_disclosures_are_not_an_unproven_counter() {
        let (keys, mut tx, args, publication) = counter_witnesses();
        tx.input.pop();
        assert!(read_disclosures(&keys, &tx).is_err());
        let mut wrong = keys.clone();
        wrong.disclosure[0].0[0][0] ^= 1;
        assert!(read_counter(&wrong, &args, publication.clone()).is_err());
        let mut changed = publication;
        changed.disclosures.values[0][0] ^= 1;
        assert!(read_counter(&keys, &args, changed).is_err());
    }

    #[test]
    fn malformed_counter_arguments_return_errors_instead_of_panicking() {
        let (keys, mut tx, args, publication) = counter_witnesses();
        for length in [0, 1, 39, args.len() - 1] {
            assert!(read_counter(&keys, &args[..length], publication.clone()).is_err());
        }
        tx.input[1].witness = Witness::new();
        assert!(read_disclosures(&keys, &tx).is_err());
        let mut wrong = keys;
        wrong.k.0.pop();
        assert!(read_counter(&wrong, &args, publication).is_err());
    }

    #[test]
    fn a_plain_publication_carries_a_full_claim_signature_without_amount_signatures() {
        let word = WotsParams::for_bytes(4);
        let message = WotsParams::for_bytes(32);
        let b = Wots256Secret::from_entropy(word, [1; 32]);
        let alice_b = WotsSecret::from_entropy(word, [2; 32]);
        let alice_e = WotsSecret::from_entropy(message, [3; 32]);
        let hub_e = Wots256Secret::from_entropy(message, [4; 32]);
        let digest = [5; 32];
        let wire = Babe::respond_exit_wire(
            &b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_b.sign(&128u32.to_be_bytes()).unwrap(),
            &alice_e.sign(&digest).unwrap(),
            &hub_e.sign(&digest).unwrap(),
            None,
        );
        // The exact e_wire used by respond() on every publication input.
        let n = 4 * word.total_digits() as usize;
        let mut args = wire[..wire.len() - n].to_vec();
        args.reverse();
        let signature = Babe::read_plain_publication_hashes(&args).unwrap();
        assert!(wots::verify_msg(
            &Hashes(hub_e.public().digits),
            &Hashes(signature),
            &digest
        ));
        for length in [0, 1, args.len() - 1] {
            assert!(Babe::read_plain_publication_hashes(&args[..length]).is_err());
        }
    }

    fn funded_opening() -> (Opening, HolderKeys, HubKeys) {
        use crate::opening::{Context, Holder};
        use k256::{elliptic_curve::PrimeField as _, Scalar};

        let seed: [u8; 32] =
            hex("2c45ef335c38f0e7417c97395c863c450c35fa34a629267e999994292aaa9ee7")
                .try_into()
                .unwrap();
        let spend: [u8; 32] =
            hex("548fedbbc7e1fb67570cac75bf01a723c31987d60bfcd44eafd8c1da39b7c54f")
                .try_into()
                .unwrap();
        let spend = Option::<Scalar>::from(Scalar::from_repr(spend.into())).unwrap();
        let holder = Holder::load(
            include_bytes!("../tests/golden/holder-funded.bin"),
            seed,
            spend,
            &Context {
                tip: 0,
                trust: Trust::mock(),
            },
        )
        .unwrap();
        let opening = holder.opening().unwrap();
        let keys = HolderKeys::derive(&seed, &opening.base.parts().nonce);
        // The frozen format-1 fixture header is MAGIC, u16 format, u8
        // derivation, u8 party, u8 phase, then its documented test secret.
        let hub = include_bytes!("../tests/golden/hub-funded.bin");
        let hub = HubKeys::from_secret(hub[9..41].try_into().unwrap());
        assert_eq!(
            hub.sealed.public(),
            *opening.base.parts().response_keys.keys()
        );
        (opening, keys, hub)
    }

    #[test]
    fn corrected_assertions_use_separate_proof_amount_and_lamport_keys() {
        let (opening, keys, hub) = funded_opening();
        let signature = Hashes(hub.b_corr.sign(&15u32.to_be_bytes()).unwrap().hashes);
        let corrected = Corrected::new(&opening, 15, &signature).unwrap();
        let trust = Trust::mock();
        let proof = corrected.mock_proof(&trust).unwrap();
        let full_proof = mock_proof(&opening, &trust).unwrap();
        assert!(corrected.proves(&trust, &proof));
        assert!(!corrected.proves(&trust, &full_proof));
        assert!(!proves(&opening, &trust, &proof));
        let tx = corrected.assert(&keys, &proof).unwrap();
        assert!(Corrected::is_assert(&opening, &tx));
        assert!(!is_assert(&opening, &tx));
        assert!(asserted(&opening, &tx).is_err());
        let (adopted, posted, b) = Corrected::asserted(&opening, &tx).unwrap();
        assert_eq!(adopted.amount(), 15);
        assert_eq!(posted, proof);
        assert_eq!(b.message(), 15u32.to_be_bytes());
        assert!(opening.babe.spec.b_key.verify(&b).is_err());
        let full_tx = assert(&opening, &keys, &full_proof).unwrap();
        assert_eq!(asserted(&opening, &full_tx).unwrap(), full_proof);
        assert!(Corrected::asserted(&opening, &full_tx).is_err());
    }

    #[test]
    fn a_corrected_assertion_reuses_its_posted_amount_signature_despite_digit_aliases() {
        let (opening, keys, hub) = funded_opening();
        let signature = Hashes(hub.b_corr.sign(&15u32.to_be_bytes()).unwrap().hashes);
        let corrected = Corrected::new(&opening, 15, &signature).unwrap();
        let proof = corrected.mock_proof(&Trust::mock()).unwrap();
        let original = corrected.assert(&keys, &proof).unwrap();
        for alias in [vec![16], vec![16, 1]] {
            let mut tx = original.clone();
            let mut arguments = args(&tx);
            let end = arguments.len();
            alias_digit(&mut arguments, lichen_babe::setup::DIGITS + 2..end, &alias);
            replace_arguments(&mut tx, 0, arguments);
            let (adopted, posted, b) = Corrected::asserted(&opening, &tx).unwrap();
            assert_eq!(adopted.amount(), 15);
            assert_eq!(posted, proof);
            assert_eq!(b.message(), 15u32.to_be_bytes());
        }
        let mut changed = original;
        let mut arguments = args(&changed);
        let at =
            lichen_babe::setup::DIGITS + 2 + 2 * WotsParams::for_bytes(4).total_digits() as usize;
        arguments[at + 1][0] ^= 1;
        replace_arguments(&mut changed, 0, arguments);
        assert!(Corrected::asserted(&opening, &changed).is_err());
    }

    #[test]
    fn corrected_amounts_must_be_signed_within_the_cap_including_zero() {
        let (opening, _, hub) = funded_opening();
        let signature = Hashes(hub.b_corr.sign(&0u32.to_be_bytes()).unwrap().hashes);
        assert_eq!(Corrected::new(&opening, 0, &signature).unwrap().amount(), 0);
        assert!(Corrected::new(&opening, 1, &signature).is_err());
        let amount = opening.base.parts().cap + 1;
        let signature = Hashes(hub.b_corr.sign(&amount.to_be_bytes()).unwrap().hashes);
        assert!(Corrected::new(&opening, amount, &signature).is_err());
    }

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
