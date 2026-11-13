//! A funded native opening with a small wallet, and the roles tests play on
//! it: the holder who claims, the Hub that answers, and each ending on
//! Bitcoin. Proof authority is the mock; garbled instances are genuine.

use crate::{
    contract::{Opened, Stage},
    graph::Babe,
    mock_proof, mock_verify,
    native::{journal, open_native, Authority, Holder, Native},
    native_mock_counter, native_mock_counter_verify, sig,
};
use anyhow::{ensure, Context, Result};
use k256::Scalar;
use licash::{
    authorization::{Authorization, SignedProposal},
    chain::{Activation, Chain},
    claim::{self, ActivationRecord, Branch, Item, Source, UnsignedClaim},
    crypto::{self, Point},
    disclosure::Disclosures,
    guest::CounterInput,
    note::Evidence as Issuance,
    note::{Derivation, HoldRoot, IssuerKeys, Keyset, Message, Note},
    recovery::{CounterJournal, Evidence, HolderWitness, ItemWitness, Publication, SourceWitness},
    request::Request,
    source::{Certificate, Metadata},
    spend::{Output, Proposal, ProposalId, TimedLock, TimedTerms},
    statement::{HubResponse, StatementInput},
};
use lngap_btc::bitcoin::{Transaction, Txid};
use lngap_btc::regtest::Regtest;
use lngap_lamport::winternitz::WotsSig;

/// The fixture holder's request-key and hold seed, and spending root.
pub const SEED: [u8; 32] = [7; 32];
pub const SPEND: u64 = 17;

pub fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
}
pub fn hold() -> HoldRoot {
    HoldRoot::from_seed(&SEED).unwrap()
}
/// Wallet-side derivation of output `index` under spending root `root`.
pub fn derive(root: u64, hold: &HoldRoot, denom: u64, index: u32) -> Derivation {
    Derivation::derive(denom, point(root), hold.public(), &[23; 32], index).unwrap()
}
/// The owner key of `derive(root, .., index)`.
pub fn owner(root: u64, index: u32) -> Scalar {
    Scalar::from(root)
        + licash::note::derive_output(&point(root), &[23; 32], index)
            .unwrap()
            .tweak
            .to_scalar()
            .unwrap()
}
/// An ordinary issuance of `d` by `keys`.
pub fn mint(keys: &IssuerKeys, keyset: &Keyset, d: &Derivation) -> Note {
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

/// With `LICASH_NATIVE_INPUTS=<dir>`, keep a guest input for `r0/host`
/// (`native-exec`, `native-counter-exec`, `native-prove`, ...).
pub fn export(name: &str, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = std::env::var_os("LICASH_NATIVE_INPUTS") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(std::path::Path::new(&dir).join(name), bytes)?;
    }
    Ok(())
}

/// What a fixture opens: the holder's notes at this Hub, the assertion
/// deadline (blocks ahead) and the Hub's response window. `root` is the
/// holder's spending root, and `issuer` the Hub's keys when the Hub already
/// exists: a second holder's contract with the Hub of another fixture.
pub struct Terms {
    pub denominations: Vec<u64>,
    pub t_a: u32,
    pub w_h: u16,
    pub root: u64,
    pub issuer: Option<IssuerKeys>,
}

impl Default for Terms {
    fn default() -> Self {
        Self {
            denominations: vec![2048, 1024],
            t_a: 60,
            w_h: 6,
            root: SPEND,
            issuer: None,
        }
    }
}

/// A funded native contract with one Hub, and the holder's wallet there.
pub struct Fixture<'a> {
    pub label: String,
    pub rt: &'a Regtest,
    pub issuer: IssuerKeys,
    pub c: Opened,
    pub n: Native,
    /// The holder's spending root.
    pub root: u64,
    /// The holder's notes: `ds[i]` is owned by `owner(root, i)`.
    pub ds: Vec<Derivation>,
    pub notes: Vec<Note>,
}

/// The Hub's view of a posted request: R and P as read from the chain.
pub struct Requested {
    pub request: Request,
    pub publication: Publication,
    pub r: Txid,
    pub p: Txid,
    pub alice_b: WotsSig,
    pub alice_e: WotsSig,
}

pub fn fixture<'a>(rt: &'a Regtest, label: &str) -> Result<Fixture<'a>> {
    fixture_with(rt, label, Terms::default())
}

pub fn fixture_with<'a>(rt: &'a Regtest, label: &str, terms: Terms) -> Result<Fixture<'a>> {
    fixture_under(rt, label, terms, Authority::mock())
}

