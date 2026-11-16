#[allow(dead_code)]
mod net;

use licash::spend::ChainProfile;
use licash_api::Fee;
use licash_wallet::Wallet;
use net::{Link, Net, HUB};

#[tokio::test]
async fn a_live_wal_snapshot_keeps_identity_notes_and_irrevocable_reservations() {
    let net = Net::new();
    net.start(HUB, Fee { base: 2, ppm: 0 });
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[7; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    wallet.add_hub(HUB).await.unwrap();
    wallet
        .receive(&net.pay(HUB, &wallet.address(), 100))
        .unwrap();
    let keyset = *net.hub(HUB).info().keysets[0].id();
    wallet.claim_notes(&[1; 32], &keyset, 70, 32).unwrap();
    assert_eq!(wallet.backup_seed().unwrap(), [7; 32]);
    let to = dir.path().join("snapshot.sqlite");
    wallet.backup_to(&to).unwrap();
    wallet.unclaim_notes(&[1; 32]).unwrap();
    let restored = Wallet::open(&to, Link(net)).unwrap();
    assert_eq!(restored.address(), wallet.address());
    assert_eq!(restored.backup_seed().unwrap(), [7; 32]);
    assert_eq!(restored.balance().unwrap()[0].available, 32);
    assert_eq!(wallet.balance().unwrap()[0].available, 100);
    assert_eq!(restored.claimed_notes(&[1; 32], &keyset).unwrap().len(), 2);
}

#[test]
fn backup_never_overwrites_an_existing_file_or_invents_a_missing_seed() {
    let net = Net::new();
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create_without_seed(
        &dir.path().join("wallet.sqlite"),
        &[7; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net),
    )
    .unwrap();
    assert!(wallet.backup_seed().is_err());
    let to = dir.path().join("keep");
    std::fs::write(&to, b"keep this file").unwrap();
    assert!(wallet.backup_to(&to).is_err());
    assert_eq!(std::fs::read(to).unwrap(), b"keep this file");
}
