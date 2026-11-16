//! Recovery without Bitcoin Core: the 32-item publication, its filing, and
//! what the Hub's statement decides it is owed.

use bitcoin::{
    absolute,
    block::{Header, Version},
    consensus::{deserialize, serialize},
    hashes::{sha256, sha256d, Hash},
    pow::CompactTarget,
    transaction, Amount, Block, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
};
use k256::Scalar;
use licash::{
    authorization::{Authorization, PreparedGrant, SignedProposal},
    base::{Base, ChainTerms, Fees, GraphTerms, Limits, Parts, Party, Recipe},
    chain::{self, Activation, ActivationRef, Chain, Checkpoint, Inclusion},
    claim::{self, ActivationRecord, Branch, Item, Source, UnsignedClaim},
    crypto::{self, Point},
    hold,
    note::Evidence as Issuance,
    note::{Derivation, HoldRoot, IssuerKeys, Keyset, Note},
    recovery::{
        self, disclosures, evidence_from_bytes, evidence_to_bytes, Evidence, Filing, Owed,
        Publication, SourceWitness, Witness,
    },
    request::Request,
    source::Certificate,
    spend::{ChainProfile, Output, Proposal, TimedLock, TimedTerms},
};

const SEED: [u8; 32] = [7; 32];
const SPEND: u64 = 17;
const LIMIT: usize = 128;
fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
}
fn hold() -> HoldRoot {
    HoldRoot::from_seed(&SEED).unwrap()
}
fn party(value: u64) -> Party {
    let mut script = vec![0x51, 0x20];
    script.extend(point(value + 3).x_only());
    Party {
        funding: point(value).x_only(),
        payment: point(value + 1).x_only(),
        delayed: point(value + 2).x_only(),
        payout_script: script,
    }
}
fn base(keyset: Keyset) -> Base {
    let mut deposit = [31; 36];
    deposit[32..].copy_from_slice(&7u32.to_le_bytes());
    Base::new(Parts {
        nonce: [53; 32],
        claim_key: point(19).x_only(),
        keyset,
        deposit,
        cap: 128,
        parties: [party(23), party(29)],
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: 1_000,
            reserve_alice: 20_000,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 8,
            request_height: 800,
            response_window: 12,
            publication_window: 3,
            assert_window: 12,
            challenge_window: 24,
            answer_window: 12,
        },
        recipe: Recipe {
            graph_version: 2,
            program: [11; 32],
            instances: 4,
            kept: 2,
        },
        chain: ChainTerms {
            checkpoint: Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap(),
            funding_depth: 1,
            work_depth: 6,
            max_headers: 1_000,
        },
        limits: Limits {
            max_transaction_bytes: 100_000,
            max_publication_bytes: 64_000,
            max_authorizations: LIMIT as u32,
            max_source_inputs: 128,
        },
        fees: Fees {
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    })
    .unwrap()
}

/// Wallet-side derivation: a holder output at (shared, index).
fn derive(root: u64, hold: &HoldRoot, denom: u64, shared: u8, index: u32) -> Derivation {
    Derivation::derive(denom, point(root), hold.public(), &[shared; 32], index).unwrap()
}
fn owner(root: u64, shared: u8, index: u32) -> Scalar {
    Scalar::from(root)
        + licash::note::derive_output(&point(root), &[shared; 32], index)
            .unwrap()
            .tweak
            .to_scalar()
            .unwrap()
}
fn mint(keys: &IssuerKeys, keyset: &Keyset, d: &Derivation) -> Note {
    let blinded = d.blinded().unwrap();
    let proof = keys
        .prove_blinded(d.message().denomination(), blinded)
        .unwrap();
    Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, d).unwrap(),
        Issuance::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: crypto::dleq_commit(&Scalar::from(71u64), &blinded.to_projective().unwrap()),
        },
    )
    .unwrap()
}

struct Opened {
    issuer: IssuerKeys,
    base: Base,
}

fn open() -> Opened {
    open_with(3)
}

fn open_with(max_pow: u32) -> Opened {
    let issuer = IssuerKeys::random(max_pow).unwrap();
    let base = base(issuer.keyset().unwrap());
    Opened { issuer, base }
}

/// The statement's chain with no block after Base's checkpoint.
fn checkpoint_chain() -> Chain {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    Chain::verify(cp.clone(), &cp.commitment(), vec![], 100).unwrap()
}

/// What the Hub's statement decides of `p` filed with `witness` under
/// `request`, against `evidence`, on `chain` (or the checkpoint's alone).
fn decide(
    o: &Opened,
    request: &Request,
    p: &Publication,
    witness: &Witness,
    evidence: &[Evidence],
    chain: Option<&Chain>,
) -> Result<Owed, licash::Error> {
    let filing = Filing::new(p.clone(), witness.clone()).unwrap();
    let chain = chain.cloned().unwrap_or_else(checkpoint_chain);
    recovery::owed(&o.base, request, &filing.to_bytes(), evidence, &chain)
}

fn claim(
    o: &Opened,
    items: Vec<Item>,
    sources: Vec<Source>,
    activations: Vec<ActivationRecord>,
    authorizations: &[Derivation],
    amount: u32,
) -> UnsignedClaim {
    let mut authorizations: Vec<_> = authorizations.iter().map(|d| d.message().clone()).collect();
    authorizations.sort_by_key(|m| m.identifier().unwrap());
    let p = o.base.parts();
    UnsignedClaim::new(
        claim::Context {
            base: o.base.digest(),
            deposit: p.deposit,
            cap: p.cap,
        },
        &p.keyset,
        amount,
        claim::Parts {
            items,
            sources,
            activations,
            authorizations,
        },
        LIMIT,
    )
    .unwrap()
}

fn hold_keys(ds: &[Derivation]) -> Vec<(licash::note::Message, crypto::ScalarBytes)> {
    ds.iter()
        .map(|d| (d.message().clone(), d.hold_key(&hold()).unwrap()))
        .collect()
}

fn spend(o: &Opened, notes: &[Note], owners: &[Scalar], to: Vec<Output>) -> SignedProposal {
    let keyset = &o.base.parts().keyset;
    let proposal = Proposal::new(keyset, notes, point(43), to, None).unwrap();
    SignedProposal::sign(keyset, proposal, notes, Authorization::Plain, owners).unwrap()
}

fn bob(denom: u64, index: u32) -> Output {
    let bob = HoldRoot::from_seed(&[9; 32]).unwrap();
    Output::from_derivation(&derive(41, &bob, denom, 29, index)).unwrap()
}

