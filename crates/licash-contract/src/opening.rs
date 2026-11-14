//! A contract's opening as its two parties play it, each from its own
//! secrets and the other's messages ([`wire`]). The holder ([`Holder`])
//! and the hub ([`Hub`]) each compile the graph from Base, freeze F and R
//! and derive D, and check what the other sends before answering. The hub
//! commits to its instances before the holder chooses which to keep, and
//! the holder signs F last.
//!
//! A machine's `step` takes one input and says what to do next: messages
//! to send, F to fund or to sign, F to broadcast. A refused input (an
//! error) leaves the machine as it was: its driver decides whether to go
//! on with the next or give the opening up.
//!
//! A holder is stored after each step she takes ([`Holder::to_bytes`]):
//! her driver keeps the instances she asks it to ([`Out::Keep`]), then
//! stores her, then carries out the rest. Resumed ([`Holder::load`]), she
//! asks again what the step to her phase asked ([`Holder::pending`]). A
//! hub is stored likewise ([`Hub::to_bytes`], [`Hub::load`],
//! [`Hub::pending`]), its secret in its state; it keeps only its
//! instances' commitments and hashlocks, and builds each kept one again
//! to send it ([`Out::Serve`], [`Hub::instance`]). Each says whether its
//! driver may give the opening up ([`Holder::abandonable`],
//! [`Hub::abandonable`]), and once F is signed whole each keeps its side
//! of the contract ([`Opening`], [`HubOpening`]). From its signature on,
//! each watches F on the chain until it stands or never can ([`Fate`]),
//! giving it up past its gate's deadline ([`Holder::lapsed`],
//! [`Hub::lapsed`]) by spending a coin of its own of F ([`Funding`]).

use std::collections::HashMap;

use anyhow::{bail, ensure, Context as _, Result};
use ark_bn254::{Bn254, Fr};
use ark_groth16::VerifyingKey;
use k256::Scalar;
use licash::base::{
    Base, ChainTerms, Fees, FinalDigest, GraphTerms, Limits, Parts, Party, Programs, Recipe,
    ResponseKeys,
};
use licash::chain::{self, Checkpoint};
use licash::crypto::{self, Point};
use licash::note::Keyset;
use licash::opening::{FrozenPrefix, GraphOutput, RequestLeaf};
use licash::request::Signer;
use licash::spend::ChainProfile;
use licash::statement::Journal;
use lichen_babe::reveal::{check_presigs, points};
use lichen_babe::setup::{instance, par, Hash, Instance, DIGITS};
use lngap_btc::adaptor::AdaptorSig;
use lngap_btc::bitcoin::consensus::{deserialize, serialize};
use lngap_btc::bitcoin::hashes::Hash as _;
use lngap_btc::bitcoin::secp256k1::{Message, Secp256k1, XOnlyPublicKey};
use lngap_btc::bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use lngap_btc::bitcoin::taproot;
use lngap_btc::bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, TxOut};
use lngap_channel::{ChannelParams, PartyPubKeys};
use lngap_lamport::winternitz::{Wots256Public, WotsParams, WotsPublic};
use lngap_sessions::session::Terms;
use lngap_sessions::skeleton::{sig_name, sign_all, Presign, Signed};

use crate::graph::{Babe, Corrected, ExitKeys, Publication, Spec, Windows};
use crate::keys::{Dealt, HolderKeys, HubKeys};
use crate::wire::{self, Envelope, Kind, Shown, STAGES};
use crate::{INSTANCES, KEPT};

mod fate;
mod state;
pub use fate::{Fate, Funding, Spent};
pub use state::{HubPhase, Phase};

/// The statement BABE encrypts under: its Groth16 verifying key and the
/// public inputs of the journal fixed at opening.
pub struct Statement {
    pub vk: VerifyingKey<Bn254>,
    pub public: Vec<Fr>,
}

/// One assertion's setup as the holder keeps it: her adaptor
/// pre-signatures of its challenge, her kept instances by index (1-based,
/// as the shares), and the opened instances' shares, checked.
pub struct Setup {
    pub presigs: Vec<[AdaptorSig; 4]>,
    pub instances: Vec<(u64, Instance)>,
    pub opened: OpenedShares,
}

pub type OpenedShares = Vec<(u64, Vec<[[u8; 32]; 4]>)>;

/// Who vouches for proofs: the programs Base must name and the statement
/// the instances are garbled under, a party's own and never a peer's.
/// `mock` marks an insecure authority, under which anyone "proves"
/// anything: an opening is of one kind or the other, and both parties
/// say which.
#[derive(Clone, Copy)]
pub struct Trust {
    pub programs: Programs,
    pub statement: fn(&Journal) -> Statement,
    pub mock: bool,
}

/// What a step is judged against: the party's own chain tip and trust.
#[derive(Clone, Copy)]
pub struct Context {
    pub tip: u32,
    pub trust: Trust,
}

/// The statement journal the instances encrypt under: `c ‖ D ‖ H(pk_H ‖ O_D)`.
pub fn journal(base: &Base, d: &FinalDigest, c: u32) -> Journal {
    let p = base.parts();
    Journal {
        c,
        d: *d.as_bytes(),
        keys: p.response_keys.keys().digest(&p.deposit),
    }
}

/// What a machine is given.
pub enum Input {
    /// A message from the other party.
    Message(Vec<u8>),
    /// The hub's F as it funded it, unsigned, and the coins it spends,
    /// one per input: the answer to [`Out::Fund`].
    Funded {
        funding: Transaction,
        prevouts: Vec<TxOut>,
    },
    /// F with the inputs [`Out::Sign`] named signed: its answer.
    Signed(Transaction),
}

/// What a machine asks of its driver.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// A message for the other party.
    Send(Vec<u8>),
    /// The holder's: keep her instance `index` (0-based) of assertion
    /// `stage`, its commitment `commitment`, before storing her state; she
    /// keeps only its hashlock, and reads it back for [`Opening::setups`].
    Keep {
        stage: usize,
        index: usize,
        commitment: Hash,
        bytes: Vec<u8>,
    },
    /// The hub's: send the holder her kept instance `index` (0-based) of
    /// assertion `stage`, the message [`Hub::instance`] builds again.
    Serve { stage: usize, index: usize },
    /// The hub's: fund F, spending the holder's `deposit`, the coin
    /// `coin`, among native segwit inputs of its own, paying `outputs` as
    /// they are (the graph's, and her change if she is owed any) and each
    /// of the `publication` scripts once, worth what the hub chooses;
    /// answer [`Input::Funded`].
    Fund {
        deposit: OutPoint,
        coin: TxOut,
        outputs: Vec<TxOut>,
        publication: [ScriptBuf; 5],
    },
    /// Sign F's `inputs` (0-based) and no other, spending `prevouts`, one
    /// per input, as a taproot signature commits to them all: the hub's
    /// all its own once she consents, the holder's her deposit once the
    /// hub has signed. Answer [`Input::Signed`].
    Sign {
        funding: Transaction,
        prevouts: Vec<TxOut>,
        inputs: Vec<usize>,
    },
    /// The hub's: F, complete, for the chain.
    Broadcast(Transaction),
}

fn send(kind: Kind, id: [u8; 32], body: Vec<u8>) -> Out {
    Out::Send(Envelope { kind, id, body }.to_bytes())
}

/// Refused unless an opening under mock proofs or not (`mock`) is judged
/// under a trust of its kind.
fn same_kind(mock: bool, ctx: &Context) -> Result<()> {
    let kind = |mock| if mock { "mock" } else { "genuine" };
    ensure!(
        ctx.trust.mock == mock,
        "an opening under {} proofs, not {}",
        kind(mock),
        kind(ctx.trust.mock)
    );
    Ok(())
}

fn party(keys: &PartyPubKeys) -> Party {
    Party {
        funding: keys.funding.serialize(),
        payment: keys.payment.serialize(),
        delayed: keys.delayed.serialize(),
        payout_script: keys.payout_spk.to_bytes(),
    }
}

fn party_pub(p: &Party) -> Result<PartyPubKeys> {
    Ok(PartyPubKeys {
        funding: XOnlyPublicKey::from_slice(&p.funding)?,
        payment: XOnlyPublicKey::from_slice(&p.payment)?,
        delayed: XOnlyPublicKey::from_slice(&p.delayed)?,
        payout_spk: ScriptBuf::from_bytes(p.payout_script.clone()),
    })
}

fn deposit(base: &Base) -> Result<OutPoint> {
    Ok(deserialize(&base.parts().deposit)?)
}

/// Whether `script` is native segwit, whose spends' scriptSigs are empty:
/// their signatures cannot change the txid of F, which R and the
/// publication coins spend before it is signed.
fn segwit(script: &ScriptBuf) -> bool {
    script.is_p2wpkh() || script.is_p2wsh() || script.is_p2tr()
}

/// What the holder pays into F: her reserve and her fee.
fn price(graph: &GraphTerms, fees: &Fees) -> Result<u64> {
    graph
        .reserve_alice
        .checked_add(fees.opening_fee_quote)
        .context("a price past money")
}

