use k256::Scalar;
use licash::{
    base::{
        Base, ChainTerms, Fees, GraphTerms, Limits, Parts, Party, Recipe, ResponseKeys,
        RESPONSE_KEYS_BYTES,
    },
    chain::Checkpoint,
    crypto::{self, tagged_hash, Point, Signature},
    note::IssuerKeys,
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    request::Signer,
    spend::ChainProfile,
    statement::SecretKeys,
};

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

#[test]
fn canonical_response_keys_bind_the_sealed_ten_role_shape_not_a_short_k() {
    let keys = response_keys();
    let bytes = keys.to_bytes();
    assert_eq!(bytes.len(), RESPONSE_KEYS_BYTES);
    assert_eq!(keys.keys().k.len(), 324);
    assert_eq!(
        keys.keys()
            .disclosure
            .iter()
            .map(|key| key.len())
            .sum::<usize>(),
        2_067
    );
    assert_eq!(ResponseKeys::from_bytes(&bytes).unwrap(), keys);
    let mut short_k = keys.keys().clone();
    short_k.k.0.truncate(67);
    assert!(ResponseKeys::new(short_k).is_err());
    let mut missing = keys.keys().clone();
    missing.disclosure[4].0.pop();
    assert!(ResponseKeys::new(missing).is_err());
    assert_eq!(format!("{keys:?}"), "ResponseKeys(..)");
}

#[test]
fn response_codec_rejects_truncation_trailing_roles_and_count_substitution() {
    let bytes = response_keys().to_bytes();
    for n in 0..bytes.len() {
        assert!(ResponseKeys::from_bytes(&bytes[..n]).is_err());
    }
    for field in 0..5 {
        let mut changed = bytes.clone();
        let at = b"LICASH-RSPK\x02".len();
        match field {
            0 => changed.push(0),
            1 => changed[0] ^= 1,
            2 => changed[at] = 9,
            3 => changed[at + 1] = 1,
            _ => changed[at + 2..at + 6].copy_from_slice(&u32::MAX.to_be_bytes()),
        }
        assert!(ResponseKeys::from_bytes(&changed).is_err());
    }
}

#[test]
fn strict_response_encoding_preserves_existing_sealed_endpoint_commitments() {
    let keys = response_keys();
    let original = keys.keys().digest(&[31; 36]);
    let decoded = ResponseKeys::from_bytes(&keys.to_bytes()).unwrap();
    assert_eq!(decoded.keys().digest(&[31; 36]), original);
    let mut changed = keys.keys().clone();
    changed.e_counter.0[0][0] ^= 1;
    assert_ne!(
        ResponseKeys::new(changed).unwrap().to_bytes(),
        keys.to_bytes()
    );
}

#[test]
fn complete_checkpoint_codec_preserves_the_existing_commitment_preimage() {
    for (profile, tag) in [
        (ChainProfile::Core30Mainnet, 0),
        (ChainProfile::Core30DefaultRegtest, 1),
    ] {
        let cp = Checkpoint::genesis(profile).unwrap();
        let time = u32::from_le_bytes(cp.header()[68..72].try_into().unwrap());
        let mut old = vec![tag];
        old.extend(0u32.to_be_bytes());
        old.extend(cp.header());
        old.extend(1u32.to_be_bytes());
        old.extend(time.to_be_bytes());
        if tag == 0 {
            old.push(1);
            old.extend(time.to_be_bytes());
        } else {
            old.push(0);
        }
        assert_eq!(
            cp.commitment(),
            tagged_hash("licash/header-checkpoint/v2", &[&old])
        );
        let mut expected = b"LICASH-CP\x02".to_vec();
        expected.extend(old);
        assert_eq!(cp.to_bytes(), expected);
        let restored = Checkpoint::from_bytes(&cp.to_bytes()).unwrap();
        assert_eq!(restored.commitment(), cp.commitment());
        assert_eq!(restored.profile(), profile);
        assert_eq!(restored.height(), 0);
        assert_eq!(restored.header(), cp.header());
    }
}

