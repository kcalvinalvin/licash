//! Timed locks: admission, settlement, refunds and the scan that decides
//! between them, on self-mined regtest blocks.

use std::path::PathBuf;

use bitcoin::{
    absolute::LockTime,
    block::{Header, Version},
    hashes::Hash,
    pow::CompactTarget,
    transaction, Amount, Block, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, SignedProposal},
    chain::{activation_script, Checkpoint},
    claim::Branch,
    note::{Derivation, IssuerKeys, Keyset, MintResponse, Note},
    spend::{lock_hash, ChainProfile, Output, Proposal, ProposalId, TimedLock, TimedTerms},
    unlink::{Nonce, Session},
};
use licash_hub::{
    Error, Excess, Fee, Forwarded, Hub, Ledger, Notice, Policy, Refusal, Settled, Timing,
};

const MAX_POW: u32 = 6;
const SECRET: [u8; 32] = [7; 32];

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

fn policy() -> Policy {
    Policy {
        lock: Fee { base: 1, ppm: 0 },
        timing: Timing {
            window: 50,
            reaction: 2,
            min_depth: 1,
            max_headers: 1000,
            bury: 3,
            max_lag: 3600,
        },
        ..Policy::default()
    }
}

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

/// A note of `denom` to `to`, issued before the hub's ledger began.
fn mint(files: &Files, to: &OwnerKeys, denom: u64) -> (Note, Derivation) {
    let (keys, keyset) = issuer(files);
    let d = Ephemeral::random()
        .derive(&to.address(), denom, Branch::Pay, 0)
        .unwrap();
    let output = Output::from_derivation(&d).unwrap();
    let note = Nonce::new(&keyset, &output)
        .unwrap()
        .issue(&keys)
        .unwrap()
        .open(&keyset, &d)
        .unwrap();
    (note, d)
}

/// The regtest chain the hub scans, genesis first.
struct Chain {
    blocks: Vec<Block>,
    /// Makes each block's coinbase unique, across forks too.
    mined: u32,
}

impl Chain {
    fn new() -> Self {
        Self {
            blocks: vec![bitcoin::constants::genesis_block(Network::Regtest)],
            mined: 0,
        }
    }

    fn height(&self) -> u32 {
        self.blocks.len() as u32 - 1
    }

    fn block(&mut self, activation: Option<[u8; 32]>) -> Block {
        self.mined += 1;
        let previous = self.blocks.last().unwrap().header;
        let mut output = vec![TxOut {
            value: Amount::from_sat(50),
            script_pubkey: ScriptBuf::new_op_return(self.mined.to_be_bytes()),
        }];
        if let Some(secret) = activation {
            output.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(activation_script(&secret)),
            });
        }
        let coinbase = Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![0x01, 0x01]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output,
        };
        let mut block = Block {
            header: Header {
                version: Version::from_consensus(0x2000_0000),
                prev_blockhash: previous.block_hash(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: previous.time + 600,
                bits: CompactTarget::from_consensus(0x207f_ffff),
                nonce: 0,
            },
            txdata: vec![coinbase],
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        while block.header.validate_pow(block.header.target()).is_err() {
            block.header.nonce += 1;
        }
        block
    }

    /// Mine `n` blocks and have the hub scan them.
    fn mine(&mut self, hub: &Hub, n: u32) {
        for _ in 0..n {
            let block = self.block(None);
            hub.observe(self.height() + 1, &block).unwrap();
            self.blocks.push(block);
        }
    }

    /// Mine a block that activates `secret`: its height.
    fn activate(&mut self, hub: &Hub, secret: [u8; 32]) -> u32 {
        let block = self.block(Some(secret));
        hub.observe(self.height() + 1, &block).unwrap();
        self.blocks.push(block);
        self.height()
    }

    /// Tell the hub the backend's tip is this chain's.
    fn sync(&self, hub: &Hub) {
        let tip = self.blocks.last().unwrap().block_hash().to_byte_array();
        assert!(hub.synced(self.height(), &tip).unwrap());
    }

    /// Drop the blocks from `height` on, for the hub too.
    fn reorg(&mut self, hub: &Hub, height: u32) {
        hub.rewind(height).unwrap();
        self.blocks.truncate(height as usize);
    }

    /// The checkpoint at `height`.
    fn checkpoint(&self, height: u32) -> Checkpoint {
        let headers = &self.blocks[..=height as usize];
        let times = headers
            .iter()
            .rev()
            .take(11)
            .rev()
            .map(|b| b.header.time)
            .collect();
        let bytes = bitcoin::consensus::serialize(&headers[height as usize].header);
        Checkpoint::new(
            ChainProfile::Core30DefaultRegtest,
            height,
            bytes.try_into().unwrap(),
            times,
            None,
        )
        .unwrap()
    }
}

/// A hub following a chain from genesis, scanned to height 10.
fn setup(policy: Policy) -> (Files, Hub, Chain) {
    let files = files();
    let hub = Hub::open(&files.seed, &files.db, policy).unwrap();
    let mut chain = Chain::new();
    hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
    chain.mine(&hub, 10);
    chain.sync(&hub);
    (files, hub, chain)
}

/// A payer's lock and the derivations of its two branches.
struct Locking {
    signed: SignedProposal,
    notes: Vec<Note>,
    pay: Vec<Derivation>,
    refund: Vec<Derivation>,
    /// The payer's note's.
    input: Derivation,
}

impl Locking {
    fn id(&self) -> ProposalId {
        self.signed.proposal().id()
    }
}

fn locking(
    files: &Files,
    pay: &[(Address, u64)],
    refund: &[(Address, u64)],
    terms: TimedTerms,
) -> Locking {
    let (_, keyset) = issuer(files);
    let payer = owner(1);
    let value = pay.iter().map(|(_, d)| d).sum();
    let (note, d) = mint(files, &payer, value);
    let e = Ephemeral::random();
    let derive = |outputs: &[(Address, u64)], branch| -> Vec<Derivation> {
        (0..)
            .zip(outputs)
            .map(|(i, (to, denom))| e.derive(to, *denom, branch, i).unwrap())
            .collect()
    };
    let (pay, refund) = (derive(pay, Branch::Pay), derive(refund, Branch::Refund));
    let outputs = |ds: &[Derivation]| -> Vec<Output> {
        ds.iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect()
    };
    let notes = vec![note];
    let lock = TimedLock::new(terms, outputs(&refund)).unwrap();
    let proposal = Proposal::new(&keyset, &notes, e.public(), outputs(&pay), Some(lock)).unwrap();
    let owners = [d.ownership().owner_secret(payer.spend_secret()).unwrap()];
    let signed =
        SignedProposal::sign(&keyset, proposal, &notes, Authorization::Plain, &owners).unwrap();
    Locking {
        signed,
        notes,
        pay,
        refund,
        input: d,
    }
}

