use std::path::{Path, PathBuf};

use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, Grant, PreparedGrant, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{ChainProfile, Output, Proposal, TimedLock, TimedTerms},
    unlink::{Nonce, Request, Session},
};
use licash_hub::{Error, Fee, Hub, Ledger, Notes, Notice, Policy, Refusal, Stats, Unlinks};

const MAX_POW: u32 = 6;

struct Files {
    _dir: tempfile::TempDir,
    seed: PathBuf,
    db: PathBuf,
}

fn files() -> Files {
    let dir = tempfile::tempdir().unwrap();
    let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
    Hub::init(&seed, &db, MAX_POW).unwrap();
    Files {
        _dir: dir,
        seed,
        db,
    }
}

fn open(files: &Files, policy: Policy) -> Hub {
    Hub::open(&files.seed, &files.db, policy).unwrap()
}

fn fee(base: u64, ppm: u64) -> Policy {
    Policy {
        spend: Fee { base, ppm },
        ..Policy::default()
    }
}

/// The hub's issuer keys, from its seed file: what it issued earlier.
fn issuer(files: &Files) -> (IssuerKeys, Keyset) {
    let mut seed = [0; 32];
    hex::decode_to_slice(
        std::fs::read_to_string(&files.seed).unwrap().trim(),
        &mut seed,
    )
    .unwrap();
    let keys = IssuerKeys::from_seed(&seed, 0, MAX_POW).unwrap();
    let keyset = keys.keyset().unwrap();
    (keys, keyset)
}

fn owner(seed: u8) -> OwnerKeys {
    OwnerKeys::from_seed(&[seed; 32]).unwrap()
}

/// A note of `denom` to `to`, as issued before the hub's ledger began.
fn mint(files: &Files, to: &OwnerKeys, denom: u64) -> Coin {
    let (keys, keyset) = issuer(files);
    mint_under(&keys, &keyset, to, denom)
}

fn mint_under(keys: &IssuerKeys, keyset: &Keyset, to: &OwnerKeys, denom: u64) -> Coin {
    let d = Ephemeral::random()
        .derive(&to.address(), denom, Branch::Pay, 0)
        .unwrap();
    let output = Output::from_derivation(&d).unwrap();
    let proof = keys.prove_blinded(denom, output.blinded()).unwrap();
    let note = Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, &d).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: Nonce::new(keyset, &output).unwrap().commit().unwrap(),
        },
    )
    .unwrap();
    Coin {
        note,
        derivation: d,
        owner: to.clone(),
    }
}

#[derive(Clone)]
struct Coin {
    note: Note,
    derivation: Derivation,
    owner: OwnerKeys,
}

/// A spend of `inputs` to `outputs`, with the derivations of its outputs.
struct Spend {
    inputs: Vec<Coin>,
    proposal: Proposal,
    outputs: Vec<Derivation>,
}

impl Spend {
    fn new(keyset: &Keyset, inputs: &[Coin], outputs: &[(Address, u64)]) -> Self {
        let e = Ephemeral::random();
        let outputs: Vec<Derivation> = outputs
            .iter()
            .enumerate()
            .map(|(i, (to, denom))| e.derive(to, *denom, Branch::Pay, i).unwrap())
            .collect();
        let proposal = Proposal::new(
            keyset,
            &notes(inputs),
            e.public(),
            outputs
                .iter()
                .map(|d| Output::from_derivation(d).unwrap())
                .collect(),
            None,
        )
        .unwrap();
        Self {
            inputs: inputs.to_vec(),
            proposal,
            outputs,
        }
    }

    fn notes(&self) -> Vec<Note> {
        notes(&self.inputs)
    }

    fn offer(&self, hub: &Hub) -> Result<Grant, Error> {
        hub.offer(&self.proposal, &self.notes())
    }

    fn sign(&self, keyset: &Keyset, authorization: Authorization) -> SignedProposal {
        let secrets: Vec<_> = self
            .inputs
            .iter()
            .map(|c| {
                c.derivation
                    .ownership()
                    .owner_secret(c.owner.spend_secret())
                    .unwrap()
            })
            .collect();
        SignedProposal::sign(
            keyset,
            self.proposal.clone(),
            &self.notes(),
            authorization,
            &secrets,
        )
        .unwrap()
    }

