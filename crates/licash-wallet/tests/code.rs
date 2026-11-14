//! Payment codes: written and read back, as text and for QR codes, with
//! the BIP321 URIs and bare addresses payers scan, and the wallet's.

#[allow(dead_code)]
mod net;

use bitcoin::{
    bech32::{
        primitives::iter::{ByteIterExt, Fe32IterExt},
        Bech32m, Fe32, Hrp,
    },
    secp256k1::{Secp256k1, SecretKey},
    NetworkKind,
};
use licash::{address::OwnerKeys, spend::ChainProfile};
use licash_api::Fee;
use licash_onchain::silent::Keys;
use licash_wallet::{Error, HubId, LicashCode, PaymentCode, Wallet, MAX_HUBS};
use net::{Link, Net, HUB};

const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

fn licash(network: ChainProfile, hubs: usize) -> LicashCode {
    let owner = OwnerKeys::from_seed(&[1; 32]).unwrap();
    LicashCode {
        network,
        address: owner.address(),
        mailbox: Some([7; 32]),
        hubs: (0..hubs as u8).map(|n| HubId([n; 16])).collect(),
    }
}

fn silent(network: NetworkKind) -> licash_onchain::silent::SpAddress {
    Keys::from_seed(&[2; 32], network).unwrap().address()
}

/// A `licash` value of `version` writing `bytes`, under `hrp`.
fn encoded(hrp: &str, version: Fe32, bytes: &[u8]) -> String {
    let hrp = Hrp::parse(hrp).unwrap();
    let chars = bytes
        .iter()
        .copied()
        .bytes_to_fes()
        .with_checksum::<Bech32m>(&hrp);
    chars.with_witness_version(version).chars().collect()
}

/// The bytes of `code` as version 0 writes them, records after.
fn bytes(network: u8, records: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut bytes = vec![network];
    bytes.extend(licash(REGTEST, 0).address.to_bytes());
    for (kind, value) in records {
        bytes.extend([*kind, value.len() as u8]);
        bytes.extend(value);
    }
    bytes
}

fn refused(text: &str, says: &str) {
    match text.parse::<PaymentCode>() {
        Err(Error::Invalid(why)) => assert!(why.contains(says), "{why:?} lacks {says:?}"),
        other => panic!("{text}: {other:?}"),
    }
}

#[test]
fn codes_read_back_as_written_in_text_and_for_qr_codes() {
    for network in ChainProfile::ALL {
        let kind = match network {
            ChainProfile::Core30Mainnet => NetworkKind::Main,
            _ => NetworkKind::Test,
        };
        let code = PaymentCode {
            address: None,
            amount: None,
            silent: Some(silent(kind)),
            licash: Some(licash(network, 2)),
        };
        let text = code.to_string();
        let prefix = match kind {
            NetworkKind::Main => "bitcoin:?sp=sp1q",
            NetworkKind::Test => "bitcoin:?sp=tsp1q",
        };
        assert!(text.starts_with(prefix), "{text}");
        let hrp = if kind == NetworkKind::Main {
            "&licash=licash1q"
        } else {
            "&licash=tlicash1q"
        };
        assert!(text.contains(hrp), "{text}");
        assert_eq!(text.parse::<PaymentCode>().unwrap(), code);
        let qr = code.qr();
        assert_eq!(qr, text.to_uppercase());
        assert_eq!(qr.parse::<PaymentCode>().unwrap(), code);
        code.check(network).unwrap();
        // Small enough for a QR code of version 13 at level M.
        assert!(qr.len() < 440, "{} characters", qr.len());
        // Bare, the parts read alone.
        let bare = code.licash.as_ref().unwrap().to_string();
        assert_eq!(bare.parse::<LicashCode>().unwrap(), licash(network, 2));
        let bare: PaymentCode = bare.to_uppercase().parse().unwrap();
        assert_eq!((bare.licash, bare.silent), (Some(licash(network, 2)), None));
        let sp: PaymentCode = silent(kind).to_string().parse().unwrap();
        assert_eq!((sp.silent, sp.licash), (Some(silent(kind)), None));
    }
    // Without a mailbox or hubs: just the wallet.
    let plain = LicashCode {
        mailbox: None,
        ..licash(REGTEST, 0)
    };
    assert_eq!(plain.to_string().parse::<LicashCode>().unwrap(), plain);
    // At most MAX_HUBS hubs are written.
    let many = licash(REGTEST, MAX_HUBS + 3);
    let read: LicashCode = many.to_string().parse().unwrap();
    assert_eq!(read.hubs, many.hubs[..MAX_HUBS]);
}