/// Bob 2 and 1 and the hub 1 if it pays; the payer 4 if it refunds.
fn standard(files: &Files, hub: &Hub, terms: TimedTerms) -> Locking {
    let (bob, payer) = (owner(2).address(), owner(1).address());
    let pay = [(bob, 2), (bob, 1), (hub.info().address, 1)];
    locking(files, &pay, &[(payer, 4)], terms)
}

fn terms(hub: &Hub, expiry: u32, depth: u32) -> TimedTerms {
    let checkpoint = hub.checkpoint().unwrap().commitment();
    TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        checkpoint,
        expiry,
        depth,
        lock_hash(&SECRET),
    )
    .unwrap()
}

fn opened(keyset: &Keyset, issued: &[MintResponse], ds: &[Derivation]) -> Vec<Note> {
    assert_eq!(issued.len(), ds.len());
    issued
        .iter()
        .zip(ds)
        .map(|(i, d)| i.open(keyset, d).unwrap())
        .collect()
}

fn refused<T>(result: Result<T, Error>) -> Refusal {
    match result {
        Err(Error::Refused(refusal)) => refusal,
        Err(e) => panic!("not a refusal: {e}"),
        Ok(_) => panic!("not refused"),
    }
}

fn kinds(hub: &Hub) -> Vec<String> {
    let events = hub.events(0, 100).unwrap();
    events.into_iter().map(|e| e.kind).collect()
}

#[test]
fn a_lock_is_certified_once_and_paid_on_its_preimage() {
    let (files, hub, chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    let certificate = hub.lock(&l.signed, &l.notes).unwrap();
    certificate.verify(&keyset).unwrap();
    assert_eq!(certificate.metadata().source_id(), l.id());
    // Asked again, the same certificate; the inputs are spent.
    let again = hub.lock(&l.signed, &l.notes).unwrap();
    assert_eq!(again.to_bytes(), certificate.to_bytes());
    let lock = hub.locked(&l.id()).unwrap();
    assert_eq!((lock.settled, lock.preimage, lock.own), (None, None, false));
    assert!(lock.proposal == *l.signed.proposal());

    assert_eq!(refused(hub.settle(&l.id(), &[8; 32])), Refusal::Preimage);
    let paid = hub.settle(&l.id(), &SECRET).unwrap();
    let notes = opened(&keyset, &paid[..2], &l.pay[..2]);
    assert_eq!(
        notes
            .iter()
            .map(|n| n.message().denomination())
            .sum::<u64>(),
        3
    );
    // Settled again, the same issuances; never refunded.
    let replayed = hub.settle(&l.id(), &SECRET).unwrap();
    assert_eq!(
        replayed
            .iter()
            .map(MintResponse::to_bytes)
            .collect::<Vec<_>>(),
        paid.iter().map(MintResponse::to_bytes).collect::<Vec<_>>()
    );
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Settled);
    let lock = hub.locked(&l.id()).unwrap();
    assert_eq!(
        (lock.settled, lock.preimage),
        (Some(Settled::Pay), Some(SECRET))
    );

    let stats = Ledger::open(&files.db).unwrap().stats().unwrap();
    assert_eq!((stats.locks, stats.open_locks, stats.spent), (1, 0, 1));
    // Three outputs to Bob and the hub; the refund branch stays reserved.
    assert_eq!((stats.issued, stats.income), (3, 1));
    assert_eq!(kinds(&hub), ["started", "locked", "settled"]);
    let settled = hub.events(0, 10).unwrap()[2].notice();
    assert_eq!(
        settled,
        Some(Notice::Settled {
            proposal: hex::encode(l.id().as_bytes()),
            amount: 4,
            fee: 1,
            branch: Settled::Pay
        })
    );
}

#[test]
fn each_branch_pays_its_own_fee() {
    let (files, hub, chain) = setup(Policy {
        refund: Fee { base: 1, ppm: 0 },
        ..policy()
    });
    let expiry = chain.height() + 5;
    let (bob, payer, me) = (owner(2).address(), owner(1).address(), hub.info().address);
    // The pay branch's fee, but nothing in the refund branch.
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    assert_eq!(
        refused(hub.lock(&l.signed, &l.notes)),
        Refusal::Fee { due: 1, paid: 0 }
    );
    // The refund branch's fee, but none in the pay branch.
    let pay = [(bob, 4)];
    let l = locking(
        &files,
        &pay,
        &[(payer, 2), (payer, 1), (me, 1)],
        terms(&hub, expiry, 1),
    );
    assert_eq!(
        refused(hub.lock(&l.signed, &l.notes)),
        Refusal::Fee { due: 1, paid: 0 }
    );
    // Both.
    let pay = [(owner(2).address(), 2), (owner(2).address(), 1), (me, 1)];
    let refund = [(owner(1).address(), 2), (owner(1).address(), 1), (me, 1)];
    let l = locking(&files, &pay, &refund, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    let stats = Ledger::open(&files.db).unwrap().stats().unwrap();
    assert_eq!((stats.locks, stats.income), (1, 0));
}

#[test]
fn each_branch_pays_for_the_notes_it_makes_for_others_than_the_hub() {
    // 1 in the pay branch, and a sat a note in either.
    let (files, hub, chain) = setup(Policy {
        output_msat: 1_000,
        ..policy()
    });
    let expiry = chain.height() + 5;
    let (bob, payer, me) = (owner(2).address(), owner(1).address(), hub.info().address);
    // Two notes for Bob: 3 with the pay branch's fee.
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    assert_eq!(
        refused(hub.lock(&l.signed, &l.notes)),
        Refusal::Fee { due: 3, paid: 1 }
    );
    // One note for Bob paid for, but three for the payer in the refund
    // branch, which pays 1.
    let pay = [(bob, 4), (me, 2), (me, 2)];
    let l = locking(
        &files,
        &pay,
        &[(payer, 4), (payer, 2), (payer, 1), (me, 1)],
        terms(&hub, expiry, 1),
    );
    assert_eq!(
        refused(hub.lock(&l.signed, &l.notes)),
        Refusal::Fee { due: 3, paid: 1 }
    );
    // Each branch pays for its own notes.
    let l = locking(
        &files,
        &pay,
        &[(payer, 4), (payer, 2), (me, 2)],
        terms(&hub, expiry, 1),
    );
    hub.lock(&l.signed, &l.notes).unwrap();
}

#[test]
fn a_refund_waits_for_the_expiry_at_depth_and_excludes_payment() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let (expiry, depth) = (chain.height() + 5, 2);
    let l = standard(&files, &hub, terms(&hub, expiry, depth));
    hub.lock(&l.signed, &l.notes).unwrap();
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Unexpired);
    chain.mine(&hub, expiry - chain.height());
    // Expired: Bob's payment is too late, and the refund not due yet.
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::LockExpired);
    chain.mine(&hub, depth - 1);
    assert_eq!(chain.height(), expiry + depth - 1);
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Unexpired);
    chain.mine(&hub, 1);
    let refunded = hub.refund(&l.id()).unwrap();
    let notes = opened(&keyset, &refunded, &l.refund);
    assert_eq!(notes[0].message().denomination(), 4);
    let again = hub.refund(&l.id()).unwrap();
    assert_eq!(again[0].to_bytes(), refunded[0].to_bytes());
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Settled);
    assert_eq!(hub.locked(&l.id()).unwrap().settled, Some(Settled::Refund));
    let stats = Ledger::open(&files.db).unwrap().stats().unwrap();
    assert_eq!((stats.issued, stats.income, stats.open_locks), (1, 0, 0));
}