/// A fixture whose proofs `authority` vouches for. Under a real authority
/// the `paid_*` endings and `respond_counter` do not apply: they make mock
/// proofs.
pub fn fixture_under<'a>(
    rt: &'a Regtest,
    label: &str,
    terms: Terms,
    authority: Authority,
) -> Result<Fixture<'a>> {
    let root = terms.root;
    let issuer = match terms.issuer {
        Some(issuer) => issuer,
        None => IssuerKeys::random(12)?,
    };
    let keyset = issuer.keyset()?;
    let gate = |b: &licash::base::Base, p: &_, g: &[u8], tip| {
        b.holder_gate(p, g, &authority.programs, &SEED, &Scalar::from(root), tip)
    };
    let holder = Holder {
        keyset: keyset.clone(),
        spend_root: point(root),
        seed: SEED,
        gate: &gate,
    };
    let (c, n) = open_native(rt, label, (terms.t_a, terms.w_h), holder, authority)?;
    let ds: Vec<_> = terms
        .denominations
        .iter()
        .enumerate()
        .map(|(i, denom)| derive(root, &hold(), *denom, i as u32))
        .collect();
    let notes = ds.iter().map(|d| mint(&issuer, &keyset, d)).collect();
    Ok(Fixture {
        label: label.into(),
        rt,
        issuer,
        c,
        n,
        root,
        ds,
        notes,
    })
}

impl Fixture<'_> {
    pub fn keyset(&self) -> &Keyset {
        &self.n.base.parts().keyset
    }

    /// The holder's claim of every note in her (possibly stale) backup.
    pub fn claim(&self) -> Result<Publication> {
        let items = self
            .ds
            .iter()
            .map(|d| Item::Settled(d.message().clone()))
            .collect();
        self.claim_of(items, vec![], vec![], &self.ds)
    }

    /// A claim of `items` for their full value (fallbacks of a plain source
    /// add none), authorized by the hold key of each of `held`.
    pub fn claim_of(
        &self,
        items: Vec<Item>,
        sources: Vec<Source>,
        activations: Vec<ActivationRecord>,
        held: &[Derivation],
    ) -> Result<Publication> {
        let p = self.n.base.parts();
        let mut authorizations: Vec<Message> = held.iter().map(|d| d.message().clone()).collect();
        authorizations.sort_by_key(|m| m.identifier().unwrap());
        let fallback = |item: &Item| match item {
            Item::Pending { source, .. } => sources
                .iter()
                .any(|s| matches!(s, Source::Plain { id, .. } if id == source)),
            Item::Settled(_) => false,
        };
        let amount: u64 = items
            .iter()
            .filter(|item| !fallback(item))
            .map(|item| item.message().denomination())
            .sum();
        let claim = UnsignedClaim::new(
            claim::Context {
                final_digest: *self.n.d.as_bytes(),
                deposit: p.deposit,
                cap: p.cap,
            },
            self.keyset(),
            u32::try_from(amount)?,
            claim::Parts {
                items,
                sources,
                activations,
                authorizations,
            },
            p.limits.max_authorizations as usize,
        )?;
        let keys: Vec<_> = held
            .iter()
            .map(|d| (d.message().clone(), d.hold_key(&hold()).unwrap()))
            .collect();
        Ok(Publication::sign(claim, &keys, 128)?)
    }

    /// The holder's private evidence for `claim()`.
    pub fn witness(&self) -> HolderWitness {
        HolderWitness {
            items: self
                .ds
                .iter()
                .zip(&self.notes)
                .map(|(d, note)| ItemWitness::Settled {
                    note: Box::new(note.clone()),
                    ownership: d.ownership(),
                })
                .collect(),
            sources: vec![],
            chain: None,
            activations: vec![],
        }
    }

    /// A timed lock of the holder's notes `locked`: `pay` is issued if the
    /// preimage of the terms' hash is known in time, `refund` otherwise.
    pub fn lock(
        &self,
        locked: &[usize],
        pay: &[Derivation],
        refund: &[Derivation],
        terms: TimedTerms,
    ) -> Result<SignedProposal> {
        let outputs = |ds: &[Derivation]| {
            ds.iter()
                .map(Output::from_derivation)
                .collect::<Result<Vec<_>, _>>()
        };
        let notes: Vec<_> = locked.iter().map(|i| self.notes[*i].clone()).collect();
        let owners: Vec<_> = locked.iter().map(|i| owner(self.root, *i as u32)).collect();
        let notes = notes.as_slice();
        let lock = TimedLock::new(terms, outputs(refund)?)?;
        let proposal = Proposal::new(self.keyset(), notes, point(43), outputs(pay)?, Some(lock))?;
        Ok(SignedProposal::sign(
            self.keyset(),
            proposal,
            notes,
            Authorization::Plain,
            &owners,
        )?)
    }

    /// R then P; the Hub reads both from the chain. Returns the Hub's view.
    pub fn request(&self) -> Result<Requested> {
        self.request_of(self.claim()?)
    }

    pub fn request_of(&self, publication: Publication) -> Result<Requested> {
        let request = Request {
            amount: publication.claim().amount(),
            claim_digest: *publication.claim().digest(),
        };
        let r = self.n.request(&self.c, self.rt, request)?;
        let p = self.n.publish(self.rt, &publication)?;
        // The Hub: R's witness decodes under the frozen keys; P binds it.
        let (read, alice_b, alice_e) = self.n.read_request(&r)?;
        ensure!(read == request, "the Hub reads the holder's request");
        let start = self.n.base.parts().chain.checkpoint.height();
        let (seen, posted) = self.n.observe_publication(self.rt, &read, start)?;
        ensure!(seen == p && posted.to_bytes() == publication.to_bytes());
        Ok(Requested {
            request,
            publication: posted,
            r: r.compute_txid(),
            p,
            alice_b,
            alice_e,
        })
    }
}