#[test]
fn bip21_uris_and_bare_addresses_are_read() {
    let deposit =
        "bitcoin:bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080?amount=0.00021&label=Licash";
    let code: PaymentCode = deposit.parse().unwrap();
    assert_eq!(code.amount, Some(21_000));
    let address = code.address.clone().unwrap();
    assert!(address.is_valid_for_network(bitcoin::Network::Regtest));
    assert_eq!((code.silent, code.licash.clone()), (None, None));
    assert_eq!(code.qr(), deposit.split('&').next().unwrap().to_uppercase());
    code.check(REGTEST).unwrap();
    let bare: PaymentCode = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"
        .parse()
        .unwrap();
    assert!(bare.address.is_some() && bare.amount.is_none());
    // Base58 stays as it is: upper case would be another address.
    let legacy: PaymentCode = "bitcoin:1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"
        .parse()
        .unwrap();
    assert!(legacy
        .qr()
        .starts_with("bitcoin:1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"));
    refused("bitcoin:?sp=tsp1qnope", "");
    refused("bitcoin:?amount=1", "nothing to pay");
    refused("bitcoin:?req-pop=x&sp=tsp1q", "requires req-pop");
    let sp = silent(NetworkKind::Test);
    refused(&format!("bitcoin:?sp={sp}&SP={sp}"), "sp twice");
    refused("hello", "");
}

#[test]
fn later_versions_and_records_are_skipped_and_malformed_codes_refused() {
    let mailbox = (1, vec![7; 32]);
    let hub = |n: u8| (2, vec![n; 16]);
    // A record of a type this does not know, skipped.
    let text = encoded(
        "tlicash",
        Fe32::Q,
        &bytes(1, &[mailbox.clone(), hub(0), (9, vec![1, 2])]),
    );
    let read: LicashCode = text.parse().unwrap();
    assert_eq!(
        (read.mailbox, read.hubs),
        (Some([7; 32]), vec![HubId([0; 16])])
    );
    // A later version: refused alone, ignored next to a silent payment
    // address.
    let later = encoded("tlicash", Fe32::P, &bytes(1, &[]));
    assert!(later.parse::<LicashCode>().is_err());
    let sp = silent(NetworkKind::Test);
    let code: PaymentCode = format!("bitcoin:?licash={later}&sp={sp}").parse().unwrap();
    assert_eq!((code.licash, code.silent), (None, Some(sp)));
    refused(&format!("bitcoin:?licash={later}"), "nothing to pay");
    // Malformed.
    let bad = |hrp: &str, bytes: Vec<u8>, says: &str| {
        refused(
            &format!("bitcoin:?licash={}", encoded(hrp, Fe32::Q, &bytes)),
            says,
        )
    };
    bad(
        "tlicash",
        bytes(1, &[hub(0), mailbox.clone()]),
        "out of order",
    );
    bad(
        "tlicash",
        bytes(1, &[mailbox.clone(), mailbox.clone()]),
        "mailbox",
    );
    bad("tlicash", bytes(1, &[(1, vec![7; 31])]), "mailbox");
    bad("tlicash", bytes(1, &[(2, vec![0; 15])]), "hubs");
    let many: Vec<_> = (0..=MAX_HUBS as u8).map(hub).collect();
    bad("tlicash", bytes(1, &many), "hubs");
    let mut cut = bytes(1, std::slice::from_ref(&mailbox));
    cut.truncate(cut.len() - 1);
    bad("tlicash", cut, "cut short");
    bad("tlicash", bytes(1, &[])[..60].to_vec(), "too short");
    bad("licash", bytes(1, &[]), "under licash");
    bad("tlicash", bytes(0, &[]), "under tlicash");
    bad("tlicash", bytes(2, &[]), "network 2");
    bad("other", bytes(1, &[]), "prefix other");
    let mut text = encoded("tlicash", Fe32::Q, &bytes(1, &[]));
    text.pop();
    text.push('q');
    refused(&text, "");
}

#[test]
fn codes_of_another_network_are_refused_whole() {
    let test = silent(NetworkKind::Test);
    let signet = PaymentCode {
        address: None,
        amount: None,
        silent: Some(test),
        licash: Some(licash(ChainProfile::Core30Signet, 1)),
    };
    signet.check(ChainProfile::Core30Signet).unwrap();
    // Mutinynet shares signet's prefixes: only the code tells them apart,
    // and a payment on the other chain is lost.
    for other in [
        ChainProfile::Mutinynet,
        REGTEST,
        ChainProfile::Core30Mainnet,
    ] {
        assert!(signet.check(other).is_err(), "{other}");
    }
    let main = PaymentCode {
        silent: Some(silent(NetworkKind::Main)),
        licash: None,
        ..signet.clone()
    };
    assert!(main.check(REGTEST).is_err());
    main.check(ChainProfile::Core30Mainnet).unwrap();
    let address: PaymentCode = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
        .parse()
        .unwrap();
    assert!(address.check(REGTEST).is_err());
    address.check(ChainProfile::Core30Mainnet).unwrap();
}

const HUB2: &str = "http://hub2.test";

fn wallet(net: &std::sync::Arc<Net>, dir: &tempfile::TempDir, seed: u8) -> Wallet<Link> {
    let path = dir.path().join(format!("{seed}.sqlite"));
    Wallet::create(&path, &[seed; 32], REGTEST, Link(net.clone())).unwrap()
}