#[test]
fn publication_codec_is_strict() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let ds: Vec<_> = (0..3).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let publication = Publication::sign(
        claim(&o, items, vec![], vec![], &ds, 12),
        &hold_keys(&ds),
        LIMIT,
    )
    .unwrap();
    let bytes = publication.to_bytes();
    assert!(Publication::from_bytes(&keyset, LIMIT, &bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(Publication::from_bytes(&keyset, LIMIT, &bytes[..n]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Publication::from_bytes(&keyset, LIMIT, &trailing).is_err());
    let mut forged = bytes.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1; // a Q signature byte
    assert!(Publication::from_bytes(&keyset, LIMIT, &forged).is_err());
    assert!(Publication::from_bytes(&keyset, 2, &bytes).is_err());
}

const Z: [u8; 32] = [83; 32];

fn terms() -> TimedTerms {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    TimedTerms::new(
        cp.profile(),
        cp.commitment(),
        4,
        3,
        sha256::Hash::hash(&Z).to_byte_array(),
    )
    .unwrap()
}

fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    sha256d::Hash::hash(&[a.as_slice(), b.as_slice()].concat()).to_byte_array()
}
fn merkle(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    while hashes.len() > 1 {
        if hashes.len() % 2 == 1 {
            hashes.push(*hashes.last().unwrap());
        }
        hashes = hashes.chunks(2).map(|p| pair(&p[0], &p[1])).collect();
    }
    hashes[0]
}

/// Seven regtest-difficulty blocks; block 1 carries the Pay activation.
fn activated_chain() -> (Chain, Activation) {
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    let mut previous: Header = deserialize(cp.header()).unwrap();
    let (mut headers, mut activation) = (vec![], None);
    for height in 1..=7u32 {
        let reserved = [7; 32];
        let mut txdata = vec![Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, height as u8, 0]),
                witness: bitcoin::Witness::from_slice(&[reserved]),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        }];
        if height == 1 {
            txdata.push(Transaction {
                version: transaction::Version::TWO,
                lock_time: absolute::LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint {
                        txid: Txid::from_byte_array([17; 32]),
                        vout: 1,
                    },
                    witness: bitcoin::Witness::from_slice(&[vec![1; 3], vec![0x51]]),
                    ..Default::default()
                }],
                output: vec![TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::from_bytes(chain::activation_script(&Z)),
                }],
            });
        }
        let mut wtxids: Vec<_> = txdata
            .iter()
            .map(|tx| tx.compute_wtxid().to_byte_array())
            .collect();
        wtxids[0] = [0; 32];
        let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        script.extend(pair(&merkle(wtxids), &reserved));
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
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        while block.header.validate_pow(block.header.target()).is_err() {
            block.header.nonce += 1;
        }
        if height == 1 {
            let raw = serialize(&block);
            activation = Some(Activation {
                reference: ActivationRef {
                    height: 1,
                    block_hash: block.block_hash().to_byte_array(),
                    txid: block.txdata[1].compute_txid().to_byte_array(),
                    output: 0,
                },
                inclusion: Inclusion::capture(&raw, 1, raw.len()).unwrap(),
            });
        }
        headers.push(serialize(&block.header).try_into().unwrap());
        previous = block.header;
    }
    (
        Chain::verify(cp.clone(), &cp.commitment(), headers, 100).unwrap(),
        activation.unwrap(),
    )
}

#[test]
fn pending_sources_follow_activation_refund_and_preimage_rules() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let (chain, activation) = activated_chain();
    // Bob pays the holder 4 sats under an issuer-certified timed lock.
    let bob_hold = HoldRoot::from_seed(&[9; 32]).unwrap();
    let bob_note = mint(&o.issuer, &keyset, &derive(41, &bob_hold, 4, 29, 0));
    let pay = derive(SPEND, &hold, 4, 61, 0);
    let lock = TimedLock::new(terms(), vec![bob(4, 1)]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&bob_note),
        point(43),
        vec![Output::from_derivation(&pay).unwrap()],
        Some(lock),
    )
    .unwrap();
    let bob_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&bob_note),
        Authorization::Plain,
        &[owner(41, 29, 0)],
    )
    .unwrap();
    let cert =
        Certificate::attest(&o.issuer, &bob_signed, std::slice::from_ref(&bob_note)).unwrap();
    let bob_id = bob_signed.proposal().id();
    // The holder locked one of her own notes to Carol and claims its refund.
    let own = derive(SPEND, &hold, 4, 23, 40);
    let own_note = mint(&o.issuer, &keyset, &own);
    let refund = derive(SPEND, &hold, 4, 67, 0);
    let carol = derive(53, &HoldRoot::from_seed(&[11; 32]).unwrap(), 4, 31, 0);
    let lock = TimedLock::new(terms(), vec![Output::from_derivation(&refund).unwrap()]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&own_note),
        point(47),
        vec![Output::from_derivation(&carol).unwrap()],
        Some(lock),
    )
    .unwrap();
    let own_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&own_note),
        Authorization::Plain,
        &[owner(SPEND, 23, 40)],
    )
    .unwrap();
    let own_id = own_signed.proposal().id();
    let settled = derive(SPEND, &hold, 4, 23, 0);
    let settled_note = mint(&o.issuer, &keyset, &settled);

    let items = vec![
        Item::Settled(settled.message().clone()),
        Item::Pending {
            message: pay.message().clone(),
            source: bob_id,
            branch: Branch::Pay,
            index: 0,
            blinding: *pay.blinding(),
            preimage: Some(Z),
        },
        Item::Pending {
            message: refund.message().clone(),
            source: own_id,
            branch: Branch::Refund,
            index: 0,
            blinding: *refund.blinding(),
            preimage: None,
        },
    ];
    let sources = vec![
        Source::Admitted(cert),
        Source::OwnInputs {
            id: own_id,
            inputs: vec![own_note.message().identifier().unwrap()],
            terms: terms(),
        },
    ];
    let signers = [settled.clone(), pay.clone(), refund.clone(), own];
    let publish = |activations| {
        let unsigned = claim(
            &o,
            items.clone(),
            sources.clone(),
            activations,
            &signers,
            12,
        );
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let p = Publication::from_bytes(&keyset, LIMIT, &p.to_bytes()).unwrap();
        let request = Request {
            amount: 12,
            claim_digest: *p.claim().digest(),
        };
        (p, request)
    };
    // The public witness: the settled note, each source's body (in P's
    // source-id order), and the given activations' inclusions.
    let witness = |activations: Vec<Inclusion>| {
        let mut sources = vec![
            (
                bob_id,
                SourceWitness::Certified {
                    proposal: bob_signed.proposal().clone(),
                    signatures: bob_signed.signatures().to_vec(),
                },
            ),
            (
                own_id,
                SourceWitness::Signed {
                    signed: own_signed.clone(),
                    notes: vec![own_note.clone()],
                },
            ),
        ];
        sources.sort_by_key(|(id, _)| *id);
        let sources = sources.into_iter().map(|(_, w)| w).collect();
        Witness {
            notes: vec![settled_note.clone()],
            sources,
            activations,
        }
    };
    let bob_elsewhere = spend(
        &o,
        std::slice::from_ref(&bob_note),
        &[owner(41, 29, 0)],
        vec![bob(4, 2)],
    );
    let shared = Evidence::Spend {
        signed: bob_elsewhere,
        notes: vec![bob_note],
        unlock: None,
    };
    let counter = |p: &Publication, r: &Request, w: &Witness, e: Evidence| {
        decide(&o, r, p, w, &[e], Some(&chain))
    };

    // Activated certified Pay: expiry and input sharing cannot void it.
    let record = ActivationRecord {
        source: bob_id,
        reference: activation.reference,
    };
    let (p, request) = publish(vec![record]);
    let activated = witness(vec![activation.inclusion.clone()]);
    let uncontested = decide(&o, &request, &p, &activated, &[], Some(&chain)).unwrap();
    assert_eq!((uncontested.owed, uncontested.ineligible), (12, None));
    let expired = || Evidence::Expired { source: bob_id };
    assert!(counter(&p, &request, &activated, expired()).is_err());
    assert!(counter(&p, &request, &activated, shared.clone()).is_err());
    // The Pay preimage voids the holder's own refund; the rest stays owed.
    let preimage = |preimage| Evidence::PayPreimage {
        source: own_id,
        preimage,
    };
    let c = counter(&p, &request, &activated, preimage(Z)).unwrap();
    assert_eq!((c.mask, c.owed), (0b100, 8));
    // Voiding the refund discloses the preimage, which is what the payee of
    // the lock needs to collect.
    let posted = disclosures(&[preimage(Z)]).unwrap();
    assert_eq!((posted.values[0], &c.disclosures), (Z, &posted));
    assert!(counter(&p, &request, &activated, preimage([0; 32])).is_err());

    // Without an advertised activation, expiry or input sharing voids the Pay.
    let (p, request) = publish(vec![]);
    let plain = witness(vec![]);
    let c = counter(&p, &request, &plain, expired()).unwrap();
    assert_eq!((c.mask, c.owed), (0b010, 8));
    assert_eq!(counter(&p, &request, &plain, shared).unwrap().mask, 0b010);
    // The Hub can't shorten the chain to show an expiry it doesn't bury.
    let short = Chain::verify(
        chain.checkpoint().clone(),
        &chain.checkpoint().commitment(),
        (1..=5).map(|h| *chain.header(h).unwrap()).collect(),
        100,
    )
    .unwrap();
    assert!(decide(&o, &request, &p, &plain, &[expired()], Some(&short)).is_err());

    // An advertised activation must come with its inclusion.
    let (p, _) = publish(vec![record]);
    assert!(Filing::new(p, witness(vec![])).is_err());
}

