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
//! asks again what the step to her phase asked ([`Holder::pending`]).

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
use lngap_btc::bitcoin::secp256k1::XOnlyPublicKey;
use lngap_btc::bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, TxOut};
use lngap_channel::{ChannelParams, PartyPubKeys};
use lngap_lamport::winternitz::{Wots256Public, WotsParams, WotsPublic};
use lngap_sessions::session::Terms;
use lngap_sessions::skeleton::{sig_name, sign_all, Presign, Signed};

use crate::graph::{Babe, Corrected, ExitKeys, Publication, Spec, Windows};
use crate::keys::{HolderKeys, HubKeys};
use crate::wire::{self, Envelope, Kind, Shown, STAGES};
use crate::{INSTANCES, KEPT};

mod state;
pub use state::Phase;

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
    /// The hub's F as it funded it, unsigned, and the five publication
    /// coins: the answer to [`Out::Fund`].
    Funded {
        funding: Transaction,
        publication: Box<[(OutPoint, TxOut); 5]>,
    },
    /// The holder's F with her inputs signed: the answer to [`Out::Sign`].
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
    /// The hub's: fund F, paying `graph` and spending the holder's
    /// `deposit` among its inputs, and five coins paying `publication`;
    /// answer [`Input::Funded`].
    Fund {
        deposit: OutPoint,
        graph: TxOut,
        publication: [ScriptBuf; 5],
    },
    /// The holder's: sign her inputs of F; answer [`Input::Signed`].
    Sign(Transaction),
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
    /// Her coin F spends, Base's `O_D`.
    pub deposit: OutPoint,
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
        let keys = self.check_terms(&base, ctx)?;
        let out = vec![send(Kind::Keys, self.id, keys.encode())];
        self.base = Some((base, keys));
        Ok(out)
    }

    /// Base checked as hers and as quoted; her keys under it.
    fn check_terms(&self, base: &Base, ctx: &Context) -> Result<wire::Keys> {
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
        let programs = Programs {
            holder: r.holder_program,
            counter: r.counter_program,
        };
        ensure!(
            programs == ctx.trust.programs,
            "programs she does not trust"
        );
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
            ctx.trust.programs,
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

    /// The hub's commitment checked; F frozen on it.
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
        let out = vec![Out::Sign(m.funding.clone())];
        self.signed = Some(m);
        Ok(out)
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
        let hub = party_pub(&base.parts().parties[1])?.payment;
        check_sigs(&graph.set, &hub, &m.sigs, |_| false)
    }

    /// F with her inputs signed, all of it: hers to send the hub.
    fn sign(&mut self, funding: Transaction) -> Result<Vec<Out>> {
        ensure!(
            self.signed.is_some(),
            "F to sign before the hub's signatures"
        );
        ensure!(self.funding.is_none(), "F signed again");
        self.check_funding(&funding)?;
        let out = vec![self.funded(&funding)];
        self.funding = Some(funding);
        Ok(out)
    }

    fn funded(&self, funding: &Transaction) -> Out {
        let funded = wire::Funded {
            funding: funding.clone(),
        };
        send(Kind::Funded, self.id, funded.encode())
    }

    fn check_funding(&self, funding: &Transaction) -> Result<()> {
        ensure!(
            self.signed.is_some(),
            "F to sign before the hub's signatures"
        );
        let (_, frozen, _) = self.commit.as_ref().expect("Commit before Signed");
        ensure!(
            funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        ensure!(
            funding
                .input
                .iter()
                .all(|i| !i.witness.is_empty() || !i.script_sig.is_empty()),
            "F not signed whole"
        );
        Ok(())
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

/// The hub's side of an opening.
pub struct Hub {
    keys: HubKeys,
    quote: Quote,
    id: Option<[u8; 32]>,
    base: Option<Base>,
    holder: Option<(wire::Keys, Babe)>,
    /// F frozen, and all its instances of each assertion.
    frozen: Option<(Frozen, [Vec<Instance>; STAGES])>,
    choice: Option<([Vec<usize>; STAGES], Vec<Presign>)>,
    signed: bool,
    funded: bool,
}

impl Hub {
    /// The hub's side of one opening, under a secret drawn for it alone.
    pub fn new(quote: Quote) -> Self {
        use rand_core::RngCore;
        let mut secret = [0; 32];
        rand_core::OsRng.fill_bytes(&mut secret);
        Self::with(&secret, quote)
    }

    /// [`Hub::new`] under `secret`. For fixtures only: a hub must never
    /// open twice under one secret.
    #[cfg(feature = "fixture")]
    pub fn with_secret(secret: &[u8; 32], quote: Quote) -> Self {
        Self::with(secret, quote)
    }

    fn with(secret: &[u8; 32], quote: Quote) -> Self {
        Self {
            keys: HubKeys::from_secret(secret),
            quote,
            id: None,
            base: None,
            holder: None,
            frozen: None,
            choice: None,
            signed: false,
            funded: false,
        }
    }

    pub fn step(&mut self, input: Input, ctx: &Context) -> Result<Vec<Out>> {
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
            Input::Funded {
                funding,
                publication,
            } => self.fund(funding, *publication, ctx),
            Input::Signed(_) => bail!("the holder signs her inputs"),
        }
    }

    fn id(&self) -> [u8; 32] {
        self.id.expect("an opening's id from its Open")
    }

    /// M1: Base, the holder's side and deadlines in it, answers.
    fn open(&mut self, id: [u8; 32], body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(self.base.is_none(), "Open again");
        let o = wire::Open::decode(body)?;
        same_kind(o.mock, ctx)?;
        let q = &self.quote;
        let base = offer(
            q,
            id,
            &o,
            ctx.trust.programs,
            &q.chain.checkpoint,
            party(&self.keys.party.public()),
            ResponseKeys::new(self.keys.sealed.public())?,
        )?;
        let out = vec![send(Kind::Terms, id, base.to_bytes())];
        self.id = Some(id);
        self.base = Some(base);
        Ok(out)
    }

    /// M3: the holder's keys, with which the graph compiles. F to fund
    /// follows.
    fn keys(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        ensure!(self.holder.is_none(), "Keys again");
        let base = self.base.as_ref().context("Keys before Open")?;
        let keys = wire::Keys::decode(body)?;
        let draft = compile(base, &keys, draft_hmsg())?;
        let fund = Out::Fund {
            deposit: deposit(base)?,
            graph: graph_output(&draft)?,
            publication: draft
                .publication_trees(&disclosure(base)?)?
                .map(|t| t.script_pubkey()),
        };
        self.holder = Some((keys, draft));
        Ok(vec![fund])
    }

    /// F funded: frozen, D, and the instances under its journals, which it
    /// commits to.
    fn fund(
        &mut self,
        funding: Transaction,
        publication: [(OutPoint, TxOut); 5],
        ctx: &Context,
    ) -> Result<Vec<Out>> {
        ensure!(self.frozen.is_none(), "F funded again");
        let base = self.base.as_ref().context("F before Open")?;
        let (keys, draft) = self.holder.as_ref().context("F before the holder's keys")?;
        let deposit = deposit(base)?;
        let spends = funding
            .input
            .iter()
            .filter(|i| i.previous_output == deposit)
            .count();
        ensure!(spends == 1, "F spends the deposit {spends} times");
        let frozen = freeze(base, keys, draft, funding, publication)?;
        let dealt = [&self.keys.full, &self.keys.corrected];
        let instances: [Vec<Instance>; STAGES] = std::array::from_fn(|s| {
            let st = (ctx.trust.statement)(&journal(base, &frozen.d, s as u32));
            let d = dealt[s];
            par(INSTANCES, |i| {
                instance(
                    &d.seeds[i],
                    &d.dealer.shares(i as u64 + 1),
                    &st.vk,
                    &st.public,
                )
                .0
            })
        });
        let commitments = instances
            .each_ref()
            .map(|x| x.iter().map(Instance::commitment).collect());
        let commit = wire::Commit {
            funding: frozen.funding.clone(),
            publication: frozen.publication.coins.clone(),
            d: *frozen.d.as_bytes(),
            dealings: dealt.map(|d| d.dealing.clone()),
            commitments,
        };
        let out = vec![send(Kind::Commit, self.id(), commit.encode())];
        self.frozen = Some((frozen, instances));
        Ok(out)
    }

    /// M5: the holder's choice, kept for good: the same again has the
    /// same answer, another is refused. The opened instances and the kept
    /// answer.
    fn choose(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        let m = wire::Choose::decode(body)?;
        if let Some((kept, _)) = &self.choice {
            ensure!(*kept == m.kept, "a second choice");
            return Ok(self.chosen());
        }
        let (frozen, instances) = self
            .frozen
            .as_ref()
            .context("a choice before the commitments")?;
        check_choice(&m.kept)?;
        let base = self.base.as_ref().expect("Open before F");
        let (keys, _) = self.holder.as_ref().expect("Keys before F");
        let hmsg =
            std::array::from_fn(|s| m.kept[s].iter().map(|i| instances[s][*i].hmsg).collect());
        let (_, set) = finish(base, keys, frozen, hmsg)?;
        self.choice = Some((m.kept, set));
        Ok(self.chosen())
    }

    fn chosen(&self) -> Vec<Out> {
        let (kept, _) = self.choice.as_ref().expect("a choice");
        let (_, instances) = self.frozen.as_ref().expect("F before a choice");
        let dealt = [&self.keys.full, &self.keys.corrected];
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
        for (s, kept) in kept.iter().enumerate() {
            for i in kept {
                let kept = wire::Kept {
                    stage: s,
                    index: *i,
                    instance: instances[s][*i].to_bytes(),
                };
                out.push(send(Kind::Instance, self.id(), kept.encode()));
            }
        }
        out
    }

    /// M7: the holder's pre-signatures on its dealings, her signatures of
    /// all the graph but the withheld, and her consent, through its gate.
    /// Its signatures answer.
    fn consent(&mut self, body: &[u8], ctx: &Context) -> Result<Vec<Out>> {
        ensure!(!self.signed, "Consent again");
        let (_, set) = self.choice.as_ref().context("consent before a choice")?;
        let (frozen, _) = self.frozen.as_ref().expect("F before a choice");
        let base = self.base.as_ref().expect("Open before F");
        let (keys, _) = self.holder.as_ref().expect("Keys before F");
        let m = wire::Consent::decode(body)?;
        let sighashes = [
            Babe::challenge_sighash(set)?,
            Babe::challenge_corrected_sighash(set)?,
        ];
        let dealings = [&self.keys.full.dealing, &self.keys.corrected.dealing];
        for s in 0..STAGES {
            ensure!(
                check_presigs(
                    &keys.adaptor[s],
                    &sighashes[s],
                    &points(dealings[s]),
                    &m.presigs[s]
                ),
                "pre-signatures of assertion {s} not on its dealing"
            );
        }
        let alice = party_pub(&base.parts().parties[0])?.payment;
        check_sigs(set, &alice, &m.sigs, Babe::withheld)?;
        let retained = ResponseKeys::new(self.keys.sealed.public())?;
        let d = base.hub_gate(
            &frozen.prefix,
            &frozen.graph.script_pubkey.to_bytes(),
            &ctx.trust.programs,
            &retained,
            &m.consent,
            ctx.tip,
        )?;
        ensure!(d == frozen.d, "the gates agree on D");
        let signed = wire::Signed {
            sigs: sign_all(set, &self.keys.party.payment)?,
            funding: frozen.funding.clone(),
        };
        let out = vec![send(Kind::Signed, self.id(), signed.encode())];
        self.signed = true;
        Ok(out)
    }

    /// M9: F as frozen, signed whole, to broadcast.
    fn funded(&mut self, body: &[u8]) -> Result<Vec<Out>> {
        ensure!(self.signed, "F before the hub signed");
        ensure!(!self.funded, "F again");
        let (frozen, _) = self.frozen.as_ref().expect("F before signing");
        let m = wire::Funded::decode(body)?;
        ensure!(
            m.funding.compute_txid() == frozen.funding_op.txid,
            "F not as frozen"
        );
        ensure!(
            m.funding
                .input
                .iter()
                .all(|i| !i.witness.is_empty() || !i.script_sig.is_empty()),
            "F not signed whole"
        );
        self.funded = true;
        Ok(vec![Out::Broadcast(m.funding)])
    }
}