    /// Offer, sign and release: the notes of the outputs.
    fn complete(&self, hub: &Hub, keyset: &Keyset) -> Vec<Note> {
        let grant = self.offer(hub).unwrap();
        let signed = self.sign(keyset, Authorization::Granted(Box::new(grant)));
        let unlock = hub.release(&signed, &self.notes()).unwrap();
        (0..self.outputs.len())
            .map(|i| {
                signed
                    .open_granted_output(keyset, &self.notes(), i, &self.outputs[i], &unlock)
                    .unwrap()
            })
            .collect()
    }

    fn coin(&self, notes: &[Note], index: usize, owner: &OwnerKeys) -> Coin {
        Coin {
            note: notes[index].clone(),
            derivation: self.outputs[index].clone(),
            owner: owner.clone(),
        }
    }
}

fn notes(coins: &[Coin]) -> Vec<Note> {
    coins.iter().map(|c| c.note.clone()).collect()
}

fn refused(result: Result<impl Sized, Error>) -> Refusal {
    match result {
        Err(Error::Refused(refusal)) => refusal,
        Err(e) => panic!("expected a refusal, got {e}"),
        Ok(_) => panic!("expected a refusal"),
    }
}

fn setup_error(result: Result<impl Sized, Error>) -> String {
    match result {
        Err(Error::Setup(why)) => why,
        Err(e) => panic!("expected a setup error, got {e}"),
        Ok(_) => panic!("expected a setup error"),
    }
}

fn notices(hub: &Hub) -> Vec<Notice> {
    hub.events(0, 100)
        .unwrap()
        .into_iter()
        .map(|e| e.notice().unwrap())
        .collect()
}

/// `request` with another capability.
fn forge(request: &Request) -> Request {
    let mut bytes = request.to_bytes();
    bytes[10 + 32 + 8 + 33] ^= 1;
    Request::from_bytes(&bytes).unwrap()
}

fn sql(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).unwrap()
}

#[test]
fn init_never_replaces_a_seed_or_ledger_and_open_needs_both_matching() {
    let a = files();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&a.seed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let seed = std::fs::read(&a.seed).unwrap();
    let dir = a.db.parent().unwrap();
    let (other_seed, other_db) = (dir.join("other-seed"), dir.join("other.sqlite"));
    setup_error(Hub::init(&a.seed, &a.db, MAX_POW));
    setup_error(Hub::init(&other_seed, &a.db, MAX_POW));
    assert!(!other_seed.exists());
    // A fresh ledger for an existing seed would take its spent notes again.
    setup_error(Hub::init(&a.seed, &other_db, MAX_POW));
    assert!(!other_db.exists());
    assert_eq!(std::fs::read(&a.seed).unwrap(), seed);
    setup_error(Hub::init(&other_seed, &other_db, 63));
    assert!(!other_seed.exists() && !other_db.exists());

    setup_error(Hub::open(&other_seed, &a.db, Policy::default()));
    setup_error(Hub::open(&a.seed, &other_db, Policy::default()));
    assert!(!other_db.exists());
    let b = files();
    let why = setup_error(Hub::open(&b.seed, &a.db, Policy::default()));
    assert!(why.contains("does not match"), "{why}");

    let hub = open(&a, Policy::default());
    let why = setup_error(Hub::open(&a.seed, &a.db, Policy::default()));
    assert!(why.contains("another hub"), "{why}");
    drop(hub);
    sql(&a.db).pragma_update(None, "user_version", 2).unwrap();
    let why = setup_error(Hub::open(&a.seed, &a.db, Policy::default()));
    assert!(why.contains("schema 2"), "{why}");
}

#[test]
fn a_hub_names_one_nostr_key_of_its_seed_across_restarts() {
    use bitcoin::secp256k1::{Keypair, Secp256k1};
    let (a, b) = (files(), files());
    let hub = open(&a, Policy::default());
    let secret = hub.nostr_secret();
    let named = hub.advertise().unwrap().nostr.unwrap();
    let pair = Keypair::from_seckey_slice(&Secp256k1::new(), &secret).unwrap();
    assert_eq!(named, hex::encode(pair.x_only_public_key().0.serialize()));
    let seed = std::fs::read_to_string(&a.seed).unwrap();
    assert_ne!(hex::encode(secret), seed.trim());
    drop(hub);
    let again = open(&a, Policy::default());
    assert_eq!(again.nostr_secret(), secret);
    assert_eq!(again.advertise().unwrap().nostr.unwrap(), named);
    assert_ne!(open(&b, Policy::default()).nostr_secret(), secret);
}

