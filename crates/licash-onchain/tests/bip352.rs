//! Silent payments against BIP352's test vectors (tests/data, from
//! bip-0352/send_and_receive_test_vectors.json of bitcoin/bips).

use std::{collections::BTreeSet, str::FromStr};

use bitcoin::{
    absolute,
    bech32::{
        primitives::iter::{ByteIterExt, Fe32IterExt},
        Bech32m, Fe32, Hrp,
    },
    bip32::{DerivationPath, Xpriv},
    consensus::deserialize,
    hashes::{sha256, Hash},
    key::{Keypair, Secp256k1, TweakedPublicKey},
    secp256k1::{Message, PublicKey, SecretKey, XOnlyPublicKey},
    transaction, Amount, NetworkKind, OutPoint, Script, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Txid, Witness,
};
use licash_onchain::silent::{self, Error, Keys, Scanner, SpAddress, CHANGE};
use serde_json::Value;

fn vectors() -> Vec<Value> {
    serde_json::from_str(include_str!("data/bip352_vectors.json")).unwrap()
}

struct Vin {
    input: TxIn,
    prevout: ScriptBuf,
    secret: Option<SecretKey>,
}

fn vins(given: &Value) -> Vec<Vin> {
    let text = |v: &Value| v.as_str().unwrap().to_owned();
    given["vin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|vin| {
            let witness = match text(&vin["txinwitness"]) {
                w if w.is_empty() => Witness::new(),
                w => deserialize(&hex::decode(w).unwrap()).unwrap(),
            };
            let input = TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_str(&text(&vin["txid"])).unwrap(),
                    vout: vin["vout"].as_u64().unwrap() as u32,
                },
                script_sig: ScriptBuf::from_hex(&text(&vin["scriptSig"])).unwrap(),
                sequence: Sequence::MAX,
                witness,
            };
            Vin {
                input,
                prevout: ScriptBuf::from_hex(&text(&vin["prevout"]["scriptPubKey"]["hex"]))
                    .unwrap(),
                secret: vin
                    .get("private_key")
                    .map(|k| SecretKey::from_str(k.as_str().unwrap()).unwrap()),
            }
        })
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    let all = v.as_array().map(Vec::as_slice).unwrap_or_default();
    all.iter().map(|s| s.as_str().unwrap().to_owned()).collect()
}

fn hex_of(key: &XOnlyPublicKey) -> String {
    hex::encode(key.serialize())
}