/// The Hub's side of a request.
impl Fixture<'_> {
    /// Evidence that the holder paid her last note away after her backup:
    /// her signed spend of it to another wallet.
    pub fn paid_away(&self) -> Result<Vec<Evidence>> {
        Ok(vec![self.spent(self.notes.len() - 1)?])
    }

    /// The holder's signed spend of note `i` alone, to another wallet.
    pub fn spent(&self, i: usize) -> Result<Evidence> {
        let notes = &self.notes[i..=i];
        let value = notes[0].message().denomination();
        let payee = derive(41, &HoldRoot::from_seed(&[9; 32])?, value, 0);
        let outputs = vec![Output::from_derivation(&payee)?];
        let proposal = Proposal::new(self.keyset(), notes, point(43), outputs, None)?;
        let signed = SignedProposal::sign(
            self.keyset(),
            proposal,
            notes,
            Authorization::Plain,
            &[owner(self.root, i as u32)],
        )?;
        Ok(Evidence::Spend {
            signed,
            notes: notes.to_vec(),
            unlock: None,
        })
    }

    /// The counter guest's run on its decoded input: what the Hub proves.
    /// `chain` is needed only to cite a lock's expiry.
    pub fn counter(
        &self,
        q: &Requested,
        evidence: &[Evidence],
        chain: Option<Chain>,
    ) -> Result<CounterJournal> {
        let bytes = self.counter_input(q, evidence, chain)?;
        Ok(CounterInput::from_bytes(&bytes)?.run()?)
    }

    /// The counter guest's input, as the guest decodes it.
    pub fn counter_input(
        &self,
        q: &Requested,
        evidence: &[Evidence],
        chain: Option<Chain>,
    ) -> Result<Vec<u8>> {
        let input = CounterInput {
            base: self.n.base.clone(),
            prefix: self.n.prefix.clone(),
            publication: q.publication.clone(),
            evidence: evidence.to_vec(),
            chain,
        };
        let bytes = input.to_bytes()?;
        export(&format!("{}-counter.bin", self.label), &bytes)?;
        Ok(bytes)
    }

    /// The Hub posts `respond`: it has no counter.
    pub fn respond_plain(&self, q: &Requested) -> Result<HubResponse> {
        let (_, hub_e) = self.c.respond_plain(self.rt, &q.alice_b, &q.alice_e)?;
        Ok(HubResponse::Plain { e_sig: sig(&hub_e) })
    }

    /// The Hub posts `respond_counter` sealing `counter` and publishing
    /// `posted`. The holder reads every part of it back from the chain.
    pub fn respond_counter(
        &self,
        q: &Requested,
        counter: &CounterJournal,
        posted: &Disclosures,
    ) -> Result<HubResponse> {
        self.respond_sealed(q, counter, posted, native_mock_counter(counter)?)
    }

    /// `respond_counter` carrying `k`, the counter journal with its seal.
    pub fn respond_sealed(
        &self,
        q: &Requested,
        counter: &CounterJournal,
        posted: &Disclosures,
        k: Vec<u8>,
    ) -> Result<HubResponse> {
        let (hub_e, k_sig, corr) = self.c.respond_masked_publishing(
            self.rt,
            &q.alice_b,
            &q.alice_e,
            &k,
            counter.owed,
            posted,
        )?;
        let publication = self.c.publication(self.rt)?;
        ensure!(&publication.disclosures == posted, "the chunks read back");
        Ok(HubResponse::Counter {
            e_sig: sig(&hub_e),
            k,
            k_sig: sig(&k_sig),
            b_corr: counter.owed,
            corr_sig: sig(&corr),
            publication: Box::new(publication),
        })
    }

    /// The 32 slots a posted `respond_counter` published, as anyone reads
    /// them from the chain.
    pub fn published(&self) -> Result<[[u8; 32]; 32]> {
        Ok(self.c.publication(self.rt)?.disclosures.values)
    }
}

