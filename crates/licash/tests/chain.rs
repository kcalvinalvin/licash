//! Synthetic blocks below test static header/commitment checks, not UTXO or
//! script consensus. Historical mainnet headers are independent real fixtures.

use bitcoin::{
    absolute,
    block::{Header, Version},
    consensus::{deserialize, serialize},
    hashes::{sha256, sha256d, Hash},
    pow::CompactTarget,
    transaction, Amount, Block, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
    Witness,
};
use licash::{
    chain::{self, Activation, ActivationRef, Chain, Checkpoint, Inclusion, WitnessInclusion},
    spend::{ChainProfile, TimedTerms},
};
use std::str::FromStr;

fn hash_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut bytes = a.to_vec();
    bytes.extend(b);
    sha256d::Hash::hash(&bytes).to_byte_array()
}

fn merkle(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    while hashes.len() > 1 {
        if !hashes.len().is_multiple_of(2) {
            hashes.push(*hashes.last().unwrap());
        }
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| hash_pair(&pair[0], &pair[1]))
            .collect();
    }
    hashes[0]
}

fn target(index: u8, activation: bool) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([index; 32]),
                vout: 1,
            },
            witness: Witness::from_slice(&[vec![index; 3], vec![0x51]]),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(if activation {
                chain::activation_script(&[83; 32])
            } else {
                vec![0x6a, 1, index]
            }),
        }],
    }
}

fn block(previous: &Header, height: u32, targets: Vec<Transaction>) -> Block {
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
    txdata.extend(targets);
    let mut wtxids: Vec<_> = txdata
        .iter()
        .map(|tx| tx.compute_wtxid().to_byte_array())
        .collect();
    wtxids[0] = [0; 32];
    let commitment = hash_pair(&merkle(wtxids), &reserved);
    let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    script.extend(commitment);
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
    remine(&mut block);
    block
}

fn remine(block: &mut Block) {
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block.header.nonce = 0;
    while block.header.validate_pow(block.header.target()).is_err() {
        block.header.nonce += 1;
    }
}

fn fixture(n: u8) -> (Checkpoint, Vec<Block>, Chain) {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    let mut previous: Header = deserialize(cp.header()).unwrap();
    let mut blocks = vec![];
    for height in 1..=3 {
        let targets = if height == 1 {
            (0..n).map(|i| target(i + 17, i == 0)).collect()
        } else {
            vec![]
        };
        let next = block(&previous, height, targets);
        previous = next.header;
        blocks.push(next);
    }
    let headers = blocks
        .iter()
        .map(|b| serialize(&b.header).try_into().unwrap())
        .collect();
    let chain = Chain::verify(cp.clone(), &cp.commitment(), headers, 3).unwrap();
    (cp, blocks, chain)
}

#[test]
fn real_mainnet_headers_cross_the_same_core30_retarget_boundary() {
    // Retained historical headers/context, from our own node; no runtime API.
    let anchor = hex::decode("0100000049c1daab3b6536ff1b2633c3a316a6e06ec287676cdeec4ca7baae6b00000000ac10b36b8f354b3353207de15940a5edbc05bb8364af75b4b5409e7823f2b48923ec3a4bffff001dbd5fa412").unwrap().try_into().unwrap();
    let next = hex::decode("010000004b0360d834a330ec7833e30e1f523ee05a0793361e29a73421964f980000000027b64a020af294e903feed93768705336a20090612a043f47af462a2f5e5b564f8ee3a4b6ad8001dd3a43707").unwrap().try_into().unwrap();
    let times = vec![
        1262146470, 1262146941, 1262149581, 1262149719, 1262149814, 1262150129, 1262150214,
        1262150242, 1262151952, 1262152015, 1262152739,
    ];
    let cp = Checkpoint::new(
        ChainProfile::Core30Mainnet,
        32_255,
        anchor,
        times,
        Some(1261130161),
    )
    .unwrap();
    let chain = Chain::verify(cp.clone(), &cp.commitment(), vec![next], 1).unwrap();
    assert_eq!(chain.tip_height(), 32_256);
    assert_eq!(
        chain.block_hash(32_256).unwrap(),
        BlockHash::from_str("000000004f2886a170adb7204cb0c7a824217dd24d11a74423d564c4e0904967")
            .unwrap()
            .to_byte_array()
    );
    assert_eq!(chain.confirmations(32_255), Some(2));
    assert_eq!(chain.confirmations(32_254), None);
    assert_eq!(chain.confirmations(32_257), None);
}

