//! BABE's graph (`licash_regtest::graph`): the pre-signed set as both parties
//! build it, pinned; and each way a dispute ends, played on regtest under
//! mainnet's relay policy, with the real reveal and a stand-in for the
//! garbling (Alice holds a kept instance's `msg` when her proof is true).
//!
//! Preserved from Lichen d01bb5b plus the committed Licash adapter (MIT).
//! The pre-existing pinned tests must survive downstream ownership unchanged.

use std::collections::HashMap;

use bitcoincore_rpc::json::{AddressType, SignRawTransactionInput};
use bitcoincore_rpc::RpcApi;
use licash_regtest::graph::{Babe, Corrected, ExitKeys, Publication, Spec, Windows};
use lichen_babe::reveal::{
    check_presigs, complete, extract, points, reveal_args, AdaptorKeys, LamportKey,
};
use lichen_babe::setup::{Hash, Secret, DIGITS};
use lichen_babe::vsss::{deal, Dealer};
use lngap_btc::bitcoin::hashes::{sha256, Hash as _};
use lngap_btc::bitcoin::{Amount, OutPoint, Transaction, TxIn, Txid};
use lngap_btc::keys::Seed;
use lngap_btc::regtest::Regtest;
use lngap_btc::witness::witness_args_consumption_order;
use lngap_channel::{ChannelParams, PartyKeys, Role};
use lngap_lamport::winternitz::{Wots256Secret, WotsParams, WotsSecret, WotsSig};
use lngap_sessions::session::Terms;
use lngap_sessions::skeleton::{sign_all, Presign, Signed};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

// The holder graph's K witness width, not an upstream default.
const K_LEN: usize = 128;
const POSTED_ITEMS: usize = 15;

/// Both parties' secrets for a contract, and the hub's dealing.
struct Parties {
    alice: PartyKeys,
    hub: PartyKeys,
    b_key: WotsSecret,
    hub_b: Wots256Secret,
    lamport: LamportKey,
    adaptor: AdaptorKeys,
    msgs: Vec<[u8; 32]>,
    dealer: Dealer,
    dealing: lichen_babe::vsss::Dealing,
    exit: Exit,
    corrected: CorrectedKeys,
}

/// The corrected exit's second set: the hub's key for `b_corr`, Alice's,
/// her Lamport and adaptor keys, the kept instances' msgs, the dealing.
struct CorrectedKeys {
    hub_b: Wots256Secret,
    b_key: WotsSecret,
    lamport: LamportKey,
    adaptor: AdaptorKeys,
    msgs: Vec<[u8; 32]>,
    dealer: Dealer,
    dealing: lichen_babe::vsss::Dealing,
}

/// The exit's one-time keys: Alice's for `E`, the hub's for `E` in each
/// leaf and for `K`.
struct Exit {
    e: WotsSecret,
    plain: Wots256Secret,
    counter: Wots256Secret,
    k: Wots256Secret,
    /// For an exit whose counter is data: the key for its 32-byte digest.
    k32: Wots256Secret,
}

/// Items of counter data in [`data_exit_spec`].
const COUNTER_ITEMS: usize = 20;

fn parties(label: &str) -> Parties {
    let rng = &mut ChaCha20Rng::from_seed(sha256::Hash::hash(label.as_bytes()).to_byte_array());
    let a = Seed::from_label(&format!("{label}/alice"));
    let (dealer, dealing) = deal(DIGITS, 2, rng);
    // its own randomness, so the first set's is that of a graph without it
    let rng2 = &mut ChaCha20Rng::from_seed(
        sha256::Hash::hash(format!("{label}/corrected").as_bytes()).to_byte_array(),
    );
    let a2 = Seed::from_label(&format!("{label}/alice/corrected"));
    let (dealer2, dealing2) = deal(DIGITS, 2, rng2);
    let corrected = CorrectedKeys {
        hub_b: Wots256Secret::from_entropy(
            WotsParams::for_bytes(4),
            Seed::from_label(&format!("{label}/hub")).derive_bytes("b_corr"),
        ),
        b_key: WotsSecret::from_entropy(WotsParams::for_bytes(4), a2.derive_bytes("b")),
        lamport: LamportKey::new(&a2, DIGITS),
        adaptor: AdaptorKeys::new(&a2, DIGITS),
        msgs: (0..2)
            .map(|_| core::array::from_fn(|_| rng2.next_u32() as u8))
            .collect(),
        dealer: dealer2,
        dealing: dealing2,
    };
    Parties {
        alice: PartyKeys::from_seed(Role::User, a.clone()),
        hub: PartyKeys::from_seed(Role::Hub, Seed::from_label(&format!("{label}/hub"))),
        b_key: WotsSecret::from_entropy(WotsParams::for_bytes(4), a.derive_bytes("b")),
        hub_b: Wots256Secret::from_entropy(
            WotsParams::for_bytes(4),
            Seed::from_label(&format!("{label}/hub")).derive_bytes("b"),
        ),
        lamport: LamportKey::new(&a, DIGITS),
        adaptor: AdaptorKeys::new(&a, DIGITS),
        msgs: (0..3)
            .map(|_| core::array::from_fn(|_| rng.next_u32() as u8))
            .collect(),
        dealer,
        dealing,
        exit: Exit {
            e: WotsSecret::from_entropy(WotsParams::for_bytes(32), a.derive_bytes("e")),
            plain: Wots256Secret::from_entropy(
                WotsParams::for_bytes(32),
                Seed::from_label(&format!("{label}/hub")).derive_bytes("e_plain"),
            ),
            counter: Wots256Secret::from_entropy(
                WotsParams::for_bytes(32),
                Seed::from_label(&format!("{label}/hub")).derive_bytes("e_counter"),
            ),
            k: Wots256Secret::from_entropy(
                WotsParams::for_bytes(K_LEN as u32),
                Seed::from_label(&format!("{label}/hub")).derive_bytes("k"),
            ),
            k32: Wots256Secret::from_entropy(
                WotsParams::for_bytes(32),
                Seed::from_label(&format!("{label}/hub")).derive_bytes("k32"),
            ),
        },
        corrected,
    }
}

/// [`exit_spec`] with the corrected exit.
fn corrected_spec(p: &Parties, t_close: u32, t_a: u32) -> Spec {
    let c = &p.corrected;
    Spec {
        corrected: Some(Corrected {
            hub_b: c.hub_b.public(),
            b_key: c.b_key.public(),
            lamport: c.lamport.public(),
            adaptor: c.adaptor.public(),
            hmsg: c
                .msgs
                .iter()
                .map(|m| sha256::Hash::hash(m).to_byte_array())
                .collect(),
        }),
        ..exit_spec(p, t_close, t_a)
    }
}

/// [`spec`] with the exit.
fn exit_spec(p: &Parties, t_close: u32, t_a: u32) -> Spec {
    let x = &p.exit;
    Spec {
        exit: Some(ExitKeys {
            e_key: x.e.public(),
            hub_e_plain: x.plain.public(),
            hub_e_counter: x.counter.public(),
            hub_k: x.k.public(),
            counter_items: 0,
        }),
        ..spec(p, t_close, t_a)
    }
}

