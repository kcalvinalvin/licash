//! Opening a contract on Lichen's BABE graph, the holder and the Hub each
//! through `licash_contract::opening` over a byte pipe:
//! Base -> native request keys/leaf -> frozen F/R -> final D -> instances
//! under the native statement journals -> holder and Hub gates -> funding.
//! Bitcoin Core funds and signs F for both: its wallet holds the deposit.
//!
//! The request leaf signs the native `b` and `E`; the later links (respond,
//! assert, challenge, payout bits) read those signatures through the request
//! keys as ln-gap's HASH160 WOTS type (same chains, digit order and
//! checksum). The claim body P is its own transaction, and the counter
//! response publishes its mandatory unlocks in five signed chunks.

use crate::contract::{self, Opened, Statement};
use crate::mock_statement;
use anyhow::{anyhow, bail, ensure, Context, Result};
use bitcoincore_rpc::json::{AddressType, FundRawTransactionOptions};
use bitcoincore_rpc::RpcApi;
use k256::Scalar;
use licash::base::{Base, ChainTerms, Fees, FinalDigest, GraphTerms, Limits, Programs, Recipe};
use licash::chain::{
    self, Activation, ActivationRef, Chain, Checkpoint, HeaderBytes, Inclusion, WitnessInclusion,
};
use licash::note::Keyset;
use licash::opening::FrozenPrefix;
use licash::recovery::Publication;
use licash::request::{Request, Signer};
use licash::spend::{ChainProfile, TimedTerms};
use licash::statement::Journal;
use licash::statement::Observed;
use licash_contract::keys::{HolderKeys, HubKeys};
pub use licash_contract::opening::journal;
use licash_contract::opening::{self as machine, Input, Out, Quote, Start, Trust};
use licash_contract::wire::{Kind, Wanted};
use licash_contract::{INSTANCES, KEPT};
use lngap_btc::bitcoin::consensus::serialize;
use lngap_btc::bitcoin::hashes::{sha256, Hash as _};
use lngap_btc::bitcoin::{
    absolute, transaction, Amount, Block, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
};
use lngap_btc::regtest::Regtest;
use lngap_lamport::winternitz::{WotsParams, WotsSig};
use lngap_sessions::skeleton::Presign;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The opening's native state, as both parties hold it after the gates.
pub struct Native {
    pub base: Base,
    pub prefix: FrozenPrefix,
    pub d: FinalDigest,
    /// Alice's request-key seed (a test holds both sides).
    pub seed: [u8; 32],
}

/// Who vouches for proofs in an opening: the programs Base names (and both
/// gates must trust) and the statement the instances are garbled under.
#[derive(Clone, Copy)]
pub struct Authority {
    pub programs: Programs,
    pub statement: fn(&Journal) -> Statement,
    /// Whether anyone may "prove" anything under it.
    pub mock: bool,
}

impl Authority {
    /// Insecure fixture authority: any holder may "prove" any journal.
    pub fn mock() -> Self {
        Self {
            programs: Programs {
                holder: [11; 32],
                counter: [13; 32],
            },
            statement: mock_statement,
            mock: true,
        }
    }

    /// The native guests built by `r0/build-guest.sh native-claim` and
    /// `native-counter`: Base names their image ids, and the instances are
    /// garbled under RISC Zero's Groth16 key and the holder image. The ids
    /// are read from `LICASH_NATIVE_GUESTS` (default `r0/guests`).
    pub fn real() -> Result<Self> {
        let dir = std::env::var_os("LICASH_NATIVE_GUESTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../r0/guests"));
        let image = |name: &str| -> Result<[u8; 32]> {
            let path = dir.join(format!("{name}.id"));
            let id = std::fs::read_to_string(&path)
                .with_context(|| format!("{}: run r0/build-guest.sh {name}", path.display()))?;
            hex::decode(id.trim())?
                .try_into()
                .map_err(|_| anyhow!("{name}: a 32-byte image id"))
        };
        let programs = Programs {
            holder: image("native-claim")?,
            counter: image("native-counter")?,
        };
        ensure!(
            *REAL_HOLDER.get_or_init(|| programs.holder) == programs.holder,
            "one holder image per process"
        );
        Ok(Self {
            programs,
            statement: real_statement,
            mock: false,
        })
    }

    pub fn trust(&self) -> Trust {
        Trust {
            programs: self.programs,
            statement: self.statement,
            mock: self.mock,
        }
    }
}