/// The paper's example at half scale. Alice pays Bob 50 from a 128 note with
/// 78 change, under a grant. The Hub records the spend and withholds the
/// unlock. Alice claims her input with the change as fallbacks.
struct Withheld {
    o: Opened,
    note: Note,
    signed: SignedProposal,
    unlock: [u8; 32],
    paid: Vec<Derivation>,
    change: Vec<Derivation>,
    request: Request,
    publication: Publication,
    witness: Witness,
}

fn withheld() -> Withheld {
    let o = open_with(7);
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let n = derive(SPEND, &hold, 128, 23, 0);
    let note = mint(&o.issuer, &keyset, &n);
    let bob_hold = HoldRoot::from_seed(&[9; 32]).unwrap();
    let paid: Vec<_> = [32, 16, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(41, &bob_hold, *d, 29, i as u32))
        .collect();
    let change: Vec<_> = [64, 8, 4, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(SPEND, &hold, *d, 71, i as u32))
        .collect();
    let outputs = paid
        .iter()
        .chain(&change)
        .map(|d| Output::from_derivation(d).unwrap())
        .collect();
    let notes = std::slice::from_ref(&note);
    let proposal = Proposal::new(&keyset, notes, point(43), outputs, None).unwrap();
    let prepared = PreparedGrant::new(&o.issuer, &proposal).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &[owner(SPEND, 23, 0)],
    )
    .unwrap();
    let unlock = prepared.release(&keyset, notes, &signed).unwrap();
    let id = signed.proposal().id();
    let mut items = vec![Item::Settled(n.message().clone())];
    items.extend(change.iter().enumerate().map(|(i, d)| Item::Pending {
        message: d.message().clone(),
        source: id,
        branch: Branch::Pay,
        index: (paid.len() + i) as u32,
        blinding: *d.blinding(),
        preimage: None,
    }));
    let source = Source::Plain {
        id,
        inputs: vec![n.message().identifier().unwrap()],
    };
    let held: Vec<_> = std::iter::once(n.clone()).chain(change.clone()).collect();
    let unsigned = claim(&o, items, vec![source], vec![], &held, 128);
    let publication = Publication::sign(unsigned, &hold_keys(&held), LIMIT).unwrap();
    let request = Request {
        amount: 128,
        claim_digest: *publication.claim().digest(),
    };
    let witness = Witness {
        notes: vec![note.clone()],
        sources: vec![SourceWitness::Signed {
            signed: signed.clone(),
            notes: vec![note.clone()],
        }],
        activations: vec![],
    };
    Withheld {
        o,
        note,
        signed,
        unlock,
        paid,
        change,
        request,
        publication,
        witness,
    }
}

