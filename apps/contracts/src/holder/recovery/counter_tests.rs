//! A real holder statement over synthetic regtest blocks, not a mocked
//! statement result. Like the SDK's recovery/chain fixtures, this checks
//! evidence and signatures without a node or an expensive contract opening.

use super::*;
use bitcoin::{
    absolute, block, consensus,
    hashes::Hash as _,
    pow::CompactTarget,
    secp256k1::{Secp256k1, XOnlyPublicKey},
    taproot::{LeafVersion, TaprootBuilder},
    transaction, Block, Sequence, TxIn,
};
use k256::Scalar;
use licash::{
    base::{Base, ChainTerms, Fees, GraphTerms, Limits, Parts, Party, Recipe, ResponseKeys},
    chain::{self, Checkpoint},
    crypto::{self, Point},
    disclosure::{Disclosures, SignedDisclosures, CHUNK_DIGITS},
    note::{Derivation, Evidence, HoldRoot, IssuerKeys, Note},
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    spend::ChainProfile,
    statement::{SecretKeys, K_LEN},
    wots::SecretKey,
};

const SEED: [u8; 32] = [7; 32];

fn point(n: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(n)))
}

fn base() -> (Base, IssuerKeys) {
    let issuer = IssuerKeys::random(3).unwrap();
    let party = |n| {
        let mut payout_script = vec![0x51, 0x20];
        payout_script.extend(point(n + 3).x_only());
        Party {
            funding: point(n).x_only(),
            payment: point(n + 1).x_only(),
            delayed: point(n + 2).x_only(),
            payout_script,
        }
    };
    let keys = SecretKeys::from_entropy(
        std::array::from_fn(|i| [i as u8 + 1; 32]),
        std::array::from_fn(|i| [i as u8 + 11; 32]),
    );
    let base = Base::new(Parts {
        nonce: [53; 32],
        spend_root: point(17),
        claim_key: point(19).x_only(),
        keyset: issuer.keyset().unwrap(),
        deposit: [31; 36],
        cap: 128,
        response_keys: ResponseKeys::new(keys.public()).unwrap(),
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
            max_authorizations: 32,
            max_source_inputs: 128,
        },
        fees: Fees {
            sweep_fee_max: 1_000,
            external_fee_max: 2_000,
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    })
    .unwrap();
    (base, issuer)
}

fn prefix(base: &Base) -> (FrozenPrefix, Transaction, Vec<u8>, Vec<u8>) {
    let parts = base.parts();
    let keys = Signer::derive(&SEED, &base.digest()).public();
    let (alice, hub, delay) = (
        parts.parties[0].payment,
        parts.parties[1].payment,
        parts.graph.to_self_delay,
    );
    let script = ScriptBuf::from_bytes(
        keys.request_leaf_for_base(&base.digest(), &alice, &hub, delay)
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
        .control_block(&(script.clone(), LeafVersion::TapScript))
        .unwrap()
        .serialize();
    let leaf =
        RequestLeaf::for_base(base.digest(), keys, alice, hub, delay, control.clone()).unwrap();
    let value = parts.graph.graph_value().unwrap();
    let deposit = base.deposit_outpoint();
    let funding = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array(deposit.txid),
                vout: deposit.index,
            },
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: spk.clone(),
        }],
    };
    let request = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: funding.compute_txid(),
                vout: 0,
            },
            sequence: Sequence::from_height(delay),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value - 10_000),
            script_pubkey: spk.clone(),
        }],
        ..funding.clone()
    };
    let output = |value| chain::Output {
        value,
        script: spk.to_bytes(),
    };
    let prefix = FrozenPrefix::new(
        chain::Transaction::parse(&consensus::serialize(&funding), 100_000).unwrap(),
        chain::Transaction::parse(&consensus::serialize(&request), 100_000).unwrap(),
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
    .unwrap();
    (prefix, request, script.to_bytes(), control)
}

fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    bitcoin::hashes::sha256d::Hash::hash(&[a.as_slice(), b.as_slice()].concat()).to_byte_array()
}