/// The holder image of [`Authority::real`]: `Authority::statement` is a
/// plain function, so the image it binds is process state.
static REAL_HOLDER: OnceLock<[u8; 32]> = OnceLock::new();

fn real_statement(journal: &Journal) -> Statement {
    let image = REAL_HOLDER.get().expect("Authority::real");
    Statement {
        vk: lichen_babe::risc0::verifying_key(),
        public: lichen_babe::risc0::public_inputs(image, &journal.to_bytes()).to_vec(),
    }
}

/// What the holder brings to an opening: the issuer keyset she will hold
/// notes of, her request-key seed and her spending root's secret.
pub struct Holder {
    pub keyset: Keyset,
    pub seed: [u8; 32],
    pub spend: Scalar,
}

/// A checkpoint at the node's tip, both parties' view of the chain.
pub fn checkpoint(rt: &Regtest) -> Result<Checkpoint> {
    let anchor_hash = rt.rpc.get_best_block_hash()?;
    let anchor = rt.rpc.get_block_header(&anchor_hash)?;
    let mut times = vec![anchor.time];
    let mut previous = anchor;
    for _ in 1..11 {
        previous = rt.rpc.get_block_header(&previous.prev_blockhash)?;
        times.push(previous.time);
    }
    times.reverse();
    Ok(Checkpoint::new(
        ChainProfile::Core30DefaultRegtest,
        rt.height()?,
        serialize(&anchor).try_into().unwrap(),
        times,
        None,
    )?)
}

/// The fixture Hub's terms: a 21-bit graph of 1-sat units, its challenge
/// and the holder's answer 12 and 6 blocks.
pub fn quote(keyset: Keyset, checkpoint: Checkpoint) -> Quote {
    Quote {
        keyset,
        cap: 4_096,
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: 0,
            reserve_alice: 20_000,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 2,
            response_window: 0,
            assert_height: 0,
            hub_challenge: 12,
            holder_answer: 6,
        },
        recipe: Recipe {
            graph_version: 1,
            holder_program: [0; 32],
            counter_program: [0; 32],
            full_instances: INSTANCES as u16,
            full_kept: KEPT as u16,
            corrected_instances: INSTANCES as u16,
            corrected_kept: KEPT as u16,
        },
        chain: ChainTerms {
            checkpoint,
            funding_depth: 1,
            request_depth: 1,
            publication_depth: 1,
            lookback: 6,
            observation_margin: 1,
            max_headers: 1_000,
        },
        limits: Limits {
            max_transaction_bytes: 100_000,
            max_publication_bytes: 64_000,
            max_authorizations: 128,
            max_source_inputs: 128,
        },
        fees: Fees {
            sweep_fee_max: 1_000,
            external_fee_max: 2_000,
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    }
}

/// Which party a message or effect is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Holder,
    Hub,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Side::Holder => Side::Hub,
            Side::Hub => Side::Holder,
        }
    }
}

/// A message a party refused on the way, and why: it went on without it.
#[derive(Debug)]
pub struct Refusal {
    pub by: Side,
    pub kind: Option<Kind>,
    pub error: String,
}

/// What a test does to the messages of an opening: each in turn, with its
/// kind, becomes the messages it returns, delivered in order.
pub type Tamper<'a> = &'a mut dyn FnMut(Kind, Vec<u8>) -> Vec<Vec<u8>>;