#[test]
fn senders_derive_the_outputs_of_every_vector() {
    for case in vectors() {
        let comment = case["comment"].as_str().unwrap();
        for sending in case["sending"].as_array().unwrap() {
            let (given, expected) = (&sending["given"], &sending["expected"]);
            let vins = vins(given);
            let counted: Vec<(&Vin, PublicKey)> = vins
                .iter()
                .filter_map(|v| Some((v, silent::input_key(&v.input, &v.prevout)?)))
                .collect();
            let keys: Vec<String> = counted.iter().map(|(_, k)| k.to_string()).collect();
            assert_eq!(keys, strings(&expected["input_pub_keys"]), "{comment}");
            let secrets: Vec<(SecretKey, bool)> = counted
                .iter()
                .map(|(v, _)| (v.secret.unwrap(), v.prevout.is_p2tr()))
                .collect();
            let mut recipients = Vec::new();
            for r in given["recipients"].as_array().unwrap() {
                let address: SpAddress = r["address"].as_str().unwrap().parse().unwrap();
                assert_eq!(address.scan.to_string(), r["scan_pub_key"], "{comment}");
                assert_eq!(address.spend.to_string(), r["spend_pub_key"], "{comment}");
                assert_eq!(address.to_string(), r["address"], "{comment}");
                let count = r.get("count").and_then(Value::as_u64).unwrap_or(1);
                recipients.extend(std::iter::repeat_n(address, count as usize));
            }
            let sets: Vec<BTreeSet<String>> = expected["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|set| strings(set).into_iter().collect())
                .collect();
            let outpoints: Vec<OutPoint> = vins.iter().map(|v| v.input.previous_output).collect();
            match silent::outputs(&secrets, &outpoints, &recipients) {
                Ok(outputs) => {
                    assert_eq!(outputs.len(), recipients.len(), "{comment}");
                    let set: BTreeSet<String> = outputs.iter().map(hex_of).collect();
                    assert!(sets.contains(&set), "{comment}: {set:?}");
                }
                Err(_) => assert!(sets.iter().all(BTreeSet::is_empty), "{comment}"),
            }
        }
    }
}

#[test]
fn receivers_find_and_spend_the_outputs_of_every_vector() {
    let secp = Secp256k1::new();
    let message = Message::from_digest(sha256::Hash::hash(b"message").to_byte_array());
    let aux = sha256::Hash::hash(b"random auxiliary data").to_byte_array();
    for case in vectors() {
        let comment = case["comment"].as_str().unwrap();
        for receiving in case["receiving"].as_array().unwrap() {
            let (given, expected) = (&receiving["given"], &receiving["expected"]);
            let secret = |name: &str| {
                SecretKey::from_str(given["key_material"][name].as_str().unwrap()).unwrap()
            };
            let keys = Keys::new(
                secret("scan_priv_key"),
                secret("spend_priv_key"),
                NetworkKind::Main,
            );
            let labels: Vec<u32> = given["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_u64().unwrap() as u32)
                .collect();
            let scanner = Scanner::exactly(
                secret("scan_priv_key"),
                secret("spend_priv_key").public_key(&secp),
                NetworkKind::Main,
                labels.iter().copied(),
            );
            let mut addresses = vec![scanner.address()];
            addresses.extend(labels.iter().map(|m| scanner.labeled(*m)));
            let shown: Vec<String> = addresses.iter().map(SpAddress::to_string).collect();
            assert_eq!(shown, strings(&expected["addresses"]), "{comment}");
            assert_eq!(keys.address(), scanner.address());

            let vins = vins(given);
            let tx = Transaction {
                version: transaction::Version::TWO,
                lock_time: absolute::LockTime::ZERO,
                input: vins.iter().map(|v| v.input.clone()).collect(),
                output: strings(&given["outputs"])
                    .iter()
                    .map(|k| TxOut {
                        value: Amount::from_sat(1_000),
                        script_pubkey: ScriptBuf::from_bytes(
                            [&[0x51, 0x20][..], &hex::decode(k).unwrap()].concat(),
                        ),
                    })
                    .collect(),
            };
            let outputs = silent::taproot_outputs(&tx);
            let prevouts: Vec<&Script> = vins.iter().map(|v| v.prevout.as_script()).collect();
            let found = match silent::tweak(&tx, &prevouts) {
                Some(tweak) => {
                    assert_eq!(tweak.to_string(), expected["tweak"], "{comment}");
                    scanner.scan(&tweak, &outputs)
                }
                None => Vec::new(),
            };
            if let Some(n) = expected.get("n_outputs") {
                assert_eq!(found.len() as u64, n.as_u64().unwrap(), "{comment}");
                continue;
            }
            let want: BTreeSet<(String, String, String)> = expected["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    let s = |k: &str| o[k].as_str().unwrap().to_owned();
                    (s("pub_key"), s("priv_key_tweak"), s("signature"))
                })
                .collect();
            let got: BTreeSet<(String, String, String)> = found
                .iter()
                .map(|f| {
                    let secret = keys.spend_secret(f).unwrap();
                    let pair = Keypair::from_secret_key(&secp, &secret);
                    assert_eq!(pair.x_only_public_key().0, f.output, "{comment}");
                    let signature = secp.sign_schnorr_with_aux_rand(&message, &pair, &aux);
                    secp.verify_schnorr(&signature, &message, &f.output)
                        .unwrap();
                    let tweak = hex::encode(f.tweak.secret_bytes());
                    (hex_of(&f.output), tweak, signature.to_string())
                })
                .collect();
            assert_eq!(got, want, "{comment}");
        }
    }
}

fn encode(hrp: &str, version: u8, bytes: &[u8]) -> String {
    let hrp = Hrp::parse(hrp).unwrap();
    let version = Fe32::try_from(version).unwrap();
    let chars = bytes
        .iter()
        .copied()
        .bytes_to_fes()
        .with_checksum::<Bech32m>(&hrp)
        .with_witness_version(version)
        .chars();
    chars.collect()
}

#[test]
fn addresses_read_version_0_and_the_keys_of_later_compatible_versions() {
    let keys = Keys::from_seed(&[7; 32], NetworkKind::Test).unwrap();
    let address = keys.address();
    let text = address.to_string();
    assert!(text.starts_with("tsp1q") && text.len() == 117, "{text}");
    assert_eq!(text.parse::<SpAddress>().unwrap(), address);
    // In a QR code's alphanumeric mode.
    assert_eq!(text.to_uppercase().parse::<SpAddress>().unwrap(), address);
    assert!(text.to_uppercase()[..10]
        .chars()
        .chain(text[10..].chars())
        .collect::<String>()
        .parse::<SpAddress>()
        .is_err());

    let mut keys66 = address.scan.serialize().to_vec();
    keys66.extend(address.spend.serialize());
    let mut longer = keys66.clone();
    longer.extend([0xab; 20]);
    assert_eq!(encode("tsp", 0, &keys66), text);
    for version in [1, 30] {
        let later: SpAddress = encode("tsp", version, &longer).parse().unwrap();
        assert_eq!(later, address);
    }
    let refused = [
        encode("tsp", 0, &longer),
        encode("tsp", 0, &keys66[..65]),
        encode("tsp", 1, &keys66[..65]),
        encode("tsp", 31, &longer),
        encode("bc", 0, &keys66),
        text[..text.len() - 1].to_owned() + "x",
    ];
    for address in refused {
        assert!(address.parse::<SpAddress>().is_err(), "{address}");
    }
    let mainnet = Keys::from_seed(&[7; 32], NetworkKind::Main)
        .unwrap()
        .address();
    assert!(mainnet.to_string().starts_with("sp1q"));
    assert_ne!(mainnet.scan, address.scan);
}

#[test]
fn keys_derive_at_the_paths_of_bip352() {
    let secp = Secp256k1::new();
    let seed = [9; 32];
    for (network, coin) in [(NetworkKind::Main, 0), (NetworkKind::Test, 1)] {
        let master = Xpriv::new_master(network, &seed).unwrap();
        let at = |path: String| {
            let path = DerivationPath::from_str(&path).unwrap();
            master
                .derive_priv(&secp, &path)
                .unwrap()
                .private_key
                .public_key(&secp)
        };
        let address = Keys::from_seed(&seed, network).unwrap().address();
        assert_eq!(address.scan, at(format!("m/352'/{coin}'/0'/1'/0")));
        assert_eq!(address.spend, at(format!("m/352'/{coin}'/0'/0'/0")));
        assert_eq!(address.network, network);
    }
}

#[test]
fn a_wallet_finds_its_change_and_payments_to_its_labels() {
    let secp = Secp256k1::new();
    let (scan, spend) = (secret(1), secret(4));
    let receiver = Keys::new(scan, spend, NetworkKind::Test);
    let sender = Keys::from_seed(&[2; 32], NetworkKind::Test).unwrap();
    let scanner = receiver.scanner([5]);
    let (labeled, change) = (scanner.labeled(5), scanner.labeled(CHANGE));
    // A P2WPKH input the sender holds the key of.
    let input = secret(3);
    let key = bitcoin::CompressedPublicKey(input.public_key(&secp));
    let prevout = ScriptBuf::new_p2wpkh(&key.wpubkey_hash());
    let outpoint = OutPoint {
        txid: Txid::all_zeros(),
        vout: 1,
    };
    let recipients = [receiver.address(), labeled, change, sender.address()];
    let outputs = silent::outputs(&[(input, false)], &[outpoint], &recipients).unwrap();
    let mut witness = Witness::new();
    witness.push([0x30; 71]);
    witness.push(key.to_bytes());
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness,
        }],
        output: outputs
            .iter()
            .map(|k| TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new_p2tr_tweaked(
                    TweakedPublicKey::dangerous_assume_tweaked(*k),
                ),
            })
            .collect(),
    };
    let tweak = silent::tweak(&tx, &[&prevout]).unwrap();
    let found = scanner.scan(&tweak, &outputs);
    let labels: Vec<Option<u32>> = found.iter().map(|f| f.label).collect();
    assert_eq!(labels, [None, Some(5), Some(CHANGE)]);
    for f in &found {
        let pair = Keypair::from_secret_key(&secp, &receiver.spend_secret(f).unwrap());
        assert_eq!(pair.x_only_public_key().0, f.output);
    }
    assert_eq!(sender.scanner([]).scan(&tweak, &outputs).len(), 1);
    // Without the change label, change is not found.
    let blind = Scanner::exactly(scan, spend.public_key(&secp), NetworkKind::Test, [5]);
    let labels: Vec<Option<u32>> = blind
        .scan(&tweak, &outputs)
        .iter()
        .map(|f| f.label)
        .collect();
    assert_eq!(labels, [None, Some(5)]);
    // Nothing for a transaction without a taproot output.
    let mut plain = tx.clone();
    plain.output[0].script_pubkey = prevout.clone();
    plain.output.truncate(1);
    assert_eq!(silent::tweak(&plain, &[&prevout]), None);
    assert_eq!(
        silent::outputs(&[], &[outpoint], &recipients),
        Err(Error::Send("the input keys sum to zero"))
    );
}

fn secret(byte: u8) -> SecretKey {
    SecretKey::from_slice(&[byte; 32]).unwrap()
}