/// [`spec`] with an exit whose counter is data below a 32-byte `K`.
fn data_exit_spec(p: &Parties, t_close: u32, t_a: u32) -> Spec {
    let x = &p.exit;
    Spec {
        exit: Some(ExitKeys {
            e_key: x.e.public(),
            hub_e_plain: x.plain.public(),
            hub_e_counter: x.counter.public(),
            hub_k: x.k32.public(),
            counter_items: COUNTER_ITEMS,
        }),
        ..spec(p, t_close, t_a)
    }
}

#[test]
fn publication_keys_and_explicit_request_budgets_are_checked() {
    let p = parties("publication-budget-checks");
    let babe = Babe::new(exit_spec(&p, 200, 100)).unwrap();
    let seed = Seed::from_label("publication-budget-keys");
    let keys = core::array::from_fn(|i| {
        Wots256Secret::from_entropy(
            WotsParams::for_bytes(if i == 4 { 128 } else { 224 }),
            seed.derive_bytes(&format!("chunk-{i}")),
        )
        .public()
    });
    assert!(babe.publication_trees(&keys).is_ok());
    let mut short = keys.clone();
    short[0].digits.pop();
    assert!(babe.publication_trees(&short).is_err());
    let b = p.b_key.sign(&1u32.to_be_bytes()).unwrap();
    let e = p.exit.e.sign(&[7; 32]).unwrap();
    let posted = vec![42; 1281];
    assert!(Babe::request_exit_wire(&b, &e, &posted).is_err());
    assert!(Babe::new(exit_spec(&p, 200, 100))
        .unwrap()
        .with_request_items(0)
        .is_err());
    assert!(Babe::new(exit_spec(&p, 200, 100))
        .unwrap()
        .with_request_items(801)
        .is_err());
    let larger = babe.with_request_items(24).unwrap();
    let wire = larger.request_exit_wire_for(&b, &e, &posted).unwrap();
    let args: Vec<_> = wire.into_iter().rev().collect();
    let (_, _, decoded) = Babe::read_request_exit_with_items(&args, 24).unwrap();
    assert_eq!(&decoded[..posted.len()], &posted);
    assert!(larger
        .request_exit_wire_for(&b, &e, &[0; 24 * 80 + 1])
        .is_err());
}

fn spec(p: &Parties, t_close: u32, t_a: u32) -> Spec {
    Spec {
        terms: Terms {
            id: 7,
            unit: Amount::from_sat(1),
            bits: 21,
            low_bits: 9,
            deposit: 500_000,
            t_close,
            reserve_alice: Amount::from_sat(20_000),
            reserve_hub: Amount::from_sat(100_000),
        },
        params: ChannelParams::regtest(Amount::from_sat(20_000_000)),
        pubs: [p.alice.public(), p.hub.public()],
        b_key: p.b_key.public(),
        hub_b: p.hub_b.public(),
        lamport: p.lamport.public(),
        adaptor: p.adaptor.public(),
        hmsg: p
            .msgs
            .iter()
            .map(|m| sha256::Hash::hash(m).to_byte_array())
            .collect::<Vec<Hash>>(),
        windows: Windows {
            w_h: 6,
            t_a,
            delta2: 12,
            delta1: 6,
        },
        exit: None,
        corrected: None,
    }
}

/// Both parties' signatures, Alice's on `wrongly_challenged` kept apart.
fn sign(set: &[Presign], p: &Parties) -> (Signed, HashMap<String, Vec<u8>>) {
    let mut alice = sign_all(set, &p.alice.payment).unwrap();
    let kept = alice
        .iter()
        .filter(|(n, _)| Babe::withheld(n))
        .map(|(n, s)| (n.clone(), s.clone()))
        .collect();
    alice.retain(|n, _| !Babe::withheld(n));
    (
        Signed {
            hub: sign_all(set, &p.hub.payment).unwrap(),
            alice,
        },
        kept,
    )
}

#[test]
fn the_presigned_set() {
    let p = parties("babe/set");
    let babe = Babe::new(spec(&p, 1_000, 900)).unwrap();
    let set = babe
        .presign(OutPoint {
            txid: Txid::all_zeros(),
            vout: 0,
        })
        .unwrap();
    let names: Vec<&str> = set.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "default",
            "request",
            "request_timeout",
            "respond",
            "respond_timeout",
            "assert",
            "success",
            "challenge_assert",
            "failure",
            "wrongly_challenged_0",
            "wrongly_challenged_1",
            "wrongly_challenged_2"
        ]
    );
    // the chain: each link spends the previous one's output 0
    let links: Vec<&Presign> = set.iter().filter(|x| x.link).collect();
    assert_eq!(
        links.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        ["request", "respond", "assert", "challenge_assert"]
    );
    for w in links.windows(2) {
        assert_eq!(
            w[1].tx.input[0].previous_output,
            OutPoint {
                txid: w[0].tx.compute_txid(),
                vout: 0
            },
            "{} spends {}",
            w[1].name,
            w[0].name
        );
    }
    for x in &set {
        assert_eq!(
            x.tx.version.0,
            if Babe::withheld(&x.name) { 3 } else { 2 },
            "{}: TRUC only against the challenge",
            x.name
        );
    }
    // the floors, out of the hub's reserve
    let mut prev = babe.value();
    for l in &links {
        println!(
            "{}: floor {} sat",
            l.name,
            (prev - l.tx.output[0].value - l.tx.output[1].value).to_sat()
        );
        prev = l.tx.output[0].value;
    }
    assert!(
        babe.value() - prev < babe.spec.terms.reserve_hub,
        "the floors and anchors fit the hub's reserve"
    );
    // both sign and check; Alice keeps hers on wrongly_challenged
    let (signed, kept) = sign(&set, &p);
    assert_eq!(kept.len(), 3);
    let shown: Vec<Presign> = set
        .iter()
        .filter(|x| !Babe::withheld(&x.name))
        .cloned()
        .collect();
    signed.verify(&shown, &babe.spec.pubs).unwrap();
    for (n, s) in &kept {
        set.iter()
            .find(|x| &x.name == n)
            .unwrap()
            .verify(&p.alice.public().payment, s)
            .unwrap();
    }
    // the fingerprint: the trees and every pre-signed transaction
    let mut h = sha256::Hash::engine();
    use lngap_btc::bitcoin::hashes::HashEngine;
    for t in [
        babe.tree(),
        babe.requested(),
        babe.responded(),
        babe.asserted(),
        babe.challenged(),
    ] {
        h.input(t.unwrap().script_pubkey().as_bytes());
    }
    for x in &set {
        h.input(x.name.as_bytes());
        h.input(&lngap_btc::bitcoin::consensus::serialize(&x.tx));
        h.input(x.leaf.as_bytes());
        h.input(&x.control.serialize());
    }
    let fp = sha256::Hash::from_engine(h);
    println!("fingerprint: {} pre-signed, {fp}", set.len());
    assert_eq!(
        format!("{} pre-signed, {}", set.len(), &fp.to_string()[..16]),
        "12 pre-signed, 6b64a4fcb7f5b664"
    );
}

