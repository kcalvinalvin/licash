//! Native recovery composition without Bitcoin Core: Base, gates, 32-item
//! publication, holder proof, Corrected counters and settlement.

use bitcoin::{
    absolute,
    block::{Header, Version},
    consensus::{deserialize, serialize},
    hashes::{sha256, sha256d, Hash},
    pow::CompactTarget,
    secp256k1::{Secp256k1, XOnlyPublicKey},
    taproot::{LeafVersion, TaprootBuilder},
    transaction, Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness,
};
use k256::Scalar;
use licash::{
    authorization::{Authorization, PreparedGrant, SignedProposal},
    base::{
        Base, ChainTerms, Fees, FinalDigest, GraphTerms, Limits, Parts, Party, Programs, Recipe,
        ResponseKeys,
    },
    chain::{self, Activation, ActivationRef, Chain, Checkpoint, Inclusion, WitnessInclusion},
    claim::{self, ActivationRecord, Branch, Item, Source, UnsignedClaim},
    crypto::{self, Point},
    guest::CounterInput,
    hold,
    note::Evidence as Issuance,
    note::{Derivation, HoldRoot, IssuerKeys, Keyset, Note},
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    recovery::{
        counter_disclosures, prove_counter, prove_holder, settle, Evidence, HolderWitness,
        ItemWitness, Publication, Response, SourceWitness,
    },
    request::{Request, Signer},
    source::Certificate,
    spend::{ChainProfile, Output, Proposal, TimedLock, TimedTerms},
    statement::SecretKeys,
    statement::{HubResponse, Observed, StatementInput},
};

const SEED: [u8; 32] = [7; 32];
const SPEND: u64 = 17;
const LIMIT: usize = 128;
/// The fixture recipe's program identities, as both parties trust them.
const PROGRAMS: Programs = Programs {
    holder: [11; 32],
    counter: [13; 32],
};

fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
}
fn hold() -> HoldRoot {
    HoldRoot::from_seed(&SEED).unwrap()
}
fn response_keys() -> ResponseKeys {
    ResponseKeys::new(
        SecretKeys::from_entropy(
            std::array::from_fn(|i| [i as u8 + 1; 32]),
            std::array::from_fn(|i| [i as u8 + 11; 32]),
        )
        .public(),
    )
    .unwrap()
}
fn party(value: u64) -> Party {
    let mut script = vec![0x51, 0x20];
    script.extend(point(value + 3).x_only());
    Party {
        funding: point(value).x_only(),
        payment: point(value + 1).x_only(),
        delayed: point(value + 2).x_only(),
        payout_script: script,
    }
}
fn base(keyset: Keyset) -> Base {
    let mut deposit = [31; 36];
    deposit[32..].copy_from_slice(&7u32.to_le_bytes());
    Base::new(Parts {
        nonce: [53; 32],
        spend_root: point(SPEND),
        claim_key: point(19).x_only(),
        keyset,
        deposit,
        cap: 128,
        response_keys: response_keys(),
        parties: [party(23), party(29)],
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: 1_000,
            reserve_alice: 20_000,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 8,
            response_window: 6,
            assert_height: 800,
            hub_challenge: 12,
            holder_answer: 6,
        },
        recipe: Recipe {
            graph_version: 1,
            holder_program: [11; 32],
            counter_program: [13; 32],
            full_instances: 4,
            full_kept: 2,
            corrected_instances: 4,
            corrected_kept: 2,
        },
        chain: ChainTerms {
            checkpoint: Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap(),
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
            max_authorizations: LIMIT as u32,
            max_source_inputs: 128,
        },
        fees: Fees {
            sweep_fee_max: 1_000,
            external_fee_max: 2_000,
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    })
    .unwrap()
}

/// Single request-leaf F/R fixture; returns the prefix and its graph script.
fn prefix(base: &Base) -> FrozenPrefix {
    let p = base.parts();
    let digest = base.digest();
    let keys = Signer::derive(&SEED, &digest).public();
    let (alice, hub, delay) = (
        p.parties[0].payment,
        p.parties[1].payment,
        p.graph.to_self_delay,
    );
    let script = ScriptBuf::from_bytes(
        keys.request_leaf_for_base(&digest, &alice, &hub, delay)
            .unwrap(),
    );
    let nums = XOnlyPublicKey::from_slice(
        &hex::decode("50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0").unwrap(),
    )
    .unwrap();
    let tree = TaprootBuilder::with_huffman_tree([(1, script.clone())])
        .unwrap()
        .finalize(&Secp256k1::new(), nums)
        .unwrap();
    let spk = ScriptBuf::new_p2tr_tweaked(tree.output_key());
    let control = tree
        .control_block(&(script, LeafVersion::TapScript))
        .unwrap()
        .serialize();
    let leaf = RequestLeaf::for_base(digest, keys, alice, hub, delay, control).unwrap();
    let value = p.graph.graph_value().unwrap();
    let od = base.deposit_outpoint();
    let f = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array(od.txid),
                vout: od.index,
            },
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: spk.clone(),
        }],
    };
    let r = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: f.compute_txid(),
                vout: 0,
            },
            sequence: Sequence::from_height(delay),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value - 10_000),
            script_pubkey: spk.clone(),
        }],
    };
    let output = |value| chain::Output {
        value,
        script: spk.to_bytes(),
    };
    FrozenPrefix::new(
        chain::Transaction::parse(&serialize(&f), 100_000).unwrap(),
        chain::Transaction::parse(&serialize(&r), 100_000).unwrap(),
        GraphOutput {
            index: 0,
            output: output(value),
        },
        GraphOutput {
            index: 0,
            output: output(value - 10_000),
        },
        leaf,
    )
    .unwrap()
}