#[test]
fn a_withheld_granted_payment_is_countered_only_by_publishing_its_unlock() {
    let w = withheld();
    let (o, keyset) = (&w.o, w.o.base.parts().keyset.clone());
    let owed = |e: &[Evidence]| decide(o, &w.request, &w.publication, &w.witness, e, None);
    // Unanswered, Alice recovers her input in full.
    assert_eq!(owed(&[]).unwrap().owed, 128);

    let cite = |unlock| Evidence::Spend {
        signed: w.signed.clone(),
        notes: vec![w.note.clone()],
        unlock,
    };
    let counter = |e: Evidence| owed(&[e]);
    // The Hub cannot retire her input while keeping the unlock to itself.
    assert!(counter(cite(None)).is_err());
    assert!(counter(cite(Some([5; 32]))).is_err());
    // Nor by presenting the granted spend as a plain one: the owner signed
    // the grant commitment.
    assert!(SignedProposal::from_parts(
        &keyset,
        w.signed.proposal().clone(),
        std::slice::from_ref(&w.note),
        Authorization::Plain,
        w.signed.signatures().to_vec(),
    )
    .is_err());

    // The valid evidence voids the input, values the change and names the
    // unlock the Hub's response must disclose.
    let c = counter(cite(Some(w.unlock))).unwrap();
    assert_eq!((c.mask, c.owed), (1, 78));
    let posted = disclosures(&[cite(Some(w.unlock))]).unwrap();
    assert_eq!(posted.values[0], w.unlock);
    assert!(posted.values[1..].iter().all(|slot| *slot == [0; 32]));
    assert_eq!(c.disclosures, posted);
    assert_ne!(c.disclosures, disclosures(&[cite(None)]).unwrap());

    // Bob, holding the grant, opens his 50 from the published slots alone;
    // Alice opens her change the same way.
    let Authorization::Granted(grant) = w.signed.authorization() else {
        panic!("granted fixture");
    };
    let found = grant.unlock_in(&posted.values).unwrap();
    let mut opened = 0;
    for (index, d) in w.paid.iter().chain(&w.change).enumerate() {
        let note = w
            .signed
            .open_granted_output(&keyset, std::slice::from_ref(&w.note), index, d, &found)
            .unwrap();
        note.verify(&keyset).unwrap();
        opened += note.message().denomination();
    }
    assert_eq!(opened, 128);
    assert!(grant.unlock_in(&[[0; 32]; 32]).is_none());
}