/// Refused unless her deposit is a taproot coin worth her price, whose
/// signature commits to every coin F spends (a coin lied about voids it
/// rather than letting F spend otherwise), and her change native segwit.
fn check_deposit(open: &wire::Open, graph: &GraphTerms, fees: &Fees) -> Result<()> {
    ensure!(
        open.coin.script_pubkey.is_p2tr(),
        "a deposit not a taproot coin"
    );
    ensure!(segwit(&open.change), "change to a script not native segwit");
    let price = price(graph, fees)?;
    ensure!(
        open.coin.value.to_sat() >= price,
        "a deposit of {} short of her {price}",
        open.coin.value.to_sat()
    );
    Ok(())
}

/// The change F owes the holder of `open`: her deposit but her price, none
/// below dust, which F's fee takes.
fn change(base: &Base, open: &wire::Open) -> Result<Option<TxOut>> {
    let p = base.parts();
    let left = open
        .coin
        .value
        .to_sat()
        .checked_sub(price(&p.graph, &p.fees)?)
        .context("a deposit short of her price")?;
    Ok((left >= p.graph.dust).then(|| TxOut {
        value: Amount::from_sat(left),
        script_pubkey: open.change.clone(),
    }))
}

/// F funded for the holder of `open`: spending `prevouts`, one per input,
/// each native segwit and her deposit's as hers; paying her change as
/// owed, and each of the `scripts` once, the publication coins it gives.
fn check_funds(
    base: &Base,
    open: &wire::Open,
    funding: &Transaction,
    prevouts: &[TxOut],
    scripts: &[ScriptBuf; 5],
) -> Result<[(OutPoint, TxOut); 5]> {
    ensure!(
        prevouts.len() == funding.input.len(),
        "{} coins for F's {} inputs",
        prevouts.len(),
        funding.input.len()
    );
    for (input, coin) in funding.input.iter().zip(prevouts) {
        ensure!(
            input.previous_output != open.deposit || *coin == open.coin,
            "her deposit spent as another coin"
        );
        ensure!(
            segwit(&coin.script_pubkey),
            "F spends a coin not native segwit"
        );
    }
    let paying: Vec<&TxOut> = funding
        .output
        .iter()
        .filter(|o| o.script_pubkey == open.change)
        .collect();
    match change(base, open)? {
        Some(owed) => ensure!(
            paying == [&owed],
            "F pays her change {:?}, not {}",
            paying.iter().map(|o| o.value.to_sat()).collect::<Vec<_>>(),
            owed.value.to_sat()
        ),
        None => ensure!(paying.is_empty(), "F pays her change, none owed"),
    }
    let txid = funding.compute_txid();
    let coins: Vec<(OutPoint, TxOut)> = scripts
        .iter()
        .enumerate()
        .map(|(i, script)| {
            let at: Vec<usize> = (0..funding.output.len())
                .filter(|v| funding.output[*v].script_pubkey == *script)
                .collect();
            ensure!(
                at.len() == 1,
                "F pays publication coin {i} {} times",
                at.len()
            );
            let vout = u32::try_from(at[0])?;
            Ok((OutPoint { txid, vout }, funding.output[at[0]].clone()))
        })
        .collect::<Result<_>>()?;
    Ok(coins.try_into().expect("five coins"))
}

/// Where F, as frozen, spends her `deposit`: once.
fn at(funding: &Transaction, deposit: OutPoint) -> usize {
    let at = funding
        .input
        .iter()
        .position(|i| i.previous_output == deposit);
    at.expect("F spends her deposit")
}

/// Refused unless input `index` of F is spent by a signature, over all of
/// F, of the key its coin pays, a taproot output among `prevouts`: of the
/// key alone, which a hub can check, never a script's.
fn check_key_spend(funding: &Transaction, index: usize, prevouts: &[TxOut]) -> Result<()> {
    let script = prevouts[index].script_pubkey.as_bytes();
    ensure!(script.len() == 34, "her deposit not a taproot coin");
    let key = XOnlyPublicKey::from_slice(&script[2..])?;
    let witness = &funding.input[index].witness;
    ensure!(
        witness.len() == 1,
        "her deposit spent otherwise than by its key"
    );
    let sig = taproot::Signature::from_slice(&witness[0]).context("her deposit's signature")?;
    ensure!(
        matches!(
            sig.sighash_type,
            TapSighashType::Default | TapSighashType::All
        ),
        "her deposit signed {} only",
        sig.sighash_type
    );
    let sighash = SighashCache::new(funding).taproot_key_spend_signature_hash(
        index,
        &Prevouts::All(prevouts),
        sig.sighash_type,
    )?;
    let message = Message::from_digest(sighash.to_byte_array());
    Secp256k1::verification_only()
        .verify_schnorr(&sig.signature, &message, &key)
        .context("her deposit's signature")?;
    Ok(())
}

/// The scripts of the publication coins of the graph `draft`.
fn publication_scripts(base: &Base, draft: &Babe) -> Result<[ScriptBuf; 5]> {
    Ok(draft
        .publication_trees(&disclosure(base)?)?
        .map(|t| t.script_pubkey()))
}

fn spend_root(spend: &Scalar) -> Point {
    Point::from_projective(&crypto::generator_mul(spend))
}

/// A key of `bytes` bytes from its chains' ends, refused unless one per
/// digit: the graph's scripts index them by digit, and a peer sends them.
fn chains<H: Clone>(digits: &[H], bytes: usize) -> Result<(WotsParams, Vec<H>)> {
    let params = WotsParams::for_bytes(bytes as u32);
    ensure!(
        digits.len() == params.total_digits() as usize,
        "a key of {} chains, not {}",
        digits.len(),
        params.total_digits()
    );
    Ok((params, digits.to_vec()))
}

/// The holder's keys as ln-gap's HASH160 WOTS public keys.
fn wots(digits: &[[u8; 20]], bytes: usize) -> Result<WotsPublic> {
    let (params, digits) = chains(digits, bytes)?;
    Ok(WotsPublic { params, digits })
}

/// A hub key of Base's as the graph reads it: the same chains.
fn wots256(hashes: &licash::wots::Hashes, bytes: usize) -> Result<Wots256Public> {
    let (params, digits) = chains(&hashes.0, bytes)?;
    Ok(Wots256Public { params, digits })
}

/// The disclosure keys of Base's response keys, which the publication
/// coins pay.
fn disclosure(base: &Base) -> Result<[Wots256Public; 5]> {
    let keys = base.parts().response_keys.keys();
    let [a, b, c, d, e] =
        std::array::from_fn(|i| wots256(&keys.disclosure[i], licash::disclosure::CHUNK_BYTES[i]));
    Ok([a?, b?, c?, d?, e?])
}

/// Hashlocks of a graph compiled before any instance is kept: F's graph
/// output and R do not depend on them (only `challenge_assert`'s outputs
/// do, whose leaves must differ: one placeholder per kept instance).
fn draft_hmsg() -> [Vec<Hash>; STAGES] {
    std::array::from_fn(|s| (0..KEPT).map(|i| [(s * KEPT + i + 1) as u8; 32]).collect())
}

/// The graph both parties compile: Base's terms and hub keys, the
/// holder's keys as her `Keys` message gives them, and each assertion's
/// kept instances' hashlocks.
pub fn compile(base: &Base, keys: &wire::Keys, hmsg: [Vec<Hash>; STAGES]) -> Result<Babe> {
    let p = base.parts();
    let g = &p.graph;
    let hub = p.response_keys.keys();
    let [hmsg, hmsg_corrected] = hmsg;
    let [lamport, lamport_corrected] = keys.lamport.clone();
    let [adaptor, adaptor_corrected] = keys.adaptor.clone();
    let spec = Spec {
        terms: Terms {
            id: g.id,
            unit: Amount::from_sat(g.unit_sats),
            bits: g.bits.into(),
            low_bits: g.low_bits.into(),
            deposit: g.deposit_units,
            t_close: g.close_height,
            reserve_alice: Amount::from_sat(g.reserve_alice),
            reserve_hub: Amount::from_sat(g.reserve_hub),
        },
        // The graph reads neither the channel's funding amount nor its
        // deadline margin.
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
        b_key: wots(keys.request.amount_elements(), 4)?,
        hub_b: wots256(&hub.b, 4)?,
        lamport,
        adaptor,
        hmsg,
        windows: Windows {
            w_h: g.response_window,
            t_a: g.assert_height,
            delta2: g.hub_challenge,
            delta1: g.holder_answer,
        },
        exit: Some(ExitKeys {
            e_key: wots(keys.request.claim_elements(), 32)?,
            hub_e_plain: wots256(&hub.e_plain, 32)?,
            hub_e_counter: wots256(&hub.e_counter, 32)?,
            hub_k: wots256(&hub.k, licash::statement::K_LEN)?,
            counter_items: 0,
        }),
        corrected: Some(Corrected {
            hub_b: wots256(&hub.b_corr, 4)?,
            b_key: wots(&keys.corrected_b, 4)?,
            lamport: lamport_corrected,
            adaptor: adaptor_corrected,
            hmsg: hmsg_corrected,
        }),
    };
    let (alice, hub) = (&p.parties[0].payment, &p.parties[1].payment);
    let script = keys
        .request
        .request_leaf_for_base(&base.digest(), alice, hub, g.to_self_delay)?;
    Babe::new(spec)?.with_native_request(script)
}