#[test]
fn a_hub_behind_the_chain_admits_no_lock_and_pays_no_third_party() {
    let (files, hub, chain) = setup(Policy {
        timing: Timing {
            max_lag: 1,
            ..policy().timing
        },
        ..policy()
    });
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let other = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    assert_eq!(
        refused(hub.lock(&other.signed, &other.notes)),
        Refusal::Unsynced
    );
    // The lock may have expired on a chain the hub has not scanned.
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Unsynced);
    // The backend is ahead of the scan.
    let ahead = hub.synced(chain.height() + 1, &[1; 32]).unwrap();
    assert!(!ahead);
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Unsynced);
    chain.sync(&hub);
    hub.settle(&l.id(), &SECRET).unwrap();
}

/// A rewind of the scan, however shallow, takes the tip it was synced at:
/// no lock is admitted and no third party paid, whatever the time since,
/// until the backend's tip is reached again. Blocks scanned past that tip
/// keep it synced.
#[test]
fn a_hub_rewound_below_its_synced_tip_admits_no_lock_and_pays_no_third_party() {
    let (files, hub, mut chain) = setup(policy());
    let expiry = chain.height() + 6;
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    chain.mine(&hub, 2);
    chain.sync(&hub);
    // Scanned past the synced tip: the same chain, longer.
    chain.mine(&hub, 1);
    let other = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&other.signed, &other.notes).unwrap();
    // Rewound below it: the lock may have expired on the backend's chain.
    let height = chain.height();
    chain.reorg(&hub, height - 1);
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Unsynced);
    let third = standard(&files, &hub, terms(&hub, expiry, 1));
    assert_eq!(
        refused(hub.lock(&third.signed, &third.notes)),
        Refusal::Unsynced
    );
    // Nor is a payment forwarded for a lock to the hub.
    let own = to_hub(&files, &hub, terms(&hub, expiry, 1));
    chain.mine(&hub, 1);
    chain.sync(&hub);
    hub.lock(&own.signed, &own.notes).unwrap();
    chain.reorg(&hub, chain.height());
    let hash = lock_hash(&SECRET);
    assert_eq!(
        refused(hub.forward(&own.id(), &hash, "lnbcrt1", "node", 4)),
        Refusal::Unsynced
    );
    chain.mine(&hub, 1);
    chain.sync(&hub);
    hub.settle(&l.id(), &SECRET).unwrap();
}

#[test]
fn an_activation_in_time_pays_even_once_its_block_is_reorganized_away() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let expiry = chain.height() + 5;
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    let height = chain.activate(&hub, SECRET);
    assert_eq!(hub.locked(&l.id()).unwrap().preimage, Some(SECRET));
    let activated = hub.events(0, 10).unwrap().pop().unwrap().notice();
    let proposal = hex::encode(l.id().as_bytes());
    let refunded = false;
    assert_eq!(
        activated,
        Some(Notice::Activated {
            proposal,
            height,
            refunded
        })
    );
    // The block leaves the best chain; the lock expires at depth on the new one.
    chain.reorg(&hub, height);
    chain.mine(&hub, expiry + 3 - chain.height());
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Activated);
    // Bob is paid after the expiry: the activation made the lock pay.
    let paid = hub.settle(&l.id(), &SECRET).unwrap();
    opened(&keyset, &paid[..2], &l.pay[..2]);
}

#[test]
fn an_activation_scanned_before_the_lock_counts() {
    let (files, hub, mut chain) = setup(policy());
    chain.activate(&hub, SECRET);
    let expiry = chain.height() + 5;
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    assert_eq!(hub.locked(&l.id()).unwrap().preimage, Some(SECRET));
    chain.mine(&hub, expiry + 1 - chain.height());
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Activated);
}

#[test]
fn an_activation_after_the_expiry_does_not_stop_the_refund() {
    let (files, hub, mut chain) = setup(policy());
    let expiry = chain.height() + 3;
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    chain.mine(&hub, expiry - chain.height());
    chain.activate(&hub, SECRET);
    assert_eq!(hub.locked(&l.id()).unwrap().preimage, Some(SECRET));
    hub.refund(&l.id()).unwrap();
}

#[test]
fn the_hub_as_payee_settles_after_the_expiry() {
    let (files, hub, mut chain) = setup(policy());
    let expiry = chain.height() + 3;
    let pay = [(hub.info().address, 4)];
    let l = locking(
        &files,
        &pay,
        &[(owner(1).address(), 4)],
        terms(&hub, expiry, 1),
    );
    hub.lock(&l.signed, &l.notes).unwrap();
    chain.mine(&hub, expiry - chain.height());
    hub.settle(&l.id(), &SECRET).unwrap();
    let stats = Ledger::open(&files.db).unwrap().stats().unwrap();
    assert_eq!(stats.income, 4);
}

#[test]
fn admission_checks_the_terms_against_the_chain_and_the_timing() {
    let (files, hub, chain) = setup(policy());
    let tip = chain.height();
    let refusal = |terms: TimedTerms| {
        let l = standard(&files, &hub, terms);
        refused(hub.lock(&l.signed, &l.notes))
    };
    let at = |checkpoint: [u8; 32], expiry, depth| {
        let hash = lock_hash(&SECRET);
        TimedTerms::new(
            ChainProfile::Core30DefaultRegtest,
            checkpoint,
            expiry,
            depth,
            hash,
        )
        .unwrap()
    };
    let unknown = chain.checkpoint(5).commitment();
    assert_eq!(
        refusal(at(unknown, tip + 5, 1)),
        Refusal::Checkpoint("unknown")
    );
    let mainnet = TimedTerms::new(
        ChainProfile::Core30Mainnet,
        hub.checkpoint().unwrap().commitment(),
        tip + 5,
        1,
        lock_hash(&SECRET),
    )
    .unwrap();
    assert_eq!(refusal(mainnet), Refusal::Checkpoint("another chain"));
    let deep = Policy {
        timing: Timing {
            min_depth: 2,
            ..policy().timing
        },
        ..policy()
    };
    drop(hub);
    let hub = Hub::open(&files.seed, &files.db, deep).unwrap();
    chain.sync(&hub);
    let refusal = |terms: TimedTerms| {
        let l = standard(&files, &hub, terms);
        refused(hub.lock(&l.signed, &l.notes))
    };
    let checkpoint = hub.checkpoint().unwrap().commitment();
    let timing = |reason| Refusal::Timing(reason);
    assert_eq!(
        refusal(at(checkpoint, tip + 5, 1)),
        timing("depth below the hub's least")
    );
    let window = timing("expiry outside the hub's response window");
    assert_eq!(refusal(at(checkpoint, tip, 2)), window);
    // Expiry, depth and reaction within the window of 50 from the tip.
    assert_eq!(refusal(at(checkpoint, tip + 47, 2)), window);
    let l = standard(&files, &hub, at(checkpoint, tip + 46, 2));
    hub.lock(&l.signed, &l.notes).unwrap();
    // A plain spend is not a lock, and a lock is no plain spend.
    let p = Proposal::new(
        &issuer(&files).1,
        &l.notes,
        l.signed.proposal().ephemeral(),
        l.signed.proposal().outputs().to_vec(),
        None,
    )
    .unwrap();
    assert_eq!(refused(hub.offer(&p, &l.notes)), Refusal::Spent);
    let owners = [l
        .input
        .ownership()
        .owner_secret(owner(1).spend_secret())
        .unwrap()];
    let keyset = issuer(&files).1;
    let plain = SignedProposal::sign(&keyset, p, &l.notes, Authorization::Plain, &owners).unwrap();
    assert_eq!(refused(hub.lock(&plain, &l.notes)), Refusal::NotALock);
    let other = standard(&files, &hub, at(checkpoint, tip + 5, 2));
    let timed = other.signed.proposal();
    assert_eq!(refused(hub.offer(timed, &other.notes)), Refusal::Timed);
}