/// Wallet-side derivation: a holder output at (shared, index).
fn derive(root: u64, hold: &HoldRoot, denom: u64, shared: u8, index: u32) -> Derivation {
    Derivation::derive(denom, point(root), hold.public(), &[shared; 32], index).unwrap()
}
fn owner(root: u64, shared: u8, index: u32) -> Scalar {
    Scalar::from(root)
        + licash::note::derive_output(&point(root), &[shared; 32], index)
            .unwrap()
            .tweak
            .to_scalar()
            .unwrap()
}
fn mint(keys: &IssuerKeys, keyset: &Keyset, d: &Derivation) -> Note {
    let blinded = d.blinded().unwrap();
    let proof = keys
        .prove_blinded(d.message().denomination(), blinded)
        .unwrap();
    Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, d).unwrap(),
        Issuance::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: crypto::dleq_commit(&Scalar::from(71u64), &blinded.to_projective().unwrap()),
        },
    )
    .unwrap()
}

struct Opened {
    issuer: IssuerKeys,
    base: Base,
    d: FinalDigest,
}

fn open() -> Opened {
    open_with(3)
}

fn open_with(max_pow: u32) -> Opened {
    let issuer = IssuerKeys::random(max_pow).unwrap();
    let base = base(issuer.keyset().unwrap());
    let prefix = prefix(&base);
    let graph = prefix.funding_graph().output.script.clone();
    let consent = base
        .holder_gate(&prefix, &graph, &PROGRAMS, &SEED, &Scalar::from(SPEND), 0)
        .unwrap();
    let d = base
        .hub_gate(&prefix, &graph, &PROGRAMS, &response_keys(), &consent, 0)
        .unwrap();
    Opened { issuer, base, d }
}

fn claim(
    o: &Opened,
    items: Vec<Item>,
    sources: Vec<Source>,
    activations: Vec<ActivationRecord>,
    authorizations: &[Derivation],
    amount: u32,
) -> UnsignedClaim {
    let mut authorizations: Vec<_> = authorizations.iter().map(|d| d.message().clone()).collect();
    authorizations.sort_by_key(|m| m.identifier().unwrap());
    let p = o.base.parts();
    UnsignedClaim::new(
        claim::Context {
            final_digest: *o.d.as_bytes(),
            deposit: p.deposit,
            cap: p.cap,
        },
        &p.keyset,
        amount,
        claim::Parts {
            items,
            sources,
            activations,
            authorizations,
        },
        LIMIT,
    )
    .unwrap()
}

fn hold_keys(ds: &[Derivation]) -> Vec<(licash::note::Message, crypto::ScalarBytes)> {
    ds.iter()
        .map(|d| (d.message().clone(), d.hold_key(&hold()).unwrap()))
        .collect()
}

fn spend(o: &Opened, notes: &[Note], owners: &[Scalar], to: Vec<Output>) -> SignedProposal {
    let keyset = &o.base.parts().keyset;
    let proposal = Proposal::new(keyset, notes, point(43), to, None).unwrap();
    SignedProposal::sign(keyset, proposal, notes, Authorization::Plain, owners).unwrap()
}

fn bob(denom: u64, index: u32) -> Output {
    let bob = HoldRoot::from_seed(&[9; 32]).unwrap();
    Output::from_derivation(&derive(41, &bob, denom, 29, index)).unwrap()
}

#[test]
fn stale_thirty_two_item_claim_is_published_proven_countered_and_owed_its_unspent_part() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let ds: Vec<_> = (0..32).map(|i| derive(SPEND, &hold, 4, 23, i)).collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    // After her backup, the holder paid Bob 64 sats from 16 of the notes.
    let owners: Vec<_> = (0..16).map(|i| owner(SPEND, 23, i)).collect();
    let payment = spend(
        &o,
        &notes[..16],
        &owners,
        (0..8).map(|i| bob(8, i)).collect(),
    );

    // The stale backup still claims all 32 items, b = 128.
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let unsigned = claim(&o, items, vec![], vec![], &ds, 128);
    let publication = Publication::sign(unsigned, &hold_keys(&ds), LIMIT).unwrap();
    let p_tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([5; 32]),
                vout: 0,
            },
            ..Default::default()
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(330),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(publication.script().unwrap()),
            },
        ],
    };
    let parsed = chain::Transaction::parse(&serialize(&p_tx), 100_000).unwrap();
    let posted = Publication::from_transaction(&keyset, LIMIT, &parsed).unwrap();
    assert_eq!(posted.to_bytes(), publication.to_bytes());
    assert!(posted.to_bytes().len() < 10_000);

    // R signs exactly this E and b with the holder's Base-derived keys.
    let request = Request {
        amount: 128,
        claim_digest: *posted.claim().digest(),
    };
    assert_eq!(
        Signer::derive(&SEED, &o.base.digest())
            .sign(request)
            .request(),
        request
    );

    let witness = HolderWitness {
        items: ds
            .iter()
            .zip(&notes)
            .map(|(d, note)| ItemWitness::Settled {
                note: Box::new(note.clone()),
                ownership: d.ownership(),
            })
            .collect(),
        sources: vec![],
        chain: None,
        activations: vec![],
    };
    let holder = prove_holder(&o.base, &o.d, &request, &posted, &witness).unwrap();
    assert_eq!(
        (holder.amount, holder.claim_digest),
        (128, request.claim_digest)
    );
    let spent = Evidence::Spend {
        signed: payment.clone(),
        notes: notes[..16].to_vec(),
        unlock: None,
    };
    let counter = prove_counter(
        &o.base,
        &o.d,
        &request,
        &posted,
        std::slice::from_ref(&spent),
        None,
    )
    .unwrap();
    assert_eq!((counter.mask, counter.owed), (0xffff, 64));

    // A Q signature over any other claim voids that item as well.
    let other = hold::Context {
        final_digest: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let record = hold::Envelope::sign(other, &hold_keys(&ds[20..21]), LIMIT)
        .unwrap()
        .records()[0]
        .clone();
    let second = Evidence::SecondClaim {
        context: other,
        record: record.clone(),
    };
    let both = prove_counter(
        &o.base,
        &o.d,
        &request,
        &posted,
        &[spent.clone(), second.clone()],
        None,
    )
    .unwrap();
    assert_eq!((both.mask, both.owed), (0xffff | 1 << 20, 60));
    // The counter guest's input survives its codec and rebuilds the request
    // from P alone.
    let input = CounterInput {
        base: o.base.clone(),
        prefix: prefix(&o.base),
        publication: posted.clone(),
        evidence: vec![spent.clone(), second],
        chain: None,
    };
    let bytes = input.to_bytes().unwrap();
    let decoded = CounterInput::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.run().unwrap(), both);
    assert_eq!(decoded.to_bytes().unwrap(), bytes);
    assert!(CounterInput::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let same = Evidence::SecondClaim {
        context: posted.claim().hold_context(),
        record,
    };
    assert!(prove_counter(&o.base, &o.d, &request, &posted, &[same], None).is_err());

    // An unrelated spend voids nothing and is not a counter.
    let unrelated = derive(SPEND, &hold, 4, 23, 40);
    let unrelated_note = mint(&o.issuer, &keyset, &unrelated);
    let unrelated = spend(
        &o,
        std::slice::from_ref(&unrelated_note),
        &[owner(SPEND, 23, 40)],
        vec![bob(4, 9)],
    );
    let unrelated = Evidence::Spend {
        signed: unrelated,
        notes: vec![unrelated_note],
        unlock: None,
    };
    assert!(prove_counter(&o.base, &o.d, &request, &posted, &[unrelated], None).is_err());

    // Settlement: what is owed is owed, with no penalty for the stale backup.
    let owed = |response, holder| settle(&o.base, &o.d, &request, response, holder).unwrap();
    assert_eq!(owed(Response::Silent, None).owed, 128);
    assert_eq!(owed(Response::Plain, Some(&holder)).owed, 128);
    assert_eq!(owed(Response::Plain, None).owed, 0);
    let corrected = owed(Response::Counter(counter), Some(&holder));
    assert_eq!(
        (corrected.owed, corrected.principal, corrected.remainder),
        (64, 0, 64)
    );
    assert_eq!(owed(Response::Counter(both), Some(&holder)).owed, 60);
    assert_eq!(owed(Response::Counter(counter), None).owed, 0);

    // Publications and proofs are bound to this exact request.
    let wrong = Request {
        amount: 127,
        ..request
    };
    assert!(prove_holder(&o.base, &o.d, &wrong, &posted, &witness).is_err());
    assert!(prove_counter(&o.base, &o.d, &wrong, &posted, &[spent], None).is_err());
}