fn graph_output(babe: &Babe) -> Result<TxOut> {
    Ok(TxOut {
        value: babe.value(),
        script_pubkey: babe.tree()?.script_pubkey(),
    })
}

/// F and R frozen, and the D they bind: what the gates and the instances'
/// journals are over.
#[derive(Clone)]
pub struct Frozen {
    pub funding: Transaction,
    pub funding_op: OutPoint,
    pub graph: TxOut,
    pub publication: Publication,
    pub prefix: FrozenPrefix,
    pub d: FinalDigest,
}

/// Freeze F, paying `draft`'s graph output once, and R as `draft` builds
/// it on F and the publication coins.
fn freeze(
    base: &Base,
    keys: &wire::Keys,
    draft: &Babe,
    funding: Transaction,
    coins: [(OutPoint, TxOut); 5],
) -> Result<Frozen> {
    let p = base.parts();
    let graph = graph_output(draft)?;
    let paying: Vec<usize> = (0..funding.output.len())
        .filter(|i| funding.output[*i] == graph)
        .collect();
    ensure!(paying.len() == 1, "F pays the graph {} times", paying.len());
    let vout = u32::try_from(paying[0])?;
    let funding_op = OutPoint {
        txid: funding.compute_txid(),
        vout,
    };
    let publication = Publication {
        coins,
        keys: disclosure(base)?,
    };
    let set = draft.presign_with_publication(funding_op, Some(&publication))?;
    let request = &set.iter().find(|x| x.name == "request").context("R")?.tx;
    let limit = p.limits.max_transaction_bytes as usize;
    let parse = |tx: &Transaction| chain::Transaction::parse(&serialize(tx), limit);
    let output = |o: &TxOut| chain::Output {
        value: o.value.to_sat(),
        script: o.script_pubkey.to_bytes(),
    };
    let prefix = FrozenPrefix::new(
        parse(&funding)?,
        parse(request)?,
        GraphOutput {
            index: vout,
            output: output(&graph),
        },
        GraphOutput {
            index: 0,
            output: output(&request.output[0]),
        },
        RequestLeaf::for_base(
            base.digest(),
            keys.request.clone(),
            p.parties[0].payment,
            p.parties[1].payment,
            p.graph.to_self_delay,
            draft.tree()?.control_block("request")?.serialize(),
        )?,
    )?;
    let d = base.final_digest(&prefix)?;
    Ok(Frozen {
        funding,
        funding_op,
        graph,
        publication,
        prefix,
        d,
    })
}

/// The graph with the kept instances' hashlocks and its pre-signed set,
/// which must keep F's graph output and R.
fn finish(
    base: &Base,
    keys: &wire::Keys,
    frozen: &Frozen,
    hmsg: [Vec<Hash>; STAGES],
) -> Result<(Babe, Vec<Presign>)> {
    let babe = compile(base, keys, hmsg)?;
    ensure!(
        babe.tree()?.script_pubkey() == frozen.graph.script_pubkey,
        "instances changed F's graph output"
    );
    let set = babe.presign_with_publication(frozen.funding_op, Some(&frozen.publication))?;
    let r = &set.iter().find(|x| x.name == "request").context("R")?.tx;
    ensure!(
        r.compute_txid().to_byte_array() == frozen.prefix.request().txid(),
        "instances changed R"
    );
    Ok((babe, set))
}

/// Check that `sigs` sign every input of `set` under `pk` but those
/// `skip` names, which they must not carry.
fn check_sigs(
    set: &[Presign],
    pk: &XOnlyPublicKey,
    sigs: &HashMap<String, Vec<u8>>,
    skip: impl Fn(&str) -> bool,
) -> Result<()> {
    let mut expected = 0;
    for p in set {
        for i in 0..p.inputs() {
            let name = sig_name(&p.name, i);
            if skip(&name) {
                continue;
            }
            expected += 1;
            let sig = sigs
                .get(&name)
                .with_context(|| format!("no signature of {name}"))?;
            p.verify_input(pk, i, sig)?;
        }
    }
    ensure!(sigs.len() == expected, "signatures of what is not to sign");
    Ok(())
}

/// How an instance's bytes split into its parts. Its commitment hashes
/// the parts as one stream, so bytes split otherwise commit alike: a kept
/// instance must have the shape the opened ones of its assertion have.
type Shape = [usize; 4];

fn shape(x: &Instance) -> Shape {
    [
        x.gc.ands.len(),
        x.gc.dre.len(),
        x.gc.constant.len(),
        x.table.len(),
    ]
}

/// A choice of `KEPT` of `INSTANCES` instances per assertion, ascending.
fn check_choice(kept: &[Vec<usize>; STAGES]) -> Result<()> {
    for k in kept {
        ensure!(
            k.len() == KEPT
                && k.windows(2).all(|w| w[0] < w[1])
                && k.iter().all(|i| *i < INSTANCES),
            "a choice of {KEPT} of {INSTANCES} instances, ascending"
        );
    }
    Ok(())
}

/// The holder's choice, drawn when the hub has committed.
fn draw_choice() -> [Vec<usize>; STAGES] {
    use rand_core::RngCore;
    std::array::from_fn(|_| {
        let mut all: Vec<usize> = (0..INSTANCES).collect();
        for i in 0..KEPT {
            let j = i + (rand_core::OsRng.next_u64() % (INSTANCES - i) as u64) as usize;
            all.swap(i, j);
        }
        let mut kept = all[..KEPT].to_vec();
        kept.sort_unstable();
        kept
    })
}

/// The instances of each assertion a choice leaves to open.
fn others(kept: &[Vec<usize>; STAGES]) -> [Vec<usize>; STAGES] {
    std::array::from_fn(|s| (0..INSTANCES).filter(|i| !kept[s].contains(i)).collect())
}

/// What a holder brings to an opening.
pub struct Start {
    /// Her seed: her request keys come from it and Base, her keys of the
    /// contract from it and the opening's id.
    pub seed: [u8; 32],
    /// Her spending root's secret.
    pub spend: Scalar,
    /// The hub's terms she opens on, notes of its keyset among them.
    pub quote: Quote,
    /// Her coin F spends, Base's `O_D`: a taproot output, `coin`, worth
    /// at least her reserve and her fee.
    pub deposit: OutPoint,
    pub coin: TxOut,
    /// Where F pays her the rest of her deposit: a native segwit script.
    pub change: ScriptBuf,
    pub wanted: wire::Wanted,
    /// Her own view of the chain, which Base's checkpoint must be.
    pub checkpoint: Checkpoint,
}

/// A contract as its holder keeps it once opened.
pub struct Opening {
    pub base: Base,
    pub prefix: FrozenPrefix,
    pub d: FinalDigest,
    pub babe: Babe,
    pub set: Vec<Presign>,
    pub publication: Publication,
    /// Both parties' signatures, hers on the withheld transactions apart
    /// in `kept`.
    pub signed: Signed,
    pub kept: HashMap<String, Vec<u8>>,
    /// Each assertion's (the exit's, then a Corrected contract's): her
    /// pre-signatures of its challenge, the opened instances' shares,
    /// checked, and her kept instances, which her driver keeps.
    pub presigs: [Vec<[AdaptorSig; 4]>; STAGES],
    pub opened: [OpenedShares; STAGES],
    pub instances: [Vec<KeptInstance>; STAGES],
    shapes: [Shape; STAGES],
    pub deposit: OutPoint,
    pub funding: Transaction,
}

/// A kept instance as its holder records it: the driver keeps its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptInstance {
    /// 0-based, as the choice.
    pub index: usize,
    pub commitment: Hash,
    pub hmsg: Hash,
}

impl Opening {
    /// Each assertion's setup, her kept instances read back by
    /// `read(stage, index, commitment)`, each checked as she took it: its
    /// commitment's, of the opened instances' shape, its hashlock hers.
    pub fn setups(
        &self,
        mut read: impl FnMut(usize, usize, &Hash) -> Result<Vec<u8>>,
    ) -> Result<[Setup; STAGES]> {
        let mut setups = Vec::with_capacity(STAGES);
        for s in 0..STAGES {
            let mut instances = Vec::with_capacity(self.instances[s].len());
            for k in &self.instances[s] {
                let bytes = read(s, k.index, &k.commitment)?;
                let x = Instance::from_bytes(&bytes).context("not an instance")?;
                ensure!(
                    x.commitment() == k.commitment
                        && shape(&x) == self.shapes[s]
                        && x.hmsg == k.hmsg,
                    "instance {} of assertion {s} not the one she kept",
                    k.index
                );
                instances.push((k.index as u64 + 1, x));
            }
            setups.push(Setup {
                presigs: self.presigs[s].clone(),
                instances,
                opened: self.opened[s].clone(),
            });
        }
        Ok(setups
            .try_into()
            .unwrap_or_else(|_| unreachable!("a setup per stage")))
    }
}

/// The holder's graph, once her instances are in: its set, and what she
/// sent the hub (her consent, pre-signatures and signatures but those she
/// withholds), apart from her signatures she withholds.
struct Graph {
    babe: Babe,
    set: Vec<Presign>,
    consent: wire::Consent,
    withheld: HashMap<String, Vec<u8>>,
}

