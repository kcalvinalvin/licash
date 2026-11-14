//! An opening as each party stores it, to resume it.
//!
//! A holder stores what she was sent and what she drew or signed, never
//! what she derives again (her keys come from her seed and the opening's
//! id, and she brings the seed and her spending secret to [`Holder::load`]
//! again). A hub stores its opening's secret, which all its keys derive
//! from, and what it was sent and signed, not its instances, which it
//! builds again to send ([`Hub::instance`]). Loading checks each part as
//! the step that took it did, but for instances it would rebuild (the
//! holder stores the opened ones' shapes) and the hub's gate, whose
//! deadline may have passed. It refuses a state of another format,
//! derivation, side, seed or trust, and never reads one otherwise.
//!
//! The format: `LCOS`, u16 [`FORMAT`], u8 [`DERIVATION`], u8 side (0, a
//! holder's; 1, a hub's), u8 phase ([`Phase`], [`HubPhase`]), then each
//! phase's section up to its own. A holder's: her id, quote, Open,
//! checkpoint and any fixture's choice; Base; the hub's Commit and her
//! choice; its Opened, the opened instances' shapes and her kept
//! instances' hashlocks; her Consent and her withheld signatures; the
//! hub's Signed; F signed whole. A hub's: its secret, quote, the id, her
//! Open and Base; her Keys; its Commit and its instances' hashlocks; her
//! choice; her Consent; its Signed, F with its own inputs signed in it;
//! F signed whole.

use anyhow::{bail, ensure, Context as _, Result};
use k256::Scalar;
use licash::base::Base;
use licash::chain::Checkpoint;

use super::{
    check_choice, check_sigs, compile, draft_hmsg, programs, same_kind, send, Context, Holder, Hub,
    Out, Start,
};
use crate::keys::DERIVATION;
use crate::wire::{self, Kind, Reader, Writer, STAGES};
use crate::INSTANCES;
use lichen_babe::vsss::Dealing;

pub const MAGIC: [u8; 4] = *b"LCOS";
/// The version of the format: a state of another is refused.
pub const FORMAT: u16 = 1;
const HOLDER: u8 = 0;
const HUB: u8 = 1;

/// How far an opening went, by the last message its holder took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Her Open sent.
    Started,
    /// Base taken, her keys sent.
    Termed,
    /// The hub's commitment taken, her choice sent.
    Committed,
    /// The opened instances taken, her kept ones coming in.
    Opened,
    /// Her kept instances in, her consent sent.
    Consented,
    /// The hub's signatures taken: F hers to sign.
    Signing,
    /// F signed whole and sent.
    Funded,
}

impl Phase {
    const ALL: [Phase; 7] = [
        Phase::Started,
        Phase::Termed,
        Phase::Committed,
        Phase::Opened,
        Phase::Consented,
        Phase::Signing,
        Phase::Funded,
    ];

    /// Whether the contract may stand from it on: her F signed, which the
    /// hub may complete and broadcast. Loaded from it on, her opening is
    /// judged by the programs and the floors it was opened under, not by
    /// her trust now: a new release may have changed them, and refusing
    /// would strand her contract.
    pub fn binding(self) -> bool {
        self >= Phase::Funded
    }
}

fn flag(byte: u8) -> Result<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        other => bail!("a flag {other}"),
    }
}

fn choice(bytes: &[u8]) -> Result<[Vec<usize>; STAGES]> {
    let kept = wire::Choose::decode(bytes)?.kept;
    check_choice(&kept)?;
    Ok(kept)
}

impl Holder {
    pub fn phase(&self) -> Phase {
        [
            (self.funding.is_some(), Phase::Funded),
            (self.signed.is_some(), Phase::Signing),
            (self.graph.is_some(), Phase::Consented),
            (self.opened.is_some(), Phase::Opened),
            (self.commit.is_some(), Phase::Committed),
            (self.base.is_some(), Phase::Termed),
        ]
        .into_iter()
        .find_map(|(reached, phase)| reached.then_some(phase))
        .unwrap_or(Phase::Started)
    }