fn block(previous: &block::Header, target: Transaction) -> Block {
    let reserved = [7; 32];
    let commitment = pair(
        &pair(&[0; 32], &target.compute_wtxid().to_byte_array()),
        &reserved,
    );
    let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    script.extend(commitment);
    let coinbase = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            witness: Witness::from_slice(&[reserved]),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(script),
        }],
    };
    let mut block = Block {
        header: block::Header {
            version: block::Version::from_consensus(4),
            prev_blockhash: previous.block_hash(),
            merkle_root: previous.merkle_root,
            time: previous.time + 600,
            bits: CompactTarget::from_consensus(0x207fffff),
            nonce: 0,
        },
        txdata: vec![coinbase, target],
    };
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    while block.header.validate_pow(block.header.target()).is_err() {
        block.header.nonce += 1;
    }
    block
}

fn fixture(k: Vec<u8>) -> StatementInput {
    let (base, issuer) = base();
    let (prefix, mut r, script, control) = prefix(&base);
    let d = base.final_digest(&prefix).unwrap();
    let hold = HoldRoot::from_seed(&SEED).unwrap();
    let derived = Derivation::derive(4, point(17), hold.public(), &[23; 32], 0).unwrap();
    let blinded = derived.blinded().unwrap();
    let proof = issuer.prove_blinded(4, blinded).unwrap();
    let note = Note::from_parts(
        &base.parts().keyset,
        *base.parts().keyset.id(),
        derived.message().clone(),
        proof.open_point(&base.parts().keyset, &derived).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *derived.blinding(),
            commit: crypto::dleq_commit(&Scalar::from(71u64), &blinded.to_projective().unwrap()),
        },
    )
    .unwrap();
    let unsigned = UnsignedClaim::new(
        claim::Context {
            final_digest: *d.as_bytes(),
            deposit: base.parts().deposit,
            cap: 128,
        },
        &base.parts().keyset,
        4,
        claim::Parts {
            items: vec![Item::Settled(derived.message().clone())],
            sources: vec![],
            activations: vec![],
            authorizations: vec![derived.message().clone()],
        },
        32,
    )
    .unwrap();
    let publication = Publication::sign(
        unsigned,
        &[(derived.message().clone(), derived.hold_key(&hold).unwrap())],
        32,
    )
    .unwrap();
    let request = Request {
        amount: 4,
        claim_digest: *publication.claim().digest(),
    };
    r.input[0].witness = Witness::from_slice(
        &Signer::derive(&SEED, &base.digest())
            .sign(request)
            .witness([17; 64], [19; 64], script, control),
    );
    let p = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([5; 32]),
                vout: 0,
            },
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(publication.script().unwrap()),
        }],
    };
    let genesis = consensus::deserialize(base.parts().chain.checkpoint.header()).unwrap();
    let rb = block(&genesis, r);
    let pb = block(&rb.header, p);
    let disclosures = Disclosures::empty();
    let disclosure_keys =
        std::array::from_fn(|i| SecretKey::from_entropy(CHUNK_DIGITS[i], [i as u8 + 11; 32]));
    let response = HubResponse::Counter {
        e_sig: SecretKey::from_entropy(licash::wots::MSG_DIGITS, [3; 32])
            .sign_msg(&request.claim_digest),
        k_sig: SecretKey::from_entropy(licash::statement::K_DIGITS, [4; 32]).sign_msg(&k),
        k,
        b_corr: 2,
        corr_sig: SecretKey::from_entropy(licash::wots::B_DIGITS, [5; 32]).sign_b(2),
        publication: Box::new(SignedDisclosures::sign(disclosures, &disclosure_keys).unwrap()),
    };
    StatementInput {
        c: 0,
        base,
        prefix,
        response,
        observed: Observed {
            headers: vec![
                consensus::serialize(&rb.header).try_into().unwrap(),
                consensus::serialize(&pb.header).try_into().unwrap(),
            ],
            request_height: 1,
            request: WitnessInclusion::capture(&consensus::serialize(&rb), 1, MAX_BLOCK).unwrap(),
            publication_height: 2,
            publication: Inclusion::capture(&consensus::serialize(&pb), 1, MAX_BLOCK).unwrap(),
        },
        holder: HolderWitness {
            items: vec![ItemWitness::Settled {
                note: Box::new(note),
                ownership: derived.ownership(),
            }],
            sources: vec![],
            chain: None,
            activations: vec![],
        },
    }
}