/// The holder's side of an opening. Each field is a phase's
/// ([`Phase`]): set by the step that reaches it, and by
/// [`Holder::load`].
pub struct Holder {
    start: Start,
    id: [u8; 32],
    keys: HolderKeys,
    /// Whether she opened under mock proofs, as she told the hub.
    mock: bool,
    fixed: Option<[Vec<usize>; STAGES]>,
    base: Option<(Base, wire::Keys)>,
    commit: Option<(wire::Commit, Frozen, [Vec<usize>; STAGES])>,
    /// The hub's opened instances, their shares, and each assertion's
    /// shape of instance, which her kept ones must have.
    opened: Option<(wire::Opened, [OpenedShares; STAGES], [Shape; STAGES])>,
    /// Her kept instances in so far, by index and hashlock: ascending
    /// once all are in.
    kept: [Vec<(usize, Hash)>; STAGES],
    graph: Option<Graph>,
    signed: Option<wire::Signed>,
    funding: Option<Transaction>,
}

impl Holder {
    /// Open with a fresh nonce: the first message, for the hub. Refused
    /// on a quote under her network's [`Floors`].
    pub fn new(start: Start, ctx: &Context) -> Result<(Self, Vec<Out>)> {
        Self::with(start, ctx, None)
    }

    /// [`Holder::new`] keeping instances `kept` of each assertion. For
    /// fixtures only: a hub that knows the choice before it commits can
    /// cheat in the instances the holder keeps.
    #[cfg(feature = "fixture")]
    pub fn with_choice(
        start: Start,
        ctx: &Context,
        kept: [Vec<usize>; STAGES],
    ) -> Result<(Self, Vec<Out>)> {
        check_choice(&kept)?;
        Self::with(start, ctx, Some(kept))
    }

    fn with(
        start: Start,
        ctx: &Context,
        fixed: Option<[Vec<usize>; STAGES]>,
    ) -> Result<(Self, Vec<Out>)> {
        start.quote.check(start.checkpoint.profile())?;
        let id = Base::fresh_nonce();
        let holder = Self::started(start, id, ctx.trust.mock, fixed);
        let q = &holder.start.quote;
        check_deposit(&holder.open(), &q.graph, &q.fees)?;
        let out = vec![send(Kind::Open, id, holder.open().encode())];
        Ok((holder, out))
    }

    /// The holder of opening `id` as she started it, her keys derived.
    fn started(
        start: Start,
        id: [u8; 32],
        mock: bool,
        fixed: Option<[Vec<usize>; STAGES]>,
    ) -> Self {
        Self {
            keys: HolderKeys::derive(&start.seed, &id),
            start,
            id,
            mock,
            fixed,
            base: None,
            commit: None,
            opened: None,
            kept: Default::default(),
            graph: None,
            signed: None,
            funding: None,
        }
    }

    /// Her first message: what she brings, her keys, her trust's kind.
    fn open(&self) -> wire::Open {
        wire::Open {
            spend_root: spend_root(&self.start.spend),
            party: self.keys.party.public(),
            deposit: self.start.deposit,
            coin: self.start.coin.clone(),
            change: self.start.change.clone(),
            wanted: self.start.wanted,
            mock: self.mock,
        }
    }

    /// The opening's id: her nonce, Base's.
    pub fn id(&self) -> [u8; 32] {
        self.id
    }

    pub fn step(&mut self, input: Input, ctx: &Context) -> Result<Vec<Out>> {
        same_kind(self.mock, ctx)?;
        match input {
            Input::Message(bytes) => {
                let m = Envelope::from_bytes(&bytes)?;
                ensure!(m.id == self.id, "a message of another opening");
                match m.kind {
                    Kind::Terms => self.terms(&m.body, ctx),
                    Kind::Commit => self.commit(&m.body),
                    Kind::Opened => self.opened(&m.body, ctx),
                    Kind::Instance => self.instance(&m.body, ctx),
                    Kind::Signed => self.signed(&m.body, ctx),
                    other => bail!("a holder is not sent {other:?}"),
                }
            }
            Input::Signed(funding) => self.sign(funding),
            Input::Funded { .. } => bail!("the hub funds F"),
        }
    }