#[test]
fn a_hub_claims_each_deposit_with_a_key_of_its_own_seed() {
    let (a, b) = (files(), files());
    let hub = open(&a, Policy::default());
    let (one, two) = (hub.deposit_secret(&[1; 32]), hub.deposit_secret(&[2; 32]));
    assert_ne!(one, two);
    assert_ne!(one, hub.nostr_secret());
    drop(hub);
    assert_eq!(open(&a, Policy::default()).deposit_secret(&[1; 32]), one);
    assert_ne!(open(&b, Policy::default()).deposit_secret(&[1; 32]), one);
}

#[test]
fn fees_round_up_and_saturate() {
    let fee = Fee { base: 3, ppm: 1 };
    assert_eq!(fee.due(0), 3);
    assert_eq!(fee.due(1), 4);
    assert_eq!(fee.due(1_000_000), 4);
    assert_eq!(fee.due(1_000_001), 5);
    assert_eq!(Fee { base: 0, ppm: 0 }.due(u64::MAX), 0);
    assert_eq!(
        Fee {
            base: 1,
            ppm: 2_000_000
        }
        .due(u64::MAX),
        u64::MAX
    );
    assert_eq!(Policy::default().spend.due(8), 0);
}

#[test]
fn a_spend_is_offered_once_released_once_and_pays_the_hub() {
    let files = files();
    let hub = open(&files, fee(1, 0));
    let (_, keyset) = issuer(&files);
    let info = hub.info();
    assert_eq!(info.keysets, vec![keyset.clone()]);
    assert_eq!(info.policy, fee(1, 0));
    assert_eq!(hub.keyset(keyset.id()), Some(&keyset));
    let (alice, bob) = (owner(1), owner(2));
    let coin = mint(&files, &alice, 8);
    let spend = Spend::new(
        &keyset,
        &[coin],
        &[
            (bob.address(), 4),
            (alice.address(), 2),
            (info.address, 1),
            (alice.address(), 1),
        ],
    );
    let grant = spend.offer(&hub).unwrap();
    grant.verify(&keyset, &spend.proposal).unwrap();
    assert_eq!(spend.offer(&hub).unwrap(), grant);
    let signed = spend.sign(&keyset, Authorization::Granted(Box::new(grant.clone())));
    let unlock = hub.release(&signed, &spend.notes()).unwrap();
    assert_eq!(hub.release(&signed, &spend.notes()).unwrap(), unlock);
    // Owners sign afresh; the release is the same.
    let again = spend.sign(&keyset, Authorization::Granted(Box::new(grant)));
    assert_eq!(hub.release(&again, &spend.notes()).unwrap(), unlock);
    for (index, d) in spend.outputs.iter().enumerate() {
        let note = signed
            .open_granted_output(&keyset, &spend.notes(), index, d, &unlock)
            .unwrap();
        note.verify(&keyset).unwrap();
    }
    let proposal = hex::encode(spend.proposal.id().as_bytes());
    assert!(matches!(&notices(&hub)[..], [
        Notice::Started { .. },
        Notice::Released { proposal: p, amount: 8, fee: 1 },
    ] if *p == proposal));
    let events = hub.events(1, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 2);
    assert!(hub.events(2, 10).unwrap().is_empty());
    assert_eq!(hub.events(0, 1).unwrap().len(), 1);
    let income: (i64, i64) = sql(&files.db)
        .query_row("SELECT count(*), sum(denom) FROM income", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(income, (1, 1));
}

#[test]
fn spends_pay_the_policy_fee_in_outputs_to_the_hub() {
    let files = files();
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let free = open(&files, Policy::default());
    let address = free.info().address;
    drop(free);
    let coin = mint(&files, &alice, 8);
    let unpaid = Spend::new(&keyset, std::slice::from_ref(&coin), &[(bob.address(), 8)]);
    let paid = Spend::new(
        &keyset,
        &[coin],
        &[(bob.address(), 4), (bob.address(), 2), (address, 2)],
    );
    let hub = open(&files, fee(1, 0));
    assert_eq!(
        refused(unpaid.offer(&hub)),
        Refusal::Fee { due: 1, paid: 0 }
    );
    drop(hub);
    // 8 at 250,000 ppm is 2, plus a base of 1.
    let hub = open(&files, fee(1, 250_000));
    assert_eq!(refused(paid.offer(&hub)), Refusal::Fee { due: 3, paid: 2 });
    drop(hub);
    let hub = open(&files, fee(0, 250_000));
    paid.offer(&hub).unwrap();
}

#[test]
fn a_spend_pays_for_the_notes_it_makes_for_others_than_the_hub() {
    let files = files();
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let free = open(&files, Policy::default());
    let address = free.info().address;
    drop(free);
    let coin = mint(&files, &alice, 8);
    let spend =
        |outputs: &[(Address, u64)]| Spend::new(&keyset, std::slice::from_ref(&coin), outputs);
    let three = spend(&[
        (bob.address(), 4),
        (bob.address(), 2),
        (bob.address(), 1),
        (address, 1),
    ]);
    let two = spend(&[(bob.address(), 4), (bob.address(), 2), (address, 2)]);
    let one = spend(&[(bob.address(), 4), (address, 2), (address, 1), (address, 1)]);
    let per_note = |output_msat| Policy {
        output_msat,
        ..fee(1, 0)
    };
    // 0.6 sat a note, to the nearest sat: 2 for three notes, 1 for two,
    // beside the base of 1. The hub's own notes cost nothing.
    let hub = open(&files, per_note(600));
    assert_eq!(hub.advertise().unwrap().fees.output_msat, 600);
    assert_eq!(refused(three.offer(&hub)), Refusal::Fee { due: 3, paid: 1 });
    two.offer(&hub).unwrap();
    one.offer(&hub).unwrap();
    drop(hub);
    // A tenth of a sat a note is nothing on three notes.
    let hub = open(&files, per_note(100));
    three.offer(&hub).unwrap();
}

#[test]
fn an_input_is_spent_once_and_a_second_signed_spend_is_alerted() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob, carol) = (owner(1), owner(2), owner(3));
    let coin = mint(&files, &alice, 8);
    let first = Spend::new(&keyset, std::slice::from_ref(&coin), &[(bob.address(), 8)]);
    let second = Spend::new(
        &keyset,
        std::slice::from_ref(&coin),
        &[(carol.address(), 8)],
    );
    let second_grant = second.offer(&hub).unwrap();
    let y = coin.note.message().identifier().unwrap();
    let other = mint(&files, &alice, 8).note.message().identifier().unwrap();
    assert_eq!(hub.spent(&[y, other]).unwrap(), [false, false]);
    first.complete(&hub, &keyset);
    assert_eq!(hub.spent(&[y, other, y]).unwrap(), [true, false, true]);
    let signed = second.sign(&keyset, Authorization::Granted(Box::new(second_grant)));
    assert_eq!(
        refused(hub.release(&signed, &second.notes())),
        Refusal::Spent
    );
    assert_eq!(refused(second.offer(&hub)), Refusal::Spent);
    let third = Spend::new(&keyset, &[coin], &[(carol.address(), 8)]);
    assert_eq!(refused(third.offer(&hub)), Refusal::Spent);
    let (by, of) = (
        hex::encode(first.proposal.id().as_bytes()),
        hex::encode(second.proposal.id().as_bytes()),
    );
    assert!(
        matches!(notices(&hub).last(), Some(Notice::DoubleSpend { proposal, spent_by })
        if *proposal == of && *spent_by == by)
    );
    // Nothing of the refused spend was recorded.
    let issued: i64 = sql(&files.db)
        .query_row("SELECT count(*) FROM issued", [], |r| r.get(0))
        .unwrap();
    assert_eq!(issued, 1);
}

