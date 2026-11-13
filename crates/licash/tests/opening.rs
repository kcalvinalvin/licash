//! Local structural contracts, not valid UTXO history or opening admission.
use bitcoin::{
    absolute,
    consensus::serialize,
    hashes::Hash,
    secp256k1::{Keypair, Secp256k1, SecretKey, XOnlyPublicKey},
    taproot::{LeafVersion, TaprootBuilder},
    transaction, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
};
use licash::{
    chain::{self, Output},
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    request::Signer,
};

#[derive(Clone)]
struct Fixture {
    f: Transaction,
    r: Transaction,
    fg: GraphOutput,
    rg: GraphOutput,
    leaf: RequestLeaf,
}
fn party(byte: u8) -> [u8; 32] {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[byte; 32]).unwrap(),
    )
    .x_only_public_key()
    .0
    .serialize()
}
fn fixture() -> Fixture {
    fixture_with_delay(2)
}
fn fixture_with_delay(delay: u16) -> Fixture {
    fixture_for_base(delay, None)
}
fn fixture_for_base(delay: u16, base: Option<[u8; 32]>) -> Fixture {
    let keys = Signer::derive(&[7; 32], &[11; 32]).public();
    let script = ScriptBuf::from_bytes(match base {
        Some(base) => keys
            .request_leaf_for_base(&base, &party(17), &party(19), delay)
            .unwrap(),
        None => keys.request_leaf(&party(17), &party(19), delay).unwrap(),
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
    let leaf = match base {
        Some(base) => {
            RequestLeaf::for_base(base, keys, party(17), party(19), delay, control).unwrap()
        }
        None => RequestLeaf::new(keys, party(17), party(19), delay, control).unwrap(),
    };
    let fg = GraphOutput {
        index: 1,
        output: Output {
            value: 100_000,
            script: spk.to_bytes(),
        },
    };
    let rg = GraphOutput {
        index: 1,
        output: Output {
            value: 90_000,
            script: spk.to_bytes(),
        },
    };
    let f = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([21; 32]),
                vout: 2,
            },
            ..Default::default()
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: spk.clone(),
            },
            TxOut {
                value: Amount::from_sat(fg.output.value),
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
                vout: fg.index,
            },
            sequence: Sequence::from_consensus(u32::from(delay)),
            ..Default::default()
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(500),
                script_pubkey: spk.clone(),
            },
            TxOut {
                value: Amount::from_sat(rg.output.value),
                script_pubkey: spk,
            },
        ],
    };
    Fixture { f, r, fg, rg, leaf }
}
impl Fixture {
    fn bind(self) -> Result<FrozenPrefix, licash::Error> {
        FrozenPrefix::new(
            chain::Transaction::parse(&serialize(&self.f), 100_000)?,
            chain::Transaction::parse(&serialize(&self.r), 100_000)?,
            self.fg,
            self.rg,
            self.leaf,
        )
    }
}

#[test]
fn actual_graph_vouts_values_scripts_and_control_are_pinned() {
    let fixture = fixture();
    let prefix = fixture.clone().bind().unwrap();
    assert_eq!(
        prefix.funding_outpoint().txid,
        fixture.f.compute_txid().to_byte_array()
    );
    assert_eq!(prefix.funding_outpoint().index, 1);
    assert_eq!(
        prefix.request_outpoint().txid,
        fixture.r.compute_txid().to_byte_array()
    );
    assert_eq!(prefix.request_outpoint().index, 1);
    assert_eq!(prefix.funding().bytes(), serialize(&fixture.f));
    assert_eq!(prefix.request().bytes(), serialize(&fixture.r));
    assert_eq!(prefix.funding_graph(), &fixture.fg);
    assert_eq!(prefix.request_graph(), &fixture.rg);
    assert_eq!(prefix.leaf().alice(), &party(17));
    assert_eq!(prefix.leaf().hub(), &party(19));
    assert_eq!(prefix.leaf().delay(), 2);
    assert_eq!(prefix.leaf().script(), fixture.leaf.script());
    assert_eq!(prefix.leaf().control(), fixture.leaf.control());
    assert_eq!(prefix.leaf().keys(), fixture.leaf.keys());
    assert_eq!(format!("{prefix:?}"), "FrozenPrefix(..)");
}

#[test]
fn expected_output_identity_never_defaults_to_zero_or_payload_selected_terms() {
    for field in 0..6 {
        let mut f = fixture();
        match field {
            0 => f.fg.index = 0,
            1 => f.rg.index = 0,
            2 => f.fg.output.value += 1,
            3 => f.rg.output.value += 1,
            4 => f.fg.output.script[2] ^= 1,
            _ => f.rg.output.script[2] ^= 1,
        }
        assert!(f.bind().is_err());
    }
}

#[test]
fn request_must_spend_the_exact_funding_outpoint_with_no_other_inputs() {
    for field in 0..5 {
        let mut f = fixture();
        match field {
            0 => f.r.input[0].previous_output.vout = 0,
            1 => f.r.input[0].previous_output.txid = Txid::from_byte_array([23; 32]),
            2 => f.r.input.push(f.r.input[0].clone()),
            3 => f.r.input[0].script_sig = ScriptBuf::from_bytes(vec![0]),
            _ => f.f.lock_time = absolute::LockTime::from_consensus(1),
        }
        assert!(f.bind().is_err());
    }
}