#[test]
fn own_locks_are_live_until_settled_or_refundable() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let alice = owner(3).address();
    let expiry = chain.height() + 5;
    let lock = |hash: [u8; 32]| {
        let e = Ephemeral::random();
        let d = e.derive(&alice, 2, Branch::Pay, 0).unwrap();
        let checkpoint = hub.checkpoint().unwrap().commitment();
        let terms = TimedTerms::new(
            ChainProfile::Core30DefaultRegtest,
            checkpoint,
            expiry,
            1,
            hash,
        )
        .unwrap();
        let outputs = vec![Output::from_derivation(&d).unwrap()];
        hub.lock_own(keyset.id(), e.public(), outputs, terms)
            .unwrap()
            .proposal
            .id()
    };
    let live = || -> Vec<ProposalId> {
        let locks = hub.live_own_locks().unwrap();
        let mut ids: Vec<_> = locks.iter().map(|l| l.proposal.id()).collect();
        ids.sort();
        ids
    };
    assert!(live().is_empty());
    let (paid, expiring) = (lock(lock_hash(&SECRET)), lock(lock_hash(&[9; 32])));
    let mut both = vec![paid, expiring];
    both.sort();
    assert_eq!(live(), both);
    hub.settle(&paid, &SECRET).unwrap();
    assert_eq!(live(), [expiring]);
    // Expired, unactivated, but not yet at depth: still payable on an
    // activation in time.
    chain.mine(&hub, expiry - chain.height());
    assert_eq!(live(), [expiring]);
    // At depth: refundable, though not refunded yet.
    chain.mine(&hub, 1);
    assert!(live().is_empty());
    assert_eq!(hub.locked(&expiring).unwrap().settled, None);
}

#[test]
fn own_locks_pay_the_payee_and_refund_nothing() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let alice = owner(3).address();
    let e = Ephemeral::random();
    let pay: Vec<Derivation> = [4, 1]
        .iter()
        .enumerate()
        .map(|(i, d)| e.derive(&alice, *d, Branch::Pay, i).unwrap())
        .collect();
    let outputs: Vec<Output> = pay
        .iter()
        .map(|d| Output::from_derivation(d).unwrap())
        .collect();
    let expiry = chain.height() + 5;
    let lock = hub
        .lock_own(
            keyset.id(),
            e.public(),
            outputs.clone(),
            terms(&hub, expiry, 1),
        )
        .unwrap();
    assert!(lock.own);
    let id = lock.proposal.id();
    lock.certificate.verify(&keyset).unwrap();
    assert_eq!(lock.certificate.metadata().source_id(), id);
    // Its owner signatures prove it on Bitcoin with its certificate, as a
    // payer's lock: its payee activates it should the hub keep its notes.
    let signatures = hub.lock_signatures(&id).unwrap();
    assert!(!signatures.is_empty());
    (lock.certificate)
        .authenticate_source(&keyset, &lock.proposal, &signatures)
        .unwrap();
    // The same request gets the same lock; another under the hash none.
    let again = hub
        .lock_own(
            keyset.id(),
            e.public(),
            outputs.clone(),
            terms(&hub, expiry, 1),
        )
        .unwrap();
    assert_eq!(again.proposal.id(), id);
    let fewer = outputs[..1].to_vec();
    let refusal = refused(hub.lock_own(keyset.id(), e.public(), fewer, terms(&hub, expiry, 1)));
    assert_eq!(refusal, Refusal::Exists);
    // Alice reveals the preimage and is paid.
    let paid = hub.settle(&id, &SECRET).unwrap();
    let notes = opened(&keyset, &paid, &pay);
    assert_eq!(
        notes
            .iter()
            .map(|n| n.message().denomination())
            .sum::<u64>(),
        5
    );

    // Another expires: the hub refunds itself and issues nothing.
    let hash = lock_hash(&[9; 32]);
    let checkpoint = hub.checkpoint().unwrap().commitment();
    let other = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        checkpoint,
        expiry,
        1,
        hash,
    )
    .unwrap();
    let e = Ephemeral::random();
    let d = e.derive(&alice, 2, Branch::Pay, 0).unwrap();
    let lock = hub
        .lock_own(
            keyset.id(),
            e.public(),
            vec![Output::from_derivation(&d).unwrap()],
            other,
        )
        .unwrap();
    let id = lock.proposal.id();
    chain.mine(&hub, expiry + 1 - chain.height());
    assert!(hub.refund(&id).unwrap().is_empty());
    assert_eq!(hub.locked(&id).unwrap().settled, Some(Settled::Refund));
    assert_eq!(refused(hub.settle(&id, &[9; 32])), Refusal::Settled);
    let stats = Ledger::open(&files.db).unwrap().stats().unwrap();
    // Alice's two outputs; the hub's own inputs are only spent.
    assert_eq!(
        (stats.locks, stats.issued, stats.spent, stats.income),
        (2, 2, 3, 0)
    );
    let own: Vec<_> = hub
        .events(0, 10)
        .unwrap()
        .iter()
        .filter_map(|e| match e.notice() {
            Some(Notice::Locked { own, fee, .. }) => Some((own, fee)),
            _ => None,
        })
        .collect();
    assert_eq!(own, [(true, 0), (true, 0)]);
}