#[test]
fn outputs_are_issued_once() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let first = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    first.complete(&hub, &keyset);
    let mut copy = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    copy.outputs = first.outputs.clone();
    copy.proposal = Proposal::new(
        &keyset,
        &copy.notes(),
        first.proposal.ephemeral(),
        first.proposal.outputs().to_vec(),
        None,
    )
    .unwrap();
    assert_eq!(refused(copy.offer(&hub)), Refusal::Reissued);
}

#[test]
fn only_the_hubs_own_offers_release_and_only_untimed_spends_are_served() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (keys, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    // A grant signed under the hub's keys that the hub never offered.
    let foreign = PreparedGrant::new(&keys, &spend.proposal).unwrap().offer();
    let signed = spend.sign(&keyset, Authorization::Granted(Box::new(foreign)));
    assert_eq!(
        refused(hub.release(&signed, &spend.notes())),
        Refusal::UnknownOffer
    );
    spend.offer(&hub).unwrap();
    assert!(matches!(
        hub.release(&signed, &spend.notes()),
        Err(Error::Invalid(_))
    ));
    let plain = spend.sign(&keyset, Authorization::Plain);
    assert!(matches!(
        hub.release(&plain, &spend.notes()),
        Err(Error::Invalid(_))
    ));
    let mut wrong = spend.notes();
    wrong[0] = mint(&files, &alice, 8).note;
    assert!(matches!(
        spend.offer(&hub).and(hub.offer(&spend.proposal, &wrong)),
        Err(Error::Invalid(_))
    ));

    let other = IssuerKeys::random(MAX_POW).unwrap();
    let other_keyset = other.keyset().unwrap();
    let elsewhere = Spend::new(
        &other_keyset,
        &[mint_under(&other, &other_keyset, &alice, 8)],
        &[(bob.address(), 8)],
    );
    assert_eq!(refused(elsewhere.offer(&hub)), Refusal::UnknownKeyset);

    let terms = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        [31; 32],
        1_000,
        3,
        [41; 32],
    )
    .unwrap();
    let coin = mint(&files, &alice, 8);
    let e = Ephemeral::random();
    let pay = e.derive(&bob.address(), 8, Branch::Pay, 0).unwrap();
    let refund = e.derive(&alice.address(), 8, Branch::Refund, 0).unwrap();
    let timed = Proposal::new(
        &keyset,
        std::slice::from_ref(&coin.note),
        e.public(),
        vec![Output::from_derivation(&pay).unwrap()],
        Some(TimedLock::new(terms, vec![Output::from_derivation(&refund).unwrap()]).unwrap()),
    )
    .unwrap();
    assert_eq!(refused(hub.offer(&timed, &[coin.note])), Refusal::Timed);
}