    /// M2: Base, all of it hers where hers, her deadlines, her programs and
    /// her chain. Her keys answer.
    fn terms(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.base.is_none(), "Terms again");
        let base = Base::from_bytes(body)?;
        let keys = self.check_terms(&base, ctx.trust.programs)?;
        let out = vec![send(Kind::Keys, self.id, keys.encode())];
        self.base = Some((base, keys));
        Ok(out)
    }

    /// Base checked as hers and as quoted under the programs `trusted`;
    /// her keys under it.
    fn check_terms(&self, base: &Base, trusted: Programs) -> Result<wire::Keys> {
        let p = base.parts();
        let s = &self.start;
        let mine = self.keys.party.public();
        ensure!(p.nonce == self.id, "Base of another opening");
        ensure!(
            p.spend_root == spend_root(&s.spend),
            "not her spending root"
        );
        ensure!(p.claim_key == mine.delayed.serialize(), "not her claim key");
        ensure!(p.parties[0] == party(&mine), "not her keys");
        ensure!(
            p.deposit[..] == serialize(&s.deposit)[..],
            "not her deposit"
        );
        ensure!(p.keyset == s.quote.keyset, "notes of another keyset");
        let r = &p.recipe;
        ensure!(programs(base) == trusted, "programs she does not trust");
        ensure!(
            r.graph_version == 1
                && [
                    r.full_instances,
                    r.full_kept,
                    r.corrected_instances,
                    r.corrected_kept
                ] == [INSTANCES, KEPT, INSTANCES, KEPT].map(|n| n as u16),
            "a recipe other than graph 1 keeping {KEPT} of {INSTANCES}"
        );
        let g = &p.graph;
        ensure!(g.unit_sats == 1, "a unit of {} sats", g.unit_sats);
        ensure!(
            g.assert_height == s.wanted.assert_height
                && g.close_height == s.wanted.close_height
                && g.response_window == s.wanted.response_window,
            "deadlines she did not ask for"
        );
        ensure!(
            p.chain.checkpoint.commitment() == s.checkpoint.commitment(),
            "a checkpoint not of her chain"
        );
        let quoted = offer(
            &s.quote,
            self.id,
            &self.open(),
            trusted,
            &s.checkpoint,
            p.parties[1].clone(),
            p.response_keys.clone(),
        )?;
        ensure!(quoted.digest() == base.digest(), "terms she was not quoted");
        let keys = self
            .keys
            .public(Signer::derive(&s.seed, &base.digest()).public());
        // What the hub would refuse to compile, she refuses now.
        compile(base, &keys, draft_hmsg())?;
        Ok(keys)
    }

    /// M4: F spending her deposit once, unsigned and paying her graph
    /// once, the publication coins her graph's, D hers, dealings and
    /// commitments of the recipe's shape. Her choice answers.
    fn commit(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        ensure!(self.commit.is_none(), "Commit again");
        self.base.as_ref().context("Commit before Terms")?;
        let c = wire::Commit::decode(body)?;
        let frozen = self.check_commit(&c)?;
        let kept = match &self.fixed {
            Some(kept) => kept.clone(),
            None => draw_choice(),
        };
        let out = vec![self.choose(&kept)];
        self.commit = Some((c, frozen, kept));
        Ok(out)
    }

    fn choose(&self, kept: &[Vec<usize>; STAGES]) -> Out {
        let choose = wire::Choose { kept: kept.clone() };
        send(Kind::Choose, self.id, choose.encode())
    }

    /// The hub's commitment checked: F unsigned, spending her deposit once
    /// as hers and coins she can sign for with it, paying her change as
    /// owed and the publication coins it names, frozen on it.
    fn check_commit(&self, c: &wire::Commit) -> Result<Frozen> {
        let (base, keys) = self.base.as_ref().context("Commit before Terms")?;
        let deposit = self.start.deposit;
        let spends = c
            .funding
            .input
            .iter()
            .filter(|i| i.previous_output == deposit)
            .count();
        ensure!(spends == 1, "F spends her deposit {spends} times");
        ensure!(
            c.funding
                .input
                .iter()
                .all(|i| i.script_sig.is_empty() && i.witness.is_empty()),
            "F signed before the graph"
        );
        let draft = compile(base, keys, draft_hmsg())?;
        let trees = draft.publication_trees(&disclosure(base)?)?;
        for ((_, coin), tree) in c.publication.iter().zip(&trees) {
            ensure!(
                coin.script_pubkey == tree.script_pubkey(),
                "a publication coin not the graph's"
            );
        }
        let frozen = freeze(base, keys, &draft, c.funding.clone(), c.publication.clone())?;
        let scripts = trees.each_ref().map(|t| t.script_pubkey());
        let coins = check_funds(base, &self.open(), &c.funding, &c.prevouts, &scripts)?;
        ensure!(coins == c.publication, "a publication coin not F's output");
        ensure!(*frozen.d.as_bytes() == c.d, "D not hers");
        for (dealing, commitments) in c.dealings.iter().zip(&c.commitments) {
            ensure!(
                dealing.commitments.len() == DIGITS
                    && dealing
                        .commitments
                        .iter()
                        .flatten()
                        .all(|v| v.len() == INSTANCES - KEPT + 1),
                "a dealing of another shape"
            );
            ensure!(
                commitments.len() == INSTANCES,
                "{} commitments",
                commitments.len()
            );
        }
        Ok(frozen)
    }

    /// M6: the instances she left, each on the dealing and its commitment.
    fn opened(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.opened.is_none(), "Opened again");
        let (c, frozen, _) = self.commit.as_ref().context("Opened before Commit")?;
        let (base, _) = self.base.as_ref().expect("Terms before Commit");
        let m = wire::Opened::decode(body)?;
        let shares = self.check_opened(&m)?;
        let statements =
            [0, 1].map(|stage| (ctx.trust.statement)(&journal(base, &frozen.d, stage)));
        let all: Vec<(usize, &Shown)> = (0..STAGES)
            .flat_map(|s| m.opened[s].iter().map(move |x| (s, x)))
            .collect();
        let built = par(all.len(), |i| {
            let (s, x) = all[i];
            let st = &statements[s];
            let (built, _) = instance(&x.seed, &x.shares, &st.vk, &st.public);
            (built.commitment() == c.commitments[s][x.index]).then(|| shape(&built))
        });
        let mut shapes: [Option<Shape>; STAGES] = [None; STAGES];
        for ((s, _), built) in all.iter().zip(built) {
            let built = built.context("an opened instance not its commitment")?;
            ensure!(
                *shapes[*s].get_or_insert(built) == built,
                "opened instances of two shapes"
            );
        }
        let shapes = shapes.map(|x| x.expect("an opened instance of each assertion"));
        self.opened = Some((m, shares, shapes));
        Ok(vec![])
    }

    /// The opened instances' shares, checked: of the instances she left,
    /// on the dealing. Not their instances, which are hers to rebuild.
    fn check_opened(&self, m: &wire::Opened) -> Result<[OpenedShares; STAGES]> {
        let (c, _, kept) = self.commit.as_ref().context("Opened before Commit")?;
        let others = others(kept);
        let mut shares: [OpenedShares; STAGES] = Default::default();
        for s in 0..STAGES {
            ensure!(
                m.opened[s]
                    .iter()
                    .map(|x| x.index)
                    .eq(others[s].iter().copied()),
                "opened instances she did not leave"
            );
            shares[s] = m.opened[s]
                .iter()
                .map(|x| (x.index as u64 + 1, x.shares.clone()))
                .collect();
            ensure!(
                c.dealings[s].verify(&shares[s], &mut rand_core::OsRng),
                "shares off the dealing"
            );
        }
        Ok(shares)
    }

    /// M10: a kept instance, its commitment's, for her driver to keep.
    /// With the last, the graph is hers to consent to: her consent,
    /// pre-signatures and signatures answer.
    fn instance(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        let (_, _, shapes) = self
            .opened
            .as_ref()
            .context("an instance before the opened ones")?;
        let (c, _, kept) = self.commit.as_ref().expect("Commit before Opened");
        let k = wire::Kept::decode(body)?;
        ensure!(
            kept[k.stage].contains(&k.index),
            "an instance she did not keep"
        );
        let instance = Instance::from_bytes(&k.instance).context("not an instance")?;
        let commitment = c.commitments[k.stage][k.index];
        ensure!(
            instance.commitment() == commitment,
            "an instance not its commitment"
        );
        ensure!(
            shape(&instance) == shapes[k.stage],
            "an instance of another shape than the opened"
        );
        let hmsg = instance.hmsg;
        drop(instance);
        let keep = Out::Keep {
            stage: k.stage,
            index: k.index,
            commitment,
            bytes: k.instance,
        };
        // Again: kept again, for a driver that lost it.
        if self.kept[k.stage].iter().any(|(j, _)| *j == k.index) {
            return Ok(vec![keep]);
        }
        let mut all = self.kept.clone();
        all[k.stage].push((k.index, hmsg));
        if (0..STAGES).any(|s| all[s].len() < kept[s].len()) {
            self.kept = all;
            return Ok(vec![keep]);
        }
        all.iter_mut().for_each(|k| k.sort_by_key(|(i, _)| *i));
        let hmsg = std::array::from_fn(|s| all[s].iter().map(|(_, h)| *h).collect());
        let graph = self.consent(hmsg, ctx)?;
        let out = vec![keep, send(Kind::Consent, self.id, graph.consent.encode())];
        self.kept = all;
        self.graph = Some(graph);
        Ok(out)
    }

    /// Her graph with her kept instances' hashlocks, through her gate.
    fn consent(&self, hmsg: [Vec<Hash>; STAGES], ctx: &Context) -> Result<Graph> {
        let (base, keys) = self.base.as_ref().expect("Terms before Commit");
        let (c, frozen, _) = self.commit.as_ref().expect("Commit before Opened");
        let (babe, set) = finish(base, keys, frozen, hmsg)?;
        let consent = self.gate(ctx)?;
        let presigs = [
            self.keys
                .adaptor
                .presign(&Babe::challenge_sighash(&set)?, &points(&c.dealings[0])),
            self.keys.corrected.adaptor.presign(
                &Babe::challenge_corrected_sighash(&set)?,
                &points(&c.dealings[1]),
            ),
        ];
        let (sigs, withheld): (HashMap<_, _>, HashMap<_, _>) =
            sign_all(&set, &self.keys.party.payment)?
                .into_iter()
                .partition(|(n, _)| !Babe::withheld(n));
        Ok(Graph {
            babe,
            set,
            consent: wire::Consent {
                consent,
                presigs,
                sigs,
            },
            withheld,
        })
    }

    /// Her funding gate at her tip: her consent to D, refused once its
    /// deadlines are too near.
    fn gate(&self, ctx: &Context) -> Result<crypto::Signature> {
        let (base, _) = self.base.as_ref().expect("Terms before Commit");
        let (_, frozen, _) = self.commit.as_ref().expect("Commit before Opened");
        let s = &self.start;
        Ok(base.holder_gate(
            &frozen.prefix,
            &frozen.graph.script_pubkey.to_bytes(),
            &ctx.trust.programs,
            &s.seed,
            &s.spend,
            ctx.tip,
        )?)
    }

    /// Her graph again from what she sent and withheld: the set her kept
    /// instances' hashlocks make, her pre-signatures on the dealings and
    /// her signatures of all of it under her key.
    fn check_graph(
        &self,
        consent: wire::Consent,
        withheld: HashMap<String, Vec<u8>>,
    ) -> Result<Graph> {
        let (base, keys) = self.base.as_ref().context("a graph before Terms")?;
        let (c, frozen, _) = self.commit.as_ref().context("a graph before Commit")?;
        let hmsg = std::array::from_fn(|s| self.kept[s].iter().map(|(_, h)| *h).collect());
        let (babe, set) = finish(base, keys, frozen, hmsg)?;
        let sighashes = [
            Babe::challenge_sighash(&set)?,
            Babe::challenge_corrected_sighash(&set)?,
        ];
        for (s, sighash) in sighashes.iter().enumerate() {
            ensure!(
                check_presigs(
                    &keys.adaptor[s],
                    sighash,
                    &points(&c.dealings[s]),
                    &consent.presigs[s]
                ),
                "her pre-signatures of assertion {s} not on its dealing"
            );
        }
        ensure!(
            withheld.keys().all(|n| Babe::withheld(n))
                && !consent.sigs.keys().any(|n| Babe::withheld(n)),
            "her signatures withheld otherwise"
        );
        let mut all = withheld.clone();
        all.extend(consent.sigs.iter().map(|(n, s)| (n.clone(), s.clone())));
        check_sigs(&set, &self.keys.party.public().payment, &all, |_| false)?;
        Ok(Graph {
            babe,
            set,
            consent,
            withheld,
        })
    }

    /// M8: the hub's signatures of all her graph, on F as frozen, with
    /// her gate passing still. Her signing F answers.
    fn signed(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.signed.is_none(), "Signed again");
        self.graph.as_ref().context("Signed before her consent")?;
        let m = wire::Signed::decode(body)?;
        self.check_signed(&m)?;
        self.gate(ctx)?;
        let out = vec![self.to_sign(&m.funding)];
        self.signed = Some(m);
        Ok(out)
    }

    /// Her deposit to sign on the hub's signed F, `funding`.
    fn to_sign(&self, funding: &Transaction) -> Out {
        let (c, _, _) = self.commit.as_ref().expect("Commit before Signed");
        Out::Sign {
            funding: funding.clone(),
            prevouts: c.prevouts.clone(),
            inputs: vec![at(funding, self.start.deposit)],
        }
    }

    fn check_signed(&self, m: &wire::Signed) -> Result<()> {
        let graph = self.graph.as_ref().context("Signed before her consent")?;
        let (base, _) = self.base.as_ref().expect("Terms before Commit");
        let (_, frozen, _) = self.commit.as_ref().expect("Commit before Opened");
        ensure!(
            m.funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        let deposit = self.start.deposit;
        ensure!(
            m.funding
                .input
                .iter()
                .all(|i| i.previous_output != deposit || i.witness.is_empty()),
            "her input of F signed by another"
        );
        ensure!(
            m.funding
                .input
                .iter()
                .all(|i| i.previous_output == deposit || !i.witness.is_empty()),
            "F's other inputs unsigned"
        );
        let hub = party_pub(&base.parts().parties[1])?.payment;
        check_sigs(&graph.set, &hub, &m.sigs, |_| false)
    }

    /// F with her deposit signed: her signature alone taken into the
    /// hub's F, signed whole then, hers to send the hub.
    fn sign(&mut self, funding: Transaction) -> Result<Vec<Out>> {
        let signed = self
            .signed
            .as_ref()
            .context("F to sign before the hub's signatures")?;
        ensure!(self.funding.is_none(), "F signed again");
        ensure!(
            funding.compute_txid() == signed.funding.compute_txid(),
            "F not as frozen"
        );
        let i = at(&funding, self.start.deposit);
        let mut whole = signed.funding.clone();
        whole.input[i].witness = funding.input[i].witness.clone();
        self.check_funding(&whole)?;
        let out = vec![self.funded(&whole)];
        self.funding = Some(whole);
        Ok(out)
    }

    fn funded(&self, funding: &Transaction) -> Out {
        let funded = wire::Funded {
            funding: funding.clone(),
        };
        send(Kind::Funded, self.id, funded.encode())
    }

    /// F as frozen, her deposit spent by her key and the rest as the hub
    /// signed it.
    fn check_funding(&self, funding: &Transaction) -> Result<()> {
        let signed = self
            .signed
            .as_ref()
            .context("F to sign before the hub's signatures")?;
        let (c, frozen, _) = self.commit.as_ref().expect("Commit before Signed");
        ensure!(
            funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        let i = at(funding, self.start.deposit);
        ensure!(
            funding
                .input
                .iter()
                .zip(&signed.funding.input)
                .enumerate()
                .all(|(j, (a, b))| j == i || a.witness == b.witness),
            "F signed otherwise than by the hub"
        );
        check_key_spend(funding, i, &c.prevouts)
    }

    /// The contract, once F is signed whole.
    pub fn opening(self) -> Result<Opening> {
        let funding = self.funding.context("an opening not funded")?;
        let (base, _) = self.base.expect("Terms before F");
        let (c, frozen, _) = self.commit.expect("Commit before F");
        let graph = self.graph.expect("her graph before F");
        let (_, opened, shapes) = self.opened.expect("Opened before F");
        let kept = &self.kept;
        let instances = std::array::from_fn(|s| {
            kept[s]
                .iter()
                .map(|(index, hmsg)| KeptInstance {
                    index: *index,
                    commitment: c.commitments[s][*index],
                    hmsg: *hmsg,
                })
                .collect()
        });
        Ok(Opening {
            base,
            prefix: frozen.prefix,
            d: frozen.d,
            babe: graph.babe,
            set: graph.set,
            publication: frozen.publication,
            signed: Signed {
                hub: self.signed.expect("the hub's signatures before F").sigs,
                alice: graph.consent.sigs,
            },
            kept: graph.withheld,
            presigs: graph.consent.presigs,
            opened,
            instances,
            shapes,
            deposit: self.start.deposit,
            funding,
        })
    }
}

/// What a hub offers: all of Base but the holder's side, the deadlines
/// she asks for and the programs, its trust's. A holder opens on the
/// quote she was given: the Base she takes is the one it makes.
#[derive(Clone)]
pub struct Quote {
    pub keyset: Keyset,
    pub cap: u32,
    /// The deadlines and response window in it are the holder's.
    pub graph: GraphTerms,
    /// The programs in it are its trust's.
    pub recipe: Recipe,
    pub chain: ChainTerms,
    pub limits: Limits,
    pub fees: Fees,
}

/// The shortest windows and shallowest depths a holder opens on, by
/// network: blocks she has to answer a challenge (`holder_answer`) and of
/// the request leaf's relative delay (`to_self_delay`), and how deep F,
/// her request and its publication must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floors {
    pub holder_answer: u16,
    pub to_self_delay: u16,
    pub depth: u32,
}