    /// Whether her driver may give the opening up and spend her deposit
    /// otherwise: until she has signed F, which the hub may then complete
    /// and broadcast. Her signed F is sent only once stored as
    /// [`Phase::Funded`], so a state short of it never let it out.
    pub fn abandonable(&self) -> bool {
        !self.phase().binding()
    }

    /// The instances she chose and has not had yet, by stage and index.
    pub fn missing(&self) -> Vec<(usize, usize)> {
        let Some((_, _, chosen)) = &self.commit else {
            return vec![];
        };
        let mut missing = vec![];
        for (s, chosen) in chosen.iter().enumerate() {
            for i in chosen {
                if !self.kept[s].iter().any(|(j, _)| j == i) {
                    missing.push((s, *i));
                }
            }
        }
        missing
    }

    /// What the step that reached her phase asked, but instances to keep:
    /// to carry out again on resuming, as her driver may not have. At
    /// [`Phase::Signing`], only while her gate passes at her tip.
    pub fn pending(&self, ctx: &Context) -> Result<Vec<Out>> {
        same_kind(self.mock, ctx)?;
        let expect = "a phase's own";
        Ok(match self.phase() {
            Phase::Started => vec![send(Kind::Open, self.id, self.open().encode())],
            Phase::Termed => {
                let (_, keys) = self.base.as_ref().expect(expect);
                vec![send(Kind::Keys, self.id, keys.encode())]
            }
            Phase::Committed => vec![self.choose(&self.commit.as_ref().expect(expect).2)],
            Phase::Opened => vec![],
            Phase::Consented => {
                let graph = self.graph.as_ref().expect(expect);
                vec![send(Kind::Consent, self.id, graph.consent.encode())]
            }
            Phase::Signing => {
                self.gate(ctx)?;
                vec![self.to_sign(&self.signed.as_ref().expect(expect).funding)]
            }
            Phase::Funded => vec![self.funded(self.funding.as_ref().expect(expect))],
        })
    }