#[test]
fn issued_outputs_unlink_once_and_only_until_the_hub_restarts() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(
        &keyset,
        &[mint(&files, &alice, 8)],
        &[(bob.address(), 4), (alice.address(), 4)],
    );
    let notes = spend.complete(&hub, &keyset);
    let (paid, change) = (spend.coin(&notes, 0, &bob), spend.coin(&notes, 1, &alice));

    let asked = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    let session = asked.clone();
    // Without the output's capability the nonce stays unused, and the
    // output looks unknown.
    let forged = forge(session.request());
    assert_eq!(refused(hub.unlink(&forged)), Refusal::NotIssued);
    let answer = hub.unlink(session.request()).unwrap();
    let unlinked = session.finish(&keyset, &answer).unwrap();
    unlinked.verify(&keyset).unwrap();
    assert!(matches!(unlinked.evidence(), Evidence::Unlinked(_)));
    assert_eq!(hub.unlink(session.request()).unwrap(), answer);
    let other = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    assert_eq!(refused(hub.unlink(other.request())), Refusal::Unlinked);
    assert_eq!(refused(hub.unlink(&forged)), Refusal::NotIssued);

    let unknown = mint(&files, &bob, 4);
    let session = Session::start(&keyset, &unknown.note, &unknown.derivation).unwrap();
    assert_eq!(refused(hub.unlink(session.request())), Refusal::NotIssued);

    drop(hub);
    let hub = open(&files, Policy::default());
    let late = Session::start(&keyset, &change.note, &change.derivation).unwrap();
    assert_eq!(refused(hub.unlink(late.request())), Refusal::Expired);
    assert_eq!(
        refused(hub.unlink(&forge(late.request()))),
        Refusal::NotIssued
    );
    let first = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    assert_eq!(refused(hub.unlink(first.request())), Refusal::Unlinked);
    // The answer to the same request is kept across restarts.
    assert_eq!(hub.unlink(asked.request()).unwrap(), answer);
    // The change is still spendable, and its swap unlinks.
    let swap = Spend::new(&keyset, &[change], &[(alice.address(), 4)]);
    let swapped = swap.coin(&swap.complete(&hub, &keyset), 0, &alice);
    let session = Session::start(&keyset, &swapped.note, &swapped.derivation).unwrap();
    let answer = hub.unlink(session.request()).unwrap();
    session.finish(&keyset, &answer).unwrap();
}

#[test]
fn offers_survive_a_restart_and_release_after_it() {
    let files = files();
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    let hub = open(&files, Policy::default());
    let grant = spend.offer(&hub).unwrap();
    drop(hub);
    let hub = open(&files, Policy::default());
    assert_eq!(spend.offer(&hub).unwrap(), grant);
    let signed = spend.sign(&keyset, Authorization::Granted(Box::new(grant)));
    let unlock = hub.release(&signed, &spend.notes()).unwrap();
    drop(hub);
    let hub = open(&files, Policy::default());
    assert_eq!(hub.release(&signed, &spend.notes()).unwrap(), unlock);
    signed
        .open_granted_output(&keyset, &spend.notes(), 0, &spend.outputs[0], &unlock)
        .unwrap();
    let started = notices(&hub)
        .iter()
        .filter(|n| matches!(n, Notice::Started { .. }))
        .count();
    assert_eq!(started, 3);
}