#[test]
fn checkpoints_and_header_limits_are_opening_context_not_prover_choices() {
    let (cp, blocks, _) = fixture(1);
    let headers: Vec<_> = blocks
        .iter()
        .map(|b| serialize(&b.header).try_into().unwrap())
        .collect();
    assert!(Chain::verify(cp.clone(), &[0; 32], headers.clone(), 3).is_err());
    assert!(Chain::verify(cp.clone(), &cp.commitment(), headers, 2).is_err());
    let h: Header = deserialize(cp.header()).unwrap();
    assert!(Checkpoint::new(cp.profile(), 0, *cp.header(), vec![], None).is_err());
    assert!(Checkpoint::new(cp.profile(), 0, *cp.header(), vec![h.time + 1], None).is_err());
    assert!(Checkpoint::new(cp.profile(), 0, *cp.header(), vec![h.time], Some(h.time)).is_err());
    let mut different = *cp.header();
    different[0] ^= 1;
    assert!(Checkpoint::new(cp.profile(), 0, different, vec![h.time], None).is_err());
}

#[test]
fn broken_ancestry_mtp_version_bits_and_pow_are_rejected() {
    let (cp, blocks, _) = fixture(1);
    for field in 0..5 {
        let mut b = blocks[0].clone();
        match field {
            0 => {
                b.header.prev_blockhash = BlockHash::all_zeros();
                remine(&mut b);
            }
            1 => {
                b.header.time = deserialize::<Header>(cp.header()).unwrap().time;
                remine(&mut b);
            }
            2 => {
                b.header.version = Version::from_consensus(3);
                remine(&mut b);
            }
            3 => {
                b.header.bits = CompactTarget::from_consensus(0x207ffffe);
                remine(&mut b);
            }
            _ => {
                while b.header.validate_pow(b.header.target()).is_ok() {
                    b.header.nonce += 1;
                }
            }
        }
        assert!(
            Chain::verify(
                cp.clone(),
                &cp.commitment(),
                vec![serialize(&b.header).try_into().unwrap()],
                1
            )
            .is_err(),
            "field {field}"
        );
    }
}

#[test]
fn detached_txid_and_witness_paths_work_for_odd_and_even_trees() {
    for n in 1..=6 {
        let (_, blocks, chain) = fixture(n);
        let raw = serialize(&blocks[0]);
        for index in 1..=usize::from(n) {
            let proof = Inclusion::capture(&raw, index, raw.len()).unwrap();
            let included = proof.verify(&chain, 1, 100_000).unwrap();
            assert_eq!(
                included.transaction().txid(),
                blocks[0].txdata[index].compute_txid().to_byte_array()
            );
            assert_eq!(included.height(), 1);
            assert_eq!(
                included.block_hash(),
                &blocks[0].block_hash().to_byte_array()
            );
            assert!(included.input_witness(0).is_err());
            let witness = WitnessInclusion::capture(&raw, index, raw.len()).unwrap();
            let included = witness.verify(&chain, 1, 100_000).unwrap();
            assert_eq!(
                included.input_witness(0).unwrap(),
                blocks[0].txdata[index].input[0].witness.to_vec()
            );
        }
    }
}

#[test]
fn witness_authentication_is_not_inferred_from_a_matching_txid_or_valid_body() {
    let (_, blocks, chain) = fixture(2);
    let raw = serialize(&blocks[0]);
    let proof = WitnessInclusion::capture(&raw, 1, raw.len()).unwrap();
    let txid = proof
        .non_witness
        .verify(&chain, 1, 100_000)
        .unwrap()
        .transaction()
        .txid();
    for field in 0..6 {
        let mut changed = proof.clone();
        match field {
            0 => {
                let mut tx: Transaction = deserialize(&changed.full_transaction).unwrap();
                tx.input[0].witness = Witness::from_slice(&[vec![99], vec![0x51]]);
                assert_eq!(tx.compute_txid().to_byte_array(), txid);
                changed.full_transaction = serialize(&tx);
            }
            1 => changed.reserved_value[0] ^= 1,
            2 => changed.witness_siblings[0][0] ^= 1,
            3 => changed.coinbase_witness_siblings[0][0] ^= 1,
            4 => {
                changed.witness_siblings.pop();
            }
            _ => changed.full_transaction.push(0),
        }
        assert!(changed.verify(&chain, 1, 100_000).is_err(), "field {field}");
    }
    assert!(proof
        .verify(&chain, 1, proof.full_transaction.len() - 1)
        .is_err());
}