#[tokio::test]
async fn payers_pay_at_a_hub_the_code_names_that_they_trust_and_hold_enough_at() {
    let net = Net::new();
    net.start(HUB, Fee { base: 0, ppm: 0 });
    net.start(HUB2, Fee { base: 1, ppm: 0 });
    *net.relays.lock().unwrap() = vec!["wss://relay.example".into()];
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob, carol) = (
        wallet(&net, &dir, 1),
        wallet(&net, &dir, 2),
        wallet(&net, &dir, 3),
    );
    alice.add_hub(HUB).await.unwrap();
    let info2 = alice.add_hub(HUB2).await.unwrap();
    bob.add_hub(HUB2).await.unwrap();
    bob.receive(&net.pay(HUB2, &bob.address(), 50)).unwrap();

    // Alice's code: her hubs, preferred first, her mailbox on their relays
    // and her silent payment address.
    let code = alice
        .payment_code(&[HUB.into(), HUB2.into()], true)
        .unwrap();
    let licash = code.licash.clone().unwrap();
    assert_eq!(licash.address, alice.address());
    let ids: Vec<HubId> = alice
        .hubs()
        .unwrap()
        .iter()
        .map(|(_, i)| HubId::of(&i.address))
        .collect();
    assert_eq!(licash.hubs, ids);
    assert_eq!(licash.hubs[1], HubId::of(&info2.address));
    let mailbox = SecretKey::from_slice(&alice.mailbox_secret()).unwrap();
    let key = mailbox.x_only_public_key(&Secp256k1::new()).0.serialize();
    assert_eq!(licash.mailbox, Some(key));
    assert_eq!(code.silent, Some(alice.silent_keys().address()));
    assert_eq!(code.to_string().parse::<PaymentCode>().unwrap(), code);
    // Of her seed alone, the same once reopened.
    drop(alice);
    let path = dir.path().join("1.sqlite");
    let alice = Wallet::open(&path, Link(net.clone())).unwrap();
    assert_eq!(
        alice
            .payment_code(&[HUB.into(), HUB2.into()], true)
            .unwrap(),
        code
    );
    // A hub named twice is named once.
    let twice = [HUB.into(), HUB2.into(), HUB2.into()];
    assert_eq!(alice.payment_code(&twice, true).unwrap(), code);
    let plain = alice.payment_code(&[HUB2.into()], false).unwrap();
    assert_eq!(plain.silent, None);
    assert_eq!(plain.licash.unwrap().hubs, vec![HubId::of(&info2.address)]);
    // Hubs she does not trust are not hers to name.
    let unknown = alice.payment_code(&["http://other.test".into()], true);
    assert!(matches!(unknown, Err(Error::UnknownHub(_))), "{unknown:?}");

    // Bob pays at the hub he trusts and holds enough at, its fee on top;
    // Carol, trusting none, pays on Bitcoin.
    assert_eq!(bob.hub_to_pay(&licash, 49).unwrap().as_deref(), Some(HUB2));
    assert_eq!(bob.hub_to_pay(&licash, 50).unwrap(), None);
    assert_eq!(carol.hub_to_pay(&licash, 1).unwrap(), None);
    let signet = LicashCode {
        network: ChainProfile::Core30Signet,
        ..licash.clone()
    };
    assert!(matches!(bob.hub_to_pay(&signet, 1), Err(Error::Invalid(_))));

    // No relays named: no mailbox.
    net.relays.lock().unwrap().clear();
    carol.add_hub(HUB).await.unwrap();
    let code = carol.payment_code(&[HUB.into()], false).unwrap();
    assert_eq!(code.licash.unwrap().mailbox, None);
}

#[tokio::test]
async fn a_payer_knows_what_a_hub_charges_and_how_much_more_it_needs_there() {
    let net = Net::new();
    // 1 and 10% of what a payment spends.
    net.start(
        HUB,
        Fee {
            base: 1,
            ppm: 100_000,
        },
    );
    net.start(HUB2, Fee { base: 0, ppm: 0 });
    let dir = tempfile::tempdir().unwrap();
    let bob = wallet(&net, &dir, 2);
    bob.add_hub(HUB).await.unwrap();
    bob.add_hub(HUB2).await.unwrap();
    bob.receive(&net.pay(HUB, &bob.address(), 50)).unwrap();
    // Paying 40 spends 48 of the notes of 32, 16 and 2, its fee 1 + 5; a
    // lock of 40 as much.
    assert_eq!(bob.lacks(HUB, 40).unwrap(), 0);
    assert_eq!(bob.send_fee(HUB, 40).unwrap(), 6);
    assert_eq!(bob.lock_fee(HUB, 40).unwrap(), 6);
    // Paying 50 spends 57 held, its fee 1 + 6: 7 more.
    assert_eq!(bob.lacks(HUB, 50).unwrap(), 7);
    let short = bob.send_fee(HUB, 50);
    assert!(
        matches!(short, Err(Error::Insufficient { .. })),
        "{short:?}"
    );
    assert_eq!(bob.lacks(&format!("{HUB}/"), 50).unwrap(), 7);
    // Holding none, and free there.
    assert_eq!(bob.lacks(HUB2, 30).unwrap(), 30);
    let unknown = bob.lacks("http://other.test", 1);
    assert!(matches!(unknown, Err(Error::UnknownHub(_))), "{unknown:?}");
}