/// The holder's side after the response, to her payout.
impl Fixture<'_> {
    /// The holder guest's input, as the guest decodes it.
    pub fn statement(
        &self,
        q: &Requested,
        c: u32,
        response: HubResponse,
        holder: HolderWitness,
    ) -> Result<StatementInput> {
        let input = StatementInput {
            c,
            base: self.n.base.clone(),
            prefix: self.n.prefix.clone(),
            observed: self.n.observed(self.rt, &q.r, &q.p)?,
            response,
            holder,
        };
        let bytes = input.to_bytes()?;
        export(&format!("{}-holder-c{c}.bin", self.label), &bytes)?;
        Ok(StatementInput::from_bytes(&bytes)?)
    }

    /// The payout's bits of `amount` collected: the Hub's clear bits at once,
    /// the holder's set bits after the delay, both reading her signature
    /// `wire`.
    pub fn collect(
        &self,
        payout: &Transaction,
        stage: Stage,
        amount: u32,
        wire: &[Vec<u8>],
    ) -> Result<()> {
        let (rt, c) = (self.rt, &self.c);
        let terms = &c.babe.spec.terms;
        let (set, clear): (Vec<u32>, Vec<u32>) =
            (terms.low_bits..terms.bits).partition(|i| (amount >> i) & 1 == 1);
        let sweep = c.bit_spend(rt, payout, stage, "hub_bit", &clear, wire)?;
        c.post(rt, &sweep)?;
        let take = c.bit_spend(rt, payout, stage, "alice_bit", &set, wire)?;
        let claimed: u64 = take
            .input
            .iter()
            .map(|i| {
                payout.output[i.previous_output.vout as usize]
                    .value
                    .to_sat()
            })
            .sum();
        let low = (1u32 << terms.low_bits) - 1;
        ensure!(
            claimed == u64::from(amount & !low),
            "the holder's payout outputs are worth {claimed}, not {amount}"
        );
        let early = rt.test_accept(&take).unwrap_err();
        ensure!(early.contains("non-BIP68-final"), "{early}");
        rt.mine(u64::from(c.babe.spec.params.delta))?;
        let taken = c.post(rt, &take)?;
        let value: u64 = taken.output.iter().map(|o| o.value.to_sat()).sum();
        ensure!(
            value + 2_000 > u64::from(amount & !511),
            "the holder collects her bits"
        );
        Ok(())
    }

    /// No proven counter: the holder proves the Full statement and, with no
    /// challenge, is paid her whole request.
    pub fn paid_in_full(
        &self,
        q: &Requested,
        response: HubResponse,
        holder: HolderWitness,
    ) -> Result<()> {
        let (rt, c) = (self.rt, &self.c);
        let (j, paid) = self
            .statement(q, 0, response, holder)?
            .run(&mut native_mock_counter_verify)?;
        ensure!(j == journal(&self.n.base, &self.n.d, 0) && paid == q.request.amount);
        let asserted = c.assert(rt, &mock_proof(&j))?;
        let (_, read) = c.asserted(&asserted)?;
        ensure!(mock_verify(&j, &read), "the Hub reads the holder's proof");
        let success = c.timeout(rt, "success")?;
        ensure!(success.output == c.set["success"].tx.output);
        self.collect(&success, Stage::Full, paid, &Babe::request_wire(&q.alice_b))
    }

    /// A proven counter: the Full statement is refused, and the holder
    /// proves the corrected one and is paid `owed`, the Hub's `b_corr`.
    pub fn paid_corrected(
        &self,
        q: &Requested,
        response: HubResponse,
        holder: HolderWitness,
        owed: u32,
    ) -> Result<()> {
        let (rt, c) = (self.rt, &self.c);
        let full = self.statement(q, 0, response.clone(), holder.clone())?;
        let refused = full.run(&mut native_mock_counter_verify).unwrap_err();
        ensure!(refused.to_string().contains("countered"), "{refused}");
        let (j, paid) = self
            .statement(q, 1, response, holder)?
            .run(&mut native_mock_counter_verify)?;
        ensure!(j == journal(&self.n.base, &self.n.d, 1) && paid == owed);
        let proof = mock_proof(&j);
        let hub_corr = Opened::responded_corrected(&rt.get_tx(&c.txid("respond_counter"))?)?;
        ensure!(hub_corr.message() == owed.to_be_bytes());
        ensure!(
            c.accepts(
                rt,
                "assert_corrected",
                c.assert_corrected_wire(&proof, &hub_corr, q.request.amount)?
            )
            .is_err(),
            "no more than the Hub's b_corr"
        );
        let asserted = c.assert_corrected(rt, &proof, &hub_corr)?;
        let (_, read) = c.asserted(&asserted)?;
        ensure!(mock_verify(&j, &read), "the Hub reads the corrected proof");
        let success = c.timeout(rt, "success_corrected")?;
        ensure!(success.output == c.set["success_corrected"].tx.output);
        let mine = Opened::asserted_b_corr(&asserted)?;
        self.collect(&success, Stage::Corrected, owed, &Babe::request_wire(&mine))
            .context("corrected payout")
    }
}