#[test]
fn publication_codec_is_strict_and_unique_per_transaction() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let ds: Vec<_> = (0..3).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let publication = Publication::sign(
        claim(&o, items, vec![], vec![], &ds, 12),
        &hold_keys(&ds),
        LIMIT,
    )
    .unwrap();
    let bytes = publication.to_bytes();
    assert!(Publication::from_bytes(&keyset, LIMIT, &bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(Publication::from_bytes(&keyset, LIMIT, &bytes[..n]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Publication::from_bytes(&keyset, LIMIT, &trailing).is_err());
    let mut forged = bytes.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1; // a Q signature byte
    assert!(Publication::from_bytes(&keyset, LIMIT, &forged).is_err());
    assert!(Publication::from_bytes(&keyset, 2, &bytes).is_err());
    let script = ScriptBuf::from_bytes(publication.script().unwrap());
    let twice = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![
            TxOut {
                value: Amount::ZERO,
                script_pubkey: script.clone(),
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: script,
            },
        ],
    };
    let parsed = chain::Transaction::parse(&serialize(&twice), 100_000).unwrap();
    assert!(Publication::from_transaction(&keyset, LIMIT, &parsed).is_err());
}

#[test]
fn funding_gates_require_own_keys_root_compiled_graph_retained_keys_and_time() {
    let issuer = IssuerKeys::random(3).unwrap();
    let base = base(issuer.keyset().unwrap());
    let prefix = prefix(&base);
    let graph = prefix.funding_graph().output.script.clone();
    let secret = Scalar::from(SPEND);
    let consent = base
        .holder_gate(&prefix, &graph, &PROGRAMS, &SEED, &secret, 0)
        .unwrap();
    assert!(base
        .holder_gate(&prefix, &graph, &PROGRAMS, &[8; 32], &secret, 0)
        .is_err());
    assert!(base
        .holder_gate(&prefix, &graph, &PROGRAMS, &SEED, &Scalar::from(18u64), 0)
        .is_err());
    assert!(base
        .holder_gate(&prefix, &[0x51], &PROGRAMS, &SEED, &secret, 0)
        .is_err());
    assert!(base
        .holder_gate(&prefix, &graph, &PROGRAMS, &SEED, &secret, 790)
        .is_err());
    // An opening naming a proof program the party did not build is refused.
    for untrusted in [
        Programs {
            holder: [12; 32],
            ..PROGRAMS
        },
        Programs {
            counter: [14; 32],
            ..PROGRAMS
        },
    ] {
        assert!(base
            .holder_gate(&prefix, &graph, &untrusted, &SEED, &secret, 0)
            .is_err());
        assert!(base
            .hub_gate(&prefix, &graph, &untrusted, &response_keys(), &consent, 0)
            .is_err());
    }
    base.hub_gate(&prefix, &graph, &PROGRAMS, &response_keys(), &consent, 0)
        .unwrap();
    let other = ResponseKeys::new(SecretKeys::random().public()).unwrap();
    assert!(base
        .hub_gate(&prefix, &graph, &PROGRAMS, &other, &consent, 0)
        .is_err());
    assert!(base
        .hub_gate(
            &prefix,
            &graph,
            &PROGRAMS,
            &response_keys(),
            &crypto::Signature([0; 64]),
            0
        )
        .is_err());
}

const Z: [u8; 32] = [83; 32];

fn terms() -> TimedTerms {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    TimedTerms::new(
        cp.profile(),
        cp.commitment(),
        4,
        3,
        sha256::Hash::hash(&Z).to_byte_array(),
    )
    .unwrap()
}

fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    sha256d::Hash::hash(&[a.as_slice(), b.as_slice()].concat()).to_byte_array()
}
fn merkle(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    while hashes.len() > 1 {
        if hashes.len() % 2 == 1 {
            hashes.push(*hashes.last().unwrap());
        }
        hashes = hashes.chunks(2).map(|p| pair(&p[0], &p[1])).collect();
    }
    hashes[0]
}