#[test]
fn signed_garbage_k_can_be_exported_and_proven_as_a_full_claim() {
    let mut k = vec![0; K_LEN];
    k[20] = 1;
    let mut input = fixture(k);
    let StatementResult::Full(journal, paid) = classify_statement(&mut input).unwrap() else {
        panic!("a garbage counter was accepted")
    };
    assert_eq!(paid, 4);
    assert_eq!(journal.c, 0);
    let exported = input.to_bytes().unwrap();
    let mut imported = StatementInput::from_bytes(&exported).unwrap();
    assert!(matches!(
        classify_statement(&mut imported).unwrap(),
        StatementResult::Full(_, 4)
    ));
}

#[test]
fn a_well_formed_signed_counter_with_an_invalid_seal_pays_the_full_claim() {
    let mut k = vec![0; K_LEN];
    k[4..8].copy_from_slice(&2u32.to_be_bytes());
    let mut input = fixture(k);
    assert!(matches!(
        classify_statement(&mut input).unwrap(),
        StatementResult::Full(_, 4)
    ));
}

#[test]
fn a_bad_signature_is_not_a_full_claim_fallback() {
    let mut input = fixture(vec![0; K_LEN]);
    let HubResponse::Counter { e_sig, .. } = &mut input.response else {
        unreachable!()
    };
    e_sig.0[0][0] ^= 1;
    assert!(classify_statement(&mut input)
        .unwrap_err()
        .to_string()
        .contains("counter response signatures"));
    assert_eq!(input.c, 0);
}