    /// Her opening as she stores it: to store after each step taken,
    /// before carrying out what it asked (her instances to keep first).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(&MAGIC);
        w.u16(FORMAT);
        w.u8(DERIVATION);
        w.u8(HOLDER);
        w.u8(self.phase() as u8);
        w.bytes(&self.id);
        w.quote(&self.start.quote);
        w.blob(&self.open().encode());
        w.blob(&self.start.checkpoint.to_bytes());
        match &self.fixed {
            None => w.u8(0),
            Some(kept) => {
                w.u8(1);
                w.blob(&wire::Choose { kept: kept.clone() }.encode());
            }
        }
        if let Some((base, _)) = &self.base {
            w.blob(&base.to_bytes());
        }
        if let Some((c, _, kept)) = &self.commit {
            w.blob(&c.encode());
            w.blob(&wire::Choose { kept: kept.clone() }.encode());
        }
        if let Some((m, _, shapes)) = &self.opened {
            w.blob(&m.encode());
            shapes.iter().flatten().for_each(|n| w.u64(*n as u64));
            for kept in &self.kept {
                w.count(kept.len());
                for (i, hmsg) in kept {
                    w.u8(u8::try_from(*i).expect("an instance of 256"));
                    w.bytes(hmsg);
                }
            }
        }
        if let Some(graph) = &self.graph {
            w.blob(&graph.consent.encode());
            w.sigs(&graph.withheld);
        }
        if let Some(m) = &self.signed {
            w.blob(&m.encode());
        }
        if let Some(funding) = &self.funding {
            w.tx(funding);
        }
        w.0
    }

    /// Her opening as [`Holder::to_bytes`] stored it, under her `seed`
    /// and spending secret `spend` and her trust: each part checked as
    /// the step that took it did, but the opened instances, and once
    /// [`Phase::binding`] her trust's programs and her floors.
    pub fn load(bytes: &[u8], seed: [u8; 32], spend: Scalar, ctx: &Context) -> Result<Self> {
        let mut r = Reader(bytes);
        ensure!(r.array::<4>()? == MAGIC, "not an opening's state");
        let format = r.u16()?;
        ensure!(format == FORMAT, "an opening's state of format {format}");
        let derivation = r.u8()?;
        ensure!(derivation == DERIVATION, "keys of derivation {derivation}");
        ensure!(r.u8()? == HOLDER, "not a holder's opening");
        let phase = r.u8()?;
        let phase = *Phase::ALL
            .get(usize::from(phase))
            .with_context(|| format!("phase {phase}"))?;
        let id = r.array()?;
        let quote = r.quote()?;
        let open = wire::Open::decode(r.blob()?)?;
        same_kind(open.mock, ctx)?;
        let checkpoint = Checkpoint::from_bytes(r.blob()?)?;
        let fixed = match flag(r.u8()?)? {
            false => None,
            true => Some(choice(r.blob()?)?),
        };
        ensure!(
            cfg!(feature = "fixture") || fixed.is_none(),
            "a fixture's opening"
        );
        let start = Start {
            seed,
            spend,
            quote,
            deposit: open.deposit,
            coin: open.coin.clone(),
            change: open.change.clone(),
            wanted: open.wanted,
            checkpoint,
        };
        if !phase.binding() {
            start.quote.check(start.checkpoint.profile())?;
        }
        let mut h = Holder::started(start, id, open.mock, fixed);
        ensure!(
            h.open() == open,
            "an opening of another seed or spending secret"
        );
        if phase >= Phase::Termed {
            let base = Base::from_bytes(r.blob()?)?;
            let trusted = match phase.binding() {
                true => programs(&base),
                false => ctx.trust.programs,
            };
            let keys = h.check_terms(&base, trusted)?;
            h.base = Some((base, keys));
        }
        if phase >= Phase::Committed {
            let c = wire::Commit::decode(r.blob()?)?;
            let frozen = h.check_commit(&c)?;
            let kept = choice(r.blob()?)?;
            ensure!(
                h.fixed.as_ref().is_none_or(|f| *f == kept),
                "a choice other than the fixture's"
            );
            h.commit = Some((c, frozen, kept));
        }
        if phase >= Phase::Opened {
            let m = wire::Opened::decode(r.blob()?)?;
            let shares = h.check_opened(&m)?;
            let mut shapes = [[0; 4]; STAGES];
            for n in shapes.iter_mut().flatten() {
                *n = usize::try_from(r.u64()?)?;
            }
            let chosen = &h.commit.as_ref().expect("Commit before Opened").2;
            for (s, chosen) in chosen.iter().enumerate() {
                for _ in 0..r.count(33)? {
                    let (i, hmsg) = (usize::from(r.u8()?), r.array()?);
                    ensure!(
                        chosen.contains(&i) && !h.kept[s].iter().any(|(j, _)| *j == i),
                        "instance {i} of assertion {s} kept not as chosen"
                    );
                    h.kept[s].push((i, hmsg));
                }
            }
            let all = h.missing().is_empty();
            ensure!(
                all == (phase >= Phase::Consented),
                "her kept instances {} in, her consent {}",
                if all { "all" } else { "not all" },
                if all { "not sent" } else { "sent" }
            );
            ensure!(
                !all || h.kept.iter().all(|k| k.windows(2).all(|w| w[0].0 < w[1].0)),
                "her kept instances out of order"
            );
            h.opened = Some((m, shares, shapes));
        }
        if phase >= Phase::Consented {
            let consent = wire::Consent::decode(r.blob()?)?;
            let withheld = r.sigs()?;
            h.graph = Some(h.check_graph(consent, withheld)?);
        }
        if phase >= Phase::Signing {
            let m = wire::Signed::decode(r.blob()?)?;
            h.check_signed(&m)?;
            h.signed = Some(m);
        }
        if phase >= Phase::Funded {
            let funding = r.tx()?;
            h.check_funding(&funding)?;
            h.funding = Some(funding);
        }
        ensure!(r.0.is_empty(), "bytes past an opening's state");
        Ok(h)
    }
}

/// How far an opening went, by the last input its hub took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HubPhase {
    /// Her Open taken, Base sent.
    Termed,
    /// Her keys taken: F its to fund.
    Keyed,
    /// F funded and frozen, its commitment sent.
    Committed,
    /// Her choice taken, the opened instances and hers sent.
    Chosen,
    /// Her consent taken through its gate: its own inputs of F to sign.
    Consented,
    /// F with its own inputs signed taken, its signatures sent.
    Signed,
    /// F signed whole taken: its to broadcast.
    Funded,
}