/// Seven regtest-difficulty blocks; block 1 carries the Pay activation.
fn activated_chain() -> (Chain, Activation) {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    let mut previous: Header = deserialize(cp.header()).unwrap();
    let (mut headers, mut activation) = (vec![], None);
    for height in 1..=7u32 {
        let reserved = [7; 32];
        let mut txdata = vec![Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, height as u8, 0]),
                witness: Witness::from_slice(&[reserved]),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        }];
        if height == 1 {
            txdata.push(Transaction {
                version: transaction::Version::TWO,
                lock_time: absolute::LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint {
                        txid: Txid::from_byte_array([17; 32]),
                        vout: 1,
                    },
                    witness: Witness::from_slice(&[vec![1; 3], vec![0x51]]),
                    ..Default::default()
                }],
                output: vec![TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::from_bytes(chain::activation_script(&Z)),
                }],
            });
        }
        let mut wtxids: Vec<_> = txdata
            .iter()
            .map(|tx| tx.compute_wtxid().to_byte_array())
            .collect();
        wtxids[0] = [0; 32];
        let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        script.extend(pair(&merkle(wtxids), &reserved));
        txdata[0].output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(script),
        });
        let mut block = Block {
            header: Header {
                version: Version::from_consensus(4),
                prev_blockhash: previous.block_hash(),
                merkle_root: previous.merkle_root,
                time: previous.time + 600,
                bits: CompactTarget::from_consensus(0x207fffff),
                nonce: 0,
            },
            txdata,
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        while block.header.validate_pow(block.header.target()).is_err() {
            block.header.nonce += 1;
        }
        if height == 1 {
            let raw = serialize(&block);
            activation = Some(Activation {
                reference: ActivationRef {
                    height: 1,
                    block_hash: block.block_hash().to_byte_array(),
                    txid: block.txdata[1].compute_txid().to_byte_array(),
                    output: 0,
                },
                inclusion: Inclusion::capture(&raw, 1, raw.len()).unwrap(),
            });
        }
        headers.push(serialize(&block.header).try_into().unwrap());
        previous = block.header;
    }
    (
        Chain::verify(cp.clone(), &cp.commitment(), headers, 100).unwrap(),
        activation.unwrap(),
    )
}