// ----- on regtest -----

// Prefix characterization only. It does not change the current opening
// digest, request-key derivation or accepted proof statement.
#[test]
fn instance_hashlocks_leave_whole_and_corrected_request_prefixes_unchanged() {
    let p = parties("opening-prefix/hashlocks");
    let funding = OutPoint {
        txid: Txid::from_byte_array([0x72; 32]),
        vout: 2,
    };
    for spec in [exit_spec(&p, 1_000, 900), corrected_spec(&p, 1_000, 900)] {
        let original = Babe::new(spec.clone())
            .unwrap()
            .with_request_items(200)
            .unwrap();
        let before: HashMap<_, _> = original
            .presign(funding)
            .unwrap()
            .into_iter()
            .map(|tx| (tx.name.clone(), tx))
            .collect();
        for corrected in [false, true] {
            let mut changed = spec.clone();
            if corrected {
                let Some(second) = changed.corrected.as_mut() else {
                    continue;
                };
                second.hmsg[0][0] ^= 1;
            } else {
                changed.hmsg[0][0] ^= 1;
            }
            let changed = Babe::new(changed).unwrap().with_request_items(200).unwrap();
            assert_eq!(
                original.tree().unwrap().script_pubkey(),
                changed.tree().unwrap().script_pubkey()
            );
            let after: HashMap<_, _> = changed
                .presign(funding)
                .unwrap()
                .into_iter()
                .map(|tx| (tx.name.clone(), tx))
                .collect();
            for name in ["request", "respond", "assert"] {
                assert_eq!(before[name].tx, after[name].tx, "stable prefix {name}");
            }
            let affected = if corrected {
                "challenge_corrected"
            } else {
                "challenge_assert"
            };
            assert_ne!(before[affected].tx, after[affected].tx);
        }
    }
}

// Funding witnesses are created only after the final graph passes the prefix
// check. This test does not simulate durable reservations or a long setup.
#[test]
fn native_segwit_funding_can_reserve_a_request_identity_before_setup() -> anyhow::Result<()> {
    use bitcoincore_rpc::json::FundRawTransactionOptions;
    use lngap_btc::bitcoin::{absolute, transaction, TxOut};

    let rt = Regtest::start_standard()?;
    let p = parties("opening-prefix/funding");
    for corrected in [false, true] {
        let address = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))?
            .assume_checked();
        let (deposit, _) = rt.fund(&address.script_pubkey(), Amount::from_sat(500_000))?;
        let height = rt.height()?;
        let final_spec = if corrected {
            corrected_spec(&p, height + 1_000, height + 900)
        } else {
            exit_spec(&p, height + 1_000, height + 900)
        };
        let mut placeholder = final_spec.clone();
        for (i, hashlock) in placeholder.hmsg.iter_mut().enumerate() {
            *hashlock = sha256::Hash::hash(&[0x70, i as u8]).to_byte_array();
        }
        if let Some(second) = placeholder.corrected.as_mut() {
            for (i, hashlock) in second.hmsg.iter_mut().enumerate() {
                *hashlock = sha256::Hash::hash(&[0x71, i as u8]).to_byte_array();
            }
        }
        let prefix = Babe::new(placeholder)?.with_request_items(200)?;
        let output = TxOut {
            value: prefix.value(),
            script_pubkey: prefix.tree()?.script_pubkey(),
        };
        let skeleton = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: deposit,
                ..Default::default()
            }],
            output: vec![output.clone()],
        };
        let funding = rt
            .rpc
            .fund_raw_transaction(
                &skeleton,
                Some(&FundRawTransactionOptions {
                    add_inputs: Some(true),
                    lock_unspents: Some(true),
                    ..Default::default()
                }),
                None,
            )?
            .transaction()?;
        assert!(funding
            .input
            .iter()
            .any(|input| input.previous_output == deposit));
        assert!(funding
            .input
            .iter()
            .all(|input| input.script_sig.is_empty() && input.witness.is_empty()));
        #[derive(serde::Deserialize)]
        struct LockedInput {
            txid: Txid,
            vout: u32,
        }
        let locked: Vec<LockedInput> = rt.rpc.call("listlockunspent", &[])?;
        for input in &funding.input {
            assert!(
                locked.iter().any(|coin| OutPoint {
                    txid: coin.txid,
                    vout: coin.vout
                } == input.previous_output),
                "every selected input is reserved"
            );
            let prevout = rt
                .rpc
                .get_tx_out(
                    &input.previous_output.txid,
                    input.previous_output.vout,
                    Some(true),
                )?
                .expect("confirmed reserved funding input");
            assert!(prevout.confirmations > 0);
            let script = prevout.script_pub_key.script()?;
            assert!(
                script.is_p2wpkh() || script.is_p2wsh() || script.is_p2tr(),
                "native witness inputs only"
            );
        }
        let root = OutPoint {
            txid: funding.compute_txid(),
            vout: u32::try_from(
                funding
                    .output
                    .iter()
                    .position(|o| o == &output)
                    .expect("exact graph output"),
            )?,
        };
        let prepared_r = prefix
            .presign(root)?
            .into_iter()
            .find(|tx| tx.name == "request")
            .unwrap()
            .tx;
        // Other wallet activity must not consume the reserved setup inputs.
        rt.fund(&address.script_pubkey(), Amount::from_sat(10_000))?;
        assert!(!rt.rpc.get_raw_mempool()?.contains(&root.txid));
        assert!(rt.is_unspent(&deposit)?);
        for input in &funding.input {
            assert!(rt.is_unspent(&input.previous_output)?);
        }
        let final_graph = Babe::new(final_spec)?.with_request_items(200)?;
        assert_eq!(final_graph.tree()?.script_pubkey(), output.script_pubkey);
        let final_r = final_graph
            .presign(root)?
            .into_iter()
            .find(|tx| tx.name == "request")
            .unwrap()
            .tx;
        assert_eq!(prepared_r, final_r);
        assert_eq!(prepared_r.compute_txid(), final_r.compute_txid());
        let signed = rt
            .rpc
            .sign_raw_transaction_with_wallet(&funding, None, None)?;
        assert!(signed.complete);
        let signed = signed.transaction()?;
        assert!(signed
            .input
            .iter()
            .all(|input| input.script_sig.is_empty() && !input.witness.is_empty()));
        assert_eq!(signed.compute_txid(), funding.compute_txid());
        assert_ne!(signed.compute_wtxid(), funding.compute_wtxid());
        let (confirmed, _) = rt.send_and_confirm(&signed)?;
        assert_eq!(confirmed, root.txid);
        assert_eq!(rt.get_tx(&confirmed)?.compute_txid(), root.txid);
        assert!(rt.is_unspent(&root)?);
        assert!(!rt.is_unspent(&deposit)?);
    }
    Ok(())
}