#[test]
fn csv_sequence_and_single_input_value_conservation_are_checked() {
    for field in 0..6 {
        let mut f = fixture();
        match field {
            0 => f.r.version = transaction::Version::ONE,
            1 => f.r.input[0].sequence = Sequence::from_consensus(1 << 31 | 2),
            2 => f.r.input[0].sequence = Sequence::from_consensus(1 << 22 | 2),
            3 => f.r.input[0].sequence = Sequence::from_consensus(1),
            4 => f.r.output[0].value = Amount::from_sat(20_000),
            _ => f.r.input[0].sequence = Sequence::from_consensus(3),
        }
        assert!(f.bind().is_err());
    }
}

#[test]
fn zero_delay_does_not_hide_an_active_relative_lock() {
    let mut f = fixture_with_delay(0);
    f.r.input[0].sequence = Sequence::from_consensus(0xffff);
    assert!(f.bind().is_err());
    for (version, sequence) in [(2, 0), (2, 0xffff_fffd), (1, 0xffff), (-1, 0)] {
        let mut f = fixture_with_delay(0);
        f.r.version = transaction::Version(version);
        f.r.input[0].sequence = Sequence::from_consensus(sequence);
        f.bind().unwrap();
    }
}

#[test]
fn funding_preparation_shape_is_not_mistaken_for_confirmed_native_inputs() {
    for field in 0..3 {
        let mut f = fixture();
        match field {
            0 => f.f.input[0].script_sig = ScriptBuf::from_bytes(vec![0]),
            1 => f.f.input.push(f.f.input[0].clone()),
            _ => f.f.input[0].previous_output = OutPoint::null(),
        }
        assert!(f.bind().is_err());
    }
    // A valid shape is accepted without UTXO/confirmation claims. Preparation
    // and full opening admission must still verify the actual previous coins.
    fixture().bind().unwrap();
}

#[test]
fn wrong_keys_or_control_paths_cannot_relabel_the_funding_root() {
    let original = fixture();
    let mut wrong = original.clone();
    let mut control = wrong.leaf.control().to_vec();
    control[0] ^= 1;
    wrong.leaf =
        RequestLeaf::new(wrong.leaf.keys().clone(), party(17), party(19), 2, control).unwrap();
    assert!(wrong.bind().is_err());
    let mut wrong = original;
    wrong.leaf = RequestLeaf::new(
        Signer::derive(&[7; 32], &[12; 32]).public(),
        party(17),
        party(19),
        2,
        wrong.leaf.control().to_vec(),
    )
    .unwrap();
    assert!(wrong.bind().is_err());
}

#[test]
fn control_blocks_require_tapscript_nums_and_exact_shape() {
    let f = fixture();
    for control in [vec![], vec![0; 33], vec![0; 34]] {
        assert!(RequestLeaf::new(f.leaf.keys().clone(), party(17), party(19), 2, control).is_err());
    }
    let mut control = f.leaf.control().to_vec();
    control[0] = (control[0] & 1) | 0xc2;
    assert!(RequestLeaf::new(f.leaf.keys().clone(), party(17), party(19), 2, control).is_err());
    let mut control = f.leaf.control().to_vec();
    control[1..33].copy_from_slice(&party(17));
    assert!(RequestLeaf::new(f.leaf.keys().clone(), party(17), party(19), 2, control).is_err());
}

#[test]
fn prefix_commitment_binds_complete_bodies_not_only_graph_outputs() {
    let f = fixture();
    let original = f.clone().bind().unwrap().commitment();
    let mut changed = f.clone();
    changed.r.lock_time = absolute::LockTime::from_consensus(1);
    assert_ne!(changed.bind().unwrap().commitment(), original);
    let mut changed = f.clone();
    changed.rg.index = 0;
    changed.rg.output.value = 500;
    assert_ne!(changed.bind().unwrap().commitment(), original);
    let mut changed = f;
    changed.f.output[0].value = Amount::from_sat(2_000);
    changed.r.input[0].previous_output.txid = changed.f.compute_txid();
    assert_ne!(changed.bind().unwrap().commitment(), original);
}

#[test]
fn full_opening_base_access_is_explicit_and_unbound_carriers_fail_closed() {
    assert!(fixture().bind().unwrap().bound_base().is_err());
    let base = [43; 32];
    let prefix = fixture_for_base(2, Some(base)).bind().unwrap();
    assert_eq!(prefix.bound_base().unwrap(), &base);
    assert_ne!(prefix.commitment(), fixture().bind().unwrap().commitment());
    // All-zero bytes are an explicit opaque value, not a missing-field default.
    // The future Base-preimage predicate, not this accessor, proves its meaning.
    assert_eq!(
        fixture_for_base(2, Some([0; 32]))
            .bind()
            .unwrap()
            .bound_base()
            .unwrap(),
        &[0; 32]
    );
}

#[test]
fn relabeling_base_metadata_does_not_match_the_existing_funding_control_root() {
    let mut f = fixture_for_base(2, Some([43; 32]));
    f.leaf = RequestLeaf::for_base(
        [47; 32],
        f.leaf.keys().clone(),
        party(17),
        party(19),
        2,
        f.leaf.control().to_vec(),
    )
    .unwrap();
    assert!(f.bind().is_err());
}