/// Claims of a timed lock, by either side of it.
impl Fixture<'_> {
    fn pending(
        branch: Branch,
        source: ProposalId,
        ds: &[Derivation],
        z: Option<[u8; 32]>,
    ) -> Vec<Item> {
        ds.iter()
            .enumerate()
            .map(|(i, d)| Item::Pending {
                message: d.message().clone(),
                source,
                branch,
                index: i as u32,
                blinding: *d.blinding(),
                preimage: z,
            })
            .collect()
    }

    /// The payer's claim: her notes outside `locked`, and the `refund`
    /// outputs of her own lock `signed` of the notes `locked`.
    pub fn refund_claim(
        &self,
        locked: &[usize],
        signed: &SignedProposal,
        refund: &[Derivation],
    ) -> Result<(Publication, HolderWitness)> {
        let notes: Vec<_> = locked.iter().map(|i| self.notes[*i].clone()).collect();
        let source = Metadata::from_source(self.keyset(), signed, &notes)?;
        let id = signed.proposal().id();
        let free: Vec<usize> = (0..self.ds.len()).filter(|i| !locked.contains(i)).collect();
        let mut items: Vec<_> = free
            .iter()
            .map(|i| Item::Settled(self.ds[*i].message().clone()))
            .collect();
        items.extend(Self::pending(Branch::Refund, id, refund, None));
        let sources = vec![Source::OwnInputs {
            id,
            inputs: source.inputs().to_vec(),
            terms: source.terms().clone(),
        }];
        // Every note, locked or not, authorizes the claim with its hold key.
        let held: Vec<_> = self.ds.iter().chain(refund).cloned().collect();
        let publication = self.claim_of(items, sources, vec![], &held)?;
        let mut witness = self.witness();
        witness.items = free.iter().map(|i| witness.items[*i].clone()).collect();
        witness
            .items
            .extend(refund.iter().map(|d| ItemWitness::Pending {
                ownership: d.ownership(),
            }));
        witness.sources = vec![SourceWitness::Signed {
            signed: signed.clone(),
            notes,
        }];
        Ok((publication, witness))
    }

    /// The payee's claim: her notes, and the `pay` outputs of the lock the
    /// issuer certified, opened with its preimage `z`. `activation` is where
    /// she published `z` on Bitcoin before the lock expired, if she did.
    pub fn pay_claim(
        &self,
        certificate: &Certificate,
        source: &SignedProposal,
        pay: &[Derivation],
        z: [u8; 32],
        activation: Option<Activation>,
    ) -> Result<(Publication, HolderWitness)> {
        let id = source.proposal().id();
        let mut items: Vec<_> = self
            .ds
            .iter()
            .map(|d| Item::Settled(d.message().clone()))
            .collect();
        items.extend(Self::pending(Branch::Pay, id, pay, Some(z)));
        let records = activation
            .iter()
            .map(|a| ActivationRecord {
                source: id,
                reference: a.reference,
            })
            .collect();
        let held: Vec<_> = self.ds.iter().chain(pay).cloned().collect();
        let sources = vec![Source::Admitted(certificate.clone())];
        let publication = self.claim_of(items, sources, records, &held)?;
        let mut witness = self.witness();
        witness
            .items
            .extend(pay.iter().map(|d| ItemWitness::Pending {
                ownership: d.ownership(),
            }));
        witness.sources = vec![SourceWitness::Certified {
            proposal: source.proposal().clone(),
            signatures: source.signatures().to_vec(),
        }];
        if activation.is_some() {
            witness.chain = Some(self.n.chain(self.rt)?);
        }
        witness.activations = activation.into_iter().collect();
        Ok((publication, witness))
    }
}