impl HubPhase {
    const ALL: [HubPhase; 7] = [
        HubPhase::Termed,
        HubPhase::Keyed,
        HubPhase::Committed,
        HubPhase::Chosen,
        HubPhase::Consented,
        HubPhase::Signed,
        HubPhase::Funded,
    ];

    /// Whether the contract may stand from it on: its graph signed, with
    /// which the holder may complete F. Loaded from it on, its opening is
    /// judged by the programs it was opened under, not by its trust now.
    pub fn binding(self) -> bool {
        self >= HubPhase::Signed
    }
}

fn dealing(d: &Dealing) -> Vec<u8> {
    let mut w = Writer::default();
    w.dealing(d);
    w.0
}

impl Hub {
    /// How far its opening went: none before an Open.
    pub fn phase(&self) -> Option<HubPhase> {
        [
            (self.funded.is_some(), HubPhase::Funded),
            (self.signed.is_some(), HubPhase::Signed),
            (self.consent.is_some(), HubPhase::Consented),
            (self.choice.is_some(), HubPhase::Chosen),
            (self.frozen.is_some(), HubPhase::Committed),
            (self.holder.is_some(), HubPhase::Keyed),
            (self.base.is_some(), HubPhase::Termed),
        ]
        .into_iter()
        .find_map(|(reached, phase)| reached.then_some(phase))
    }

    /// Whether its driver may give the opening up and forget it: until it
    /// has signed the graph, with which the holder may complete F and the
    /// contract stands. Its signatures are sent only once stored as
    /// [`HubPhase::Signed`].
    pub fn abandonable(&self) -> bool {
        !self.phase().is_some_and(HubPhase::binding)
    }

    /// What the step that reached its phase asked: to carry out again on
    /// resuming, as its driver may not have.
    pub fn pending(&self) -> Result<Vec<Out>> {
        let expect = "a phase's own";
        let Some(phase) = self.phase() else {
            return Ok(vec![]);
        };
        Ok(match phase {
            HubPhase::Termed => vec![send(Kind::Terms, self.id(), self.base().to_bytes())],
            HubPhase::Keyed => vec![self.to_fund(&self.holder.as_ref().expect(expect).1)?],
            HubPhase::Committed => {
                let (_, c, _) = self.frozen.as_ref().expect(expect);
                vec![send(Kind::Commit, self.id(), c.encode())]
            }
            HubPhase::Chosen => self.chosen(),
            HubPhase::Consented => vec![self.to_sign()],
            HubPhase::Signed => {
                let signed = self.signed.as_ref().expect(expect);
                vec![send(Kind::Signed, self.id(), signed.encode())]
            }
            HubPhase::Funded => vec![Out::Broadcast(self.funded.clone().expect(expect))],
        })
    }

