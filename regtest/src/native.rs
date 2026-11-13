//! Opening a contract on Lichen's BABE graph:
//! Base -> native request keys/leaf -> frozen F/R -> final D -> instances
//! under the native statement journals -> holder and Hub gates -> funding.
//!
//! The request leaf signs the native `b` and `E`; the later links (respond,
//! assert, challenge, payout bits) read those signatures through the request
//! keys as ln-gap's HASH160 WOTS type (same chains, digit order and
//! checksum). The claim body P is its own transaction, and the counter
//! response publishes its mandatory unlocks in five signed chunks.

use crate::contract::{self, Opened, Setup, Statement};
use crate::graph::{Babe, Publication as Coins, Spec};
use crate::mock_statement;
use anyhow::{anyhow, ensure, Context, Result};
use bitcoincore_rpc::json::{AddressType, FundRawTransactionOptions};
use bitcoincore_rpc::RpcApi;
use licash::base::{
    Base, ChainTerms, Fees, FinalDigest, GraphTerms, Limits, Parts, Party, Programs, Recipe,
    ResponseKeys,
};
use licash::chain::{
    self, Activation, ActivationRef, Chain, Checkpoint, HeaderBytes, Inclusion, WitnessInclusion,
};
use licash::crypto::{Point, Signature};
use licash::note::Keyset;
use licash::opening::{FrozenPrefix, GraphOutput, RequestLeaf};
use licash::recovery::Publication;
use licash::request::{Request, Signer};
use licash::spend::{ChainProfile, TimedTerms};
use licash::statement::Journal;
use licash::statement::Observed;
use lngap_btc::bitcoin::consensus::serialize;
use lngap_btc::bitcoin::hashes::Hash as _;
use lngap_btc::bitcoin::{
    absolute, transaction, Amount, Block, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
};
use lngap_btc::regtest::Regtest;
use lngap_lamport::winternitz::{WotsParams, WotsPublic, WotsSig};
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
        })
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

/// The holder's funding gate, run on her device: returns her consent.
pub type HolderGate<'a> =
    &'a dyn Fn(&Base, &FrozenPrefix, &[u8], u32) -> std::result::Result<Signature, licash::Error>;

/// What the holder brings to an opening: the issuer keyset she will hold
/// notes of, her spending root, her request-key seed and her funding gate.
pub struct Holder<'a> {
    pub keyset: Keyset,
    pub spend_root: Point,
    pub seed: [u8; 32],
    pub gate: HolderGate<'a>,
}

