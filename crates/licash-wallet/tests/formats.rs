//! The formats a wallet file stores beside the protocol's bytes: JSON
//! bodies of the hub API and unlink sessions. A file written by one
//! version is read by the next, so a change here breaks wallets in the
//! field. The fixtures come from `write_fixtures`; never rewrite them to
//! make a change pass.

#[allow(dead_code)]
mod net;

use std::path::PathBuf;

use licash::{note::Note, unlink::Session};
use licash_api::{Fee, Info, Offer, Release};
use licash_wallet::{Error, Wallet};
use net::{Fault, Link, Net, HUB};
use serde::{de::DeserializeOwned, Serialize};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The fixture `name`, which serializes back to the same text.
fn read<T: Serialize + DeserializeOwned>(name: &str) -> T {
    let text = std::fs::read_to_string(fixture(name)).unwrap();
    let value: T = serde_json::from_str(&text).unwrap();
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        text.trim_end(),
        "{name}"
    );
    value
}

#[test]
fn stored_formats_still_read() {
    let info: Info = read("info.json");
    let keysets = info.keysets().unwrap();
    let lookup = |id: &[u8; 32]| keysets.iter().find(|k| k.id() == id);
    let offer: Offer = read("offer.json");
    let (proposal, inputs) = offer.decode(lookup).unwrap();
    let release: Release = read("release.json");
    let (signed, notes) = release.decode(lookup).unwrap();
    assert_eq!(signed.proposal().id(), proposal.id());
    assert_eq!(notes, inputs);

    let session: Session = read("session.json");
    assert_eq!(session.request().keyset(), keysets[0].id());
    let note = std::fs::read_to_string(fixture("note.hex")).unwrap();
    let note = Note::from_bytes(&keysets[0], &hex::decode(note.trim()).unwrap()).unwrap();
    assert_eq!(note.message().denomination(), 8);
}

#[tokio::test]
#[ignore = "writes the fixtures: only for a new format, with a migration"]
async fn write_fixtures() {
    let net = Net::new();
    net.start(HUB, Fee { base: 2, ppm: 0 });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.sqlite");
    let wallet = Wallet::create(&path, &[1; 32], Link(net.clone())).unwrap();
    wallet.add_hub(HUB).await.unwrap();
    wallet
        .receive(&net.pay(HUB, &wallet.address(), 12))
        .unwrap();
    net.fail(licash_api::UNLINK, Fault::Lost);
    assert!(wallet.unlink().await.is_err());
    net.fail(licash_api::RELEASE, Fault::Lost);
    let sent = wallet.send(HUB, &wallet.address(), 1).await;
    assert!(matches!(sent, Err(Error::Pending { .. })), "{sent:?}");

    let db = rusqlite::Connection::open(&path).unwrap();
    let text = |sql: &str| -> String { db.query_row(sql, [], |r| r.get(0)).unwrap() };
    let files = [
        ("info.json", text("SELECT info FROM hubs")),
        ("offer.json", text("SELECT offer FROM sends")),
        ("release.json", text("SELECT release FROM sends")),
        (
            "session.json",
            text("SELECT session FROM notes WHERE session IS NOT NULL"),
        ),
        (
            "note.hex",
            text("SELECT hex(note) FROM notes WHERE denom = 8").to_lowercase(),
        ),
    ];
    std::fs::create_dir_all(fixture("")).unwrap();
    for (name, text) in files {
        std::fs::write(fixture(name), text + "\n").unwrap();
    }
}