#[test]
fn the_last_matching_coinbase_commitment_is_the_only_authoritative_one() {
    let (cp, mut blocks, _) = fixture(1);
    let valid = blocks[0].txdata[0].output.last().unwrap().clone();
    let mut wrong = valid.clone();
    let mut bytes = wrong.script_pubkey.as_bytes().to_vec();
    bytes[6] ^= 1;
    wrong.script_pubkey = ScriptBuf::from_bytes(bytes);
    blocks[0].txdata[0].output.push(wrong);
    remine(&mut blocks[0]);
    let chain = Chain::verify(
        cp.clone(),
        &cp.commitment(),
        vec![serialize(&blocks[0].header).try_into().unwrap()],
        1,
    )
    .unwrap();
    let raw = serialize(&blocks[0]);
    assert!(WitnessInclusion::capture(&raw, 1, raw.len())
        .unwrap()
        .verify(&chain, 1, 100_000)
        .is_err());
    // Appending a correct higher-index commitment (with permitted suffix)
    // restores the required BIP141 selection, not a first-match shortcut.
    let mut script = valid.script_pubkey.as_bytes().to_vec();
    script.extend([0x51, 0x51]);
    blocks[0].txdata[0].output.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(script),
    });
    remine(&mut blocks[0]);
    let chain = Chain::verify(
        cp.clone(),
        &cp.commitment(),
        vec![serialize(&blocks[0].header).try_into().unwrap()],
        1,
    )
    .unwrap();
    let raw = serialize(&blocks[0]);
    WitnessInclusion::capture(&raw, 1, raw.len())
        .unwrap()
        .verify(&chain, 1, 100_000)
        .unwrap();
}

#[test]
fn missing_coinbase_witness_commitment_is_rejected_after_valid_txid_inclusion() {
    let (cp, mut blocks, _) = fixture(1);
    blocks[0].txdata[0].output.pop();
    // Keep the stripped coinbase above the separate 64-byte-leaf guard so
    // the txid proof succeeds and this case reaches commitment selection.
    blocks[0].txdata[0].output[0].script_pubkey = ScriptBuf::from_bytes(vec![0x51, 0x51]);
    remine(&mut blocks[0]);
    let chain = Chain::verify(
        cp.clone(),
        &cp.commitment(),
        vec![serialize(&blocks[0].header).try_into().unwrap()],
        1,
    )
    .unwrap();
    let raw = serialize(&blocks[0]);
    let proof = WitnessInclusion::capture(&raw, 1, raw.len()).unwrap();
    proof.non_witness.verify(&chain, 1, 100_000).unwrap();
    assert!(proof
        .verify(&chain, 1, 100_000)
        .err()
        .unwrap()
        .to_string()
        .contains("witness commitment missing"));
}

#[test]
fn nonwitness_paths_enforce_coinbase_depth_index_body_and_resource_bounds() {
    let (_, blocks, chain) = fixture(3);
    let raw = serialize(&blocks[0]);
    let proof = Inclusion::capture(&raw, 1, raw.len()).unwrap();
    assert!(Inclusion::capture(&raw, 0, raw.len()).is_err());
    assert!(Inclusion::capture(&raw, 1, raw.len() - 1).is_err());
    for field in 0..7 {
        let mut changed = proof.clone();
        match field {
            0 => changed.index = 0,
            1 => changed.siblings = vec![[0; 32]; 32],
            2 => {
                changed.coinbase_siblings.pop();
            }
            3 => changed.siblings[0][0] ^= 1,
            4 => changed.coinbase = changed.transaction.clone(),
            5 => changed.transaction = serialize(&blocks[0].txdata[1]),
            _ => changed.transaction.push(0),
        }
        assert!(changed.verify(&chain, 1, 100_000).is_err());
    }
    assert!(proof.verify(&chain, 1, 0).is_err());
    assert!(proof.verify(&chain, 4, 100_000).is_err());
}