#[test]
fn the_signets_share_a_genesis_block_but_not_a_checkpoint() {
    let signet = Checkpoint::genesis(ChainProfile::Core30Signet).unwrap();
    let mutinynet = Checkpoint::genesis(ChainProfile::Mutinynet).unwrap();
    assert_eq!(signet.header(), mutinynet.header());
    assert_ne!(signet.commitment(), mutinynet.commitment());
    let at = b"LICASH-CP\x02".len();
    for (cp, tag) in [(&signet, 3), (&mutinynet, 4)] {
        let bytes = cp.to_bytes();
        assert_eq!(bytes[at], tag);
        let restored = Checkpoint::from_bytes(&bytes).unwrap();
        assert_eq!(restored.profile(), cp.profile());
        assert_eq!(restored.commitment(), cp.commitment());
        // A signet's genesis under mainnet's tag is refused.
        let mut changed = bytes.clone();
        changed[at] = 0;
        assert!(Checkpoint::from_bytes(&changed).is_err());
    }
    // Both retarget, from the genesis block's time on.
    let start = [&[1][..], &1_598_918_400u32.to_be_bytes()].concat();
    assert!(signet.to_bytes().ends_with(&start));
    assert!(mutinynet.to_bytes().ends_with(&start));
}

#[test]
fn checkpoint_codec_has_no_missing_timestamp_or_retarget_context_fallback() {
    let bytes = Checkpoint::genesis(ChainProfile::Core30Mainnet)
        .unwrap()
        .to_bytes();
    for n in 0..bytes.len() {
        assert!(Checkpoint::from_bytes(&bytes[..n]).is_err());
    }
    let at = b"LICASH-CP\x02".len();
    for field in 0..6 {
        let mut changed = bytes.clone();
        match field {
            0 => changed.push(0),
            1 => changed[0] ^= 1,
            2 => changed[at] = 2,
            3 => changed[at + 85..at + 89].copy_from_slice(&12u32.to_be_bytes()),
            4 => changed[at + 85..at + 89].fill(0),
            _ => {
                let n = changed.len();
                changed[n - 4] ^= 1;
            }
        }
        assert!(Checkpoint::from_bytes(&changed).is_err());
    }
}

fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
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
fn parts() -> Parts {
    let mut deposit = [31; 36];
    deposit[32..].copy_from_slice(&7u32.to_le_bytes());
    Parts {
        nonce: [53; 32],
        spend_root: point(17),
        claim_key: point(19).x_only(),
        keyset: IssuerKeys::random(2).unwrap().keyset().unwrap(),
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

fn prefix(base: &Base, bound: bool, named_deposit: bool) -> FrozenPrefix {
    prefix_variant(base, bound, named_deposit, None)
}

fn prefix_variant(
    base: &Base,
    bound: bool,
    named_deposit: bool,
    mismatch: Option<u8>,
) -> FrozenPrefix {
    use bitcoin::{
        absolute,
        consensus::serialize,
        hashes::Hash,
        secp256k1::{Secp256k1, XOnlyPublicKey},
        taproot::{LeafVersion, TaprootBuilder},
        transaction, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    };
    let p = base.parts();
    let alice = if mismatch == Some(0) {
        point(71).x_only()
    } else {
        p.parties[0].payment
    };
    let delay = p.graph.to_self_delay + u16::from(mismatch == Some(1));
    let value = p.graph.graph_value().unwrap() - u64::from(mismatch == Some(2));
    let digest = base.digest();
    let keys = Signer::derive(&[7; 32], &digest).public();
    let script = ScriptBuf::from_bytes(if bound {
        keys.request_leaf_for_base(&digest, &alice, &p.parties[1].payment, delay)
            .unwrap()
    } else {
        keys.request_leaf(&alice, &p.parties[1].payment, delay)
            .unwrap()
    });
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
    let leaf = if bound {
        RequestLeaf::for_base(digest, keys, alice, p.parties[1].payment, delay, control).unwrap()
    } else {
        RequestLeaf::new(keys, alice, p.parties[1].payment, delay, control).unwrap()
    };
    let od = base.deposit_outpoint();
    let f = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array(if named_deposit { od.txid } else { [37; 32] }),
                vout: od.index,
            },
            ..Default::default()
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: spk.clone(),
            },
            TxOut {
                value: Amount::from_sat(value),
                script_pubkey: spk.clone(),
            },
        ],
    };
    let r = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: f.compute_txid(),
                vout: 1,
            },
            sequence: Sequence::from_height(delay),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value - 10_000),
            script_pubkey: spk.clone(),
        }],
    };
    // A single-leaf fixture, intentionally not a complete BABE graph or UTXO
    // proof. This only characterizes the stateless Base/prefix boundary.
    FrozenPrefix::new(
        licash::chain::Transaction::parse(&serialize(&f), 100_000).unwrap(),
        licash::chain::Transaction::parse(&serialize(&r), 100_000).unwrap(),
        GraphOutput {
            index: 1,
            output: licash::chain::Output {
                value,
                script: spk.to_bytes(),
            },
        },
        GraphOutput {
            index: 0,
            output: licash::chain::Output {
                value: value - 10_000,
                script: spk.to_bytes(),
            },
        },
        leaf,
    )
    .unwrap()
}