impl Floors {
    pub fn of(profile: ChainProfile) -> Self {
        let (holder_answer, to_self_delay, depth) = match profile {
            ChainProfile::Core30DefaultRegtest => (6, 6, 1),
            // A day of blocks.
            ChainProfile::Core30Mainnet | ChainProfile::Core30Signet => (144, 144, 3),
            ChainProfile::Mutinynet => (2_880, 2_880, 6),
        };
        Self {
            holder_answer,
            to_self_delay,
            depth,
        }
    }
}

impl Quote {
    /// Refused under `profile`'s floors.
    pub fn check(&self, profile: ChainProfile) -> Result<()> {
        let f = Floors::of(profile);
        let g = &self.graph;
        ensure!(
            g.holder_answer >= f.holder_answer,
            "{} blocks to answer, not {} at least",
            g.holder_answer,
            f.holder_answer
        );
        ensure!(
            g.to_self_delay >= f.to_self_delay,
            "a delay of {} blocks, not {} at least",
            g.to_self_delay,
            f.to_self_delay
        );
        let c = &self.chain;
        for (what, depth) in [
            ("F", c.funding_depth),
            ("a request", c.request_depth),
            ("a publication", c.publication_depth),
        ] {
            ensure!(
                depth >= f.depth,
                "{what} {depth} blocks deep, not {} at least",
                f.depth
            );
        }
        Ok(())
    }
}

/// The programs `base` names.
fn programs(base: &Base) -> Programs {
    let r = &base.parts().recipe;
    Programs {
        holder: r.holder_program,
        counter: r.counter_program,
    }
}

/// What a hub lets a holder ask of it beyond its quote, at the tip she
/// opens at: how long its collateral may stay locked, Base's close height
/// at most that many blocks ahead, and how long it has to respond to her
/// request at least. Its policy, checked as her Open comes in only:
/// neither Base nor its state keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub lock: u32,
    pub response_window: u16,
}

impl Bounds {
    /// Refused unless what she wants at `tip` is within them.
    pub fn check(&self, wanted: &wire::Wanted, tip: u32) -> Result<()> {
        let lock = wanted.close_height.saturating_sub(tip);
        ensure!(
            lock <= self.lock,
            "collateral locked {lock} blocks, not {} at most",
            self.lock
        );
        ensure!(
            wanted.response_window >= self.response_window,
            "a response window of {} blocks, not {} at least",
            wanted.response_window,
            self.response_window
        );
        Ok(())
    }
}

/// Base on `quote` for the holder of `open`: her side and deadlines,
/// the `programs` trusted and her `checkpoint`, with the hub's party and
/// response keys. The hub builds it to offer, she again to check that it
/// offers what it quoted.
fn offer(
    quote: &Quote,
    id: [u8; 32],
    open: &wire::Open,
    programs: Programs,
    checkpoint: &Checkpoint,
    hub: Party,
    response_keys: ResponseKeys,
) -> Result<Base> {
    let mut graph = quote.graph.clone();
    graph.assert_height = open.wanted.assert_height;
    graph.close_height = open.wanted.close_height;
    graph.response_window = open.wanted.response_window;
    let mut recipe = quote.recipe.clone();
    recipe.holder_program = programs.holder;
    recipe.counter_program = programs.counter;
    let mut chain = quote.chain.clone();
    chain.checkpoint = checkpoint.clone();
    Ok(Base::new(Parts {
        nonce: id,
        spend_root: open.spend_root,
        claim_key: open.party.delayed.serialize(),
        keyset: quote.keyset.clone(),
        deposit: serialize(&open.deposit)
            .try_into()
            .expect("an outpoint's 36 bytes"),
        cap: quote.cap,
        response_keys,
        parties: [party(&open.party), hub],
        graph,
        recipe,
        chain,
        limits: quote.limits.clone(),
        fees: quote.fees.clone(),
    })?)
}

/// The holder's choice as its hub took it: the instances she keeps of
/// each assertion (0-based), and the graph with their hashlocks and its
/// pre-signed set.
struct Choice {
    kept: [Vec<usize>; STAGES],
    babe: Babe,
    set: Vec<Presign>,
}

/// A contract as its hub keeps it once opened: what it agreed with the
/// holder, to answer her and dispute from.
pub struct HubOpening {
    pub base: Base,
    pub prefix: FrozenPrefix,
    pub d: FinalDigest,
    pub babe: Babe,
    pub set: Vec<Presign>,
    pub publication: Publication,
    /// The holder's keys, which the graph compiles with.
    pub holder: wire::Keys,
    /// The instances she keeps of each assertion, 0-based.
    pub kept: [Vec<usize>; STAGES],
    /// Her consent to D, her pre-signatures of each assertion's challenge
    /// and her signatures of all the graph but those she withholds.
    pub consent: wire::Consent,
    /// Its signatures of all the graph.
    pub sigs: HashMap<String, Vec<u8>>,
    pub funding: Transaction,
}