/// The Hub's evidence travels to its prover as bytes the guest decodes
/// strictly, verifying the notes and the signed spend again.
#[test]
fn evidence_codec_is_strict() {
    let w = withheld();
    let keyset = w.o.base.parts().keyset.clone();
    let id = w.signed.proposal().id();
    let context = hold::Context {
        base: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let record = hold::Envelope::sign(context, &hold_keys(&w.change[..1]), LIMIT)
        .unwrap()
        .records()[0]
        .clone();
    let evidence = vec![
        Evidence::Spend {
            signed: w.signed.clone(),
            notes: vec![w.note.clone()],
            unlock: Some(w.unlock),
        },
        Evidence::SecondClaim { context, record },
        Evidence::PayPreimage {
            source: id,
            preimage: [5; 32],
        },
        Evidence::Expired { source: id },
    ];
    let bytes = evidence_to_bytes(&evidence);
    let decoded = evidence_from_bytes(&keyset, LIMIT, &bytes).unwrap();
    assert_eq!(evidence_to_bytes(&decoded), bytes);
    let c = decide(
        &w.o,
        &w.request,
        &w.publication,
        &w.witness,
        &decoded[..1],
        None,
    )
    .unwrap();
    assert_eq!((c.mask, c.owed), (1, 78));
    for n in (0..bytes.len()).step_by(7) {
        assert!(evidence_from_bytes(&keyset, LIMIT, &bytes[..n]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(evidence_from_bytes(&keyset, LIMIT, &trailing).is_err());
    let other = IssuerKeys::random(7).unwrap().keyset().unwrap();
    assert!(evidence_from_bytes(&other, LIMIT, &bytes).is_err());
    assert_eq!(
        evidence_from_bytes(&keyset, LIMIT, &[0; 4]).unwrap().len(),
        0
    );
}

/// A claim of the refund of an unsubmitted lock carries Q's authorization for
/// the lock's inputs, because the Hub may never have seen the lock. A note
/// claimed in one contract and the refund of a lock of it in another are then
/// two claims of the same value, and the first voids the second.
#[test]
fn second_claim_of_an_input_voids_the_outputs_of_a_lock_of_the_holders_own_notes() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let own = derive(SPEND, &hold, 4, 23, 40);
    let own_note = mint(&o.issuer, &keyset, &own);
    let refund = derive(SPEND, &hold, 4, 67, 0);
    let carol = derive(53, &HoldRoot::from_seed(&[11; 32]).unwrap(), 4, 31, 0);
    let lock = TimedLock::new(terms(), vec![Output::from_derivation(&refund).unwrap()]).unwrap();
    let notes = std::slice::from_ref(&own_note);
    let outputs = vec![Output::from_derivation(&carol).unwrap()];
    let body = Proposal::new(&keyset, notes, point(47), outputs, Some(lock)).unwrap();
    let owners = [owner(SPEND, 23, 40)];
    let signed = SignedProposal::sign(&keyset, body, notes, Authorization::Plain, &owners).unwrap();
    let id = signed.proposal().id();
    let settled = derive(SPEND, &hold, 4, 23, 0);
    let items = vec![
        Item::Settled(settled.message().clone()),
        Item::Pending {
            message: refund.message().clone(),
            source: id,
            branch: Branch::Refund,
            index: 0,
            blinding: *refund.blinding(),
            preimage: None,
        },
    ];
    let sources = vec![Source::OwnInputs {
        id,
        inputs: vec![own_note.message().identifier().unwrap()],
        terms: terms(),
    }];
    let signers = [settled.clone(), refund.clone(), own.clone()];
    let unsigned = claim(&o, items, sources, vec![], &signers, 8);
    let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
    let request = Request {
        amount: 8,
        claim_digest: *p.claim().digest(),
    };
    let witness = Witness {
        notes: vec![mint(&o.issuer, &keyset, &settled)],
        sources: vec![SourceWitness::Signed {
            signed,
            notes: vec![own_note],
        }],
        activations: vec![],
    };
    let owed = |e: &[Evidence]| decide(&o, &request, &p, &witness, e, None);
    assert_eq!(owed(&[]).unwrap().owed, 8);

    // The locked note was claimed in another contract.
    let elsewhere = hold::Context {
        base: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let second = |signer: &Derivation| Evidence::SecondClaim {
        context: elsewhere,
        record: hold::Envelope::sign(elsewhere, &hold_keys(std::slice::from_ref(signer)), LIMIT)
            .unwrap()
            .records()[0]
            .clone(),
    };
    let c = owed(&[second(&own)]).unwrap();
    assert_eq!((c.mask, c.owed), (0b10, 4));
    // A claim of a note that this one does not rely on is no evidence.
    let unrelated = derive(SPEND, &hold, 4, 23, 41);
    assert!(owed(&[second(&unrelated)]).is_err());
}

/// The value left after a counter is capped by the request, whatever the
/// denominations of the surviving items add up to.
#[test]
fn surviving_value_beyond_u64_is_capped_at_the_request() {
    let o = open_with(63);
    let keyset = o.base.parts().keyset.clone();
    let ds: Vec<_> = (0..3)
        .map(|i| derive(SPEND, &hold(), 1 << 63, 23, i))
        .collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let unsigned = claim(&o, items, vec![], vec![], &ds, 128);
    let p = Publication::sign(unsigned, &hold_keys(&ds), LIMIT).unwrap();
    let request = Request {
        amount: 128,
        claim_digest: *p.claim().digest(),
    };
    let spent = Evidence::Spend {
        signed: spend(
            &o,
            &notes[..1],
            &[owner(SPEND, 23, 0)],
            vec![bob(1 << 63, 0)],
        ),
        notes: notes[..1].to_vec(),
        unlock: None,
    };
    let witness = Witness {
        notes,
        sources: vec![],
        activations: vec![],
    };
    let c = decide(&o, &request, &p, &witness, &[spent], None).unwrap();
    assert_eq!((c.mask, c.owed), (0b1, 128));
}

#[test]
fn second_claim_signature_cannot_be_assigned_to_a_different_note_with_the_same_q() {
    let o = open();
    let first = derive(SPEND, &hold(), 4, 23, 0);
    let second = derive(SPEND, &hold(), 8, 23, 0);
    assert_eq!(first.message().hold(), second.message().hold());
    let unsigned = claim(
        &o,
        vec![Item::Settled(second.message().clone())],
        vec![],
        vec![],
        std::slice::from_ref(&second),
        8,
    );
    let p = Publication::sign(unsigned, &hold_keys(std::slice::from_ref(&second)), LIMIT).unwrap();
    let request = Request {
        amount: 8,
        claim_digest: *p.claim().digest(),
    };
    let context = hold::Context {
        base: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let envelope = hold::Envelope::sign(context, &hold_keys(&[first]), LIMIT).unwrap();
    let mut record = envelope.records()[0].clone();
    record.message = second.message().clone();
    let evidence = Evidence::SecondClaim { context, record };
    let witness = Witness {
        notes: vec![mint(&o.issuer, &o.base.parts().keyset, &second)],
        sources: vec![],
        activations: vec![],
    };
    assert_eq!(
        decide(&o, &request, &p, &witness, &[], None).unwrap().owed,
        8
    );
    assert!(decide(&o, &request, &p, &witness, &[evidence], None).is_err());
}

/// Input Q authority is required only for OwnInputs. Certified outputs belong
/// to their payee and cannot be retired by a signature from the payer's Q key.
/// For OwnInputs, however, even one conflicting input retires every dependent
/// output, on either branch. Such a source must be recovered in one claim.
#[test]
fn input_claim_conflicts_preserve_certified_outputs_but_retire_all_own_outputs() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let inputs: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let notes: Vec<_> = inputs.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let pay: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 61, i)).collect();
    let refund: Vec<_> = (0..2).map(|i| derive(SPEND, &hold(), 4, 67, i)).collect();
    let outputs = |ds: &[Derivation]| {
        ds.iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect()
    };
    let lock = TimedLock::new(terms(), outputs(&refund)).unwrap();
    let proposal = Proposal::new(&keyset, &notes, point(47), outputs(&pay), Some(lock)).unwrap();
    let owners: Vec<_> = (0..2).map(|i| owner(SPEND, 23, i)).collect();
    let signed =
        SignedProposal::sign(&keyset, proposal, &notes, Authorization::Plain, &owners).unwrap();
    let id = signed.proposal().id();
    let cert = Certificate::attest(&o.issuer, &signed, &notes).unwrap();
    let settled = derive(SPEND, &hold(), 4, 23, 40);
    let (chain, activation) = activated_chain();
    let publish = |branch: Branch, admitted: bool, indices: &[usize]| {
        let ds = if branch == Branch::Pay { &pay } else { &refund };
        let mut items = vec![Item::Settled(settled.message().clone())];
        let mut signers = vec![settled.clone()];
        for i in indices {
            let d = &ds[*i];
            items.push(Item::Pending {
                message: d.message().clone(),
                source: id,
                branch,
                index: *i as u32,
                blinding: *d.blinding(),
                preimage: (branch == Branch::Pay).then_some(Z),
            });
            signers.push(d.clone());
        }
        let (mut activations, mut inclusions) = (vec![], vec![]);
        let source = if admitted {
            if branch == Branch::Pay {
                activations.push(ActivationRecord {
                    source: id,
                    reference: activation.reference,
                });
                inclusions.push(activation.inclusion.clone());
            }
            Source::Admitted(cert.clone())
        } else {
            signers.extend(inputs.iter().cloned());
            Source::OwnInputs {
                id,
                inputs: signed
                    .proposal()
                    .inputs()
                    .iter()
                    .map(|m| m.identifier().unwrap())
                    .collect(),
                terms: terms(),
            }
        };
        let amount = (4 * (1 + indices.len())) as u32;
        let unsigned = claim(&o, items, vec![source], activations, &signers, amount);
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let request = Request {
            amount,
            claim_digest: *p.claim().digest(),
        };
        let body = if admitted {
            SourceWitness::Certified {
                proposal: signed.proposal().clone(),
                signatures: signed.signatures().to_vec(),
            }
        } else {
            SourceWitness::Signed {
                signed: signed.clone(),
                notes: notes.clone(),
            }
        };
        let witness = Witness {
            notes: vec![mint(&o.issuer, &keyset, &settled)],
            sources: vec![body],
            activations: inclusions,
        };
        (p, request, witness)
    };
    let elsewhere = hold::Context {
        base: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let e = Evidence::SecondClaim {
        context: elsewhere,
        record: hold::Envelope::sign(elsewhere, &hold_keys(&inputs[..1]), LIMIT)
            .unwrap()
            .records()[0]
            .clone(),
    };
    let e = std::slice::from_ref(&e);
    for branch in [Branch::Pay, Branch::Refund] {
        let (p, r, w) = publish(branch, true, &[0, 1]);
        let eligible = decide(&o, &r, &p, &w, &[], Some(&chain)).unwrap();
        assert_eq!((eligible.owed, eligible.ineligible), (12, None));
        assert!(decide(&o, &r, &p, &w, e, Some(&chain)).is_err());
        let (p, r, w) = publish(branch, false, &[0, 1]);
        let c = decide(&o, &r, &p, &w, e, None).unwrap();
        assert_eq!((c.mask, c.owed), (0b110, 4));
    }
    // Even disjoint outputs of one OwnInputs source share its input Q
    // authorities: splitting this recovery across claims is not supported.
    let (first, r1, w1) = publish(Branch::Refund, false, &[0]);
    let (second, r2, w2) = publish(Branch::Refund, false, &[1]);
    for (here, r, w, other) in [(&first, &r1, &w1, &second), (&second, &r2, &w2, &first)] {
        let evidence = Evidence::SecondClaim {
            context: other.claim().hold_context(),
            record: other
                .envelope()
                .records()
                .iter()
                .find(|record| record.message == *inputs[0].message())
                .unwrap()
                .clone(),
        };
        let c = decide(&o, r, here, w, &[evidence], None).unwrap();
        assert_eq!((c.mask, c.owed), (0b10, 4));
    }
}

// The holder files P with its public witness and proves nothing; the Hub's
// statement (`recovery::owed`) decides what the claim is owed.

/// `bytes` with the unique occurrence of `inside` replaced by `with`.
fn swap(bytes: &[u8], inside: &[u8], with: &[u8]) -> Vec<u8> {
    let at: Vec<_> = bytes
        .windows(inside.len())
        .enumerate()
        .filter(|(_, w)| *w == inside)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1);
    [&bytes[..at[0]], with, &bytes[at[0] + inside.len()..]].concat()
}

/// `bytes` with the last byte of the unique occurrence of `inside` flipped.
fn flip(bytes: &[u8], inside: &[u8]) -> Vec<u8> {
    let mut with = inside.to_vec();
    *with.last_mut().unwrap() ^= 1;
    swap(bytes, inside, &with)
}

#[test]
fn the_hub_decides_what_a_filed_claim_is_owed() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let (chain, _) = activated_chain();
    let ds: Vec<_> = (0..32).map(|i| derive(SPEND, &hold, 4, 23, i)).collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    // After her backup, the holder paid Bob 64 sats from 16 of the notes.
    let owners: Vec<_> = (0..16).map(|i| owner(SPEND, 23, i)).collect();
    let payment = spend(
        &o,
        &notes[..16],
        &owners,
        (0..8).map(|i| bob(8, i)).collect(),
    );
    let items = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let p = Publication::sign(
        claim(&o, items, vec![], vec![], &ds, 128),
        &hold_keys(&ds),
        LIMIT,
    )
    .unwrap();
    let request = Request {
        amount: 128,
        claim_digest: *p.claim().digest(),
    };
    let filing = Filing::new(
        p,
        Witness {
            notes: notes.clone(),
            sources: vec![],
            activations: vec![],
        },
    )
    .unwrap();
    let bytes = filing.to_bytes();
    assert!(bytes.len() < 25_000, "{} bytes", bytes.len());
    let owed = |request: &Request, bytes: &[u8], evidence: &[Evidence]| {
        recovery::owed(&o.base, request, bytes, evidence, &chain)
    };

    // Unanswered by evidence, the claim is owed in full: nothing to prove.
    let whole = owed(&request, &bytes, &[]).unwrap();
    assert_eq!((whole.owed, whole.mask, whole.ineligible), (128, 0, None));
    // The stale backup: the Hub's evidence of the spend voids 16 items, as
    // in a counter.
    let spent = Evidence::Spend {
        signed: payment,
        notes: notes[..16].to_vec(),
        unlock: None,
    };
    let stale = owed(&request, &bytes, std::slice::from_ref(&spent)).unwrap();
    assert_eq!((stale.owed, stale.mask), (64, 0xffff));
    // A Q signature over any other claim, in any contract, voids its item.
    let other = hold::Context {
        base: [1; 32],
        deposit: [2; 36],
        amount: 4,
        claim_digest: [3; 32],
    };
    let record = hold::Envelope::sign(other, &hold_keys(&ds[20..21]), LIMIT)
        .unwrap()
        .records()[0]
        .clone();
    let second = Evidence::SecondClaim {
        context: other,
        record: record.clone(),
    };
    let both = owed(&request, &bytes, &[spent.clone(), second]).unwrap();
    assert_eq!((both.owed, both.mask), (60, 0xffff | 1 << 20));

    // The Hub's errors: it can't prove with evidence that voids nothing,
    // with this claim cited as its own second claim, or on another chain.
    let unrelated = derive(SPEND, &hold, 4, 23, 40);
    let unrelated_note = mint(&o.issuer, &keyset, &unrelated);
    let unrelated = Evidence::Spend {
        signed: spend(
            &o,
            std::slice::from_ref(&unrelated_note),
            &[owner(SPEND, 23, 40)],
            vec![bob(4, 9)],
        ),
        notes: vec![unrelated_note],
        unlock: None,
    };
    assert!(owed(&request, &bytes, &[unrelated]).is_err());
    let same = Evidence::SecondClaim {
        context: filing.publication().claim().hold_context(),
        record,
    };
    assert!(owed(&request, &bytes, &[same]).is_err());
    let signet = Checkpoint::genesis(ChainProfile::Core30Signet).unwrap();
    let elsewhere = Chain::verify(signet.clone(), &signet.commitment(), vec![], 100).unwrap();
    assert!(recovery::owed(&o.base, &request, &bytes, &[], &elsewhere).is_err());

    // A filing for another request is the holder's error: owed nothing, which
    // the Hub shows with no evidence (and may attach none).
    let wrong = Request {
        amount: 127,
        ..request
    };
    let nothing = owed(&wrong, &bytes, &[]).unwrap();
    assert_eq!((nothing.owed, nothing.mask), (0, 0));
    assert!(nothing.ineligible.is_some());
    assert_eq!(nothing.disclosures, disclosures(&[]).unwrap());
    assert!(owed(&wrong, &bytes, &[spent]).is_err());
}

#[test]
fn a_note_is_claimed_in_any_contract_its_hold_key_designates() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let (chain, _) = activated_chain();
    // Notes of a wallet whose spend root (41) isn't this contract's.
    let ds: Vec<_> = (0..2).map(|i| derive(41, &hold(), 4, 23, i)).collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let items: Vec<_> = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let unsigned = claim(&o, items, vec![], vec![], &ds, 8);
    let p = Publication::sign(unsigned.clone(), &hold_keys(&ds), LIMIT).unwrap();
    let request = Request {
        amount: 8,
        claim_digest: *p.claim().digest(),
    };
    // Filed, their Q signatures designate this contract, and they are owed.
    let signatures: Vec<_> = p.envelope().records().iter().map(|r| r.sig.0).collect();
    let filing = Filing::new(
        p,
        Witness {
            notes,
            sources: vec![],
            activations: vec![],
        },
    )
    .unwrap();
    let bytes = filing.to_bytes();
    let owed = |bytes: &[u8]| recovery::owed(&o.base, &request, bytes, &[], &chain).unwrap();
    assert_eq!((owed(&bytes).owed, owed(&bytes).ineligible), (8, None));

    // Only the notes' Q keys designate. Another hold root (a payer's, say,
    // who knows the outputs it made) doesn't derive their Q secrets, a key
    // that isn't Q's secret can't sign P, and a forged Q signature leaves the
    // claim owed nothing.
    let payer = HoldRoot::from_seed(&[9; 32]).unwrap();
    assert!(ds.iter().all(|d| d.hold_key(&payer).is_err()));
    let theirs: Vec<_> = ds
        .iter()
        .map(|d| (d.message().clone(), crypto::ScalarBytes([7; 32])))
        .collect();
    assert!(Publication::sign(unsigned, &theirs, LIMIT).is_err());
    for sig in signatures {
        let forged = owed(&flip(&bytes, &sig));
        assert_eq!(forged.owed, 0);
        assert!(forged.ineligible.is_some());
    }
}