/// Native request keys as ln-gap's HASH160 WOTS public keys.
fn wots_public(elements: &[[u8; 20]], bytes: u32) -> WotsPublic {
    WotsPublic {
        params: WotsParams::for_bytes(bytes),
        digits: elements.to_vec(),
    }
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

fn checkpoint(rt: &Regtest) -> Result<Checkpoint> {
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

fn party(keys: &lngap_channel::PartyPubKeys) -> Party {
    Party {
        funding: keys.funding.serialize(),
        payment: keys.payment.serialize(),
        delayed: keys.delayed.serialize(),
        payout_script: keys.payout_spk.to_bytes(),
    }
}

/// Base from the graph's own terms, so the gates compare like with like.
fn base(
    s: &Spec,
    p: &contract::Parties,
    keyset: Keyset,
    spend_root: Point,
    deposit: OutPoint,
    checkpoint: Checkpoint,
    programs: Programs,
) -> Result<Base> {
    let t = &s.terms;
    let c = &s.params;
    Ok(Base::new(Parts {
        nonce: Base::fresh_nonce(),
        spend_root,
        claim_key: s.pubs[0].delayed.serialize(),
        keyset,
        deposit: serialize(&deposit).try_into().unwrap(),
        cap: 4_096,
        response_keys: ResponseKeys::new(p.sealed_keys.public())?,
        parties: [party(&s.pubs[0]), party(&s.pubs[1])],
        graph: GraphTerms {
            id: t.id,
            unit_sats: t.unit.to_sat(),
            bits: u8::try_from(t.bits)?,
            low_bits: u8::try_from(t.low_bits)?,
            deposit_units: t.deposit,
            close_height: t.t_close,
            reserve_alice: t.reserve_alice.to_sat(),
            reserve_hub: t.reserve_hub.to_sat(),
            to_self_delay: c.to_self_delay,
            commit_fee: c.commit_fee.to_sat(),
            presign_fee: c.presign_fee.to_sat(),
            dust: c.dust.to_sat(),
            delta: c.delta,
            delta_prime: c.delta_prime,
            deadline_margin: 2,
            response_window: s.windows.w_h,
            assert_height: s.windows.t_a,
            hub_challenge: s.windows.delta2,
            holder_answer: s.windows.delta1,
        },
        recipe: Recipe {
            graph_version: 1,
            holder_program: programs.holder,
            counter_program: programs.counter,
            full_instances: contract::INSTANCES as u16,
            full_kept: contract::KEPT as u16,
            corrected_instances: contract::INSTANCES as u16,
            corrected_kept: contract::KEPT as u16,
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
    })?)
}

/// Open a native Corrected contract with assertion deadline `t_a` blocks
/// ahead and response window `w_h`. F is funded only after both gates pass on
/// the final D that the instances were garbled under.
pub fn open_native(
    rt: &Regtest,
    label: &str,
    (t_a, w_h): (u32, u16),
    holder: Holder<'_>,
    authority: Authority,
) -> Result<(Opened, Native)> {
    let Holder {
        keyset,
        spend_root,
        seed,
        gate: holder_gate,
    } = holder;
    let p = contract::parties(label);
    let deposit = {
        let addr = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))?
            .assume_checked();
        rt.fund(&addr.script_pubkey(), Amount::from_sat(500_000))?.0
    };
    let h = rt.height()?;
    let spec = |hmsg, hmsg2| {
        let mut s = contract::spec(&p, hmsg, Some(hmsg2), h + t_a + 140, h + t_a);
        s.windows.w_h = w_h;
        s
    };
    let draft = spec(vec![[0; 32]], vec![[0; 32]]);
    let base = base(
        &draft,
        &p,
        keyset,
        spend_root,
        deposit,
        checkpoint(rt)?,
        authority.programs,
    )?;
    // Request keys come from Base, never from final D.
    let keys = Signer::derive(&seed, &base.digest()).public();
    let (alice, hub) = (
        draft.pubs[0].payment.serialize(),
        draft.pubs[1].payment.serialize(),
    );
    let delay = draft.params.to_self_delay;
    let script = keys.request_leaf_for_base(&base.digest(), &alice, &hub, delay)?;
    let native = |mut s: Spec| -> Result<Babe> {
        s.b_key = wots_public(keys.amount_elements(), 4);
        s.exit.as_mut().context("exit")?.e_key = wots_public(keys.claim_elements(), 32);
        Babe::new(s)?.with_native_request(script.clone())
    };
    let draft = native(draft)?;
    let tree = draft.tree()?;
    let graph = TxOut {
        value: draft.value(),
        script_pubkey: tree.script_pubkey(),
    };
    // F is frozen unsigned: no funding signature exists before the gates.
    let unsigned = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: deposit,
            ..Default::default()
        }],
        output: vec![graph.clone()],
    };
    let funding = rt
        .rpc
        .fund_raw_transaction(
            &unsigned,
            Some(&FundRawTransactionOptions {
                add_inputs: Some(true),
                // F stays unsigned through the gates: keep its coins reserved.
                lock_unspents: Some(true),
                ..Default::default()
            }),
            None,
        )?
        .transaction()?;
    let vout = funding
        .output
        .iter()
        .position(|o| o == &graph)
        .context("F's graph output")?;
    let funding_op = OutPoint {
        txid: funding.compute_txid(),
        vout: u32::try_from(vout)?,
    };
    let keys256 = std::array::from_fn(|i| p.disclosure[i].public());
    let mut coins = vec![];
    for tree in draft.publication_trees(&keys256)? {
        coins.push(rt.fund(&tree.script_pubkey(), Amount::from_sat(20_000))?);
    }
    let coins = Coins {
        coins: coins.try_into().unwrap(),
        keys: keys256,
    };
    let draft_set = draft.presign_with_publication(funding_op, Some(&coins))?;
    let request = &draft_set
        .iter()
        .find(|x| x.name == "request")
        .context("R")?
        .tx;
    let parse = |tx: &Transaction| chain::Transaction::parse(&serialize(tx), 100_000);
    let output = |o: &TxOut| chain::Output {
        value: o.value.to_sat(),
        script: o.script_pubkey.to_bytes(),
    };
    let prefix = FrozenPrefix::new(
        parse(&funding)?,
        parse(request)?,
        GraphOutput {
            index: u32::try_from(vout)?,
            output: output(&graph),
        },
        GraphOutput {
            index: 0,
            output: output(&request.output[0]),
        },
        RequestLeaf::for_base(
            base.digest(),
            keys.clone(),
            alice,
            hub,
            delay,
            tree.control_block("request")?.serialize(),
        )?,
    )?;
    let d = base.final_digest(&prefix)?;
    // Instances are garbled under journals that bind final D.
    let (full, full_opened) = contract::instances(
        label,
        &p.dealer,
        &p.dealing,
        &(authority.statement)(&journal(&base, &d, 0)),
    )?;
    let (second, second_opened) = contract::instances(
        &format!("{label}/corrected"),
        &p.corrected.dealer,
        &p.corrected.dealing,
        &(authority.statement)(&journal(&base, &d, 1)),
    )?;
    let hmsg = |kept: &[(u64, lichen_babe::setup::Instance)]| -> Vec<[u8; 32]> {
        kept.iter().map(|(_, x)| x.hmsg).collect()
    };
    let babe = native(spec(hmsg(&full), hmsg(&second)))?;
    ensure!(
        babe.tree()?.script_pubkey() == graph.script_pubkey,
        "instances changed F's graph output"
    );
    let set = babe.presign_with_publication(funding_op, Some(&coins))?;
    let r = &set.iter().find(|x| x.name == "request").context("R")?.tx;
    ensure!(
        r.compute_txid().to_byte_array() == prefix.request().txid(),
        "instances changed R"
    );
    // Gates: each side compiles the graph itself and checks the deadline.
    let graph_script = graph.script_pubkey.to_bytes();
    let tip = rt.height()?;
    let consent = holder_gate(&base, &prefix, &graph_script, tip)?;
    let retained = ResponseKeys::new(p.sealed_keys.public())?;
    ensure!(
        base.hub_gate(
            &prefix,
            &graph_script,
            &authority.programs,
            &retained,
            &consent,
            tip
        )? == d,
        "the gates agree on D"
    );
    let full = Setup {
        presigs: contract::presign(Babe::challenge_sighash(&set)?, &p.adaptor, &p.dealing)?,
        instances: full,
        opened: full_opened,
    };
    let corrected = Setup {
        presigs: contract::presign(
            Babe::challenge_corrected_sighash(&set)?,
            &p.corrected.adaptor,
            &p.corrected.dealing,
        )?,
        instances: second,
        opened: second_opened,
    };
    let (signed, kept) = contract::sign(&set, &p)?;
    let f = rt
        .rpc
        .sign_raw_transaction_with_wallet(&funding, None, None)?;
    ensure!(f.complete, "F signatures");
    let f = f.transaction()?;
    ensure!(f.compute_txid() == funding_op.txid, "F's txid moved");
    rt.send_and_confirm(&f)?;
    rt.mine(u64::from(delay))?;
    Ok((
        Opened {
            set: set.into_iter().map(|x| (x.name.clone(), x)).collect(),
            p,
            babe,
            publication: coins,
            signed,
            kept,
            full,
            corrected: Some(corrected),
            deposit,
        },
        Native {
            base,
            prefix,
            d,
            seed,
        },
    ))
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
