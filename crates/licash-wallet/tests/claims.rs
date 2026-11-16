//! Notes a contract's force close claims on Bitcoin: taken out of every
//! payment, all or none, given back only when asked.

#[allow(dead_code)]
mod net;

use std::sync::Arc;

use licash::spend::ChainProfile;
use licash_api::Fee;
use licash_wallet::{ClaimNote, Error, Wallet};
use net::{Link, Net, HUB};

const FEE: Fee = Fee { base: 2, ppm: 0 };
const ID: [u8; 32] = [7; 32];

struct Party {
    _dir: tempfile::TempDir,
    wallet: Wallet<Link>,
    keyset: [u8; 32],
}

/// A wallet of the owner of `[seed; 32]` that added the hub at [`HUB`],
/// paid `amounts` there.
async fn party(net: &Arc<Net>, seed: u8, amounts: &[u64]) -> Party {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("w.sqlite"),
        &[seed; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    wallet.add_hub(&format!("{HUB}/")).await.unwrap();
    for amount in amounts {
        wallet
            .receive(&net.pay(HUB, &wallet.address(), *amount))
            .unwrap();
    }
    let keyset = *net.hub(HUB).info().keysets[0].id();
    Party {
        _dir: dir,
        wallet,
        keyset,
    }
}

fn network() -> Arc<Net> {
    let net = Net::new();
    net.start(HUB, FEE);
    net
}

fn available(wallet: &Wallet<Link>) -> u64 {
    wallet.balance().unwrap().iter().map(|b| b.available).sum()
}

fn denominations(notes: &[ClaimNote]) -> Vec<u64> {
    notes
        .iter()
        .map(|n| n.note.message().denomination())
        .collect()
}

#[tokio::test]
async fn a_claim_takes_the_largest_notes_its_cap_holds_out_of_every_payment() {
    let net = network();
    // 64 + 32 + 4, then 4.
    let p = party(&net, 1, &[100, 4]).await;
    let w = &p.wallet;
    assert_eq!(available(w), 104);

    // 32 would pass the cap, and the second 4 too.
    let claimed = w.claim_notes(&ID, &p.keyset, 70, 32).unwrap();
    assert_eq!(denominations(&claimed), [64, 4]);
    assert_eq!(available(w), 36);
    let again = w.claimed_notes(&ID, &p.keyset).unwrap();
    assert_eq!(denominations(&again), [64, 4]);

    // No payment takes them.
    let to = party(&net, 2, &[]).await.wallet.address();
    assert!(matches!(
        w.send(HUB, &to, 40).await,
        Err(Error::Insufficient { .. })
    ));
    w.send(HUB, &to, 30).await.unwrap();
    assert_eq!(
        denominations(&w.claimed_notes(&ID, &p.keyset).unwrap()),
        [64, 4]
    );
}

#[tokio::test]
async fn a_claimed_note_shows_its_ownership_under_the_wallet_s_spending_root() {
    let net = network();
    let p = party(&net, 1, &[100]).await;
    let w = &p.wallet;
    let claimed = w.claim_notes(&ID, &p.keyset, 1_000, 32).unwrap();
    let ownerships = w.ownerships(&claimed).unwrap();
    assert_eq!(ownerships.len(), claimed.len());
    let root = w.address().spend();
    for (n, o) in claimed.iter().zip(&ownerships) {
        o.verify(&root, n.note.message()).unwrap();
    }
    // Another wallet's ownerships are not hers, nor hers of its notes.
    let other = party(&net, 2, &[100]).await;
    let theirs = other
        .wallet
        .claim_notes(&ID, &other.keyset, 1_000, 32)
        .unwrap();
    assert!(w.ownerships(&theirs).is_err());
    let root = other.wallet.address().spend();
    assert!(ownerships[0]
        .verify(&root, claimed[0].note.message())
        .is_err());
}

#[tokio::test]
async fn a_claim_takes_as_many_notes_as_it_may_hold() {
    let net = network();
    let p = party(&net, 1, &[100]).await;
    let claimed = p.wallet.claim_notes(&ID, &p.keyset, 1_000, 2).unwrap();
    assert_eq!(denominations(&claimed), [64, 32]);
}

#[tokio::test]
async fn coverage_previews_whole_notes_once_without_reserving_them() {
    let net = network();
    let p = party(&net, 1, &[100, 4]).await;
    let requests = [(p.keyset, 70, 32), (p.keyset, 70, 32)];
    assert_eq!(p.wallet.claimable_amounts(&requests).unwrap(), [68, 36]);
    assert_eq!(available(&p.wallet), 104);
    assert!(p.wallet.claimed_notes(&ID, &p.keyset).unwrap().is_empty());
    let first = p.wallet.claim_notes(&ID, &p.keyset, 70, 32).unwrap();
    assert_eq!(denominations(&first).iter().sum::<u64>(), 68);
    assert_eq!(p.wallet.claimable_amounts(&requests).unwrap(), [36, 0]);
    let second = p.wallet.claim_notes(&[8; 32], &p.keyset, 70, 32).unwrap();
    assert_eq!(denominations(&second).iter().sum::<u64>(), 36);
}

#[tokio::test]
async fn coverage_obeys_note_limits_caps_and_exact_keysets() {
    let net = network();
    let p = party(&net, 1, &[64]).await;
    assert_eq!(
        p.wallet.claimable_amounts(&[(p.keyset, 60, 32)]).unwrap(),
        [0]
    );
    assert_eq!(
        p.wallet.claimable_amounts(&[(p.keyset, 128, 0)]).unwrap(),
        [0]
    );
    assert_eq!(
        p.wallet.claimable_amounts(&[([9; 32], 128, 32)]).unwrap(),
        [0]
    );
    let p = party(&net, 2, &[100]).await;
    assert_eq!(
        p.wallet
            .claimable_amounts(&[(p.keyset, 1_000, 2), (p.keyset, 1_000, 1)])
            .unwrap(),
        [96, 4]
    );
}

#[tokio::test]
async fn coverage_excludes_notes_reserved_by_payments_unlinks_or_another_copy() {
    let net = network();
    let p = party(&net, 1, &[100, 4]).await;
    let db = rusqlite::Connection::open(p._dir.path().join("w.sqlite")).unwrap();
    db.execute(
        "UPDATE notes SET session = 'unlink-in-progress' WHERE denom = 64",
        [],
    )
    .unwrap();
    db.execute("UPDATE notes SET gone_at = 1 WHERE denom = 32", [])
        .unwrap();
    db.execute(
        "UPDATE notes SET spent_by = ?1 WHERE y IN (SELECT y FROM notes WHERE denom = 4 LIMIT 1)",
        [&ID[..]],
    )
    .unwrap();
    assert_eq!(
        p.wallet
            .claimable_amounts(&[(p.keyset, 1_000, 32)])
            .unwrap(),
        [4]
    );
    assert_eq!(
        denominations(
            &p.wallet
                .claim_notes(&[8; 32], &p.keyset, 1_000, 32)
                .unwrap()
        ),
        [4]
    );
}

#[tokio::test]
async fn a_recovery_claims_once_and_its_notes_come_back_when_given_up() {
    let net = network();
    let p = party(&net, 1, &[100]).await;
    let w = &p.wallet;
    w.claim_notes(&ID, &p.keyset, 1_000, 32).unwrap();
    assert_eq!(available(w), 0);
    assert!(matches!(
        w.claim_notes(&ID, &p.keyset, 1_000, 32),
        Err(Error::Invalid(_))
    ));
    // Another contract's recovery finds none left.
    assert!(w
        .claim_notes(&[8; 32], &p.keyset, 1_000, 32)
        .unwrap()
        .is_empty());

    assert_eq!(w.unclaim_notes(&ID).unwrap(), 3);
    assert_eq!(available(w), 100);
    assert!(w.claimed_notes(&ID, &p.keyset).unwrap().is_empty());
}

#[tokio::test]
async fn a_claim_of_a_keyset_the_wallet_never_had_is_refused() {
    let net = network();
    let p = party(&net, 1, &[100]).await;
    assert!(matches!(
        p.wallet.claim_notes(&ID, &[9; 32], 1_000, 32),
        Err(Error::Invalid(_))
    ));
    assert_eq!(available(&p.wallet), 100);
}

#[tokio::test]
async fn claimed_notes_keep_their_keys_and_read_back_from_their_bytes() {
    let net = network();
    let p = party(&net, 1, &[100]).await;
    let claimed = p.wallet.claim_notes(&ID, &p.keyset, 1_000, 32).unwrap();
    let keys = p.wallet.hold_keys(&claimed).unwrap();
    assert_eq!(keys.len(), 3);
    for (note, (message, _)) in claimed.iter().zip(&keys) {
        assert_eq!(note.note.message(), message);
    }

    let keyset = net.hub(HUB).keyset(&p.keyset).unwrap().clone();
    for note in &claimed {
        let back = ClaimNote::from_bytes(&keyset, &note.to_bytes()).unwrap();
        assert_eq!(back.to_bytes(), note.to_bytes());
    }
    assert!(ClaimNote::from_bytes(&keyset, &claimed[0].to_bytes()[1..]).is_err());

    // Another wallet's keys do not derive them.
    let other = party(&net, 2, &[]).await;
    assert!(other.wallet.hold_keys(&claimed).is_err());
}