#[test]
fn a_lock_reserves_its_outputs_which_unlink_once_issued() {
    let free = Policy {
        lock: Fee::default(),
        ..policy()
    };
    let (files, hub, chain) = setup(free);
    let (_, keyset) = issuer(&files);
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    // A lock reusing the payer's refund output is refused.
    let (bob, payer) = (owner(2).address(), owner(1).address());
    let copy = locking(
        &files,
        &[(bob, 4)],
        &[(payer, 4)],
        terms(&hub, chain.height() + 5, 1),
    );
    let (signed, notes) = (&copy.signed, &copy.notes);
    let reused = Proposal::new(
        &keyset,
        notes,
        signed.proposal().ephemeral(),
        l.signed.proposal().lock().unwrap().refund().to_vec(),
        Some(signed.proposal().lock().unwrap().clone()),
    )
    .unwrap();
    let owners = [copy
        .input
        .ownership()
        .owner_secret(owner(1).spend_secret())
        .unwrap()];
    let reused =
        SignedProposal::sign(&keyset, reused, notes, Authorization::Plain, &owners).unwrap();
    assert_eq!(refused(hub.lock(&reused, notes)), Refusal::Reissued);

    let paid = hub.settle(&l.id(), &SECRET).unwrap();
    let note = paid[0].open(&keyset, &l.pay[0]).unwrap();
    let session = Session::start(&keyset, &note, &l.pay[0]).unwrap();
    let answer = hub.unlink(session.request()).unwrap();
    let fresh = session.finish(&keyset, &answer).unwrap();
    assert_eq!(fresh.message().denomination(), 2);
    // The refund branch was reserved, never issued: a note of it from
    // the keys alone does not unlink.
    let (keys, _) = issuer(&files);
    let refund = Output::from_derivation(&l.refund[0]).unwrap();
    let forged = Nonce::new(&keyset, &refund)
        .unwrap()
        .issue(&keys)
        .unwrap()
        .open(&keyset, &l.refund[0])
        .unwrap();
    let session = Session::start(&keyset, &forged, &l.refund[0]).unwrap();
    assert_eq!(refused(hub.unlink(session.request())), Refusal::NotIssued);
}

#[test]
fn the_scan_extends_its_tip_from_checkpoints_that_are_buried_blocks_of_it() {
    let files = files();
    let hub = Hub::open(&files.seed, &files.db, policy()).unwrap();
    let mut chain = Chain::new();
    let first = chain.block(None);
    assert_eq!(refused(hub.observe(1, &first)), Refusal::NoChain);
    assert!(hub.scanned().unwrap().is_none() && hub.checkpoint().is_none());
    hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
    hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
    chain.mine(&hub, 6);
    // Scanned already, ignored; elsewhere refused.
    hub.observe(3, &chain.blocks[3]).unwrap();
    let stray = chain.block(None);
    let block = |reason| Refusal::Block(reason);
    assert_eq!(
        refused(hub.observe(8, &stray)),
        block("does not extend the scanned tip")
    );
    let elsewhere = block("another block at its height: rewind first");
    assert_eq!(refused(hub.observe(3, &stray)), elsewhere);
    let mut forged = stray.clone();
    forged.txdata[0].output[0].value = Amount::from_sat(1);
    assert_eq!(refused(hub.observe(7, &forged)), block("merkle root"));

    // Checkpoints: scanned blocks, buried.
    assert_eq!(
        refused(hub.add_checkpoint(&chain.checkpoint(5))),
        Refusal::Checkpoint("not buried")
    );
    let mut fork = Chain {
        blocks: chain.blocks[..2].to_vec(),
        mined: 100,
    };
    for _ in 0..2 {
        let b = fork.block(None);
        fork.blocks.push(b);
    }
    assert_eq!(
        refused(hub.add_checkpoint(&fork.checkpoint(2))),
        Refusal::Checkpoint("not in the scanned chain")
    );
    hub.add_checkpoint(&chain.checkpoint(4)).unwrap();
    assert_eq!(hub.checkpoint().unwrap().height(), 4);

    // A reorganization of a checkpoint is refused, and alerted once.
    chain.sync(&hub);
    let deep = block("a reorganization of a checkpoint");
    assert_eq!(refused(hub.rewind(4)), deep);
    // The scan stopped: the hub is not synced.
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    assert_eq!(refused(hub.lock(&l.signed, &l.notes)), Refusal::Unsynced);
    assert_eq!(refused(hub.rewind(2)), deep);
    chain.reorg(&hub, 5);
    assert_eq!(hub.scanned().unwrap().unwrap().0, 4);
    chain.mine(&hub, 2);
    let reorgs: Vec<_> = kinds(&hub).into_iter().filter(|k| k == "reorg").collect();
    assert_eq!(reorgs.len(), 1);
    let tip = (6, chain.blocks[6].block_hash().to_byte_array());

    // A restart keeps the scan and the checkpoints; a lock deciding past
    // the checkpoint's header budget is refused.
    drop(hub);
    let tight = Policy {
        timing: Timing {
            max_headers: 5,
            ..policy().timing
        },
        ..policy()
    };
    let hub = Hub::open(&files.seed, &files.db, tight).unwrap();
    assert_eq!(hub.scanned().unwrap(), Some(tip));
    assert_eq!(
        hub.scanned_at(5).unwrap(),
        Some(chain.blocks[5].block_hash().to_byte_array())
    );
    assert_eq!(hub.checkpoint().unwrap().height(), 4);
    chain.sync(&hub);
    let l = standard(&files, &hub, terms(&hub, 7, 1));
    let budget = Refusal::Timing("beyond the checkpoint's header budget");
    assert_eq!(refused(hub.lock(&l.signed, &l.notes)), budget);
}

#[test]
fn locks_settlements_and_refunds_replay_after_a_restart() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let expiry = chain.height() + 5;
    let paid = standard(&files, &hub, terms(&hub, expiry, 1));
    let certificate = hub.lock(&paid.signed, &paid.notes).unwrap();
    let settled = hub.settle(&paid.id(), &SECRET).unwrap();
    let back = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&back.signed, &back.notes).unwrap();
    let alice = owner(3).address();
    let e = Ephemeral::random();
    let d = e.derive(&alice, 2, Branch::Pay, 0).unwrap();
    let own = vec![Output::from_derivation(&d).unwrap()];
    let own_terms = || {
        let checkpoint = hub.checkpoint().unwrap().commitment();
        let hash = lock_hash(&[9; 32]);
        TimedTerms::new(
            ChainProfile::Core30DefaultRegtest,
            checkpoint,
            expiry,
            1,
            hash,
        )
        .unwrap()
    };
    let own_lock = hub
        .lock_own(keyset.id(), e.public(), own.clone(), own_terms())
        .unwrap();
    let own_terms = own_terms();
    chain.mine(&hub, expiry + 1 - chain.height());
    let refunded = hub.refund(&back.id()).unwrap();

    // Restarted under a dearer policy, behind the chain.
    drop(hub);
    let dearer = Policy {
        lock: Fee { base: 5, ppm: 0 },
        ..policy()
    };
    let hub = Hub::open(&files.seed, &files.db, dearer).unwrap();
    let again = hub.lock(&paid.signed, &paid.notes).unwrap();
    assert_eq!(again.to_bytes(), certificate.to_bytes());
    let bytes = |issued: &[MintResponse]| -> Vec<Vec<u8>> {
        issued.iter().map(MintResponse::to_bytes).collect()
    };
    let replayed = hub.settle(&paid.id(), &SECRET).unwrap();
    assert_eq!(bytes(&replayed), bytes(&settled));
    opened(&keyset, &replayed[..2], &paid.pay[..2]);
    assert_eq!(bytes(&hub.refund(&back.id()).unwrap()), bytes(&refunded));
    let same = hub
        .lock_own(keyset.id(), e.public(), own, own_terms)
        .unwrap();
    assert_eq!(same.proposal.id(), own_lock.proposal.id());
}