/// Open a native Corrected contract with assertion deadline `t_a` blocks
/// ahead and response window `w_h`. F is funded only after both gates pass on
/// the final D that the instances were garbled under.
pub fn open_native(
    rt: &Regtest,
    label: &str,
    (t_a, w_h): (u32, u16),
    holder: Holder,
    authority: Authority,
) -> Result<(Opened, Native)> {
    let (c, n, refused) =
        open_native_with(rt, label, (t_a, w_h), holder, authority, &mut |_, m| {
            vec![m]
        })?;
    ensure!(refused.is_empty(), "an honest opening refused: {refused:?}");
    Ok((c, n))
}

/// [`open_native`], every message through `tamper`; the messages refused
/// on the way. The holder keeps instances 3 and 4 of each assertion.
pub fn open_native_with(
    rt: &Regtest,
    label: &str,
    deadlines: (u32, u16),
    holder: Holder,
    authority: Authority,
    tamper: Tamper<'_>,
) -> Result<(Opened, Native, Vec<Refusal>)> {
    let seed = holder.seed;
    let played = play(rt, label, deadlines, holder, authority, tamper)?;
    let o = played.opening;
    rt.mine(u64::from(o.base.parts().graph.to_self_delay))?;
    let [full, corrected] = o.setups(|_, _, commitment| {
        played
            .kept
            .get(commitment)
            .cloned()
            .context("a kept instance lost")
    })?;
    let alice = HolderKeys::derive(&seed, &o.base.parts().nonce);
    Ok((
        Opened {
            set: o.set.into_iter().map(|x| (x.name.clone(), x)).collect(),
            p: contract::parties(label, alice, HubKeys::from_secret(&hub_secret(label))),
            babe: o.babe,
            publication: o.publication,
            signed: o.signed,
            kept: o.kept,
            full,
            corrected: Some(corrected),
            deposit: o.deposit,
        },
        Native {
            base: o.base,
            prefix: o.prefix,
            d: o.d,
            seed,
        },
        played.refused,
    ))
}

/// The fixture hub's secret for the opening labelled `label`.
fn hub_secret(label: &str) -> [u8; 32] {
    sha256::Hash::hash(format!("{label}/hub").as_bytes()).to_byte_array()
}

/// An opening as its machines played it: the holder's contract, her kept
/// instances by commitment as her driver kept them, each party's state as
/// it stored it at each phase, and the messages refused on the way.
pub struct Played {
    pub opening: machine::Opening,
    pub kept: HashMap<[u8; 32], Vec<u8>>,
    pub holders: BTreeMap<machine::Phase, Vec<u8>>,
    pub hubs: BTreeMap<machine::HubPhase, Vec<u8>>,
    pub refused: Vec<Refusal>,
}