/// The hub's side of an opening. Each field is a phase's ([`HubPhase`]):
/// set by the step that reaches it, and by [`Hub::load`].
pub struct Hub {
    /// The opening's own secret, which all its keys derive from.
    secret: [u8; 32],
    keys: HubKeys,
    quote: Quote,
    /// What it lets her ask, until her Open: none once loaded, which is
    /// never before it.
    bounds: Option<Bounds>,
    id: Option<[u8; 32]>,
    /// The holder's Open, and the Base that answers it.
    base: Option<(wire::Open, Base)>,
    holder: Option<(wire::Keys, Babe)>,
    /// F frozen, the commitment to its instances, and each one's hashlock:
    /// the instances themselves are dropped, and built again to send.
    frozen: Option<(Frozen, wire::Commit, [Vec<Hash>; STAGES])>,
    choice: Option<Choice>,
    /// Her consent, through its gate.
    consent: Option<wire::Consent>,
    /// Its signatures of the graph, with F its own inputs signed.
    signed: Option<wire::Signed>,
    funded: Option<Transaction>,
}

impl Hub {
    /// The hub's side of one opening, under a secret drawn for it alone.
    pub fn new(quote: Quote, bounds: Bounds) -> Self {
        use rand_core::RngCore;
        let mut secret = [0; 32];
        rand_core::OsRng.fill_bytes(&mut secret);
        Self::with(&secret, quote, Some(bounds))
    }

    /// [`Hub::new`] under `secret`. For fixtures only: a hub must never
    /// open twice under one secret.
    #[cfg(feature = "fixture")]
    pub fn with_secret(secret: &[u8; 32], quote: Quote, bounds: Bounds) -> Self {
        Self::with(secret, quote, Some(bounds))
    }

    fn with(secret: &[u8; 32], quote: Quote, bounds: Option<Bounds>) -> Self {
        Self {
            secret: *secret,
            keys: HubKeys::from_secret(secret),
            quote,
            bounds,
            id: None,
            base: None,
            holder: None,
            frozen: None,
            choice: None,
            consent: None,
            signed: None,
            funded: None,
        }
    }

    pub fn step(&mut self, input: Input, ctx: &Context) -> Result<Vec<Out>> {
        if self.base.is_some() {
            same_kind(self.mock(), ctx)?;
        }
        match input {
            Input::Message(bytes) => {
                let m = Envelope::from_bytes(&bytes)?;
                if let Some(id) = self.id {
                    ensure!(m.id == id, "a message of another opening");
                }
                match m.kind {
                    Kind::Open => self.open(m.id, &m.body, ctx),
                    Kind::Keys => self.keys(&m.body),
                    Kind::Choose => self.choose(&m.body),
                    Kind::Consent => self.consent(&m.body, ctx),
                    Kind::Funded => self.funded(&m.body),
                    other => bail!("a hub is not sent {other:?}"),
                }
            }
            Input::Funded { funding, prevouts } => self.fund(funding, prevouts, ctx),
            Input::Signed(funding) => self.sign(funding, ctx),
        }
    }

    fn id(&self) -> [u8; 32] {
        self.id.expect("an opening's id from its Open")
    }

    fn base(&self) -> &Base {
        &self.base.as_ref().expect("Open before").1
    }

    /// Whether the holder opened under mock proofs.
    fn mock(&self) -> bool {
        self.base.as_ref().expect("Open before").0.mock
    }