#[test]
fn pending_sources_follow_activation_refund_and_preimage_rules() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let (chain, activation) = activated_chain();
    // Bob pays the holder 4 sats under an issuer-certified timed lock.
    let bob_hold = HoldRoot::from_seed(&[9; 32]).unwrap();
    let bob_note = mint(&o.issuer, &keyset, &derive(41, &bob_hold, 4, 29, 0));
    let pay = derive(SPEND, &hold, 4, 61, 0);
    let lock = TimedLock::new(terms(), vec![bob(4, 1)]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&bob_note),
        point(43),
        vec![Output::from_derivation(&pay).unwrap()],
        Some(lock),
    )
    .unwrap();
    let bob_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&bob_note),
        Authorization::Plain,
        &[owner(41, 29, 0)],
    )
    .unwrap();
    let cert =
        Certificate::attest(&o.issuer, &bob_signed, std::slice::from_ref(&bob_note)).unwrap();
    let bob_id = bob_signed.proposal().id();
    // The holder locked one of her own notes to Carol and claims its refund.
    let own = derive(SPEND, &hold, 4, 23, 40);
    let own_note = mint(&o.issuer, &keyset, &own);
    let refund = derive(SPEND, &hold, 4, 67, 0);
    let carol = derive(53, &HoldRoot::from_seed(&[11; 32]).unwrap(), 4, 31, 0);
    let lock = TimedLock::new(terms(), vec![Output::from_derivation(&refund).unwrap()]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&own_note),
        point(47),
        vec![Output::from_derivation(&carol).unwrap()],
        Some(lock),
    )
    .unwrap();
    let own_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&own_note),
        Authorization::Plain,
        &[owner(SPEND, 23, 40)],
    )
    .unwrap();
    let own_id = own_signed.proposal().id();
    let settled = derive(SPEND, &hold, 4, 23, 0);
    let settled_note = mint(&o.issuer, &keyset, &settled);

    let items = vec![
        Item::Settled(settled.message().clone()),
        Item::Pending {
            message: pay.message().clone(),
            source: bob_id,
            branch: Branch::Pay,
            index: 0,
            blinding: *pay.blinding(),
            preimage: Some(Z),
        },
        Item::Pending {
            message: refund.message().clone(),
            source: own_id,
            branch: Branch::Refund,
            index: 0,
            blinding: *refund.blinding(),
            preimage: None,
        },
    ];
    let sources = vec![
        Source::Admitted(cert),
        Source::OwnInputs {
            id: own_id,
            inputs: vec![own_note.message().identifier().unwrap()],
            terms: terms(),
        },
    ];
    let signers = [settled.clone(), pay.clone(), refund.clone(), own];
    let publish = |activations| {
        let unsigned = claim(
            &o,
            items.clone(),
            sources.clone(),
            activations,
            &signers,
            12,
        );
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let p = Publication::from_bytes(&keyset, LIMIT, &p.to_bytes()).unwrap();
        let request = Request {
            amount: 12,
            claim_digest: *p.claim().digest(),
        };
        (p, request)
    };
    let mut witness = HolderWitness {
        items: vec![
            ItemWitness::Settled {
                note: Box::new(settled_note),
                ownership: settled.ownership(),
            },
            ItemWitness::Pending {
                ownership: pay.ownership(),
            },
            ItemWitness::Pending {
                ownership: refund.ownership(),
            },
        ],
        sources: vec![
            SourceWitness::Certified {
                proposal: bob_signed.proposal().clone(),
                signatures: bob_signed.signatures().to_vec(),
            },
            SourceWitness::Signed {
                signed: own_signed,
                notes: vec![own_note],
            },
        ],
        chain: Some(chain.clone()),
        activations: vec![activation.clone()],
    };
    let bob_elsewhere = spend(
        &o,
        std::slice::from_ref(&bob_note),
        &[owner(41, 29, 0)],
        vec![bob(4, 2)],
    );
    let shared = Evidence::Spend {
        signed: bob_elsewhere,
        notes: vec![bob_note],
        unlock: None,
    };
    let counter = |p: &Publication, r: &Request, e: Evidence| {
        prove_counter(&o.base, &o.d, r, p, &[e], Some(&chain))
    };

    // Activated certified Pay: expiry and input sharing cannot void it.
    let record = ActivationRecord {
        source: bob_id,
        reference: activation.reference,
    };
    let (p, request) = publish(vec![record]);
    prove_holder(&o.base, &o.d, &request, &p, &witness).unwrap();
    // The holder guest's input carries sources, chain and activations; each
    // is rebuilt through its validating constructor.
    let empty = Inclusion {
        transaction: vec![],
        index: 0,
        siblings: vec![],
        coinbase: vec![],
        coinbase_siblings: vec![],
    };
    let input = StatementInput {
        c: 0,
        base: o.base.clone(),
        prefix: prefix(&o.base),
        observed: Observed {
            headers: vec![],
            request_height: 1,
            request: WitnessInclusion {
                non_witness: empty.clone(),
                full_transaction: vec![],
                witness_siblings: vec![],
                coinbase_witness_siblings: vec![],
                reserved_value: [0; 32],
            },
            publication_height: 2,
            publication: empty,
        },
        response: HubResponse::Plain {
            e_sig: licash::wots::Hashes(vec![[4; 32]; 67]),
        },
        holder: witness.clone(),
    };
    let bytes = input.to_bytes().unwrap();
    let decoded = StatementInput::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.to_bytes().unwrap(), bytes);
    prove_holder(&o.base, &o.d, &request, &p, &decoded.holder).unwrap();
    assert!(counter(&p, &request, Evidence::Expired { source: bob_id }).is_err());
    assert!(counter(&p, &request, shared.clone()).is_err());
    // The Pay preimage voids the holder's own refund; the rest stays owed.
    let c = counter(
        &p,
        &request,
        Evidence::PayPreimage {
            source: own_id,
            preimage: Z,
        },
    )
    .unwrap();
    assert_eq!((c.mask, c.owed), (0b100, 8));
    // Voiding the refund publishes the preimage, which is what the payee of
    // the lock needs to collect.
    let posted = counter_disclosures(&[Evidence::PayPreimage {
        source: own_id,
        preimage: Z,
    }])
    .unwrap();
    assert_eq!((posted.values[0], c.disclosure), (Z, posted.digest()));
    assert!(counter(
        &p,
        &request,
        Evidence::PayPreimage {
            source: own_id,
            preimage: [0; 32]
        }
    )
    .is_err());

    // Without an advertised activation, expiry or input sharing voids the Pay.
    let (p, request) = publish(vec![]);
    let c = counter(&p, &request, Evidence::Expired { source: bob_id }).unwrap();
    assert_eq!((c.mask, c.owed), (0b010, 8));
    let input = CounterInput {
        base: o.base.clone(),
        prefix: prefix(&o.base),
        publication: p.clone(),
        evidence: vec![Evidence::Expired { source: bob_id }],
        chain: Some(chain.clone()),
    };
    let decoded = CounterInput::from_bytes(&input.to_bytes().unwrap()).unwrap();
    assert_eq!(decoded.run().unwrap(), c);
    assert_eq!(counter(&p, &request, shared).unwrap().mask, 0b010);
    let short = Chain::verify(
        chain.checkpoint().clone(),
        &chain.checkpoint().commitment(),
        (1..=5).map(|h| *chain.header(h).unwrap()).collect(),
        100,
    )
    .unwrap();
    assert!(prove_counter(
        &o.base,
        &o.d,
        &request,
        &p,
        &[Evidence::Expired { source: bob_id }],
        Some(&short)
    )
    .is_err());

    // An advertised activation must be positively proven by the holder.
    let (p, request) = publish(vec![record]);
    witness.activations.clear();
    assert!(prove_holder(&o.base, &o.d, &request, &p, &witness).is_err());
}

#[test]
fn counter_k_carries_the_journal_values_and_rejects_other_layouts() {
    use licash::{
        recovery::CounterJournal,
        statement::K_LEN,
        statement::{counter_journal, counter_k},
    };
    let j = CounterJournal {
        final_digest: [1; 32],
        claim_digest: [2; 32],
        mask: 0b10,
        owed: 2048,
        disclosure: [3; 32],
    };
    let k = counter_k(&j, &[9; 128]).unwrap();
    assert_eq!(k.len(), K_LEN);
    assert_eq!(counter_journal(&[1; 32], &[2; 32], &k, [3; 32]).unwrap(), j);
    assert_ne!(counter_journal(&[1; 32], &[2; 32], &k, [4; 32]).unwrap(), j);
    assert!(counter_k(&j, &[9; 127]).is_err());
    let mut padded = k.clone();
    padded[20] = 1;
    assert!(counter_journal(&[1; 32], &[2; 32], &padded, [3; 32]).is_err());
    assert_eq!(j.to_bytes()[64..72], [0, 0, 0, 2, 0, 0, 8, 0]);
    assert_eq!(j.to_bytes()[72..], [3; 32]);
}

/// The paper's example at half scale. Alice pays Bob 50 from a 128 note with
/// 78 change, under a grant. The Hub records the spend and withholds the
/// unlock. Alice claims her input with the change as fallbacks.
struct Withheld {
    o: Opened,
    note: Note,
    signed: SignedProposal,
    unlock: [u8; 32],
    paid: Vec<Derivation>,
    change: Vec<Derivation>,
    request: Request,
    publication: Publication,
    witness: HolderWitness,
}