#[test]
fn licash_publication_presigned_set_is_pinned() {
    use lngap_btc::bitcoin::consensus::serialize;
    use lngap_btc::bitcoin::hashes::HashEngine;
    use lngap_btc::bitcoin::TxOut;

    let mut p = parties("licash/publication-set");
    p.msgs.truncate(2);
    let babe = Babe::new(corrected_spec(&p, 1_000, 900))
        .unwrap()
        .with_request_items(200)
        .unwrap();
    let seed = Seed::from_label("licash/publication-set/keys");
    let keys = core::array::from_fn(|i| {
        Wots256Secret::from_entropy(
            WotsParams::for_bytes(if i == 4 { 128 } else { 224 }),
            seed.derive_bytes(&format!("chunk-{i}")),
        )
        .public()
    });
    let trees = babe.publication_trees(&keys).unwrap();
    let publication = Publication {
        coins: core::array::from_fn(|i| {
            (
                OutPoint {
                    txid: Txid::from_byte_array([0x54; 32]),
                    vout: i as u32,
                },
                TxOut {
                    value: Amount::from_sat(100_000),
                    script_pubkey: trees[i].script_pubkey(),
                },
            )
        }),
        keys,
    };
    let set = babe
        .presign_with_publication(
            OutPoint {
                txid: Txid::from_byte_array([0x53; 32]),
                vout: 0,
            },
            Some(&publication),
        )
        .unwrap();
    let plain = set.iter().find(|tx| tx.name == "respond").unwrap();
    let counter = set.iter().find(|tx| tx.name == "respond_counter").unwrap();
    assert_eq!(plain.tx, counter.tx, "one response transaction body");
    assert_eq!(
        (plain.tx.input.len(), plain.extra.len(), counter.extra.len()),
        (6, 5, 5)
    );
    let mut hash = sha256::Hash::engine();
    for tree in [
        babe.tree(),
        babe.requested(),
        babe.responded(),
        babe.asserted(),
        babe.challenged(),
        babe.asserted_corrected(),
        babe.challenged_corrected(),
    ] {
        hash.input(tree.unwrap().script_pubkey().as_bytes());
    }
    for tree in trees {
        hash.input(tree.script_pubkey().as_bytes());
    }
    for tx in &set {
        hash.input(tx.name.as_bytes());
        hash.input(&serialize(&tx.tx));
        hash.input(tx.leaf.as_bytes());
        hash.input(&tx.control.serialize());
        hash.input(&(tx.extra.len() as u64).to_le_bytes());
        for (prevout, leaf, control) in &tx.extra {
            hash.input(&serialize(prevout));
            hash.input(&(leaf.len() as u64).to_le_bytes());
            hash.input(leaf.as_bytes());
            hash.input(&control.serialize());
        }
    }
    let fingerprint = sha256::Hash::from_engine(hash);
    println!(
        "Licash publication fingerprint: {} pre-signed, {fingerprint}",
        set.len()
    );
    assert_eq!(set.len(), 18);
    assert_eq!(
        fingerprint.to_string(),
        "d889e601c0412608e0bb2eedc40fc903ae70afe27d16a35f805402b204ddddc6"
    );
}

/// A contract funded and pre-signed, Alice's adaptor pre-signatures
/// checked by the hub.
struct Opened {
    p: Parties,
    babe: Babe,
    set: HashMap<String, Presign>,
    signed: Signed,
    kept: HashMap<String, Vec<u8>>,
    presigs: Vec<[lngap_btc::adaptor::AdaptorSig; 4]>,
    /// With the corrected exit, Alice's pre-signatures of
    /// `challenge_corrected`.
    presigs2: Vec<[lngap_btc::adaptor::AdaptorSig; 4]>,
}

fn open(rt: &Regtest, label: &str) -> Opened {
    open_with(rt, label, spec)
}

fn open_with(rt: &Regtest, label: &str, spec: fn(&Parties, u32, u32) -> Spec) -> Opened {
    let p = parties(label);
    let h = rt.height().unwrap();
    let babe = Babe::new(spec(&p, h + 2_000, h + 60)).unwrap();
    let (funding, _) = rt
        .fund(&babe.tree().unwrap().script_pubkey(), babe.value())
        .unwrap();
    let set = babe.presign(funding).unwrap();
    let msg = Babe::challenge_sighash(&set).unwrap();
    let pts = points(&p.dealing);
    let presigs = p.adaptor.presign(&msg, &pts);
    assert!(check_presigs(&p.adaptor.public(), &msg, &pts, &presigs));
    let presigs2 = match &babe.spec.corrected {
        Some(c) => {
            let msg = Babe::challenge_corrected_sighash(&set).unwrap();
            let pts = points(&p.corrected.dealing);
            let presigs = p.corrected.adaptor.presign(&msg, &pts);
            assert!(check_presigs(&c.adaptor, &msg, &pts, &presigs));
            presigs
        }
        None => vec![],
    };
    let (signed, kept) = sign(&set, &p);
    rt.mine(6).unwrap(); // request waits the channel's to_self_delay
    Opened {
        set: set.into_iter().map(|x| (x.name.clone(), x)).collect(),
        p,
        babe,
        signed,
        kept,
        presigs,
        presigs2,
    }
}