#[test]
fn a_refunded_lock_activated_in_time_after_a_reorganization_is_alerted() {
    let (files, hub, mut chain) = setup(policy());
    let expiry = chain.height() + 3;
    let l = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    chain.mine(&hub, expiry + 1 - chain.height());
    // Activated after the expiry: no claim on the lock, no alert.
    chain.activate(&hub, SECRET);
    hub.refund(&l.id()).unwrap();
    assert!(!kinds(&hub).contains(&"activated".to_owned()));
    // A reorganization brings an activation before the expiry.
    chain.reorg(&hub, expiry - 1);
    let height = chain.activate(&hub, SECRET);
    assert!(height < expiry);
    let alert = hub.events(0, 100).unwrap().pop().unwrap().notice();
    let proposal = hex::encode(l.id().as_bytes());
    let refunded = true;
    assert_eq!(
        alert,
        Some(Notice::Activated {
            proposal,
            height,
            refunded
        })
    );
    // The same block scanned again after a reorganization back alerts no more.
    let block = chain.blocks.pop().unwrap();
    hub.rewind(height).unwrap();
    hub.observe(height, &block).unwrap();
    let alerts = kinds(&hub).iter().filter(|k| *k == "activated").count();
    assert_eq!(alerts, 1);
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Settled);
}

/// A lock of 4 paying the hub only, refunding the payer 4.
fn to_hub(files: &Files, hub: &Hub, terms: TimedTerms) -> Locking {
    let pay = [(hub.info().address, 4)];
    locking(files, &pay, &[(owner(1).address(), 4)], terms)
}