fn withheld() -> Withheld {
    let o = open_with(7);
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let n = derive(SPEND, &hold, 128, 23, 0);
    let note = mint(&o.issuer, &keyset, &n);
    let bob_hold = HoldRoot::from_seed(&[9; 32]).unwrap();
    let paid: Vec<_> = [32, 16, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(41, &bob_hold, *d, 29, i as u32))
        .collect();
    let change: Vec<_> = [64, 8, 4, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(SPEND, &hold, *d, 71, i as u32))
        .collect();
    let outputs = paid
        .iter()
        .chain(&change)
        .map(|d| Output::from_derivation(d).unwrap())
        .collect();
    let notes = std::slice::from_ref(&note);
    let proposal = Proposal::new(&keyset, notes, point(43), outputs, None).unwrap();
    let prepared = PreparedGrant::new(&o.issuer, &proposal).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &[owner(SPEND, 23, 0)],
    )
    .unwrap();
    let unlock = prepared.release(&keyset, notes, &signed).unwrap();
    let id = signed.proposal().id();
    let mut items = vec![Item::Settled(n.message().clone())];
    items.extend(change.iter().enumerate().map(|(i, d)| Item::Pending {
        message: d.message().clone(),
        source: id,
        branch: Branch::Pay,
        index: (paid.len() + i) as u32,
        blinding: *d.blinding(),
        preimage: None,
    }));
    let source = Source::Plain {
        id,
        inputs: vec![n.message().identifier().unwrap()],
    };
    let held: Vec<_> = std::iter::once(n.clone()).chain(change.clone()).collect();
    let unsigned = claim(&o, items, vec![source], vec![], &held, 128);
    let publication = Publication::sign(unsigned, &hold_keys(&held), LIMIT).unwrap();
    let request = Request {
        amount: 128,
        claim_digest: *publication.claim().digest(),
    };
    let mut witnesses = vec![ItemWitness::Settled {
        note: Box::new(note.clone()),
        ownership: n.ownership(),
    }];
    witnesses.extend(change.iter().map(|d| ItemWitness::Pending {
        ownership: d.ownership(),
    }));
    let witness = HolderWitness {
        items: witnesses,
        sources: vec![SourceWitness::Signed {
            signed: signed.clone(),
            notes: vec![note.clone()],
        }],
        chain: None,
        activations: vec![],
    };
    Withheld {
        o,
        note,
        signed,
        unlock,
        paid,
        change,
        request,
        publication,
        witness,
    }
}

#[test]
fn a_withheld_granted_payment_is_countered_only_by_publishing_its_unlock() {
    let w = withheld();
    let (o, keyset) = (&w.o, w.o.base.parts().keyset.clone());
    // Without a counter Alice recovers her input in full.
    let holder = prove_holder(&o.base, &o.d, &w.request, &w.publication, &w.witness).unwrap();
    let owed = |response, holder| settle(&o.base, &o.d, &w.request, response, holder).unwrap();
    assert_eq!(owed(Response::Plain, Some(&holder)).owed, 128);

    let cite = |unlock| Evidence::Spend {
        signed: w.signed.clone(),
        notes: vec![w.note.clone()],
        unlock,
    };
    let counter =
        |e: Evidence| prove_counter(&o.base, &o.d, &w.request, &w.publication, &[e], None);
    // The Hub cannot retire her input while keeping the unlock to itself.
    assert!(counter(cite(None)).is_err());
    assert!(counter(cite(Some([5; 32]))).is_err());
    // Nor by presenting the granted spend as a plain one: the owner signed
    // the grant commitment.
    assert!(SignedProposal::from_parts(
        &keyset,
        w.signed.proposal().clone(),
        std::slice::from_ref(&w.note),
        Authorization::Plain,
        w.signed.signatures().to_vec(),
    )
    .is_err());

    // The valid counter voids the input, values the change and commits the
    // unlock it must publish.
    let journal = counter(cite(Some(w.unlock))).unwrap();
    assert_eq!((journal.mask, journal.owed), (1, 78));
    let posted = counter_disclosures(&[cite(Some(w.unlock))]).unwrap();
    assert_eq!(posted.values[0], w.unlock);
    assert!(posted.values[1..].iter().all(|slot| *slot == [0; 32]));
    assert_eq!(journal.disclosure, posted.digest());
    assert_ne!(
        journal.disclosure,
        counter_disclosures(&[cite(None)]).unwrap().digest()
    );
    assert_eq!(owed(Response::Counter(journal), Some(&holder)).owed, 78);

    // Bob, holding the grant, opens his 50 from the published slots alone;
    // Alice opens her change the same way.
    let Authorization::Granted(grant) = w.signed.authorization() else {
        panic!("granted fixture");
    };
    let found = grant.unlock_in(&posted.values).unwrap();
    let mut opened = 0;
    for (index, d) in w.paid.iter().chain(&w.change).enumerate() {
        let note = w
            .signed
            .open_granted_output(&keyset, std::slice::from_ref(&w.note), index, d, &found)
            .unwrap();
        note.verify(&keyset).unwrap();
        opened += note.message().denomination();
    }
    assert_eq!(opened, 128);
    assert!(grant.unlock_in(&[[0; 32]; 32]).is_none());

    // The counter guest's input carries the unlock through its codec.
    let input = CounterInput {
        base: o.base.clone(),
        prefix: prefix(&o.base),
        publication: w.publication.clone(),
        evidence: vec![cite(Some(w.unlock))],
        chain: None,
    };
    let bytes = input.to_bytes().unwrap();
    assert_eq!(
        CounterInput::from_bytes(&bytes).unwrap().run().unwrap(),
        journal
    );
}