#[test]
fn a_database_changed_under_the_running_hub_halts_it() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    spend.offer(&hub).unwrap();
    // As a restored backup would be: one write behind.
    sql(&files.db)
        .execute("UPDATE meta SET seq = seq - 1", [])
        .unwrap();
    assert!(!hub.halted());
    assert!(matches!(spend.offer(&hub), Err(Error::Halted)));
    assert!(hub.halted());
    assert!(matches!(hub.events(0, 10), Err(Error::Halted)));
    let session =
        Session::start(&keyset, &spend.inputs[0].note, &spend.inputs[0].derivation).unwrap();
    assert!(matches!(hub.unlink(session.request()), Err(Error::Halted)));
    drop(hub);
    // A restart serves the database as it is, under a new boot.
    let hub = open(&files, Policy::default());
    spend.offer(&hub).unwrap();
}

#[test]
fn a_hub_is_shared_across_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Hub>();
}

#[test]
fn a_hub_that_answered_halts_when_its_database_forgets_the_answer() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    let paid = spend.coin(&spend.complete(&hub, &keyset), 0, &bob);
    let first = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    hub.unlink(first.request()).unwrap();
    // As a file restored under the hub would read, with the write count
    // unchanged: the nonce looks unanswered.
    sql(&files.db)
        .execute("UPDATE issued SET answered = NULL", [])
        .unwrap();
    let second = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    assert_eq!(refused(hub.unlink(second.request())), Refusal::Unlinked);
    assert!(matches!(hub.unlink(first.request()), Err(Error::Halted)));
    assert!(matches!(hub.unlink(second.request()), Err(Error::Halted)));
}

#[test]
fn outputs_offered_before_a_restart_and_released_after_it_do_not_unlink() {
    let files = files();
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(&keyset, &[mint(&files, &alice, 8)], &[(bob.address(), 8)]);
    let hub = open(&files, Policy::default());
    spend.offer(&hub).unwrap();
    drop(hub);
    let hub = open(&files, Policy::default());
    let paid = spend.coin(&spend.complete(&hub, &keyset), 0, &bob);
    let session = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    assert_eq!(refused(hub.unlink(session.request())), Refusal::Expired);
}

#[test]
fn racing_releases_retire_an_input_once() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let signed = |coin: &Coin| {
        let spend = Spend::new(&keyset, std::slice::from_ref(coin), &[(bob.address(), 8)]);
        let grant = spend.offer(&hub).unwrap();
        let signed = spend.sign(&keyset, Authorization::Granted(Box::new(grant)));
        (signed, spend.notes())
    };
    let race = |releases: &[(SignedProposal, Vec<Note>)]| -> Vec<Result<_, Error>> {
        std::thread::scope(|scope| {
            let tasks: Vec<_> = releases
                .iter()
                .map(|(signed, notes)| scope.spawn(|| hub.release(signed, notes)))
                .collect();
            tasks.into_iter().map(|t| t.join().unwrap()).collect()
        })
    };
    // The same release four times at once.
    let same = signed(&mint(&files, &alice, 8));
    let unlocks: Vec<_> = race(&vec![same; 4])
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(unlocks.iter().all(|unlock| *unlock == unlocks[0]));
    // Four spends of one input at once.
    let coin = mint(&files, &alice, 8);
    let rivals: Vec<_> = (0..4).map(|_| signed(&coin)).collect();
    let results = race(&rivals);
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for result in results.into_iter().filter(Result::is_err) {
        assert_eq!(refused(result), Refusal::Spent);
    }
    let released = notices(&hub)
        .iter()
        .filter(|n| matches!(n, Notice::Released { .. }))
        .count();
    assert_eq!(released, 2);
}

#[test]
fn a_double_spend_is_alerted_once_per_proposal() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let coin = mint(&files, &alice, 8);
    let late = Spend::new(&keyset, std::slice::from_ref(&coin), &[(bob.address(), 8)]);
    let grant = late.offer(&hub).unwrap();
    Spend::new(&keyset, &[coin], &[(bob.address(), 8)]).complete(&hub, &keyset);
    let signed = late.sign(&keyset, Authorization::Granted(Box::new(grant)));
    for _ in 0..3 {
        assert_eq!(refused(hub.release(&signed, &late.notes())), Refusal::Spent);
    }
    let alerts = notices(&hub)
        .iter()
        .filter(|n| matches!(n, Notice::DoubleSpend { .. }))
        .count();
    assert_eq!(alerts, 1);
}