#[test]
fn fabricated_or_malformed_filings_are_owed_nothing() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let (chain, _) = activated_chain();
    let ds: Vec<_> = (0..3).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&o.issuer, &keyset, d)).collect();
    let items: Vec<_> = ds
        .iter()
        .map(|d| Item::Settled(d.message().clone()))
        .collect();
    let p = Publication::sign(
        claim(&o, items, vec![], vec![], &ds, 12),
        &hold_keys(&ds),
        LIMIT,
    )
    .unwrap();
    let request = Request {
        amount: 12,
        claim_digest: *p.claim().digest(),
    };
    let file = |notes: Vec<Note>| {
        Filing::new(
            p.clone(),
            Witness {
                notes,
                sources: vec![],
                activations: vec![],
            },
        )
    };
    let bytes = file(notes.clone()).unwrap().to_bytes();
    let owed = |bytes: &[u8], evidence: &[Evidence]| {
        recovery::owed(&o.base, &request, bytes, evidence, &chain)
    };
    assert_eq!(owed(&bytes, &[]).unwrap().owed, 12);
    let nothing = |bytes: &[u8]| {
        let o = owed(bytes, &[]).unwrap();
        assert_eq!((o.owed, o.mask), (0, 0));
        assert!(o.ineligible.is_some());
    };

    // A note another issuer signed, or the issuer's signature forged.
    let other = IssuerKeys::random(3).unwrap();
    let mut fake = notes.clone();
    fake[1] = mint(&other, &other.keyset().unwrap(), &ds[1]);
    nothing(&file(fake).unwrap().to_bytes());
    nothing(&flip(&bytes, &notes[1].signature().0));
    let borrowed = swap(&bytes, &notes[1].signature().0, &notes[0].signature().0);
    let reason = owed(&borrowed, &[]).unwrap().ineligible.unwrap();
    assert!(!reason.contains("point"), "{reason}");
    nothing(&borrowed);
    // A filing that doesn't decode: cut short, extended, not a filing (P
    // alone), or nothing at all.
    for n in [0, 9, bytes.len() / 2, bytes.len() - 1] {
        nothing(&bytes[..n]);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    nothing(&trailing);
    nothing(&p.to_bytes());
    // A witness that doesn't line up with P can't be filed.
    assert!(file(notes[..2].to_vec()).is_err());
    assert!(file(vec![notes[1].clone(), notes[0].clone(), notes[2].clone()]).is_err());

    // Evidence against an ineligible claim is the Hub's error.
    let spent = Evidence::Spend {
        signed: spend(&o, &notes[..1], &[owner(SPEND, 23, 0)], vec![bob(4, 0)]),
        notes: notes[..1].to_vec(),
        unlock: None,
    };
    assert_eq!(owed(&bytes, std::slice::from_ref(&spent)).unwrap().owed, 8);
    assert!(owed(&trailing, &[spent]).is_err());
}