impl Opened {
    fn sigs(&self, name: &str) -> (&[u8], &[u8]) {
        (
            &self.signed.hub[name],
            self.signed.alice.get(name).or(self.kept.get(name)).unwrap(),
        )
    }
    /// Post link `name` with its wire, and mine it.
    fn link(&self, rt: &Regtest, name: &str, wire: Vec<Vec<u8>>) -> Transaction {
        let (h, a) = self.sigs(name);
        let tx = self.set[name].finish(wire, h, a);
        let (txid, _) = rt
            .send_and_confirm(&tx)
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        println!("{name}: {} vB", tx.vsize());
        rt.get_tx(&txid).unwrap()
    }
    /// Post terminal `name`, a coin of the node's wallet paying its fee.
    fn terminal(&self, rt: &Regtest, name: &str, wire: Vec<Vec<u8>>) -> Transaction {
        let p = &self.set[name];
        let (h, a) = self.sigs(name);
        let mut tx = p.finish(wire, h, a);
        let fee = Amount::from_sat(2 * (tx.vsize() as u64 + 120));
        // a confirmed coin of exactly the fee (TRUC spends no unconfirmed v2)
        let addr = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))
            .unwrap()
            .assume_checked();
        let (op, coin) = {
            let txid = rt
                .rpc
                .send_to_address(&addr, fee, None, None, None, None, None, None)
                .unwrap();
            rt.mine(1).unwrap();
            let t = rt.get_tx(&txid).unwrap();
            let v = t
                .output
                .iter()
                .position(|o| o.script_pubkey == addr.script_pubkey())
                .unwrap();
            (
                OutPoint {
                    txid,
                    vout: v as u32,
                },
                t.output[v].clone(),
            )
        };
        tx.input.push(TxIn {
            previous_output: op,
            ..Default::default()
        });
        let utxos: Vec<SignRawTransactionInput> =
            [(tx.input[0].previous_output, &p.prevout), (op, &coin)]
                .iter()
                .map(|(op, o)| SignRawTransactionInput {
                    txid: op.txid,
                    vout: op.vout,
                    script_pub_key: o.script_pubkey.clone(),
                    redeem_script: None,
                    amount: Some(o.value),
                })
                .collect();
        let tx = rt
            .rpc
            .sign_raw_transaction_with_wallet(&tx, Some(&utxos), None)
            .unwrap()
            .transaction()
            .unwrap();
        let (txid, _) = rt
            .send_and_confirm(&tx)
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        println!("{name}: {} vB with its fee coin", tx.vsize());
        rt.get_tx(&txid).unwrap()
    }
    fn args(tx: &Transaction) -> Vec<Vec<u8>> {
        witness_args_consumption_order(&tx.input[0].witness)[2..].to_vec()
    }
    /// Alice's request of `b`, and the hub's response, read from it.
    fn request_and_respond(&self, rt: &Regtest, b: u32) -> Transaction {
        let req = self.link(
            rt,
            "request",
            Babe::request_wire(&self.p.b_key.sign(&b.to_be_bytes()).unwrap()),
        );
        let alice_b =
            WotsSig::from_consumption_order(WotsParams::for_bytes(4), &Self::args(&req)).unwrap();
        let read = u32::from_be_bytes(alice_b.message().try_into().unwrap());
        self.link(
            rt,
            "respond",
            Babe::respond_wire(&self.p.hub_b.sign(&read.to_be_bytes()).unwrap(), &alice_b),
        )
    }
    /// Alice's request of `b` with her claim `e`, posted as `posted`; her
    /// signatures on `b` and `E`, read back from it.
    fn request_exit(
        &self,
        rt: &Regtest,
        b: u32,
        e: &[u8; 32],
        posted: &[u8],
    ) -> (Transaction, WotsSig, WotsSig) {
        let wire = Babe::request_exit_wire(
            &self.p.b_key.sign(&b.to_be_bytes()).unwrap(),
            &self.p.exit.e.sign(e).unwrap(),
            posted,
        )
        .unwrap();
        let req = self.link(rt, "request", wire);
        let (alice_b, alice_e, read) = Babe::read_request_exit(&Self::args(&req)).unwrap();
        assert_eq!(
            (alice_b.message(), alice_e.message(), &read[..]),
            (b.to_be_bytes().to_vec(), e.to_vec(), posted)
        );
        (req, alice_b, alice_e)
    }
    /// The hub's response to that request: the plain leaf (`k` none) or the
    /// counter leaf, its `E` signed under `key`.
    fn respond_exit_wire(
        &self,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        key: &Wots256Secret,
        k: Option<&[u8; K_LEN]>,
    ) -> Vec<Vec<u8>> {
        let hub_b = self.p.hub_b.sign(&alice_b.message()).unwrap();
        let k = k.map(|k| self.p.exit.k.sign(k).unwrap());
        Babe::respond_exit_wire(
            &hub_b,
            alice_b,
            alice_e,
            &key.sign(&alice_e.message()).unwrap(),
            k.as_ref(),
        )
    }
    /// The hub's corrected `respond_counter`: `K`, and `b_corr` signed.
    fn respond_corrected_wire(
        &self,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        k: &[u8; K_LEN],
        b_corr: u32,
    ) -> Vec<Vec<u8>> {
        let x = &self.p.exit;
        let hub_b = self.p.hub_b.sign(&alice_b.message()).unwrap();
        let hub_e = x.counter.sign(&alice_e.message()).unwrap();
        let b_corr = self.p.corrected.hub_b.sign(&b_corr.to_be_bytes()).unwrap();
        Babe::respond_corrected_wire(
            &hub_b,
            alice_b,
            alice_e,
            &hub_e,
            &x.k.sign(k).unwrap(),
            &b_corr,
        )
    }
    /// `assert_corrected`'s witness: π1 (digits at random) under the second
    /// Lamport key, the hub's `b_corr` as read from `respond_counter`, and
    /// Alice's signature on `mine`.
    fn assert_corrected_wire(&self, responded: &Transaction, mine: u32) -> Vec<Vec<u8>> {
        let mut rng = ChaCha20Rng::seed_from_u64(10);
        let digits: Vec<u8> = (0..DIGITS).map(|_| (rng.next_u32() % 4) as u8).collect();
        let c = &self.p.corrected;
        let (_, hub) = Babe::read_respond_corrected(
            &Self::args(responded),
            WotsParams::for_bytes(K_LEN as u32),
        )
        .unwrap();
        Babe::assert_corrected_wire(
            &c.lamport.sign(&digits),
            &[1; 64],
            &[2; 32],
            &hub,
            &c.b_key.sign(&mine.to_be_bytes()).unwrap(),
        )
    }
    /// The hub challenges the corrected assertion: the second dealing's
    /// secrets of π1's digits.
    fn challenge_corrected(&self, rt: &Regtest, asserted: &Transaction) -> Transaction {
        let args = Self::args(asserted);
        let ((signed, _, _), _) = Babe::read_assert_corrected(&args).unwrap();
        let preimages: Vec<[u8; 20]> = signed.iter().map(|a| a[..].try_into().unwrap()).collect();
        let lpub = &self.babe.spec.corrected.as_ref().unwrap().lamport;
        let digits: Vec<u8> = preimages
            .iter()
            .enumerate()
            .map(|(d, x)| lpub.read(d, x).unwrap())
            .collect();
        let secrets: Vec<Secret> = digits
            .iter()
            .enumerate()
            .map(|(d, v)| self.p.corrected.dealer.reveal(d, *v))
            .collect();
        self.link(
            rt,
            "challenge_corrected",
            Babe::challenge_wire(reveal_args(
                &preimages,
                &complete(&self.presigs2, &digits, &secrets),
            )),
        )
    }
    /// Alice asserts π1 (here, digits at random).
    fn assert(&self, rt: &Regtest) -> Transaction {
        let mut rng = ChaCha20Rng::seed_from_u64(9);
        let digits: Vec<u8> = (0..DIGITS).map(|_| (rng.next_u32() % 4) as u8).collect();
        self.link(
            rt,
            "assert",
            Babe::assert_wire(&self.p.lamport.sign(&digits), &[1; 64], &[2; 32]),
        )
    }
    /// The hub challenges: π1's secrets, from Alice's preimages on chain.
    fn challenge(&self, rt: &Regtest, asserted: &Transaction) -> Transaction {
        self.link(rt, "challenge_assert", self.challenge_wire(asserted))
    }
    fn challenge_wire(&self, asserted: &Transaction) -> Vec<Vec<u8>> {
        let args = Self::args(asserted);
        let (signed, _, _) = Babe::read_assert(&args).unwrap();
        let preimages: Vec<[u8; 20]> = signed.iter().map(|a| a[..].try_into().unwrap()).collect();
        let lpub = &self.babe.spec.lamport;
        let digits: Vec<u8> = preimages
            .iter()
            .enumerate()
            .map(|(d, x)| lpub.read(d, x).unwrap())
            .collect();
        let secrets: Vec<Secret> = digits
            .iter()
            .enumerate()
            .map(|(d, v)| self.p.dealer.reveal(d, *v))
            .collect();
        Babe::challenge_wire(reveal_args(
            &preimages,
            &complete(&self.presigs, &digits, &secrets),
        ))
    }
}