    /// M1: Base, the holder's side and deadlines in it, answers, if they
    /// are within its bounds at its tip.
    fn open(&mut self, id: [u8; 32], body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.base.is_none(), "Open again");
        let o = wire::Open::decode(body)?;
        let bounds = self.bounds.expect("a hub loaded took its Open");
        bounds.check(&o.wanted, ctx.tip)?;
        check_deposit(&o, &self.quote.graph, &self.quote.fees)?;
        let base = self.offer(id, &o, ctx.trust.programs, ctx)?;
        let out = vec![send(Kind::Terms, id, base.to_bytes())];
        self.id = Some(id);
        self.base = Some((o, base));
        Ok(out)
    }

    /// Its quote's Base for the holder of `o`, naming `programs`.
    fn offer(
        &self,
        id: [u8; 32],
        o: &wire::Open,
        programs: Programs,
        ctx: &Context,
    ) -> Result<Base> {
        same_kind(o.mock, ctx)?;
        let q = &self.quote;
        offer(
            q,
            id,
            o,
            programs,
            &q.chain.checkpoint,
            party(&self.keys.party.public()),
            ResponseKeys::new(self.keys.sealed.public())?,
        )
    }

    /// M3: the holder's keys, with which the graph compiles. F to fund
    /// follows.
    fn keys(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        ensure!(self.holder.is_none(), "Keys again");
        let base = &self.base.as_ref().context("Keys before Open")?.1;
        let keys = wire::Keys::decode(body)?;
        let draft = compile(base, &keys, draft_hmsg())?;
        let out = vec![self.to_fund(&draft)?];
        self.holder = Some((keys, draft));
        Ok(out)
    }

    /// F to fund, spending her deposit and paying `draft`'s graph, her
    /// change and the publication coins.
    fn to_fund(&self, draft: &Babe) -> Result<Out> {
        let (open, base) = self.base.as_ref().expect("Open before F");
        let outputs = [Some(graph_output(draft)?), change(base, open)?];
        Ok(Out::Fund {
            deposit: open.deposit,
            coin: open.coin.clone(),
            outputs: outputs.into_iter().flatten().collect(),
            publication: publication_scripts(base, draft)?,
        })
    }

    /// Each assertion's dealing: the exit's, then a Corrected contract's.
    fn dealt(&self) -> [&Dealt; STAGES] {
        [&self.keys.full, &self.keys.corrected]
    }

    /// Assertion `stage`'s statement under F's D.
    fn statement(&self, stage: usize, ctx: &Context) -> Statement {
        let (frozen, _, _) = self.frozen.as_ref().expect("F before its statements");
        (ctx.trust.statement)(&journal(self.base(), &frozen.d, stage as u32))
    }

    /// F funded: frozen, D, and the instances under its journals, which it
    /// commits to, keeping their hashlocks and dropping them.
    fn fund(
        &mut self,
        funding: Transaction,
        prevouts: Vec<TxOut>,
        ctx: &Context,
    ) -> Result<Vec<Out>> {
        ensure!(self.frozen.is_none(), "F funded again");
        self.base.as_ref().context("F before Open")?;
        self.holder.as_ref().context("F before the holder's keys")?;
        let frozen = self.check_fund(funding, &prevouts)?;
        let base = self.base();
        let built: [Vec<(Hash, Hash)>; STAGES] = std::array::from_fn(|s| {
            let st = (ctx.trust.statement)(&journal(base, &frozen.d, s as u32));
            let d = self.dealt()[s];
            par(INSTANCES, |i| {
                let x = build(d, i, &st);
                (x.commitment(), x.hmsg)
            })
        });
        let commit = wire::Commit {
            funding: frozen.funding.clone(),
            prevouts,
            publication: frozen.publication.coins.clone(),
            d: *frozen.d.as_bytes(),
            dealings: self.dealt().map(|d| d.dealing.clone()),
            commitments: built
                .each_ref()
                .map(|b| b.iter().map(|(c, _)| *c).collect()),
        };
        let hmsg = built.map(|b| b.into_iter().map(|(_, h)| h).collect());
        let out = vec![send(Kind::Commit, self.id(), commit.encode())];
        self.frozen = Some((frozen, commit, hmsg));
        Ok(out)
    }

    /// F as funded, spending her deposit once among `prevouts` as the
    /// holder needs ([`Holder::check_commit`]), frozen on her keys.
    fn check_fund(&self, funding: Transaction, prevouts: &[TxOut]) -> Result<Frozen> {
        let (keys, draft) = self.holder.as_ref().context("F before the holder's keys")?;
        let (open, base) = self.base.as_ref().expect("Open before F");
        let deposit = deposit(base)?;
        let spends = funding
            .input
            .iter()
            .filter(|i| i.previous_output == deposit)
            .count();
        ensure!(spends == 1, "F spends the deposit {spends} times");
        let scripts = publication_scripts(base, draft)?;
        let coins = check_funds(base, open, &funding, prevouts, &scripts)?;
        freeze(base, keys, draft, funding, coins)
    }

    /// M5: the holder's choice, kept for good: the same again has the
    /// same answer, another is refused. The opened instances and the kept
    /// answer.
    fn choose(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        let m = wire::Choose::decode(body)?;
        if let Some(c) = &self.choice {
            ensure!(c.kept == m.kept, "a second choice");
            return Ok(self.chosen());
        }
        self.frozen
            .as_ref()
            .context("a choice before the commitments")?;
        self.choice = Some(self.check_choose(m.kept)?);
        Ok(self.chosen())
    }

    /// The graph with the instances `kept` the holder keeps, and its set.
    fn check_choose(&self, kept: [Vec<usize>; STAGES]) -> Result<Choice> {
        check_choice(&kept)?;
        let (keys, _) = self.holder.as_ref().expect("Keys before F");
        let (frozen, _, hmsg) = self.frozen.as_ref().expect("F before a choice");
        let hmsg = std::array::from_fn(|s| kept[s].iter().map(|i| hmsg[s][*i]).collect());
        let (babe, set) = finish(self.base(), keys, frozen, hmsg)?;
        Ok(Choice { kept, babe, set })
    }

    /// The opened instances, then each kept one to serve.
    fn chosen(&self) -> Vec<Out> {
        let kept = &self.choice.as_ref().expect("a choice").kept;
        let dealt = self.dealt();
        let opened = wire::Opened {
            opened: std::array::from_fn(|s| {
                others(kept)[s]
                    .iter()
                    .map(|i| Shown {
                        index: *i,
                        seed: dealt[s].seeds[*i],
                        shares: dealt[s].dealer.shares(*i as u64 + 1),
                    })
                    .collect()
            }),
        };
        let mut out = vec![send(Kind::Opened, self.id(), opened.encode())];
        for (stage, kept) in kept.iter().enumerate() {
            out.extend(kept.iter().map(|&index| Out::Serve { stage, index }));
        }
        out
    }

    /// The message carrying the holder's kept instance `index` of
    /// assertion `stage` ([`Out::Serve`]): built again from the opening's
    /// secret, and refused unless as committed.
    pub fn instance(&self, stage: usize, index: usize, ctx: &Context) -> Result<Vec<u8>> {
        let kept = &self
            .choice
            .as_ref()
            .context("an instance before a choice")?
            .kept;
        ensure!(
            kept.get(stage).is_some_and(|k| k.contains(&index)),
            "an instance she did not keep"
        );
        same_kind(self.mock(), ctx)?;
        let (_, c, hmsg) = self.frozen.as_ref().expect("F before a choice");
        let x = build(self.dealt()[stage], index, &self.statement(stage, ctx));
        ensure!(
            x.commitment() == c.commitments[stage][index] && x.hmsg == hmsg[stage][index],
            "instance {index} of assertion {stage} built otherwise than committed"
        );
        let kept = wire::Kept {
            stage,
            index,
            instance: x.to_bytes(),
        };
        Ok(Envelope {
            kind: Kind::Instance,
            id: self.id(),
            body: kept.encode(),
        }
        .to_bytes())
    }

    /// M7: the holder's pre-signatures on its dealings, her signatures of
    /// all the graph but the withheld, and her consent, through its gate.
    /// Its own inputs of F to sign answer.
    fn consent(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.consent.is_none(), "Consent again");
        self.choice.as_ref().context("consent before a choice")?;
        let m = wire::Consent::decode(body)?;
        self.check_consent(&m)?;
        self.gate(&m, ctx)?;
        let out = vec![self.to_sign()];
        self.consent = Some(m);
        Ok(out)
    }

    /// Its funding gate at its tip on her consent `m`, refused once its
    /// deadlines are too near.
    fn gate(&self, m: &wire::Consent, ctx: &Context) -> Result<()> {
        let (frozen, _, _) = self.frozen.as_ref().expect("F before a choice");
        let retained = ResponseKeys::new(self.keys.sealed.public())?;
        let d = self.base().hub_gate(
            &frozen.prefix,
            &frozen.graph.script_pubkey.to_bytes(),
            &ctx.trust.programs,
            &retained,
            &m.consent,
            ctx.tip,
        )?;
        ensure!(d == frozen.d, "the gates agree on D");
        Ok(())
    }

    /// Its own inputs of F to sign: all but her deposit.
    fn to_sign(&self) -> Out {
        let (frozen, c, _) = self.frozen.as_ref().expect("F before her consent");
        let (open, _) = self.base.as_ref().expect("Open before F");
        let deposit = at(&frozen.funding, open.deposit);
        Out::Sign {
            funding: frozen.funding.clone(),
            prevouts: c.prevouts.clone(),
            inputs: (0..frozen.funding.input.len())
                .filter(|i| *i != deposit)
                .collect(),
        }
    }

    /// F with its own inputs signed, through its gate at its tip still:
    /// its signatures of the graph, with F, answer. Sent only once stored
    /// so, as the holder may complete F from then on.
    fn sign(&mut self, funding: Transaction, ctx: &Context) -> Result<Vec<Out>> {
        let m = self
            .consent
            .as_ref()
            .context("F signed before her consent")?;
        ensure!(self.signed.is_none(), "F signed again");
        self.gate(m, ctx)?;
        self.check_signing(&funding)?;
        let set = &self
            .choice
            .as_ref()
            .expect("a choice before her consent")
            .set;
        let signed = wire::Signed {
            sigs: sign_all(set, &self.keys.party.payment)?,
            funding,
        };
        let out = vec![send(Kind::Signed, self.id(), signed.encode())];
        self.signed = Some(signed);
        Ok(out)
    }

    /// F as frozen, by txid, with its own inputs signed and her deposit's
    /// not.
    fn check_signing(&self, funding: &Transaction) -> Result<()> {
        let (frozen, _, _) = self.frozen.as_ref().expect("F before signing");
        ensure!(
            funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        let (open, _) = self.base.as_ref().expect("Open before F");
        ensure!(
            funding
                .input
                .iter()
                .all(|i| (i.previous_output == open.deposit) == i.witness.is_empty()),
            "F signed otherwise than its own inputs"
        );
        Ok(())
    }

    /// Her pre-signatures on its dealings and her signatures of all the
    /// graph but the withheld; not her consent, which its gate checks.
    fn check_consent(&self, m: &wire::Consent) -> Result<()> {
        let set = &self.choice.as_ref().context("consent before a choice")?.set;
        let (keys, _) = self.holder.as_ref().expect("Keys before F");
        let sighashes = [
            Babe::challenge_sighash(set)?,
            Babe::challenge_corrected_sighash(set)?,
        ];
        let dealt = self.dealt();
        for (s, sighash) in sighashes.iter().enumerate() {
            ensure!(
                check_presigs(
                    &keys.adaptor[s],
                    sighash,
                    &points(&dealt[s].dealing),
                    &m.presigs[s]
                ),
                "pre-signatures of assertion {s} not on its dealing"
            );
        }
        let alice = party_pub(&self.base().parts().parties[0])?.payment;
        check_sigs(set, &alice, &m.sigs, Babe::withheld)
    }

    /// M9: F as frozen, signed whole, to broadcast.
    fn funded(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        ensure!(self.signed.is_some(), "F before the hub signed");
        ensure!(self.funded.is_none(), "F again");
        let m = wire::Funded::decode(body)?;
        let whole = self.whole(&m.funding)?;
        let out = vec![Out::Broadcast(whole.clone())];
        self.funded = Some(whole);
        Ok(out)
    }

    /// The contract, once F signed whole is in.
    pub fn opening(self) -> Result<HubOpening> {
        let funding = self.funded.context("an opening not funded")?;
        let (_, base) = self.base.expect("Open before F");
        let (holder, _) = self.holder.expect("Keys before F");
        let (frozen, _, _) = self.frozen.expect("F frozen before F");
        let choice = self.choice.expect("a choice before F");
        let consent = self.consent.expect("her consent before F");
        let signed = self.signed.expect("its signatures before F");
        Ok(HubOpening {
            base,
            prefix: frozen.prefix,
            d: frozen.d,
            babe: choice.babe,
            set: choice.set,
            publication: frozen.publication,
            holder,
            kept: choice.kept,
            consent,
            sigs: signed.sigs,
            funding,
        })
    }

    /// F whole, to broadcast: its own signed F with her deposit's witness
    /// from `funding`, a signature of her key. What she says of its own
    /// witnesses is not taken, so she cannot spoil them.
    fn whole(&self, funding: &Transaction) -> Result<Transaction> {
        let signed = self.signed.as_ref().context("F before the hub signed")?;
        let (frozen, c, _) = self.frozen.as_ref().expect("F before signing");
        ensure!(
            funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        let (open, _) = self.base.as_ref().expect("Open before F");
        let i = at(funding, open.deposit);
        ensure!(!funding.input[i].witness.is_empty(), "F not signed whole");
        let mut whole = signed.funding.clone();
        whole.input[i].witness = funding.input[i].witness.clone();
        check_key_spend(&whole, i, &c.prevouts)?;
        Ok(whole)
    }
}

/// Instance `i` (0-based) of dealing `d`, under statement `st`.
fn build(d: &Dealt, i: usize, st: &Statement) -> Instance {
    instance(
        &d.seeds[i],
        &d.dealer.shares(i as u64 + 1),
        &st.vk,
        &st.public,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hub_bounds_how_long_she_locks_it_and_how_soon_it_answers() {
        let bounds = Bounds {
            lock: 100,
            response_window: 6,
        };
        let wanted = |close_height, response_window| wire::Wanted {
            assert_height: 1_010,
            close_height,
            response_window,
        };
        assert!(bounds.check(&wanted(1_100, 6), 1_000).is_ok());
        // A block longer locked, at the same tip or one sooner.
        let error = bounds.check(&wanted(1_101, 6), 1_000).unwrap_err();
        assert!(error.to_string().contains("locked 101 blocks"), "{error}");
        assert!(bounds.check(&wanted(1_100, 6), 999).is_err());
        assert!(bounds.check(&wanted(1_100, 6), 1_001).is_ok());
        // A block shorter to answer in.
        let error = bounds.check(&wanted(1_100, 5), 1_000).unwrap_err();
        assert!(error.to_string().contains("window of 5 blocks"), "{error}");
        assert!(bounds.check(&wanted(1_100, 7), 1_000).is_ok());
    }
}