/// Both programs read Bitcoin from Base's checkpoint. The Hub could never show
/// that a lock timed on another checkpoint expired, so its Pay outputs are not
/// claimable under this Base, whatever the claim says about an activation.
#[test]
fn pay_outputs_of_a_lock_timed_on_another_checkpoint_are_not_claimable() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let prove = |terms: TimedTerms| {
        let payer = HoldRoot::from_seed(&[9; 32]).unwrap();
        let note = mint(&o.issuer, &keyset, &derive(41, &payer, 4, 29, 0));
        let notes = std::slice::from_ref(&note);
        let pay = derive(SPEND, &hold(), 4, 61, 0);
        let lock = TimedLock::new(terms, vec![bob(4, 1)]).unwrap();
        let outputs = vec![Output::from_derivation(&pay).unwrap()];
        let body = Proposal::new(&keyset, notes, point(43), outputs, Some(lock)).unwrap();
        let owners = [owner(41, 29, 0)];
        let signed =
            SignedProposal::sign(&keyset, body, notes, Authorization::Plain, &owners).unwrap();
        let cert = Certificate::attest(&o.issuer, &signed, notes).unwrap();
        let items = vec![Item::Pending {
            message: pay.message().clone(),
            source: signed.proposal().id(),
            branch: Branch::Pay,
            index: 0,
            blinding: *pay.blinding(),
            preimage: Some(Z),
        }];
        let signers = [pay.clone()];
        let unsigned = claim(&o, items, vec![Source::Admitted(cert)], vec![], &signers, 4);
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let request = Request {
            amount: 4,
            claim_digest: *p.claim().digest(),
        };
        let witness = HolderWitness {
            items: vec![ItemWitness::Pending {
                ownership: pay.ownership(),
            }],
            sources: vec![SourceWitness::Certified {
                proposal: signed.proposal().clone(),
                signatures: signed.signatures().to_vec(),
            }],
            chain: None,
            activations: vec![],
        };
        prove_holder(&o.base, &o.d, &request, &p, &witness).map(|j| j.amount)
    };
    let here = terms();
    assert_eq!(prove(here.clone()).unwrap(), 4);
    let hash = *here.hash();
    let elsewhere = TimedTerms::new(here.profile(), [7; 32], 4, 3, hash).unwrap();
    let refused = prove(elsewhere).unwrap_err();
    assert!(
        refused.to_string().contains("another checkpoint"),
        "{refused}"
    );
    let mainnet = TimedTerms::new(ChainProfile::Core30Mainnet, *here.checkpoint(), 4, 3, hash);
    let refused = prove(mainnet.unwrap()).unwrap_err();
    assert!(
        refused.to_string().contains("another checkpoint"),
        "{refused}"
    );
}

/// A claim of the refund of an unsubmitted lock carries Q's authorization for
/// the lock's inputs, because the Hub may never have seen the lock. A note
/// claimed in one contract and the refund of a lock of it in another are then
/// two claims of the same value, and the first voids the second.
#[test]
fn second_claim_of_an_input_voids_the_outputs_of_a_lock_of_the_holders_own_notes() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let own = derive(SPEND, &hold, 4, 23, 40);
    let own_note = mint(&o.issuer, &keyset, &own);
    let refund = derive(SPEND, &hold, 4, 67, 0);
    let carol = derive(53, &HoldRoot::from_seed(&[11; 32]).unwrap(), 4, 31, 0);
    let lock = TimedLock::new(terms(), vec![Output::from_derivation(&refund).unwrap()]).unwrap();
    let notes = std::slice::from_ref(&own_note);
    let outputs = vec![Output::from_derivation(&carol).unwrap()];
    let body = Proposal::new(&keyset, notes, point(47), outputs, Some(lock)).unwrap();
    let owners = [owner(SPEND, 23, 40)];
    let signed = SignedProposal::sign(&keyset, body, notes, Authorization::Plain, &owners).unwrap();
    let id = signed.proposal().id();
    let settled = derive(SPEND, &hold, 4, 23, 0);
    let items = vec![
        Item::Settled(settled.message().clone()),
        Item::Pending {
            message: refund.message().clone(),
            source: id,
            branch: Branch::Refund,
            index: 0,
            blinding: *refund.blinding(),
            preimage: None,
        },
    ];
    let sources = vec![Source::OwnInputs {
        id,
        inputs: vec![own_note.message().identifier().unwrap()],
        terms: terms(),
    }];
    let signers = [settled.clone(), refund.clone(), own.clone()];
    let unsigned = claim(&o, items, sources, vec![], &signers, 8);
    let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
    let request = Request {
        amount: 8,
        claim_digest: *p.claim().digest(),
    };
    let witness = HolderWitness {
        items: vec![
            ItemWitness::Settled {
                note: Box::new(mint(&o.issuer, &keyset, &settled)),
                ownership: settled.ownership(),
            },
            ItemWitness::Pending {
                ownership: refund.ownership(),
            },
        ],
        sources: vec![SourceWitness::Signed {
            signed,
            notes: vec![own_note],
        }],
        chain: None,
        activations: vec![],
    };
    prove_holder(&o.base, &o.d, &request, &p, &witness).unwrap();

    // The locked note was claimed in another contract.
    let elsewhere = hold::Context {
        final_digest: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let second = |signer: &Derivation| Evidence::SecondClaim {
        context: elsewhere,
        record: hold::Envelope::sign(elsewhere, &hold_keys(std::slice::from_ref(signer)), LIMIT)
            .unwrap()
            .records()[0]
            .clone(),
    };
    let c = prove_counter(&o.base, &o.d, &request, &p, &[second(&own)], None).unwrap();
    assert_eq!((c.mask, c.owed), (0b10, 4));
    // A claim of a note that this one does not rely on is no evidence.
    let unrelated = derive(SPEND, &hold, 4, 23, 41);
    assert!(prove_counter(&o.base, &o.d, &request, &p, &[second(&unrelated)], None).is_err());
}