#[test]
fn ambiguous_64_byte_transaction_leaves_are_rejected() {
    let (cp, _, _) = fixture(1);
    let mut tx = target(17, false);
    tx.input[0].witness.clear();
    tx.output[0].script_pubkey = ScriptBuf::from_bytes(vec![0x6a, 2, 7, 8]);
    assert_eq!(serialize(&tx).len(), 64);
    let previous = deserialize(cp.header()).unwrap();
    let b = block(&previous, 1, vec![tx]);
    let chain = Chain::verify(
        cp.clone(),
        &cp.commitment(),
        vec![serialize(&b.header).try_into().unwrap()],
        1,
    )
    .unwrap();
    let raw = serialize(&b);
    assert!(Inclusion::capture(&raw, 1, raw.len())
        .unwrap()
        .verify(&chain, 1, 100_000)
        .is_err());
}

#[test]
fn activation_checks_real_inclusion_exact_reference_shared_context_and_mature_depth() {
    let (cp, blocks, chain) = fixture(1);
    let raw = serialize(&blocks[0]);
    let activation = Activation {
        reference: ActivationRef {
            height: 1,
            block_hash: blocks[0].block_hash().to_byte_array(),
            txid: blocks[0].txdata[1].compute_txid().to_byte_array(),
            output: 0,
        },
        inclusion: Inclusion::capture(&raw, 1, raw.len()).unwrap(),
    };
    let terms = TimedTerms::new(
        cp.profile(),
        cp.commitment(),
        4,
        3,
        sha256::Hash::hash(&[83; 32]).to_byte_array(),
    )
    .unwrap();
    assert_eq!(
        activation.verify(&chain, &terms, 100_000).unwrap(),
        [83; 32]
    );
    let shallow = Chain::verify(
        cp.clone(),
        &cp.commitment(),
        vec![serialize(&blocks[0].header).try_into().unwrap()],
        1,
    )
    .unwrap();
    assert!(activation.verify(&shallow, &terms, 100_000).is_err());
    for field in 0..4 {
        let mut changed = activation.clone();
        match field {
            0 => changed.reference.height = 2,
            1 => changed.reference.block_hash[0] ^= 1,
            2 => changed.reference.txid[0] ^= 1,
            _ => changed.reference.output = 1,
        }
        assert!(changed.verify(&chain, &terms, 100_000).is_err());
    }
    for (profile, checkpoint, expiry, hash) in [
        (
            ChainProfile::Core30Mainnet,
            cp.commitment(),
            4,
            *terms.hash(),
        ),
        (cp.profile(), [0; 32], 4, *terms.hash()),
        (cp.profile(), cp.commitment(), 1, *terms.hash()),
        (cp.profile(), cp.commitment(), 4, [0; 32]),
    ] {
        let bad = TimedTerms::new(profile, checkpoint, expiry, 3, hash).unwrap();
        assert!(activation.verify(&chain, &bad, 100_000).is_err());
    }
    assert_eq!(activation.reference.to_bytes().len(), 72);
}

#[test]
fn parsed_body_views_are_canonical_bytes_not_a_claim_of_inclusion() {
    let (_, blocks, chain) = fixture(1);
    let raw = serialize(&blocks[0]);
    let proof = Inclusion::capture(&raw, 1, raw.len()).unwrap();
    let view = chain::Transaction::parse(&proof.transaction, 100_000).unwrap();
    assert_eq!(view.version(), 2);
    assert_eq!(view.lock_time(), 0);
    assert!(!view.is_coinbase());
    assert_eq!(view.inputs()[0].previous.index, 1);
    assert_eq!(view.inputs()[0].previous.txid, [17; 32]);
    assert_eq!(view.outputs()[0].value, 0);
    assert_eq!(
        view.outputs()[0].script,
        chain::activation_script(&[83; 32])
    );
    assert_eq!(view.bytes(), proof.transaction);
    assert_eq!(
        proof
            .verify(&chain, 1, 100_000)
            .unwrap()
            .transaction()
            .txid(),
        view.txid()
    );
    assert!(chain::Transaction::parse(&serialize(&blocks[0].txdata[1]), 100_000).is_err());
}