/// Play an opening's two machines until F is on the chain, every message
/// through `tamper`, the holder stored and loaded again after each step
/// she takes: assertion deadline `t_a` blocks ahead, response window `w_h`.
pub fn play(
    rt: &Regtest,
    label: &str,
    (t_a, w_h): (u32, u16),
    holder: Holder,
    authority: Authority,
    tamper: Tamper<'_>,
) -> Result<Played> {
    let deposit = {
        let addr = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))?
            .assume_checked();
        rt.fund(&addr.script_pubkey(), Amount::from_sat(500_000))?.0
    };
    let h = rt.height()?;
    let checkpoint = checkpoint(rt)?;
    let trust = authority.trust();
    let ctx = || -> Result<machine::Context> {
        Ok(machine::Context {
            tip: rt.height()?,
            trust,
        })
    };
    let start = Start {
        seed: holder.seed,
        spend: holder.spend,
        quote: quote(holder.keyset, checkpoint.clone()),
        deposit,
        wanted: Wanted {
            assert_height: h + t_a,
            close_height: h + t_a + 140,
            response_window: w_h,
        },
        checkpoint,
    };
    let hub_quote = start.quote.clone();
    let (mut alice, out) = machine::Holder::with_choice(start, &ctx()?, [vec![2, 3], vec![2, 3]])?;
    let secrets = (holder.seed, holder.spend);
    let mut kept = HashMap::new();
    let out = stored(&mut alice, None, out, &ctx()?, secrets, &mut kept)?;
    let mut holders = BTreeMap::from([(alice.phase(), alice.to_bytes())]);
    let mut hub = machine::Hub::with_secret(&hub_secret(label), hub_quote);
    let mut queue = VecDeque::new();
    let mut hubs = BTreeMap::new();
    let mut refused = vec![];
    let mut funded = false;
    route(rt, Side::Holder, out, &mut queue, tamper, &mut funded)?;
    while let Some((to, input)) = queue.pop_front() {
        let kind = match &input {
            Input::Message(m) => Kind::peek(m),
            _ => None,
        };
        let ctx = ctx()?;
        let step = match to {
            Side::Holder => {
                let (before, phase) = (alice.to_bytes(), alice.phase());
                match alice.step(input, &ctx) {
                    Ok(out) => {
                        let out = stored(&mut alice, Some(phase), out, &ctx, secrets, &mut kept)?;
                        holders.insert(alice.phase(), alice.to_bytes());
                        Ok(out)
                    }
                    Err(e) => {
                        ensure!(alice.to_bytes() == before, "a refusal changed her: {e:#}");
                        Err(e)
                    }
                }
            }
            Side::Hub => {
                let (before, phase) = (hub.to_bytes().ok(), hub.phase());
                match hub.step(input, &ctx) {
                    Ok(out) => {
                        let out = hub_stored(&mut hub, phase, out, &ctx)?;
                        let now = hub.phase().context("a hub past its Open")?;
                        hubs.insert(now, hub.to_bytes()?);
                        Ok(out)
                    }
                    Err(e) => {
                        ensure!(
                            hub.to_bytes().ok() == before,
                            "a refusal changed the hub: {e:#}"
                        );
                        Err(e)
                    }
                }
            }
        };
        match step {
            Ok(out) => route(rt, to, out, &mut queue, tamper, &mut funded)?,
            Err(e) => refused.push(Refusal {
                by: to,
                kind,
                error: format!("{e:#}"),
            }),
        }
    }
    ensure!(funded, "the opening stalled, refusing {refused:?}");
    let opening = alice.opening()?;
    agree(&opening, &hub.opening()?)?;
    Ok(Played {
        opening,
        kept,
        holders,
        hubs,
        refused,
    })
}

/// The hub's contract agrees with the holder's: one Base, D, F, set and
/// choice, her pre-signatures and both parties' signatures.
fn agree(o: &machine::Opening, h: &machine::HubOpening) -> Result<()> {
    let txids = |set: &[Presign]| -> Vec<_> {
        set.iter()
            .map(|p| (p.name.clone(), p.tx.compute_txid()))
            .collect()
    };
    let kept: Vec<Vec<usize>> = o
        .instances
        .iter()
        .map(|k| k.iter().map(|k| k.index).collect())
        .collect();
    ensure!(o.base.digest() == h.base.digest(), "two Bases");
    ensure!(o.d == h.d && o.funding == h.funding, "two Ds or Fs");
    ensure!(txids(&o.set) == txids(&h.set), "two graphs");
    ensure!(kept == h.kept, "two choices");
    ensure!(
        o.presigs == h.consent.presigs,
        "two sets of her pre-signatures"
    );
    ensure!(
        o.signed.alice == h.consent.sigs,
        "two sets of her signatures"
    );
    ensure!(o.signed.hub == h.sigs, "two sets of the hub's signatures");
    Ok(())
}