#[test]
fn an_empty_database_beside_a_seed_is_no_hub() {
    let files = files();
    let empty = files.db.with_file_name("empty.sqlite");
    std::fs::File::create(&empty).unwrap();
    let why = setup_error(Hub::open(&files.seed, &empty, Policy::default()));
    assert!(why.contains("schema 0"), "{why}");
    let other = files.db.with_file_name("other-seed");
    setup_error(Hub::init(&other, &empty, MAX_POW));
    assert!(!other.exists());
    assert!(Ledger::open(&empty).is_err());
}

#[test]
fn only_outputs_derived_for_the_hub_at_their_position_pay_its_fee() {
    let files = files();
    let hub = open(&files, fee(2, 0));
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let address = hub.info().address;
    let coin = mint(&files, &alice, 8);
    let spend = Spend::new(
        &keyset,
        std::slice::from_ref(&coin),
        &[(address, 2), (bob.address(), 4), (bob.address(), 2)],
    );
    // The hub's output moved to another position is not the hub's.
    let mut outputs = spend.proposal.outputs().to_vec();
    outputs.swap(0, 2);
    let moved = Proposal::new(
        &keyset,
        std::slice::from_ref(&coin.note),
        spend.proposal.ephemeral(),
        outputs,
        None,
    )
    .unwrap();
    assert_eq!(
        refused(hub.offer(&moved, std::slice::from_ref(&coin.note))),
        Refusal::Fee { due: 2, paid: 0 }
    );
    // Nor is one under another unlink commitment.
    let mut outputs = spend.proposal.outputs().to_vec();
    outputs[0] = Output::new(2, outputs[0].blinded(), [9; 32]).unwrap();
    let recommitted = Proposal::new(
        &keyset,
        std::slice::from_ref(&coin.note),
        spend.proposal.ephemeral(),
        outputs,
        None,
    )
    .unwrap();
    assert_eq!(
        refused(hub.offer(&recommitted, std::slice::from_ref(&coin.note))),
        Refusal::Fee { due: 2, paid: 0 }
    );
    spend.offer(&hub).unwrap();
}

#[test]
fn the_largest_denomination_is_spent_and_unlinked() {
    let dir = tempfile::tempdir().unwrap();
    let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
    Hub::init(&seed, &db, licash_hub::MAX_POW).unwrap();
    let hub = Hub::open(&seed, &db, Policy::default()).unwrap();
    let mut secret = [0; 32];
    hex::decode_to_slice(std::fs::read_to_string(&seed).unwrap().trim(), &mut secret).unwrap();
    let keys = IssuerKeys::from_seed(&secret, 0, licash_hub::MAX_POW).unwrap();
    let keyset = keys.keyset().unwrap();
    let (alice, bob) = (owner(1), owner(2));
    let top = 1u64 << licash_hub::MAX_POW;
    let spend = Spend::new(
        &keyset,
        &[mint_under(&keys, &keyset, &alice, top)],
        &[(bob.address(), top)],
    );
    let paid = spend.coin(&spend.complete(&hub, &keyset), 0, &bob);
    let session = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    let answer = hub.unlink(session.request()).unwrap();
    session.finish(&keyset, &answer).unwrap();
    assert!(
        matches!(notices(&hub).last(), Some(Notice::Released { amount, .. }) if *amount == top)
    );
    setup_error(Hub::init(
        &dir.path().join("s"),
        &dir.path().join("d"),
        licash_hub::MAX_POW + 1,
    ));
}

#[test]
fn events_of_unknown_kinds_are_read_as_stored() {
    let files = files();
    let hub = open(&files, Policy::default());
    sql(&files.db)
        .execute(
            "INSERT INTO events (at, kind, json) VALUES (1, 'future', '{\"kind\":\"future\"}'),
             (2, 'broken', 'not json')",
            [],
        )
        .unwrap();
    let events = hub.events(0, 10).unwrap();
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0].notice(), Some(Notice::Started { .. })));
    assert_eq!(events[1].kind, "future");
    assert_eq!(events[1].notice(), None);
    assert_eq!(events[2].data, serde_json::Value::from("not json"));
}