#[test]
fn a_forward_settles_its_lock_on_the_preimage_it_bought() {
    let (files, hub, mut chain) = setup(policy());
    let expiry = chain.height() + 5;
    let l = to_hub(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    assert!(matches!(
        refused(hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 5)),
        Refusal::Fee { due: 5, paid: 4 }
    ));
    let forward = hub
        .forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    assert_eq!(
        (forward.hash, forward.amount, forward.expiry, &forward.state),
        (lock_hash(&SECRET), 4, expiry, &Forwarded::Pending)
    );
    assert_eq!(
        hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
            .unwrap(),
        forward
    );
    assert!(matches!(
        refused(hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt2", "node", 4)),
        Refusal::Forward(_)
    ));
    assert_eq!(hub.forwards().unwrap(), vec![forward]);
    // One live payment per hash.
    let other = to_hub(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&other.signed, &other.notes).unwrap();
    assert!(matches!(
        refused(hub.forward(&other.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)),
        Refusal::Forward(_)
    ));

    // While the payment is under way, the lock is not refunded.
    chain.mine(&hub, 6);
    chain.sync(&hub);
    assert_eq!(refused(hub.refund(&l.id())), Refusal::Forwarding);
    assert_eq!(
        refused(hub.forward_paid(&l.id(), &[8; 32])),
        Refusal::Preimage
    );
    hub.forward_paid(&l.id(), &SECRET).unwrap();
    hub.forward_paid(&l.id(), &SECRET).unwrap();
    let lock = hub.locked(&l.id()).unwrap();
    assert_eq!(
        (lock.settled, lock.preimage),
        (Some(Settled::Pay), Some(SECRET))
    );
    assert_eq!(
        hub.forward_of(&l.id()).unwrap().unwrap().state,
        Forwarded::Paid(SECRET)
    );
    assert!(hub.forwards().unwrap().is_empty());
    assert_eq!(
        refused(hub.forward_failed(&l.id(), "late")),
        Refusal::Settled
    );
    assert_eq!(Ledger::open(&files.db).unwrap().stats().unwrap().income, 4);
    let forwarded = hub.events(0, 100).unwrap();
    let forwarded: Vec<_> = forwarded.iter().filter_map(|e| e.notice()).collect();
    assert!(forwarded.contains(&Notice::Forwarded {
        proposal: hex::encode(l.id().as_bytes()),
        amount: 4,
        failed: None,
    }));
    // Expired, the other lock is forwarded no more.
    assert_eq!(
        refused(hub.forward(&other.id(), &lock_hash(&SECRET), "lnbcrt3", "node", 4)),
        Refusal::LockExpired
    );
}

#[test]
fn a_failed_forward_refunds_its_lock_at_once() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let l = to_hub(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    hub.forward_failed(&l.id(), "no route").unwrap();
    hub.forward_failed(&l.id(), "no route").unwrap();
    assert_eq!(
        hub.forward_of(&l.id()).unwrap().unwrap().state,
        Forwarded::Failed("no route".into())
    );
    assert_eq!(hub.locked(&l.id()).unwrap().settled, Some(Settled::Refund));
    let refunded = hub.refund(&l.id()).unwrap();
    assert_eq!(opened(&keyset, &refunded, &l.refund).len(), 1);
    // Neither made again nor paid: paid after all, the hub alerts.
    assert!(matches!(
        refused(hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)),
        Refusal::Forward(_)
    ));
    assert!(!kinds(&hub).contains(&"alert".to_owned()));
    assert!(matches!(
        refused(hub.forward_paid(&l.id(), &SECRET)),
        Refusal::Forward(_)
    ));
    assert!(kinds(&hub).contains(&"alert".to_owned()));
    assert_eq!(refused(hub.settle(&l.id(), &SECRET)), Refusal::Settled);
    // Its hash activated in time owes no one: the hub gave up its claim.
    chain.activate(&hub, SECRET);
    assert!(!kinds(&hub).contains(&"activated".to_owned()));
    // A failed payment leaves its hash free.
    let again = to_hub(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&again.signed, &again.notes).unwrap();
    hub.forward(&again.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    assert!(kinds(&hub).contains(&"forwarded".to_owned()));
}

#[test]
fn a_refused_forward_is_recorded_failed_with_its_refund_never_pending() {
    let (files, hub, mut chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let l = to_hub(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    // Whatever the lock pays: refused before paying anything.
    hub.forward_refused(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", "too much")
        .unwrap();
    let forward = hub.forward_of(&l.id()).unwrap().unwrap();
    assert_eq!(forward.state, Forwarded::Failed("too much".into()));
    assert!(hub.forwards().unwrap().is_empty(), "none to start");
    assert_eq!(hub.locked(&l.id()).unwrap().settled, Some(Settled::Refund));
    let refunded = hub.refund(&l.id()).unwrap();
    assert_eq!(opened(&keyset, &refunded, &l.refund).len(), 1);
    assert!(kinds(&hub).contains(&"forwarded".to_owned()));
    // Refused again, or forwarded after all: as a failed forward.
    for again in [
        hub.forward_refused(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", "too much"),
        hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 0)
            .map(drop),
    ] {
        assert!(matches!(refused(again), Refusal::Forward(_)));
    }
    // A lock the engine would not forward is refused as is: nothing recorded.
    let late = to_hub(&files, &hub, terms(&hub, chain.height() + 2, 1));
    hub.lock(&late.signed, &late.notes).unwrap();
    chain.mine(&hub, 2);
    assert_eq!(
        refused(hub.forward_refused(&late.id(), &lock_hash(&SECRET), "lnbcrt2", "node", "no")),
        Refusal::LockExpired
    );
    assert_eq!(hub.forward_of(&late.id()).unwrap(), None);
    assert_eq!(hub.locked(&late.id()).unwrap().settled, None);
}

#[test]
fn only_a_lock_paying_the_hub_alone_is_forwarded() {
    let (files, hub, chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    assert!(matches!(
        refused(hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 1)),
        Refusal::Forward(_)
    ));
    assert_eq!(
        refused(hub.forward(
            &ProposalId::from_bytes([9; 32]),
            &lock_hash(&SECRET),
            "lnbcrt1",
            "node",
            1
        )),
        Refusal::UnknownLock
    );
    let e = Ephemeral::random();
    let d = e.derive(&owner(3).address(), 4, Branch::Pay, 0).unwrap();
    let own = hub
        .lock_own(
            keyset.id(),
            e.public(),
            vec![Output::from_derivation(&d).unwrap()],
            terms(&hub, chain.height() + 5, 1),
        )
        .unwrap();
    let id = own.proposal.id();
    assert_eq!(
        hub.own_lock_of(&lock_hash(&SECRET))
            .unwrap()
            .unwrap()
            .proposal
            .id(),
        id
    );
    assert!(hub.own_lock_of(&[0; 32]).unwrap().is_none());
    assert!(matches!(
        refused(hub.forward(&id, &lock_hash(&SECRET), "lnbcrt1", "node", 1)),
        Refusal::Forward(_)
    ));
}

#[test]
fn a_forward_is_paid_by_its_lock_settled_from_elsewhere() {
    let (files, hub, chain) = setup(policy());
    let l = to_hub(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&l.signed, &l.notes).unwrap();
    assert!(matches!(
        refused(hub.forward(&l.id(), &[8; 32], "lnbcrt1", "node", 4)),
        Refusal::Forward(_)
    ));
    hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    hub.settle(&l.id(), &SECRET).unwrap();
    assert_eq!(
        hub.forward_of(&l.id()).unwrap().unwrap().state,
        Forwarded::Paid(SECRET)
    );
    assert!(hub.forwards().unwrap().is_empty());
    assert_eq!(
        refused(hub.forward_failed(&l.id(), "late")),
        Refusal::Settled
    );
    assert_eq!(
        refused(hub.forward(&l.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)),
        Refusal::Settled
    );
    hub.forward_paid(&l.id(), &SECRET).unwrap();
    let forwarded = kinds(&hub).iter().filter(|k| *k == "forwarded").count();
    assert_eq!(forwarded, 1);
}

#[test]
fn a_hub_proves_its_identity_once_it_follows_a_chain() {
    let files = files();
    let hub = Hub::open(&files.seed, &files.db, policy()).unwrap();
    let regtest = ChainProfile::Core30DefaultRegtest;
    // Following no chain, it names no network to prove it on.
    let info = hub.advertise().unwrap();
    assert_eq!(info.proof, None);
    assert_eq!(info.proved(regtest), Ok(false));

    let chain = Chain::new();
    hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
    let info = hub.advertise().unwrap();
    assert_eq!(info.proved(regtest), Ok(true));
    assert!(info.proved(ChainProfile::Core30Signet).is_err());
    assert_eq!(info.address.spend(), hub.info().address.spend());
    // The same proof while its identity is the same, after a restart too
    // a valid one.
    assert_eq!(hub.advertise().unwrap().proof, info.proof);
    drop(hub);
    let hub = Hub::open(&files.seed, &files.db, policy()).unwrap();
    assert_eq!(hub.advertise().unwrap().proved(regtest), Ok(true));
}

#[test]
fn the_ledger_reads_back_locks_forwards_and_the_notes_they_moved() {
    let (files, hub, chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let expiry = chain.height() + 5;
    // Paid: Bob's 2 and 1 and the hub's 1.
    let paid = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&paid.signed, &paid.notes).unwrap();
    hub.settle(&paid.id(), &SECRET).unwrap();
    // Forwarded, failed and so refunded: the payer's 4.
    let failed = to_hub(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&failed.signed, &failed.notes).unwrap();
    hub.forward(&failed.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    hub.forward_failed(&failed.id(), "no route").unwrap();
    let open = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&open.signed, &open.notes).unwrap();
    // The hub's own lock, paid: Alice's 2, of inputs that were never notes.
    let e = Ephemeral::random();
    let d = e.derive(&owner(3).address(), 2, Branch::Pay, 0).unwrap();
    let own = hub
        .lock_own(
            keyset.id(),
            e.public(),
            vec![Output::from_derivation(&d).unwrap()],
            terms(&hub, expiry, 1),
        )
        .unwrap()
        .proposal
        .id();
    hub.settle(&own, &SECRET).unwrap();

    let ledger = Ledger::open(&files.db).unwrap();
    assert_eq!(ledger.tip().unwrap(), Some(chain.height()));
    let locks: Vec<_> = ledger
        .locks()
        .unwrap()
        .into_iter()
        .map(|l| (l.id, l.amount, l.own, l.paid, l.settled_at.is_some()))
        .collect();
    assert_eq!(
        locks,
        [
            (*paid.id().as_bytes(), 4, false, Some(true), true),
            (*failed.id().as_bytes(), 4, false, Some(false), true),
            (*open.id().as_bytes(), 4, false, None, false),
            (*own.as_bytes(), 2, true, Some(true), true),
        ]
    );
    let forwards = ledger.forwards().unwrap();
    let [forward] = &forwards[..] else {
        panic!("one forward, not {forwards:?}");
    };
    assert_eq!(
        (
            forward.lock,
            forward.request.as_str(),
            forward.amount,
            forward.expiry,
            forward.state.as_str(),
            forward.reason.as_deref()
        ),
        (
            *failed.id().as_bytes(),
            "lnbcrt1",
            4,
            expiry,
            "failed",
            Some("no route")
        )
    );
    assert!(forward.resolved_at >= Some(forward.created_at));

    let notes = ledger.notes().unwrap();
    let value = |issued: bool, hub: bool| -> u64 {
        notes
            .movements
            .iter()
            .filter(|m| m.issued == issued && (m.hub || !hub))
            .map(|m| m.denom)
            .sum()
    };
    // Bob's 3, the hub's 1, the refund of 4 and Alice's 2.
    assert_eq!((value(true, false), value(true, true)), (10, 1));
    // Each payer's 4, but none of the own lock's inputs.
    assert_eq!((value(false, false), value(false, true)), (12, 0));
    assert!(notes.unlinks.is_empty());
}

/// Notes made with the hub's keys outside its ledger show once it retired
/// more of a denomination than it ever issued to holders: as these tests'
/// payers' notes are made ([`mint`]). The inputs of its own locks, which
/// were never notes, show nothing.
#[test]
fn the_ledger_counts_the_notes_it_retired_beyond_those_it_issued() {
    let (files, hub, chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let excess = || Ledger::open(&files.db).unwrap().notes().unwrap().excess();
    assert_eq!(excess(), []);
    let expiry = chain.height() + 5;
    // Its own lock, paid: Alice's 4, issued by the ledger.
    let e = Ephemeral::random();
    let d = e.derive(&owner(3).address(), 4, Branch::Pay, 0).unwrap();
    let own = hub
        .lock_own(
            keyset.id(),
            e.public(),
            vec![Output::from_derivation(&d).unwrap()],
            terms(&hub, expiry, 1),
        )
        .unwrap()
        .proposal
        .id();
    hub.settle(&own, &SECRET).unwrap();
    assert_eq!(excess(), []);
    // A payer's 4, of no issuance of the ledger's, shows nothing while
    // Alice's is outstanding; another is one beyond all it issued.
    let forged = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&forged.signed, &forged.notes).unwrap();
    let one = Excess {
        keyset: 0,
        denom: 4,
        notes: 1,
    };
    assert_eq!(excess(), []);
    let another = standard(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&another.signed, &another.notes).unwrap();
    assert_eq!(excess(), [one]);
    // Paid, it issues Bob 2 and 1 and the hub 1: none of 4.
    hub.settle(&forged.id(), &SECRET).unwrap();
    assert_eq!(excess(), [one]);
    // One paying the hub 4, as for an invoice: the hub's own 4 is no
    // holder's, and hides nothing.
    let paying = to_hub(&files, &hub, terms(&hub, expiry, 1));
    hub.lock(&paying.signed, &paying.notes).unwrap();
    hub.forward(&paying.id(), &lock_hash(&SECRET), "lnbcrt1", "node", 4)
        .unwrap();
    hub.forward_paid(&paying.id(), &SECRET).unwrap();
    assert_eq!(excess(), [Excess { notes: 2, ..one }]);
}

/// What left the hub outside its ledger since a time, or may yet: what
/// the locks of its forwards pending or paid pay it. Not those failed,
/// nor those paid inside the hub.
#[test]
fn the_forwards_since_a_time_sum_what_is_paid_or_under_way() {
    let (files, hub, chain) = setup(policy());
    let expiry = chain.height() + 5;
    let forwarded = |secret: [u8; 32], node: &str| {
        let checkpoint = hub.checkpoint().unwrap().commitment();
        let profile = ChainProfile::Core30DefaultRegtest;
        let terms = TimedTerms::new(profile, checkpoint, expiry, 1, lock_hash(&secret)).unwrap();
        let l = to_hub(&files, &hub, terms);
        hub.lock(&l.signed, &l.notes).unwrap();
        hub.forward(&l.id(), &lock_hash(&secret), "lnbcrt1", node, 4)
            .unwrap();
        l.id()
    };
    assert_eq!(hub.forwarded_since(0).unwrap(), 0);
    let paid = forwarded([1; 32], "node");
    let failed = forwarded([2; 32], "node");
    forwarded([4; 32], licash_hub::LOCAL);
    let last = forwarded([3; 32], "node");
    assert_eq!(hub.forwarded_since(0).unwrap(), 12);
    hub.forward_paid(&paid, &[1; 32]).unwrap();
    hub.forward_failed(&failed, "no route").unwrap();
    assert_eq!(hub.forwarded_since(0).unwrap(), 8);
    // From the second the last was recorded in, none since the next.
    let at = hub.forward_of(&last).unwrap().unwrap().created_at;
    assert!(hub.forwarded_since(at).unwrap() >= 4);
    assert_eq!(hub.forwarded_since(at + 1).unwrap(), 0);
}

#[test]
fn a_lock_a_claim_showed_is_never_served_nor_another_of_its_input() {
    use licash::claim::{Context, Item, Parts, UnsignedClaim};
    let (files, hub, chain) = setup(policy());
    let (_, keyset) = issuer(&files);
    let payer = owner(1);
    let l = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    // Another of the same note, as she might sign any number.
    let other = {
        let e = Ephemeral::random();
        let output = |to: Address, denom, branch, i| {
            Output::from_derivation(&e.derive(&to, denom, branch, i).unwrap()).unwrap()
        };
        let pay = vec![
            output(owner(2).address(), 2, Branch::Pay, 0),
            output(owner(2).address(), 1, Branch::Pay, 1),
            output(hub.info().address, 1, Branch::Pay, 2),
        ];
        let refund = vec![output(payer.address(), 4, Branch::Refund, 0)];
        let lock = TimedLock::new(terms(&hub, chain.height() + 6, 1), refund).unwrap();
        let proposal = Proposal::new(&keyset, &l.notes, e.public(), pay, Some(lock)).unwrap();
        let owners = [l
            .input
            .ownership()
            .owner_secret(payer.spend_secret())
            .unwrap()];
        SignedProposal::sign(&keyset, proposal, &l.notes, Authorization::Plain, &owners).unwrap()
    };

    // A claim on Bitcoin shows the first, unserved, holding a note of hers
    // but not its input.
    let (kept, d) = mint(&files, &payer, 4);
    let message = kept.message().clone();
    let context = Context {
        base: [7; 32],
        deposit: [8; 36],
        cap: 1_000,
    };
    let parts = Parts {
        items: vec![Item::Settled(message.clone())],
        sources: vec![],
        activations: vec![],
        authorizations: vec![message.clone()],
    };
    let claim = UnsignedClaim::new(context, &keyset, 1, parts, 16).unwrap();
    let holds = [(message, d.hold_key(payer.hold_root()).unwrap())];
    let held = licash::recovery::Publication::sign(claim, &holds, 16)
        .unwrap()
        .held();
    let shown = licash::recovery::Evidence::Spend {
        signed: l.signed.clone(),
        notes: l.notes.clone(),
        unlock: None,
    };
    hub.retire_claimed(&[1; 32], &held, &[shown]).unwrap();

    // Neither is served after it: the note is given up, and no branch of
    // either lock is ever issued.
    assert_eq!(refused(hub.lock(&l.signed, &l.notes)), Refusal::Spent);
    assert_eq!(refused(hub.lock(&other, &l.notes)), Refusal::Spent);
    assert!(!kinds(&hub).iter().any(|kind| kind.contains("lock")));
    // A lock of a note no claim showed is served as ever.
    let free = standard(&files, &hub, terms(&hub, chain.height() + 5, 1));
    hub.lock(&free.signed, &free.notes).unwrap();
}