/// What the holder asked after a step from `phase` (none, her first), as
/// a driver that stores her carries it out: her instances kept in `store`
/// by commitment, then her state stored and loaded again under her
/// `secrets`, which must store the same bytes and ask again what a step to
/// another phase asked (a step within one asks nothing more); the rest to
/// carry out.
fn stored(
    alice: &mut machine::Holder,
    phase: Option<machine::Phase>,
    out: Vec<Out>,
    ctx: &machine::Context,
    (seed, spend): ([u8; 32], Scalar),
    store: &mut HashMap<[u8; 32], Vec<u8>>,
) -> Result<Vec<Out>> {
    let mut rest = vec![];
    for out in out {
        match out {
            Out::Keep {
                commitment, bytes, ..
            } => {
                store.insert(commitment, bytes);
            }
            out => rest.push(out),
        }
    }
    let bytes = alice.to_bytes();
    *alice = machine::Holder::load(&bytes, seed, spend, ctx)?;
    ensure!(alice.to_bytes() == bytes, "her state stored otherwise");
    let now = alice.phase();
    if phase == Some(now) {
        ensure!(rest.is_empty(), "{now:?} again, asking {rest:?}");
    } else {
        ensure!(
            alice.pending(ctx)? == rest,
            "{now:?} asks otherwise once loaded"
        );
    }
    Ok(rest)
}

/// What the hub asked after a step from `phase`, as a driver that stores
/// it carries it out: its state stored and loaded again, which must store
/// the same bytes and ask again what a step to another phase asked (a step
/// within one, a choice again, asks the same or nothing); then the
/// instances it serves built again by the loaded hub.
fn hub_stored(
    hub: &mut machine::Hub,
    phase: Option<machine::HubPhase>,
    out: Vec<Out>,
    ctx: &machine::Context,
) -> Result<Vec<Out>> {
    let bytes = hub.to_bytes()?;
    *hub = machine::Hub::load(&bytes, ctx)?;
    ensure!(hub.to_bytes()? == bytes, "its state stored otherwise");
    let now = hub.phase();
    ensure!(
        hub.pending()? == out || (phase == now && out.is_empty()),
        "{now:?} asks otherwise once loaded"
    );
    out.into_iter()
        .map(|out| match out {
            Out::Serve { stage, index } => Ok(Out::Send(hub.instance(stage, index, ctx)?)),
            out => Ok(out),
        })
        .collect()
}

/// Carry out what party `from` asks: its messages to the other through
/// `tamper`, F funded or signed by Core back to it, F to the chain.
fn route(
    rt: &Regtest,
    from: Side,
    out: Vec<Out>,
    queue: &mut VecDeque<(Side, Input)>,
    tamper: Tamper<'_>,
    funded: &mut bool,
) -> Result<()> {
    for out in out {
        match out {
            Out::Send(bytes) => {
                let kind = Kind::peek(&bytes).context("a message of no kind")?;
                for m in tamper(kind, bytes) {
                    queue.push_back((from.other(), Input::Message(m)));
                }
            }
            Out::Fund {
                deposit,
                graph,
                publication,
            } => {
                let (funding, publication) = fund(rt, deposit, graph, &publication)?;
                queue.push_back((
                    from,
                    Input::Funded {
                        funding,
                        publication: Box::new(publication),
                    },
                ));
            }
            Out::Sign(funding) => {
                let f = rt
                    .rpc
                    .sign_raw_transaction_with_wallet(&funding, None, None)?;
                ensure!(f.complete, "F signatures");
                queue.push_back((from, Input::Signed(f.transaction()?)));
            }
            Out::Broadcast(funding) => {
                rt.send_and_confirm(&funding)?;
                *funded = true;
            }
            Out::Keep { .. } => bail!("an instance to keep, not kept first"),
            Out::Serve { .. } => bail!("an instance to serve, not built first"),
        }
    }
    Ok(())
}

/// F from Core's coins, spending `deposit` and paying `graph`, unsigned
/// (its coins stay reserved), and the five publication coins.
fn fund(
    rt: &Regtest,
    deposit: OutPoint,
    graph: TxOut,
    publication: &[ScriptBuf; 5],
) -> Result<(Transaction, [(OutPoint, TxOut); 5])> {
    let unsigned = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: deposit,
            ..Default::default()
        }],
        output: vec![graph],
    };
    let funding = rt
        .rpc
        .fund_raw_transaction(
            &unsigned,
            Some(&FundRawTransactionOptions {
                add_inputs: Some(true),
                lock_unspents: Some(true),
                ..Default::default()
            }),
            None,
        )?
        .transaction()?;
    let mut coins = vec![];
    for script in publication {
        coins.push(rt.fund(script, Amount::from_sat(20_000))?);
    }
    Ok((funding, coins.try_into().unwrap()))
}