#[test]
fn pending_items_are_eligible_by_their_sources_public_bodies() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let hold = hold();
    let (chain, activation) = activated_chain();
    // Bob pays the holder 4 sats under an issuer-certified timed lock.
    let bob_hold = HoldRoot::from_seed(&[9; 32]).unwrap();
    let bob_note = mint(&o.issuer, &keyset, &derive(41, &bob_hold, 4, 29, 0));
    let pay = derive(SPEND, &hold, 4, 61, 0);
    let lock = TimedLock::new(terms(), vec![bob(4, 1)]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&bob_note),
        point(43),
        vec![Output::from_derivation(&pay).unwrap()],
        Some(lock),
    )
    .unwrap();
    let bob_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&bob_note),
        Authorization::Plain,
        &[owner(41, 29, 0)],
    )
    .unwrap();
    let cert =
        Certificate::attest(&o.issuer, &bob_signed, std::slice::from_ref(&bob_note)).unwrap();
    let bob_id = bob_signed.proposal().id();
    // The holder locked one of her own notes to Carol and claims its refund.
    let own = derive(SPEND, &hold, 4, 23, 40);
    let own_note = mint(&o.issuer, &keyset, &own);
    let refund = derive(SPEND, &hold, 4, 67, 0);
    let carol = derive(53, &HoldRoot::from_seed(&[11; 32]).unwrap(), 4, 31, 0);
    let lock = TimedLock::new(terms(), vec![Output::from_derivation(&refund).unwrap()]).unwrap();
    let body = Proposal::new(
        &keyset,
        std::slice::from_ref(&own_note),
        point(47),
        vec![Output::from_derivation(&carol).unwrap()],
        Some(lock),
    )
    .unwrap();
    let own_signed = SignedProposal::sign(
        &keyset,
        body,
        std::slice::from_ref(&own_note),
        Authorization::Plain,
        &[owner(SPEND, 23, 40)],
    )
    .unwrap();
    let own_id = own_signed.proposal().id();
    let settled = derive(SPEND, &hold, 4, 23, 0);
    let settled_note = mint(&o.issuer, &keyset, &settled);

    let items = |refund_blinding| {
        vec![
            Item::Settled(settled.message().clone()),
            Item::Pending {
                message: pay.message().clone(),
                source: bob_id,
                branch: Branch::Pay,
                index: 0,
                blinding: *pay.blinding(),
                preimage: Some(Z),
            },
            Item::Pending {
                message: refund.message().clone(),
                source: own_id,
                branch: Branch::Refund,
                index: 0,
                blinding: refund_blinding,
                preimage: None,
            },
        ]
    };
    let sources = |own_terms| {
        vec![
            Source::Admitted(cert.clone()),
            Source::OwnInputs {
                id: own_id,
                inputs: vec![own_note.message().identifier().unwrap()],
                terms: own_terms,
            },
        ]
    };
    // The sources' bodies, in P's (source-id) order.
    let mut bodies = vec![
        (
            bob_id,
            SourceWitness::Certified {
                proposal: bob_signed.proposal().clone(),
                signatures: bob_signed.signatures().to_vec(),
            },
        ),
        (
            own_id,
            SourceWitness::Signed {
                signed: own_signed,
                notes: vec![own_note.clone()],
            },
        ),
    ];
    bodies.sort_by_key(|(id, _)| *id);
    let bodies: Vec<_> = bodies.into_iter().map(|(_, w)| w).collect();
    let signers = [settled.clone(), pay.clone(), refund.clone(), own];
    let file = |items, sources, activations, inclusions| {
        let p = Publication::sign(
            claim(&o, items, sources, activations, &signers, 12),
            &hold_keys(&signers),
            LIMIT,
        )
        .unwrap();
        let request = Request {
            amount: 12,
            claim_digest: *p.claim().digest(),
        };
        let filing = Filing::new(
            p,
            Witness {
                notes: vec![settled_note.clone()],
                sources: bodies.clone(),
                activations: inclusions,
            },
        )
        .unwrap();
        (filing.to_bytes(), request)
    };
    let owed = |(bytes, request): &(Vec<u8>, Request), evidence: &[Evidence], chain: &Chain| {
        recovery::owed(&o.base, request, bytes, evidence, chain)
    };
    let nothing = |filed: &(Vec<u8>, Request), chain: &Chain| {
        let o = owed(filed, &[], chain).unwrap();
        assert_eq!(o.owed, 0);
        assert!(o.ineligible.is_some());
    };
    let record = ActivationRecord {
        source: bob_id,
        reference: activation.reference,
    };
    let preimage = Evidence::PayPreimage {
        source: own_id,
        preimage: Z,
    };

    // Activated certified Pay: eligible, and expiry can't void it; the Pay
    // preimage voids the holder's own refund, and is disclosed.
    let activated = file(
        items(*refund.blinding()),
        sources(terms()),
        vec![record],
        vec![activation.inclusion.clone()],
    );
    assert_eq!(owed(&activated, &[], &chain).unwrap().owed, 12);
    assert!(owed(&activated, &[Evidence::Expired { source: bob_id }], &chain).is_err());
    let c = owed(&activated, std::slice::from_ref(&preimage), &chain).unwrap();
    assert_eq!((c.mask, c.owed), (0b100, 8));
    assert_eq!(c.disclosures, disclosures(&[preimage]).unwrap());
    // Without an advertised activation, expiry voids the Pay.
    let plain = file(items(*refund.blinding()), sources(terms()), vec![], vec![]);
    let c = owed(&plain, &[Evidence::Expired { source: bob_id }], &chain).unwrap();
    assert_eq!((c.mask, c.owed), (0b010, 8));

    // What the holder can get wrong of a pending item leaves the claim owed
    // nothing: an activation the statement's chain doesn't bury at its
    // terms' depth, or that isn't what it says; an output the source doesn't
    // make; source metadata its body doesn't match.
    let short = Chain::verify(
        chain.checkpoint().clone(),
        &chain.checkpoint().commitment(),
        (1..=2).map(|h| *chain.header(h).unwrap()).collect(),
        100,
    )
    .unwrap();
    nothing(&activated, &short);
    let elsewhere = ActivationRecord {
        source: bob_id,
        reference: ActivationRef {
            output: 1,
            ..activation.reference
        },
    };
    nothing(
        &file(
            items(*refund.blinding()),
            sources(terms()),
            vec![elsewhere],
            vec![activation.inclusion.clone()],
        ),
        &chain,
    );
    nothing(
        &file(items(*pay.blinding()), sources(terms()), vec![], vec![]),
        &chain,
    );
    let cp = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    let later = TimedTerms::new(
        cp.profile(),
        cp.commitment(),
        5,
        3,
        sha256::Hash::hash(&Z).to_byte_array(),
    )
    .unwrap();
    nothing(
        &file(items(*refund.blinding()), sources(later), vec![], vec![]),
        &chain,
    );
}