/// Consensus does not remember which witness a txid had on an orphaned
/// branch. A defender must retain its first disclosure outside chain lookup.
#[test]
fn a_reorg_can_replace_assertion_and_challenge_witnesses() {
    let rt = Regtest::start_standard().unwrap();
    let c = open(&rt, "babe/witness-reorg");
    c.request_and_respond(&rt, 300_000);
    let make_assert = |value: u8| {
        let wire = Babe::assert_wire(&c.p.lamport.sign(&vec![value; DIGITS]), &[1; 64], &[2; 32]);
        let (h, a) = c.sigs("assert");
        c.set["assert"].finish(wire, h, a)
    };
    let a = make_assert(0);
    let b = make_assert(3);
    assert_eq!(a.compute_txid(), b.compute_txid());
    assert_ne!(a.compute_wtxid(), b.compute_wtxid());
    let (_, height) = rt.send_and_confirm(&a).unwrap();
    let ca = c.link(&rt, "challenge_assert", c.challenge_wire(&a));
    let (h, u) = c.sigs("challenge_assert");
    let cb = c.set["challenge_assert"].finish(c.challenge_wire(&b), h, u);
    assert_eq!(ca.compute_txid(), cb.compute_txid());
    assert_ne!(ca.compute_wtxid(), cb.compute_wtxid());
    let old_block = rt.rpc.get_block_hash(u64::from(height)).unwrap();
    rt.rpc.invalidate_block(&old_block).unwrap();
    // Raw transactions in generateblock bypass same-txid mempool lookup.
    rt.mine_with(std::slice::from_ref(&b)).unwrap();
    rt.mine_with(std::slice::from_ref(&cb)).unwrap();
    assert_eq!(
        rt.get_tx(&b.compute_txid()).unwrap().compute_wtxid(),
        b.compute_wtxid()
    );
    assert_eq!(
        rt.get_tx(&cb.compute_txid()).unwrap().compute_wtxid(),
        cb.compute_wtxid()
    );
    let (da, _) = extract(&c.babe.spec.lamport, &c.presigs, &Opened::args(&ca)).unwrap();
    let (db, _) = extract(&c.babe.spec.lamport, &c.presigs, &Opened::args(&cb)).unwrap();
    assert_eq!(da, vec![0; DIGITS]);
    assert_eq!(db, vec![3; DIGITS]);
}

#[test]
fn every_ending_on_regtest() {
    let rt = Regtest::start_standard().unwrap();

    // a true claim, challenged: Alice reads the secrets, opens a kept
    // instance (the stand-in: its msg), and is paid
    let c = open(&rt, "babe/true");
    c.request_and_respond(&rt, 300_000);
    let asserted = c.assert(&rt);
    let challenged = c.challenge(&rt, &asserted);
    let (digits, secrets) =
        extract(&c.babe.spec.lamport, &c.presigs, &Opened::args(&challenged)).unwrap();
    assert!(
        digits
            .iter()
            .zip(&secrets)
            .enumerate()
            .all(|(d, (v, s))| *s == c.p.dealer.reveal(d, *v)),
        "Alice reads π1's secrets"
    );
    let paid = c.terminal(
        &rt,
        "wrongly_challenged_1",
        Babe::wrongly_wire(&c.p.msgs[1]),
    );
    assert_eq!(
        &paid.output[..],
        &c.set["wrongly_challenged_1"].tx.output[..],
        "Alice's payout, in bits"
    );
    assert!(paid.output.len() > 10);

    // a false claim: no msg, so after Δ1 the hub takes everything
    let c = open(&rt, "babe/false");
    c.request_and_respond(&rt, 300_000);
    let asserted = c.assert(&rt);
    c.challenge(&rt, &asserted);
    let early = rt
        .test_accept(&c.set["failure"].finish(vec![], c.sigs("failure").0, c.sigs("failure").1))
        .unwrap_err();
    assert!(early.contains("non-BIP68-final"), "not before Δ1: {early}");
    rt.mine(u64::from(c.babe.spec.windows.delta1) - 1).unwrap();
    c.terminal(&rt, "failure", vec![]);

    // the hub gone: after w_H Alice is paid her b unverified
    let c = open(&rt, "babe/hub-gone");
    c.link(
        &rt,
        "request",
        Babe::request_wire(&c.p.b_key.sign(&300_000u32.to_be_bytes()).unwrap()),
    );
    rt.mine(u64::from(c.babe.spec.windows.w_h) - 1).unwrap();
    c.terminal(&rt, "request_timeout", vec![]);

    // Alice gone after the response: from T_A the hub takes it
    let c = open(&rt, "babe/alice-gone");
    c.request_and_respond(&rt, 300_000);
    rt.mine_to_height(c.babe.spec.windows.t_a).unwrap();
    c.terminal(&rt, "respond_timeout", vec![]);

    // a true claim the hub doesn't challenge: after Δ2, Alice is paid
    let c = open(&rt, "babe/unchallenged");
    c.request_and_respond(&rt, 300_000);
    c.assert(&rt);
    rt.mine(u64::from(c.babe.spec.windows.delta2) - 1).unwrap();
    c.terminal(&rt, "success", vec![]);

    // the hub can't respond with another b than Alice's
    let c = open(&rt, "babe/other-b");
    let req = c.link(
        &rt,
        "request",
        Babe::request_wire(&c.p.b_key.sign(&300_000u32.to_be_bytes()).unwrap()),
    );
    let alice_b =
        WotsSig::from_consumption_order(WotsParams::for_bytes(4), &Opened::args(&req)).unwrap();
    let (h, a) = c.sigs("respond");
    let lie = c.set["respond"].finish(
        Babe::respond_wire(
            &c.p.hub_b.sign(&200_000u32.to_be_bytes()).unwrap(),
            &alice_b,
        ),
        h,
        a,
    );
    assert!(rt.test_accept(&lie).is_err(), "the digits must agree");
}