#[test]
fn activation_scripts_read_back_their_secret_and_nothing_else() {
    let script = chain::activation_script(&[83; 32]);
    assert_eq!(chain::activation_secret(&script), Some([83; 32]));
    // Cut, lengthened, another push size or another magic.
    assert_eq!(chain::activation_secret(&script[..script.len() - 1]), None);
    let mut longer = script.clone();
    longer.push(0);
    assert_eq!(chain::activation_secret(&longer), None);
    for at in [0, 1, 2, 5] {
        let mut other = script.clone();
        other[at] ^= 1;
        assert_eq!(chain::activation_secret(&other), None, "byte {at}");
    }
    assert_eq!(chain::activation_secret(&[]), None);
}

fn header_bytes(hex: &str) -> chain::HeaderBytes {
    hex::decode(hex).unwrap().try_into().unwrap()
}

fn block_hash(hex: &str) -> [u8; 32] {
    BlockHash::from_str(hex).unwrap().to_byte_array()
}

#[test]
fn real_signet_headers_cross_a_retarget_boundary() {
    // Retained signet headers 330,623 to 330,625 and their context.
    let anchor = header_bytes("000000208a0ca8b84973fdee18044b1eb19d3cffa3c2232fe6a56dbdb6262dea01000000ca6b06c69db73f9a6a83dee3b7768537cf7c8fa26c28af7c5fc34fbcb8f10df598a1f66a85cd141d6fb2fe0b");
    let next = header_bytes("00000020bb759f12532ea29baa8eec23730707c348213e28159fe699818995e913000000d6f994beca0cb6de986df17f9379400b05eefab88907c2d0b67fb43bc12fec93c9a1f66aeefc141dc8c94f01");
    let after = header_bytes("00000020c7e10e6d41596d38c4af1f23a8ce2b0df64417a441c5bc29011178b10400000029b2d829883e9c476b4c2dd8ce455b2a8acf12926ba91919e68a3adfb37b29e92ea3f66aeefc141d78a5801c");
    let times = vec![
        1794544601, 1794544850, 1794545687, 1794545715, 1794545999, 1794546305, 1794546318,
        1794546575, 1794546815, 1794547290, 1794548120,
    ];
    let cp = Checkpoint::new(
        ChainProfile::Core30Signet,
        330623,
        anchor,
        times.clone(),
        Some(1793327751),
    )
    .unwrap();
    let chain = Chain::verify(cp.clone(), &cp.commitment(), vec![next, after], 2).unwrap();
    assert_eq!(chain.tip_height(), 330625);
    assert_eq!(
        chain.block_hash(330624).unwrap(),
        block_hash("00000004b178110129bcc541a41744f60d2bcea8231fafc4386d59416d0ee1c7")
    );
    assert_eq!(
        chain.block_hash(330625).unwrap(),
        block_hash("0000001127599805aaf9e61b9deba04a4d5f8104680c48d112a2d34447e637b4")
    );
    // Mutinynet retargets elsewhere, so the same headers are not its own.
    let mutinynet = Checkpoint::new(
        ChainProfile::Mutinynet,
        330623,
        anchor,
        times,
        Some(1793327751),
    )
    .unwrap();
    assert!(Chain::verify(mutinynet.clone(), &mutinynet.commitment(), vec![next], 1).is_err());
    assert_ne!(mutinynet.commitment(), cp.commitment());
}