#[test]
fn a_filed_pay_of_a_lock_timed_on_another_checkpoint_is_owed_nothing() {
    let o = open();
    let keyset = o.base.parts().keyset.clone();
    let (chain, _) = activated_chain();
    let owed = |terms: TimedTerms| {
        let payer = HoldRoot::from_seed(&[9; 32]).unwrap();
        let note = mint(&o.issuer, &keyset, &derive(41, &payer, 4, 29, 0));
        let notes = std::slice::from_ref(&note);
        let pay = derive(SPEND, &hold(), 4, 61, 0);
        let lock = TimedLock::new(terms, vec![bob(4, 1)]).unwrap();
        let outputs = vec![Output::from_derivation(&pay).unwrap()];
        let body = Proposal::new(&keyset, notes, point(43), outputs, Some(lock)).unwrap();
        let owners = [owner(41, 29, 0)];
        let signed =
            SignedProposal::sign(&keyset, body, notes, Authorization::Plain, &owners).unwrap();
        let cert = Certificate::attest(&o.issuer, &signed, notes).unwrap();
        let items = vec![Item::Pending {
            message: pay.message().clone(),
            source: signed.proposal().id(),
            branch: Branch::Pay,
            index: 0,
            blinding: *pay.blinding(),
            preimage: Some(Z),
        }];
        let signers = [pay.clone()];
        let unsigned = claim(&o, items, vec![Source::Admitted(cert)], vec![], &signers, 4);
        let p = Publication::sign(unsigned, &hold_keys(&signers), LIMIT).unwrap();
        let request = Request {
            amount: 4,
            claim_digest: *p.claim().digest(),
        };
        let filing = Filing::new(
            p,
            Witness {
                notes: vec![],
                sources: vec![SourceWitness::Certified {
                    proposal: signed.proposal().clone(),
                    signatures: signed.signatures().to_vec(),
                }],
                activations: vec![],
            },
        )
        .unwrap();
        recovery::owed(&o.base, &request, &filing.to_bytes(), &[], &chain).unwrap()
    };
    let here = terms();
    assert_eq!(
        (owed(here.clone()).owed, owed(here.clone()).ineligible),
        (4, None)
    );
    let hash = *here.hash();
    let elsewhere = TimedTerms::new(here.profile(), [7; 32], 4, 3, hash).unwrap();
    let mainnet = TimedTerms::new(ChainProfile::Core30Mainnet, *here.checkpoint(), 4, 3, hash);
    for terms in [elsewhere, mainnet.unwrap()] {
        let refused = owed(terms);
        assert_eq!(refused.owed, 0);
        let reason = refused.ineligible.unwrap();
        assert!(reason.contains("another checkpoint"), "{reason}");
    }
}