    /// Its opening as it stores it, its secret in it: to store after each
    /// step taken, before carrying out what it asked. Refused before an
    /// Open, which a hub's secret is drawn for.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let phase = self.phase().context("a hub stored before an Open")?;
        let mut w = Writer::default();
        w.bytes(&MAGIC);
        w.u16(FORMAT);
        w.u8(DERIVATION);
        w.u8(HUB);
        w.u8(phase as u8);
        w.bytes(&self.secret);
        w.quote(&self.quote);
        w.bytes(&self.id());
        let (open, base) = self.base.as_ref().expect("an Open");
        w.blob(&open.encode());
        w.blob(&base.to_bytes());
        if let Some((keys, _)) = &self.holder {
            w.blob(&keys.encode());
        }
        if let Some((_, c, hmsg)) = &self.frozen {
            w.blob(&c.encode());
            for hmsg in hmsg {
                w.count(hmsg.len());
                hmsg.iter().for_each(|h| w.bytes(h));
            }
        }
        if let Some(c) = &self.choice {
            w.blob(
                &wire::Choose {
                    kept: c.kept.clone(),
                }
                .encode(),
            );
        }
        if let Some(consent) = &self.consent {
            w.blob(&consent.encode());
        }
        if let Some(signed) = &self.signed {
            w.blob(&signed.encode());
        }
        if let Some(funding) = &self.funded {
            w.tx(funding);
        }
        Ok(w.0)
    }

    /// Its opening as [`Hub::to_bytes`] stored it, under its trust: each
    /// part checked as the step that took it did, but its instances, which
    /// [`Hub::instance`] checks as it builds them again, its gate, whose
    /// deadline may have passed since (her consent is checked alone), and
    /// once [`HubPhase::binding`] its trust's programs.
    pub fn load(bytes: &[u8], ctx: &Context) -> Result<Self> {
        let mut r = Reader(bytes);
        ensure!(r.array::<4>()? == MAGIC, "not an opening's state");
        let format = r.u16()?;
        ensure!(format == FORMAT, "an opening's state of format {format}");
        let derivation = r.u8()?;
        ensure!(derivation == DERIVATION, "keys of derivation {derivation}");
        ensure!(r.u8()? == HUB, "not a hub's opening");
        let phase = r.u8()?;
        let phase = *HubPhase::ALL
            .get(usize::from(phase))
            .with_context(|| format!("phase {phase}"))?;
        let secret = r.array()?;
        let mut h = Hub::with(&secret, r.quote()?, None);
        let id = r.array()?;
        let open = wire::Open::decode(r.blob()?)?;
        let stored = Base::from_bytes(r.blob()?)?;
        let trusted = match phase.binding() {
            true => programs(&stored),
            false => ctx.trust.programs,
        };
        let base = h.offer(id, &open, trusted, ctx)?;
        ensure!(
            stored.digest() == base.digest(),
            "Base other than its quote offers"
        );
        h.id = Some(id);
        h.base = Some((open, base));
        if phase >= HubPhase::Keyed {
            let keys = wire::Keys::decode(r.blob()?)?;
            let draft = compile(h.base(), &keys, draft_hmsg())?;
            h.holder = Some((keys, draft));
        }
        if phase >= HubPhase::Committed {
            let c = wire::Commit::decode(r.blob()?)?;
            let frozen = h.check_fund(c.funding.clone(), &c.prevouts)?;
            ensure!(
                frozen.publication.coins == c.publication,
                "publication coins other than F's"
            );
            ensure!(*frozen.d.as_bytes() == c.d, "D other than its own");
            ensure!(
                c.dealings
                    .iter()
                    .zip(h.dealt())
                    .all(|(a, d)| dealing(a) == dealing(&d.dealing)),
                "dealings other than its own"
            );
            let mut hmsg: [Vec<[u8; 32]>; STAGES] = Default::default();
            for (s, hmsg) in hmsg.iter_mut().enumerate() {
                for _ in 0..r.count(32)? {
                    hmsg.push(r.array()?);
                }
                ensure!(
                    hmsg.len() == INSTANCES && c.commitments[s].len() == INSTANCES,
                    "{} instances of assertion {s}",
                    hmsg.len()
                );
            }
            h.frozen = Some((frozen, c, hmsg));
        }
        if phase >= HubPhase::Chosen {
            h.choice = Some(h.check_choose(choice(r.blob()?)?)?);
        }
        if phase >= HubPhase::Consented {
            let consent = wire::Consent::decode(r.blob()?)?;
            h.check_consent(&consent)?;
            let (frozen, _, _) = h.frozen.as_ref().expect("F before a consent");
            h.base()
                .verify_owner_consent(&frozen.prefix, &consent.consent)?;
            h.consent = Some(consent);
        }
        if phase >= HubPhase::Signed {
            let signed = wire::Signed::decode(r.blob()?)?;
            h.check_signing(&signed.funding)?;
            let set = &h.choice.as_ref().expect("a choice before a consent").set;
            check_sigs(set, &h.keys.party.public().payment, &signed.sigs, |_| false)?;
            h.signed = Some(signed);
        }
        if phase >= HubPhase::Funded {
            let funding = r.tx()?;
            ensure!(h.whole(&funding)? == funding, "F other than its own whole");
            h.funded = Some(funding);
        }
        ensure!(r.0.is_empty(), "bytes past an opening's state");
        Ok(h)
    }
}