#[test]
fn real_mutinynet_headers_retarget_only_every_40320_blocks() {
    // Retained Mutinynet headers 3,586,463 and 3,586,464, at a multiple of
    // 2016 but not of 40,320: its difficulty carries over.
    let anchor = header_bytes("0000002039bbefa2c7484456a7eb4ddcbeb140cee69d7c5e7e887d78aab77b7189000000af6ff38b93b3d2d26dd5b4f40b20b033d3529267c1774c571d962ae15915750d8cd8f66aae77031efb092000");
    let next = header_bytes("000000205e0759d3912f7df222e1d5a1047f58f3ab023e86858fc3bc0acc3a7973020000b8b6968870402d5442085f21fa9c9c05af6c079e28666959fac36778423980caaad8f66aae77031ef84abd01");
    let times = vec![
        1794561878, 1794561908, 1794561940, 1794561971, 1794562003, 1794562033, 1794562064,
        1794562095, 1794562126, 1794562156, 1794562188,
    ];
    let cp = Checkpoint::new(
        ChainProfile::Mutinynet,
        3586463,
        anchor,
        times.clone(),
        Some(1793372692),
    )
    .unwrap();
    let chain = Chain::verify(cp.clone(), &cp.commitment(), vec![next], 1).unwrap();
    assert_eq!(
        chain.block_hash(3586464).unwrap(),
        block_hash("000001792de8a87206454d4031e8596fda28e8ed12c891b20c95bdaec9baeb68")
    );
    let signet = Checkpoint::new(
        ChainProfile::Core30Signet,
        3586463,
        anchor,
        times,
        Some(1793372692),
    )
    .unwrap();
    assert!(Chain::verify(signet.clone(), &signet.commitment(), vec![next], 1).is_err());

    // Headers 40,319 and 40,320: its first retarget, capped at the limit.
    let anchor = header_bytes("000000206e0d32c507eaa50b61f5ed80824255b026adf4314eec10b26b12d6be0c0300000bb4526aef1c886d9678c52bbec3269d92fadf8d0437d2a5740bf972e636d5b09dac4d64ae77031e9b943e00");
    let next = header_bytes("0000002079d3d0745188d8678cb18dd669b7faad7ff387545ed400faf5bcc81697000000a9b56049418f1a284c1679d4fc04fb24943df05a188e9d7d87d848a1bfa3d4e7c0ac4d64ae77031edf9c7700");
    let times = vec![
        1682811719, 1682811751, 1682811787, 1682811821, 1682811853, 1682811883, 1682811914,
        1682811958, 1682811988, 1682812021, 1682812061,
    ];
    let cp = Checkpoint::new(
        ChainProfile::Mutinynet,
        40319,
        anchor,
        times,
        Some(1598918400),
    )
    .unwrap();
    let chain = Chain::verify(cp.clone(), &cp.commitment(), vec![next], 1).unwrap();
    assert_eq!(
        chain.block_hash(40320).unwrap(),
        block_hash("000001ed64fe75ba2de977a1a47000777824a237bb807a1a252e66d3527de09a")
    );
}

#[test]
fn chains_are_told_apart_by_genesis_and_the_signets_by_block_1() {
    let genesis = |network| {
        bitcoin::blockdata::constants::genesis_block(network)
            .block_hash()
            .to_byte_array()
    };
    let signet_1 = block_hash("00000086d6b2636cb2a392d45edc4ec544a10024d30141c9adf4bfd9de533b53");
    let mutinynet_1 =
        block_hash("000002855893a0a9b24eaffc5efc770558a326fee4fc10c9da22fc19cd2954f9");
    let signet = genesis(bitcoin::Network::Signet);
    for (genesis, first, profile) in [
        (
            genesis(bitcoin::Network::Bitcoin),
            None,
            Some(ChainProfile::Core30Mainnet),
        ),
        (
            genesis(bitcoin::Network::Regtest),
            None,
            Some(ChainProfile::Core30DefaultRegtest),
        ),
        (signet, Some(signet_1), Some(ChainProfile::Core30Signet)),
        (signet, Some(mutinynet_1), Some(ChainProfile::Mutinynet)),
        // Another signet, or one not yet past genesis.
        (signet, Some([7; 32]), None),
        (signet, None, None),
        (genesis(bitcoin::Network::Testnet4), None, None),
    ] {
        assert_eq!(ChainProfile::identify(&genesis, first.as_ref()), profile);
    }
    for profile in ChainProfile::ALL {
        assert_eq!(profile.name().parse::<ChainProfile>().unwrap(), profile);
        assert_eq!(profile.is_test(), profile != ChainProfile::Core30Mainnet);
    }
    assert!("testnet".parse::<ChainProfile>().is_err());
}