#[tokio::test]
async fn a_corrected_statement_waits_for_proof_without_rebuilding_or_signing() {
    use k256::elliptic_curve::PrimeField as _;
    use licash_wallet::{Blocks, Response};
    use std::sync::atomic::AtomicUsize;

    struct Nowhere;
    impl Transport for Nowhere {
        async fn request(&self, _: &str, _: Option<Vec<u8>>) -> Result<Response, String> {
            panic!("a classified counter must not contact the hub")
        }
    }
    struct Watch(AtomicUsize);
    impl Blocks for Watch {
        async fn height(&self) -> Result<u32, String> {
            Ok(200)
        }
        async fn hash(&self, _: u32) -> Result<bitcoin::BlockHash, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(bitcoin::BlockHash::all_zeros())
        }
        async fn filter(&self, _: &bitcoin::BlockHash) -> Result<Option<Vec<u8>>, String> {
            Ok(Some(vec![0]))
        }
        async fn block(&self, _: &bitcoin::BlockHash) -> Result<Block, String> {
            panic!("the empty filter matches no spend")
        }
    }
    impl Chain for Watch {
        async fn checkpoint(&self, _: u32) -> Result<Checkpoint, String> {
            panic!("a classified counter must not rebuild its statement")
        }
        async fn header(&self, _: u32) -> Result<block::Header, String> {
            panic!("a classified counter must not fetch statement headers again")
        }
        async fn broadcast(&self, _: &Transaction) -> Result<(), String> {
            panic!("a classified counter must not sign or broadcast")
        }
    }
    // The frozen format-1 opening, using its documented fixture secrets.
    let bytes =
        include_bytes!("../../../../../crates/licash-contract/tests/golden/holder-funded.bin");
    let seed =
        hex::decode("2c45ef335c38f0e7417c97395c863c450c35fa34a629267e999994292aaa9ee7").unwrap();
    let spend =
        hex::decode("548fedbbc7e1fb67570cac75bf01a723c31987d60bfcd44eafd8c1da39b7c54f").unwrap();
    let spend: [u8; 32] = spend.try_into().unwrap();
    let spend = Option::<Scalar>::from(Scalar::from_repr(spend.into())).unwrap();
    let trust = Trust::mock();
    let holder = Holder::load(
        bytes,
        seed.try_into().unwrap(),
        spend,
        &Context { tip: 200, trust },
    )
    .unwrap();
    let o = holder.opening().unwrap();
    let keys = HolderKeys::derive(&[0; 32], &o.base.parts().nonce);
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[7; 32],
        ChainProfile::Core30DefaultRegtest,
        Nowhere,
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    let chain = Watch(AtomicUsize::new(0));
    let mut run = Run {
        wallet: &wallet,
        chain: &chain,
        store: &store,
        dir: dir.path(),
        id: [1; 32],
        rate: FeeRate::from_sat_per_vb(2).unwrap(),
        o,
        keys,
        // A genuine authority waits for an explicit proof action. The
        // frozen opening is used only to identify the watched output.
        trust: crate::trust(ChainProfile::Core30Signet).unwrap(),
        since: 100,
        refused: Default::default(),
        took: AtomicBool::new(false),
        rebuilt: AtomicBool::new(false),
    };
    assert!(!run.trust.mock);
    let spend = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![TxOut {
            value: Amount::from_sat(330),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    let mut state = State {
        notes: vec!["ab".into()],
        amount: 4,
        corrected: Some(2),
        spend: Some(spend),
        from: Some(199),
        deadline: Some(500),
        ..State::default()
    };
    for trust in [
        crate::trust(ChainProfile::Core30Signet).unwrap(),
        Trust::mock(),
    ] {
        run.trust = trust;
        for _ in 0..2 {
            assert!(matches!(
                run.proving(&mut state, 200).await.unwrap(),
                Next::Save
            ));
            assert_eq!(state.notes, vec!["ab"]);
            assert_eq!(state.amount, 4);
            assert_eq!(state.corrected, Some(2));
            // In particular, the full-claim mock prover must not run for
            // a corrected row under the regtest authority.
            assert!(state.proof.is_none());
        }
    }
    assert_eq!(chain.0.load(Ordering::SeqCst), 8);
    assert!(!store.statement_path(&run.id).exists());
}

#[test]
fn selecting_a_corrected_statement_keeps_the_original_claim_and_records_zero_explicitly() {
    let mut k = vec![0; K_LEN];
    k[4..8].copy_from_slice(&2u32.to_be_bytes());
    let mut input = fixture(k);
    input.c = 1;
    let mut state = State {
        amount: 4,
        ..State::default()
    };
    // This tests metadata after a c=1 decision, not counter validity.
    // Production reaches this transition only after the real verifier.
    let bytes = state.prepare_statement(&input, 2, 500).unwrap();
    assert_eq!(state.amount, 4);
    assert_eq!(state.corrected, Some(2));
    assert_eq!(state.deadline, Some(500));
    let exported = StatementInput::from_bytes(&bytes).unwrap();
    state.check_statement(&exported).unwrap();
    assert!(full_only(&state).is_err());
    let before = state.encode();
    assert!(state.prepare_statement(&input, 2, 501).is_err());
    assert_eq!(state.encode(), before);

    let HubResponse::Counter { b_corr, .. } = &mut input.response else {
        unreachable!()
    };
    *b_corr = 0;
    let mut zero = State {
        amount: 4,
        ..State::default()
    };
    zero.prepare_statement(&input, 0, 500).unwrap();
    assert_eq!(zero.corrected, Some(0));
    assert!(String::from_utf8(zero.encode())
        .unwrap()
        .contains("\"corrected\":0"));
    zero.check_statement(&input).unwrap();
    input.c = 0;
    assert!(zero.check_statement(&input).is_err());
}

#[test]
fn an_orphan_or_mismatched_statement_cannot_select_the_recovery_mode() {
    let mut input = fixture(vec![0; K_LEN]);
    let full = State {
        amount: 4,
        ..State::default()
    };
    full.check_statement(&input).unwrap();
    input.c = 1;
    assert!(full.check_statement(&input).is_err());
    let corrected = State {
        amount: 4,
        corrected: Some(2),
        ..State::default()
    };
    corrected.check_statement(&input).unwrap();
    let other = State {
        amount: 4,
        corrected: Some(3),
        ..State::default()
    };
    assert!(other.check_statement(&input).is_err());
    assert_eq!(full.corrected, None);
    assert_eq!(full.amount, 4);
}

#[test]
fn rejected_statement_amounts_leave_all_recovery_metadata_unchanged() {
    let mut input = fixture(vec![0; K_LEN]);
    let mut state = State {
        amount: 4,
        ..State::default()
    };
    let before = state.encode();
    assert!(state.prepare_statement(&input, 2, 500).is_err());
    assert_eq!(state.encode(), before);
    input.c = 1;
    assert!(state.prepare_statement(&input, 5, 500).is_err());
    assert_eq!(state.encode(), before);
    assert!(state.prepare_statement(&input, 3, 500).is_err());
    assert_eq!(state.encode(), before);
}

#[test]
fn corrected_metadata_cannot_resume_an_earlier_full_claim_signing_stage() {
    let state = State {
        amount: 4,
        corrected: Some(0),
        ..State::default()
    };
    for stage in Stage::ALL {
        let row = RecoveryRow {
            id: [1; 32],
            format: FORMAT,
            stage: stage.as_str().into(),
            irrevocable: true,
            error: None,
            state: state.encode(),
            created_at: 0,
        };
        let allowed = matches!(
            stage,
            Stage::Proving
                | Stage::Asserting
                | Stage::Asserted
                | Stage::Succeeding
                | Stage::Answering
                | Stage::Paid
                | Stage::Sweeping
                | Stage::Done
                | Stage::Lost
        );
        assert_eq!(State::decode(&row).is_ok(), allowed, "{stage:?}");
    }
}

#[test]
fn older_answered_and_countered_rows_commit_mode_with_the_statement_and_reject_orphans() {
    use super::super::store::New;

    let mut input = fixture(vec![0; K_LEN]);
    input.c = 1;
    for (stage, deadline) in [
        (Stage::Answered, None),
        (Stage::Countered, None),
        (Stage::Countered, Some(400)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let id = [1; 32];
        store
            .insert(&New {
                id,
                hub: "http://hub.example",
                mock: true,
                deposit: OutPoint::null(),
                value: 60_000,
                price: 23_000,
                cap: 128,
                assert_height: 800,
                close_height: 1_000,
                since: 0,
                state: b"unused opening",
            })
            .unwrap();
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let original = State {
            notes: vec!["ab".into()],
            amount: 4,
            r: Some(tx.clone()),
            p: Some(tx.clone()),
            spend: Some(tx),
            r_height: Some(1),
            p_height: Some(2),
            spent_height: Some(3),
            from: Some(3),
            deadline,
            ..State::default()
        };
        let raw = original.encode();
        store
            .start_recovery(&id, FORMAT, stage.as_str(), &raw)
            .unwrap();
        assert!(store
            .save_recovery(&id, (stage.as_str(), &raw), stage.as_str(), true, &raw)
            .unwrap());
        let mut selected = State::decode(&store.recovery(&id).unwrap().unwrap()).unwrap();
        assert!(selected.corrected.is_none());
        let bytes = selected.prepare_statement(&input, 2, 500).unwrap();

        // A crash before the row CAS leaves an orphan file, not a mode
        // decision. Export cannot use that file to switch the old row.
        store.keep_statement(&id, &bytes).unwrap();
        assert!(statement(dir.path(), &id).is_err());
        let mut full_orphan = input.clone();
        full_orphan.c = 0;
        store
            .keep_statement(&id, &full_orphan.to_bytes().unwrap())
            .unwrap();
        assert!(statement(dir.path(), &id).is_err());
        store.keep_statement(&id, &bytes).unwrap();
        assert!(State::decode(&store.recovery(&id).unwrap().unwrap())
            .unwrap()
            .corrected
            .is_none());

        assert!(store
            .save_recovery(
                &id,
                (stage.as_str(), &raw),
                Stage::Proving.as_str(),
                true,
                &selected.encode()
            )
            .unwrap());
        let row = store.recovery(&id).unwrap().unwrap();
        let resumed = State::decode(&row).unwrap();
        assert_eq!(row.format, FORMAT);
        assert!(row.irrevocable);
        assert_eq!(row.stage, Stage::Proving.as_str());
        assert_eq!(resumed.corrected, Some(2));
        assert_eq!(resumed.amount, original.amount);
        assert_eq!(resumed.notes, original.notes);
        assert_eq!(resumed.r, original.r);
        assert_eq!(resumed.p, original.p);
        assert_eq!(resumed.spend, original.spend);
        assert_eq!(statement(dir.path(), &id).unwrap(), bytes);
        assert!(resumed.proof.is_none());

        // A stale runner cannot overwrite the mode in the database. Its
        // file can win a rename race, but export must refuse the mismatch.
        assert!(!store
            .save_recovery(
                &id,
                (stage.as_str(), &raw),
                Stage::Proving.as_str(),
                true,
                &original.encode()
            )
            .unwrap());
        let mut wrong = input.clone();
        wrong.c = 0;
        store
            .keep_statement(&id, &wrong.to_bytes().unwrap())
            .unwrap();
        assert!(statement(dir.path(), &id).is_err());
        assert_eq!(
            State::decode(&store.recovery(&id).unwrap().unwrap())
                .unwrap()
                .corrected,
            Some(2)
        );
        std::fs::remove_file(store.statement_path(&id)).unwrap();
        assert!(statement(dir.path(), &id).is_err());
        assert_eq!(
            State::decode(&store.recovery(&id).unwrap().unwrap())
                .unwrap()
                .corrected,
            Some(2)
        );
    }
}