/// Where a confirmed transaction is: height, raw block and position.
fn locate(rt: &Regtest, txid: &Txid, from: u32) -> Result<(u32, Vec<u8>, usize)> {
    for height in from..=rt.height()? {
        let block: Block = rt
            .rpc
            .get_block(&rt.rpc.get_block_hash(u64::from(height))?)?;
        if let Some(i) = block
            .txdata
            .iter()
            .position(|tx| tx.compute_txid() == *txid)
        {
            return Ok((height, serialize(&block), i));
        }
    }
    anyhow::bail!("transaction {txid} is not confirmed")
}

impl Native {
    fn limit(&self) -> usize {
        self.base.parts().limits.max_authorizations as usize
    }

    /// Alice posts R: her native signatures on `b` and `E`, consuming the
    /// Base-derived signer, inside the presigned request transaction.
    pub fn request(&self, c: &Opened, rt: &Regtest, request: Request) -> Result<Transaction> {
        let signed = Signer::derive(&self.seed, &self.base.digest()).sign(request);
        c.link(rt, "request", signed.pairs().to_vec())
    }

    /// What R signed, and Alice's signatures as `respond` and the payout bits
    /// consume them, decoded and checked against the frozen request keys.
    pub fn read_request(&self, r: &Transaction) -> Result<(Request, WotsSig, WotsSig)> {
        let items: Vec<Vec<u8>> = r.input[0].witness.iter().map(<[u8]>::to_vec).collect();
        let leaf = self.prefix.leaf();
        let request = leaf
            .keys()
            .decode_witness(&items, leaf.script(), leaf.control())?;
        let order = |pairs: &[Vec<u8>]| pairs.iter().rev().cloned().collect::<Vec<_>>();
        let e = WotsSig::from_consumption_order(WotsParams::for_bytes(32), &order(&items[..134]))?;
        let b =
            WotsSig::from_consumption_order(WotsParams::for_bytes(4), &order(&items[134..154]))?;
        ensure!(
            b.message() == request.amount.to_be_bytes() && e.message() == request.claim_digest,
            "converted signatures read the native request"
        );
        Ok((request, b, e))
    }