/// The value left after a counter is capped by the request, whatever the
/// denominations of the surviving items add up to.
#[test]
fn surviving_value_beyond_u64_is_capped_at_the_request() {
    let o = open_with(63);
    let keyset = o.base.parts().keyset.clone();
    let ds: Vec<_> = (0..3)
        .map(|i| derive(SPEND, &hold(), 1 << 63, 23, i))
        .collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let unsigned = claim(&o, items, vec![], vec![], &ds, 128);
    let p = Publication::sign(unsigned, &hold_keys(&ds), LIMIT).unwrap();
    let request = Request {
        amount: 128,
        claim_digest: *p.claim().digest(),
    };
    let spent = Evidence::Spend {
        signed: spend(
            &o,
            &notes[..1],
            &[owner(SPEND, 23, 0)],
            vec![bob(1 << 63, 0)],
        ),
        notes: notes[..1].to_vec(),
        unlock: None,
    };
    let c = prove_counter(&o.base, &o.d, &request, &p, &[spent], None).unwrap();
    assert_eq!((c.mask, c.owed), (0b1, 128));
}

#[test]
fn second_claim_signature_cannot_be_assigned_to_a_different_note_with_the_same_q() {
    let o = open();
    let first = derive(SPEND, &hold(), 4, 23, 0);
    let second = derive(SPEND, &hold(), 8, 23, 0);
    assert_eq!(first.message().hold(), second.message().hold());
    let unsigned = claim(
        &o,
        vec![Item::Settled(second.message().clone())],
        vec![],
        vec![],
        std::slice::from_ref(&second),
        8,
    );
    let p = Publication::sign(unsigned, &hold_keys(std::slice::from_ref(&second)), LIMIT).unwrap();
    let request = Request {
        amount: 8,
        claim_digest: *p.claim().digest(),
    };
    let context = hold::Context {
        final_digest: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let envelope = hold::Envelope::sign(context, &hold_keys(&[first]), LIMIT).unwrap();
    let mut record = envelope.records()[0].clone();
    record.message = second.message().clone();
    let evidence = Evidence::SecondClaim { context, record };
    assert!(prove_counter(&o.base, &o.d, &request, &p, &[evidence], None).is_err());
}

/// Input Q authority is required only for OwnInputs. Certified outputs belong
/// to their payee and cannot be retired by a signature from the payer's Q key.
/// For OwnInputs, however, even one conflicting input retires every dependent
/// output, on either branch. Such a source must be recovered in one claim.
#[test]
fn input_claim_conflicts_preserve_certified_outputs_but_retire_all_own_outputs() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let inputs: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let notes: Vec<_> = inputs.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let pay: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 61, i)).collect();
    let refund: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 67, i)).collect();
    let outputs = |ds: &[Derivation]| {
        ds.iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect()
    };
    let lock = TimedLock::new(terms(), outputs(&refund)).unwrap();
    let proposal = Proposal::new(&keyset, &notes, point(47), outputs(&pay), Some(lock)).unwrap();
    let owners: Vec<_> = (0..2).map(|i| owner(SPEND, 23, i)).collect();
    let signed =
        SignedProposal::sign(&keyset, proposal, &notes, Authorization::Plain, &owners).unwrap();
    let id = signed.proposal().id();
    let cert = Certificate::attest(&o.issuer, &signed, &notes).unwrap();
    let settled = derive(SPEND, &hold(), 4, 23, 40);
    let (chain, activation) = activated_chain();
    let publish = |branch: Branch, admitted: bool, indices: &[usize]| {
        let ds = if branch == Branch::Pay { &pay } else { &refund };
        let mut items = vec![Item::Settled(settled.message().clone())];
        let mut signers = vec![settled.clone()];
        for i in indices {
            let d = &ds[*i];
            items.push(Item::Pending {
                message: d.message().clone(),
                source: id,
                branch,
                index: *i as u32,
                blinding: *d.blinding(),
                preimage: (branch == Branch::Pay).then_some(Z),
            });
            signers.push(d.clone());
        }
        let mut activations = vec![];
        let source = if admitted {
            if branch == Branch::Pay {
                activations.push(ActivationRecord {
                    source: id,
                    reference: activation.reference,
                });
            }
            Source::Admitted(cert.clone())
        } else {
            signers.extend(inputs.iter().cloned());
            Source::OwnInputs {
                id,
                inputs: signed
                    .proposal()
                    .inputs()
                    .iter()
                    .map(|m| m.identifier().unwrap())
                    .collect(),
                terms: terms(),
            }
        };
        let amount = (4 * (1 + indices.len())) as u32;
        let unsigned = claim(&o, items, vec![source], activations, &signers, amount);
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let request = Request {
            amount,
            claim_digest: *p.claim().digest(),
        };
        (p, request)
    };
    let elsewhere = hold::Context {
        final_digest: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let e = Evidence::SecondClaim {
        context: elsewhere,
        record: hold::Envelope::sign(elsewhere, &hold_keys(&inputs[..1]), LIMIT)
            .unwrap()
            .records()[0]
            .clone(),
    };
    for branch in [Branch::Pay, Branch::Refund] {
        let (p, r) = publish(branch, true, &[0, 1]);
        assert!(prove_counter(
            &o.base,
            &o.d,
            &r,
            &p,
            std::slice::from_ref(&e),
            Some(&chain)
        )
        .is_err());
        let (p, r) = publish(branch, false, &[0, 1]);
        let c = prove_counter(&o.base, &o.d, &r, &p, std::slice::from_ref(&e), None).unwrap();
        assert_eq!((c.mask, c.owed), (0b110, 4));
    }
    // Even disjoint outputs of one OwnInputs source share its input Q
    // authorities: splitting this recovery across claims is not supported.
    let (first, r1) = publish(Branch::Refund, false, &[0]);
    let (second, r2) = publish(Branch::Refund, false, &[1]);
    for (here, r, other) in [(&first, &r1, &second), (&second, &r2, &first)] {
        let evidence = Evidence::SecondClaim {
            context: other.claim().hold_context(),
            record: other
                .envelope()
                .records()
                .iter()
                .find(|record| record.message == *inputs[0].message())
                .unwrap()
                .clone(),
        };
        let c = prove_counter(&o.base, &o.d, r, here, &[evidence], None).unwrap();
        assert_eq!((c.mask, c.owed), (0b10, 4));
    }
}