/// The exit's leaves, on regtest: `request` with Alice's `E` and the
/// largest claim posted in its first witness items; `respond` plain and
/// `respond_counter` (with `K`), one transaction, each continued by
/// `assert`; the hub's `E` must be Alice's, under the leaf's key.
#[test]
fn the_exit_on_regtest() {
    let rt = Regtest::start_standard().unwrap();
    let e = sha256::Hash::hash(b"a claim").to_byte_array();
    let posted: Vec<u8> = (0..8 + 32 + 1 + 8 * 141).map(|i| i as u8).collect();
    let k = [7u8; K_LEN];

    let c = open_with(&rt, "babe/exit-plain", exit_spec);
    assert_eq!(
        c.set["respond"].tx, c.set["respond_counter"].tx,
        "one transaction, either leaf"
    );
    let (req, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    let first: Vec<u8> = req.input[0]
        .witness
        .iter()
        .take(POSTED_ITEMS)
        .flatten()
        .copied()
        .collect();
    assert_eq!(
        first, posted,
        "the claim in the first items, as the statement reads it"
    );
    c.link(
        &rt,
        "respond",
        c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.plain, None),
    );
    c.assert(&rt);

    let c = open_with(&rt, "babe/exit-counter", exit_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    // the hub's E must be Alice's, under the leaf's own key
    let (h, a) = c.sigs("respond_counter");
    let other = Babe::respond_exit_wire(
        &c.p.hub_b.sign(&300_000u32.to_be_bytes()).unwrap(),
        &alice_b,
        &alice_e,
        &c.p.exit.counter.sign(&[1; 32]).unwrap(),
        Some(&c.p.exit.k.sign(&k).unwrap()),
    );
    assert!(
        rt.test_accept(&c.set["respond_counter"].finish(other, h, a))
            .is_err(),
        "another E"
    );
    let plain_key = c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.plain, Some(&k));
    assert!(
        rt.test_accept(&c.set["respond_counter"].finish(plain_key, h, a))
            .is_err(),
        "E under the plain key"
    );
    let (h, a) = c.sigs("respond");
    let counter_key = c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.counter, None);
    assert!(
        rt.test_accept(&c.set["respond"].finish(counter_key, h, a))
            .is_err(),
        "E under the counter key"
    );
    let responded = c.link(
        &rt,
        "respond_counter",
        c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.counter, Some(&k)),
    );
    let r = Babe::read_respond_exit(&Opened::args(&responded), true).unwrap();
    let e_sig = c.p.exit.counter.sign(&e).unwrap();
    assert_eq!(
        (r.e, r.hub_e),
        (e.to_vec(), e_sig.hashes.clone()),
        "the hub's signature on E reads back whole"
    );
    assert_eq!(r.k.unwrap().message(), k.to_vec());
    println!(
        "respond_counter: {} witness items",
        responded.input[0].witness.len()
    );
    c.assert(&rt);
}