#[test]
fn the_ledger_reads_beside_the_running_hub_and_writes_nothing() {
    let files = files();
    let hub = open(&files, Policy::default());
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(
        &keyset,
        &[mint(&files, &alice, 8)],
        &[
            (bob.address(), 4),
            (bob.address(), 2),
            (hub.info().address, 2),
        ],
    );
    let paid = spend.coin(&spend.complete(&hub, &keyset), 0, &bob);
    let session = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    hub.unlink(session.request()).unwrap();
    let offer = |hub: &Hub| {
        Spend::new(&keyset, &[mint(&files, &alice, 4)], &[(bob.address(), 4)])
            .offer(hub)
            .unwrap()
    };
    offer(&hub);
    let ledger = Ledger::open(&files.db).unwrap();
    let stats = Stats {
        offers: 2,
        released: 1,
        locks: 0,
        open_locks: 0,
        spent: 1,
        issued: 3,
        unlinked: 1,
        income: 2,
        events: 2,
    };
    assert_eq!(ledger.stats().unwrap(), stats);
    assert_eq!(ledger.events(0, 10).unwrap(), hub.events(0, 10).unwrap());
    // The hub goes on writing beside it, and restarts beside it.
    offer(&hub);
    assert_eq!(ledger.stats().unwrap().offers, 3);
    drop(hub);
    let hub = open(&files, Policy::default());
    assert_eq!(ledger.stats().unwrap().events, 3);
    assert_eq!(ledger.last_event().unwrap(), 3);
    drop(hub);
    let seq = |db: &Path| -> i64 {
        sql(db)
            .query_row("SELECT seq FROM meta", [], |r| r.get(0))
            .unwrap()
    };
    let before = seq(&files.db);
    let ledger = Ledger::open(&files.db).unwrap();
    ledger.stats().unwrap();
    ledger.events(0, 10).unwrap();
    drop(ledger);
    assert_eq!(seq(&files.db), before);
}

#[cfg(unix)]
#[test]
fn one_hub_runs_on_a_database_by_any_path() {
    let files = files();
    let alias = files.db.with_file_name("alias.sqlite");
    std::os::unix::fs::symlink(&files.db, &alias).unwrap();
    let _hub = open(&files, Policy::default());
    let why = setup_error(Hub::open(&files.seed, &alias, Policy::default()));
    assert!(why.contains("another hub"), "{why}");
}

#[test]
fn the_ledger_reads_back_the_notes_it_moved_and_the_unlinks_it_answered() {
    let files = files();
    let hub = open(&files, fee(1, 0));
    let (_, keyset) = issuer(&files);
    let (alice, bob) = (owner(1), owner(2));
    let spend = Spend::new(
        &keyset,
        &[mint(&files, &alice, 8)],
        &[
            (bob.address(), 4),
            (alice.address(), 2),
            (hub.info().address, 1),
            (alice.address(), 1),
        ],
    );
    let notes = spend.complete(&hub, &keyset);
    // Bob unlinks his 4 and spends it, paying the hub again.
    let paid = spend.coin(&notes, 0, &bob);
    let session = Session::start(&keyset, &paid.note, &paid.derivation).unwrap();
    let answer = hub.unlink(session.request()).unwrap();
    let unlinked = Coin {
        note: session.finish(&keyset, &answer).unwrap(),
        ..paid
    };
    let swap = Spend::new(
        &keyset,
        &[unlinked],
        &[
            (bob.address(), 2),
            (bob.address(), 1),
            (hub.info().address, 1),
        ],
    );
    swap.complete(&hub, &keyset);

    let ledger = Ledger::open(&files.db).unwrap();
    assert!(ledger.created_at().unwrap() > 0);
    assert_eq!(ledger.tip().unwrap(), None);
    let Notes { movements, unlinks } = ledger.notes().unwrap();
    assert!(movements.iter().all(|m| m.keyset == 0));
    assert!(movements.windows(2).all(|w| w[0].at <= w[1].at));
    let moved = |issued: bool| {
        let mut moved: Vec<(u64, bool, bool)> = movements
            .iter()
            .filter(|m| m.issued == issued)
            .map(|m| (m.denom, m.hub, m.unlinked))
            .collect();
        moved.sort();
        moved
    };
    // Each spend's outputs, the hub's fee among them.
    assert_eq!(
        moved(true),
        [
            (1, false, false),
            (1, false, false),
            (1, true, false),
            (1, true, false),
            (2, false, false),
            (2, false, false),
            (4, false, false),
        ]
    );
    // The note issued before the ledger began, and Bob's unlinked 4.
    assert_eq!(moved(false), [(4, false, true), (8, false, false)]);
    assert_eq!(
        unlinks,
        [Unlinks {
            keyset: 0,
            denom: 4,
            answered: 1,
            retired: 1
        }]
    );
}