    /// Alice posts P in its own transaction, funded by the node's wallet.
    pub fn publish(&self, rt: &Regtest, publication: &Publication) -> Result<Txid> {
        // One wallet input up front: rust-bitcoin encodes an input-less
        // transaction in the segwit form, which Core cannot decode.
        let coin = rt
            .rpc
            .list_unspent(Some(1), None, None, Some(false), None)?
            .into_iter()
            .max_by_key(|u| u.amount)
            .context("a wallet coin for P")?;
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: coin.txid,
                    vout: coin.vout,
                },
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(publication.script()?),
            }],
        };
        let funded = rt
            .rpc
            .fund_raw_transaction(&tx, None, None)?
            .transaction()?;
        let signed = rt
            .rpc
            .sign_raw_transaction_with_wallet(&funded, None, None)?;
        ensure!(signed.complete, "P signatures");
        let signed = signed.transaction()?;
        println!("claim body: {} vB", signed.vsize());
        Ok(rt.send_and_confirm(&signed)?.0)
    }

    /// The Hub's view: the first P after R that binds this opening's request.
    pub fn observe_publication(
        &self,
        rt: &Regtest,
        request: &Request,
        from: u32,
    ) -> Result<(Txid, Publication)> {
        let keyset = &self.base.parts().keyset;
        for height in from..=rt.height()? {
            let block: Block = rt
                .rpc
                .get_block(&rt.rpc.get_block_hash(u64::from(height))?)?;
            for tx in block.txdata.iter().skip(1) {
                let mut stripped = tx.clone();
                stripped.input.iter_mut().for_each(|i| i.witness.clear());
                let Ok(parsed) = chain::Transaction::parse(&serialize(&stripped), 100_000) else {
                    continue;
                };
                if let Ok(p) = Publication::from_transaction(keyset, self.limit(), &parsed) {
                    if p.bind(&self.base, &self.d, request).is_ok() {
                        return Ok((tx.compute_txid(), p));
                    }
                }
            }
        }
        anyhow::bail!("no claim publication observed")
    }

    /// Every header from the opening's checkpoint to the tip.
    pub fn headers(&self, rt: &Regtest) -> Result<Vec<HeaderBytes>> {
        let start = self.base.parts().chain.checkpoint.height();
        (start + 1..=rt.height()?)
            .map(|h| -> Result<HeaderBytes> {
                let hash = rt.rpc.get_block_hash(u64::from(h))?;
                Ok(serialize(&rt.rpc.get_block_header(&hash)?)
                    .try_into()
                    .unwrap())
            })
            .collect()
    }

    /// The chain from the opening's checkpoint, as either guest verifies it.
    pub fn chain(&self, rt: &Regtest) -> Result<Chain> {
        let terms = &self.base.parts().chain;
        Ok(Chain::verify(
            terms.checkpoint.clone(),
            &terms.checkpoint.commitment(),
            self.headers(rt)?,
            terms.max_headers as usize,
        )?)
    }

    /// Timed-lock terms on this opening's chain: `hash` must be activated
    /// below height `expiry`, at `depth` confirmations.
    pub fn timed(&self, hash: [u8; 32], expiry: u32, depth: u32) -> Result<TimedTerms> {
        let checkpoint = &self.base.parts().chain.checkpoint;
        Ok(TimedTerms::new(
            checkpoint.profile(),
            checkpoint.commitment(),
            expiry,
            depth,
            hash,
        )?)
    }

    /// Publish `secret` on Bitcoin: the activation of every lock under its
    /// hash that has not expired. Anyone who knows the secret may do this.
    pub fn activate(rt: &Regtest, secret: &[u8; 32]) -> Result<Activation> {
        let script = ScriptBuf::from_bytes(chain::activation_script(secret));
        // A zero-input transaction does not serialize: name a wallet coin.
        let coin = rt
            .rpc
            .list_unspent(Some(1), None, None, None, None)?
            .into_iter()
            .next()
            .context("activation fee input")?;
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: coin.txid,
                    vout: coin.vout,
                },
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: script.clone(),
            }],
        };
        let funded = rt
            .rpc
            .fund_raw_transaction(&tx, None, None)?
            .transaction()?;
        let signed = rt
            .rpc
            .sign_raw_transaction_with_wallet(&funded, None, None)?;
        ensure!(signed.complete, "activation signatures");
        let signed = signed.transaction()?;
        let output = signed
            .output
            .iter()
            .position(|o| o.script_pubkey == script)
            .context("activation output")?;
        println!("activation: {} vB", signed.vsize());
        let (txid, _) = rt.send_and_confirm(&signed)?;
        let (height, block, index) = locate(rt, &txid, rt.height()?)?;
        Ok(Activation {
            reference: ActivationRef {
                height,
                block_hash: rt.rpc.get_block_hash(u64::from(height))?.to_byte_array(),
                txid: txid.to_byte_array(),
                output: u32::try_from(output)?,
            },
            inclusion: Inclusion::capture(&block, index, 4_000_000)?,
        })
    }

    /// Public chain evidence for the holder statement.
    pub fn observed(&self, rt: &Regtest, r: &Txid, p: &Txid) -> Result<Observed> {
        let start = self.base.parts().chain.checkpoint.height();
        let headers = self.headers(rt)?;
        let (request_height, block, i) = locate(rt, r, start + 1)?;
        let request = WitnessInclusion::capture(&block, i, 4_000_000)?;
        let (publication_height, block, i) = locate(rt, p, request_height)?;
        Ok(Observed {
            headers,
            request_height,
            request,
            publication_height,
            publication: Inclusion::capture(&block, i, 4_000_000)?,
        })
    }
}