/// An exit whose counter is data: `respond_counter` carries it below a
/// 32-byte `K`, and the plain leaf is unchanged; both read back.
#[test]
fn the_exit_with_counter_data_on_regtest() {
    let rt = Regtest::start_standard().unwrap();
    let e = sha256::Hash::hash(b"a claim").to_byte_array();
    let posted: Vec<u8> = (0..400).map(|i| i as u8).collect();
    let data: Vec<u8> = (0..COUNTER_ITEMS * 80 - 7).map(|i| (i * 3) as u8).collect();
    let k = sha256::Hash::hash(&data).to_byte_array();

    let c = open_with(&rt, "babe/exit-data-plain", data_exit_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    c.link(
        &rt,
        "respond",
        c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.plain, None),
    );
    c.assert(&rt);

    let c = open_with(&rt, "babe/exit-data-counter", data_exit_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    let hub_b = c.p.hub_b.sign(&alice_b.message()).unwrap();
    let hub_e = c.p.exit.counter.sign(&alice_e.message()).unwrap();
    let k_sig = c.p.exit.k32.sign(&k).unwrap();
    let (h, a) = c.sigs("respond_counter");
    let too_few = Babe::respond_counter_wire(
        &hub_b,
        &alice_b,
        &alice_e,
        &hub_e,
        Some(&k_sig),
        &data,
        COUNTER_ITEMS - 1,
    );
    assert!(too_few.is_err(), "the data must fit the items");
    let short = Babe::respond_counter_wire(
        &hub_b,
        &alice_b,
        &alice_e,
        &hub_e,
        Some(&k_sig),
        &data[..100],
        COUNTER_ITEMS - 1,
    )
    .unwrap();
    assert!(
        rt.test_accept(&c.set["respond_counter"].finish(short, h, a))
            .is_err(),
        "exactly the items the leaf drops"
    );
    let wire = Babe::respond_counter_wire(
        &hub_b,
        &alice_b,
        &alice_e,
        &hub_e,
        Some(&k_sig),
        &data,
        COUNTER_ITEMS,
    )
    .unwrap();
    let responded = c.link(&rt, "respond_counter", wire);
    let (r, read) = Babe::read_respond_counter(
        &Opened::args(&responded),
        WotsParams::for_bytes(32),
        COUNTER_ITEMS,
    )
    .unwrap();
    assert_eq!(r.e, e.to_vec());
    assert_eq!(r.k.unwrap().message(), k.to_vec());
    assert_eq!(
        &read[..data.len()],
        &data[..],
        "the counter data reads back"
    );
    println!(
        "respond_counter with {} bytes of data: {} vB",
        data.len(),
        responded.vsize()
    );
    c.assert(&rt);
}

/// The corrected exit's pre-signed set: the exit's, then the corrected
/// chain from `respond`'s output; its payouts read Alice's second `b` key.
#[test]
fn the_corrected_presigned_set() {
    let p = parties("babe/corrected-set");
    let exit = Babe::new(exit_spec(&p, 1_000, 900))
        .unwrap()
        .presign(OutPoint {
            txid: Txid::all_zeros(),
            vout: 0,
        })
        .unwrap();
    let babe = Babe::new(corrected_spec(&p, 1_000, 900)).unwrap();
    let set = babe
        .presign(OutPoint {
            txid: Txid::all_zeros(),
            vout: 0,
        })
        .unwrap();
    let names: Vec<&str> = set.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(
        names[exit.len()..],
        [
            "assert_corrected",
            "success_corrected",
            "challenge_corrected",
            "failure_corrected",
            "wrongly_challenged_corrected_0",
            "wrongly_challenged_corrected_1"
        ]
    );
    let by = |n: &str| set.iter().find(|x| x.name == n).unwrap();
    let txid = |n: &str| by(n).tx.compute_txid();
    assert_eq!(
        by("assert_corrected").tx.input[0].previous_output,
        by("assert").tx.input[0].previous_output,
        "both assert respond's output"
    );
    assert_eq!(
        by("challenge_corrected").tx.input[0].previous_output,
        OutPoint {
            txid: txid("assert_corrected"),
            vout: 0
        }
    );
    assert_eq!(
        by("respond").tx,
        by("respond_counter").tx,
        "one transaction, either leaf"
    );
    for n in ["success_corrected", "wrongly_challenged_corrected_0"] {
        assert_eq!(
            by(n).tx.output,
            babe.pay_alice_corrected(by(n).prevout.value).unwrap(),
            "{n} pays b_corr's bits"
        );
        assert_ne!(
            by(n).tx.output,
            babe.pay_alice(by(n).prevout.value).unwrap(),
            "{n} reads the second key"
        );
    }
    assert_eq!(
        by("failure_corrected").tx.output,
        babe.core().pay_hub(by("failure_corrected").prevout.value)
    );
    assert!(Babe::withheld("wrongly_challenged_corrected_0"));
    assert_eq!(by("wrongly_challenged_corrected_1").tx.version.0, 3);
    let mut prev = by("assert_corrected").prevout.value;
    for n in ["assert_corrected", "challenge_corrected"] {
        let l = by(n);
        println!(
            "{n}: floor {} sat",
            (prev - l.tx.output[0].value - l.tx.output[1].value).to_sat()
        );
        prev = l.tx.output[0].value;
    }
    assert!(
        babe.value() - prev < babe.spec.terms.reserve_hub,
        "the floors and anchors fit the hub's reserve"
    );
    let (signed, _) = sign(&set, &p);
    let shown: Vec<Presign> = set
        .iter()
        .filter(|x| !Babe::withheld(&x.name))
        .cloned()
        .collect();
    signed.verify(&shown, &babe.spec.pubs).unwrap();
    // each key signs once: the corrected set reuses none of the first's
    let mut same = corrected_spec(&p, 1_000, 900);
    same.corrected.as_mut().unwrap().b_key = same.b_key.clone();
    assert!(Babe::new(same).is_err());
    let mut bare = corrected_spec(&p, 1_000, 900);
    bare.exit = None;
    assert!(
        Babe::new(bare).is_err(),
        "a corrected exit corrects an exit"
    );
}

/// The corrected exit on regtest: `respond_counter` must carry the hub's
/// `b_corr`, `assert_corrected` Alice's on the same value; challenged,
/// she answers with a kept corrected instance and is paid in bits; or,
/// unchallenged, after `Δ2`.
#[test]
fn the_corrected_exit_on_regtest() {
    let rt = Regtest::start_standard().unwrap();
    let e = sha256::Hash::hash(b"a stale claim").to_byte_array();
    let posted: Vec<u8> = (0..8 + 32 + 1 + 8 * 141).map(|i| i as u8).collect();
    let k = [7u8; K_LEN];

    let c = open_with(&rt, "babe/corrected-true", corrected_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    let (h, a) = c.sigs("respond_counter");
    let uncorrected = c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.counter, Some(&k));
    assert!(
        rt.test_accept(&c.set["respond_counter"].finish(uncorrected, h, a))
            .is_err(),
        "respond_counter signs b_corr"
    );
    let mut forged = c.respond_corrected_wire(&alice_b, &alice_e, &k, 200_000);
    let n = forged.len();
    let b = 2 * WotsParams::for_bytes(4).total_digits() as usize;
    forged[n - 2 * b - 2] = vec![0; 32]; // a reveal of the b_corr signature
    assert!(
        rt.test_accept(&c.set["respond_counter"].finish(forged, h, a))
            .is_err(),
        "under the hub's key"
    );
    let wire = c.respond_corrected_wire(&alice_b, &alice_e, &k, 200_000);
    println!(
        "corrected respond_counter: {} witness items",
        wire.len() + 2
    );
    let responded = c.link(&rt, "respond_counter", wire);
    let (r, b_corr) = Babe::read_respond_corrected(
        &Opened::args(&responded),
        WotsParams::for_bytes(K_LEN as u32),
    )
    .unwrap();
    assert_eq!(
        (r.e, r.k.unwrap().message(), b_corr.message()),
        (e.to_vec(), k.to_vec(), 200_000u32.to_be_bytes().to_vec())
    );
    // Alice claims only what the hub signed
    let (h, a) = c.sigs("assert_corrected");
    let more = c.assert_corrected_wire(&responded, 250_000);
    assert!(
        rt.test_accept(&c.set["assert_corrected"].finish(more, h, a))
            .is_err(),
        "another b_corr"
    );
    let asserted = c.link(
        &rt,
        "assert_corrected",
        c.assert_corrected_wire(&responded, 200_000),
    );
    let (_, mine) = Babe::read_assert_corrected(&Opened::args(&asserted)).unwrap();
    assert_eq!(
        mine.message(),
        200_000u32.to_be_bytes().to_vec(),
        "the payout's bits read b_corr"
    );
    let challenged = c.challenge_corrected(&rt, &asserted);
    let lamport = &c.babe.spec.corrected.as_ref().unwrap().lamport;
    let (digits, secrets) = extract(lamport, &c.presigs2, &Opened::args(&challenged)).unwrap();
    assert!(
        digits
            .iter()
            .zip(&secrets)
            .enumerate()
            .all(|(d, (v, s))| *s == c.p.corrected.dealer.reveal(d, *v)),
        "Alice reads the second dealing's secrets"
    );
    let (h, a) = c.sigs("wrongly_challenged_corrected_0");
    let first =
        c.set["wrongly_challenged_corrected_0"].finish(Babe::wrongly_wire(&c.p.msgs[0]), h, a);
    assert!(
        rt.test_accept(&first).is_err(),
        "the first setup's msg opens no corrected lock"
    );
    let paid = c.terminal(
        &rt,
        "wrongly_challenged_corrected_1",
        Babe::wrongly_wire(&c.p.corrected.msgs[1]),
    );
    assert_eq!(
        paid.output, c.set["wrongly_challenged_corrected_1"].tx.output,
        "Alice's corrected payout, in bits"
    );

    // unchallenged, after Δ2
    let c = open_with(&rt, "babe/corrected-unchallenged", corrected_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    let responded = c.link(
        &rt,
        "respond_counter",
        c.respond_corrected_wire(&alice_b, &alice_e, &k, 1_024),
    );
    c.link(
        &rt,
        "assert_corrected",
        c.assert_corrected_wire(&responded, 1_024),
    );
    rt.mine(u64::from(c.babe.spec.windows.delta2) - 1).unwrap();
    c.terminal(&rt, "success_corrected", vec![]);

    // a plain response signs no b_corr: only the full assertion continues it
    let c = open_with(&rt, "babe/corrected-plain", corrected_spec);
    let (_, alice_b, alice_e) = c.request_exit(&rt, 300_000, &e, &posted);
    let responded = c.link(
        &rt,
        "respond",
        c.respond_exit_wire(&alice_b, &alice_e, &c.p.exit.plain, None),
    );
    let (h, a) = c.sigs("assert_corrected");
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let digits: Vec<u8> = (0..DIGITS).map(|_| (rng.next_u32() % 4) as u8).collect();
    let guess = c.p.hub_b.sign(&300_000u32.to_be_bytes()).unwrap(); // the hub's b, not b_corr
    let wire = Babe::assert_corrected_wire(
        &c.p.corrected.lamport.sign(&digits),
        &[1; 64],
        &[2; 32],
        &guess,
        &c.p.corrected.b_key.sign(&300_000u32.to_be_bytes()).unwrap(),
    );
    assert!(
        rt.test_accept(&c.set["assert_corrected"].finish(wire, h, a))
            .is_err(),
        "no b_corr without the hub's key"
    );
    drop(responded);
    c.assert(&rt);
}