#[test]
fn complete_base_roundtrips_with_explicit_keyset_checkpoint_and_fee_preimages() {
    let base = Base::new(parts()).unwrap();
    let bytes = base.to_bytes();
    assert!(bytes.len() < licash::base::MAX_BASE_BYTES);
    let restored = Base::from_bytes(&bytes).unwrap();
    assert_eq!(restored.to_bytes(), bytes);
    assert_eq!(restored.digest(), tagged_hash("licash/base/v2", &[&bytes]));
    assert_eq!(restored.parts().keyset.id(), base.parts().keyset.id());
    assert_eq!(
        restored.parts().chain.checkpoint.commitment(),
        base.parts().chain.checkpoint.commitment()
    );
    assert_eq!(restored.parts().fees, base.parts().fees);
    assert_eq!(restored.deposit_outpoint().index, 7);
    assert_eq!(restored.recovery_budget().unwrap().minimum_surplus(), 3_511);
    assert_eq!(format!("{restored:?}"), "Base(..)");
    for n in (0..128).chain(bytes.len() - 512..bytes.len()) {
        assert!(Base::from_bytes(&bytes[..n]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Base::from_bytes(&trailing).is_err());
    let mut flipped = bytes;
    flipped[0] ^= 1;
    assert!(Base::from_bytes(&flipped).is_err());
}

#[test]
fn every_agreed_base_field_changes_the_independent_key_context() {
    let original = parts();
    let digest = Base::new(original.clone()).unwrap().digest();
    for field in 0..54 {
        let mut p = original.clone();
        match field {
            0 => p.nonce[0] ^= 1,
            1 => p.spend_root = point(37),
            2 => p.claim_key = point(41).x_only(),
            3 => p.keyset = IssuerKeys::random(2).unwrap().keyset().unwrap(),
            4 => p.deposit[0] ^= 1,
            5 => p.cap -= 1,
            6 => {
                let mut keys = p.response_keys.keys().clone();
                keys.k.0[0][0] ^= 1;
                p.response_keys = ResponseKeys::new(keys).unwrap();
            }
            7 => p.parties[0].funding = point(43).x_only(),
            8 => p.parties[1].payment = point(47).x_only(),
            9 => p.parties[0].delayed = point(53).x_only(),
            10 => p.parties[0].payout_script[2] ^= 1,
            11 => p.graph.id += 1,
            12 => p.graph.bits += 1,
            13 => p.graph.low_bits += 1,
            14 => p.graph.deposit_units += 1,
            15 => p.graph.close_height += 1,
            16 => p.graph.reserve_alice += 1,
            17 => p.graph.reserve_hub += 1,
            18 => p.graph.to_self_delay += 1,
            19 => p.graph.commit_fee += 1,
            20 => p.graph.presign_fee += 1,
            21 => p.graph.delta += 1,
            22 => p.graph.delta_prime += 1,
            23 => p.graph.deadline_margin += 1,
            24 => p.graph.response_window += 1,
            25 => p.graph.assert_height += 1,
            26 => p.graph.hub_challenge += 1,
            27 => p.graph.holder_answer += 1,
            28 => p.recipe.holder_program[0] ^= 1,
            29 => p.recipe.counter_program[0] ^= 1,
            30 => p.recipe.full_instances += 1,
            31 => p.recipe.full_kept -= 1,
            32 => p.recipe.corrected_instances += 1,
            33 => p.recipe.corrected_kept -= 1,
            34 => p.chain.funding_depth += 1,
            35 => p.chain.publication_depth += 1,
            36 => p.chain.lookback += 1,
            37 => p.chain.observation_margin += 1,
            38 => p.chain.max_headers += 1,
            39 => p.limits.max_authorizations += 1,
            40 => p.limits.max_source_inputs += 1,
            41 => p.fees.sweep_fee_max += 1,
            42 => p.fees.external_fee_max += 1,
            43 => p.fees.max_rate_sat_per_kvb += 1,
            44 => p.fees.opening_fee_quote += 1,
            45 => p.chain.request_depth += 1,
            46 => p.limits.max_transaction_bytes += 1,
            47 => p.limits.max_publication_bytes += 1,
            48 => p.chain.checkpoint = Checkpoint::genesis(ChainProfile::Core30Mainnet).unwrap(),
            49 => p.parties[1].funding = point(59).x_only(),
            50 => p.parties[0].payment = point(61).x_only(),
            51 => p.parties[1].delayed = point(67).x_only(),
            52 => p.parties[1].payout_script[2] ^= 1,
            _ => p.deposit[32..].copy_from_slice(&8u32.to_le_bytes()),
        }
        let changed = Base::new(p).unwrap();
        assert_ne!(changed.digest(), digest, "field {field}");
        assert_eq!(
            Base::from_bytes(&changed.to_bytes()).unwrap().digest(),
            changed.digest()
        );
    }
}

#[test]
fn absent_or_unsupported_base_terms_are_rejected_not_defaulted() {
    for field in 0..17 {
        let mut p = parts();
        match field {
            0 => p.nonce = [0; 32],
            1 => p.spend_root = Point([0; 33]),
            2 => p.claim_key = [255; 32],
            3 => p.graph.unit_sats = 512,
            4 => p.graph.low_bits = 0,
            5 => p.graph.bits = 33,
            6 => p.graph.reserve_hub = 3_510,
            7 => p.graph.reserve_alice = u64::MAX,
            8 => p.graph.to_self_delay = 0,
            9 => p.graph.assert_height = p.graph.close_height,
            10 => p.chain.request_depth = 0,
            11 => p.chain.observation_margin = u32::MAX,
            12 => p.recipe.graph_version = 11,
            13 => p.recipe.full_kept = p.recipe.full_instances,
            14 => p.recipe.holder_program = [0; 32],
            15 => p.limits.max_authorizations = 31,
            _ => p.fees.max_rate_sat_per_kvb = 0,
        }
        assert!(Base::new(p).is_err(), "field {field}");
    }
}

#[test]
fn final_statement_binds_base_frozen_prefix_named_deposit_and_owner_consent() {
    let base = Base::new(parts()).unwrap();
    let prefix = prefix(&base, true, true);
    let digest = base.final_digest(&prefix).unwrap();
    assert_eq!(
        digest.as_bytes(),
        &tagged_hash(
            "licash/final-statement/v2",
            &[&base.digest(), &prefix.commitment()]
        )
    );
    let signature = crypto::schnorr_sign(
        &Scalar::from(17u64),
        &base.owner_consent_message(&prefix).unwrap(),
    )
    .unwrap();
    base.verify_owner_consent(&prefix, &signature).unwrap();
    assert!(base
        .verify_owner_consent(&prefix, &Signature([0; 64]))
        .is_err());
    assert_ne!(base.digest(), *digest.as_bytes());
    assert_eq!(format!("{digest:?}"), "FinalDigest(..)");
    let mut changed = base.parts().clone();
    changed.nonce[0] ^= 1;
    assert!(Base::new(changed).unwrap().final_digest(&prefix).is_err());
}

#[test]
fn unbound_or_differently_funded_prefixes_cannot_supply_final_statement_authority() {
    let base = Base::new(parts()).unwrap();
    assert!(base.final_digest(&prefix(&base, false, true)).is_err());
    assert!(base.final_digest(&prefix(&base, true, false)).is_err());
    // A fresh nonce is explicit local action, not a migration or a claim that
    // stale signed contexts can be silently re-keyed.
    assert_ne!(Base::fresh_nonce(), Base::fresh_nonce());
}

#[test]
fn a_matching_base_push_does_not_replace_party_csv_or_funding_value_checks() {
    let base = Base::new(parts()).unwrap();
    for mismatch in 0..3 {
        let candidate = prefix_variant(&base, true, true, Some(mismatch));
        assert_eq!(candidate.bound_base().unwrap(), &base.digest());
        assert_eq!(
            base.final_digest(&candidate).unwrap_err(),
            licash::Error::Protocol("native Base named-deposit or prefix terms mismatch".into())
        );
    }
}

/// The header budget counts from the checkpoint and must reach the assertion
/// deadline: a holder who recovers late in the contract's life still has to
/// show her request and claim body on a chain that both programs accept.
#[test]
fn header_budget_must_reach_the_assertion_deadline() {
    let mut p = parts();
    let span = p.graph.assert_height - p.chain.checkpoint.height();
    p.chain.max_headers = span;
    Base::new(p.clone()).unwrap();
    p.chain.max_headers = span - 1;
    assert!(Base::new(p).is_err());
}
