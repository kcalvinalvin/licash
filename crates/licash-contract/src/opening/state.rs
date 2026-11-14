//! A holder's opening as she stores it, to resume it: what she was sent
//! and what she drew or signed, never what she derives again (her keys
//! come from her seed and the opening's id, and she brings the seed and
//! her spending secret to [`Holder::load`] again). Loading checks each
//! part as the step that took it did, but for the opened instances it
//! would rebuild: their shapes are stored. It refuses a state of another
//! format, derivation, side, seed or trust, and never reads one otherwise.
//!
//! The format: `LCOS`, u16 [`FORMAT`], u8 [`DERIVATION`], u8 side (0, a
//! holder's), u8 [`Phase`], then each phase's section up to its own: her
//! id, quote, Open, checkpoint and any fixture's choice; Base; the hub's
//! Commit and her choice; its Opened, the opened instances' shapes and her
//! kept instances' hashlocks; her Consent and her withheld signatures; the
//! hub's Signed; F signed whole.

use anyhow::{bail, ensure, Context as _, Result};
use k256::Scalar;
use licash::base::Base;
use licash::chain::Checkpoint;

use super::{check_choice, same_kind, send, Context, Holder, Out, Start};
use crate::keys::DERIVATION;
use crate::wire::{self, Kind, Reader, Writer, STAGES};

pub const MAGIC: [u8; 4] = *b"LCOS";
/// The version of the format: a state of another is refused.
pub const FORMAT: u16 = 1;
const HOLDER: u8 = 0;

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
                vec![Out::Sign(
                    self.signed.as_ref().expect(expect).funding.clone(),
                )]
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
    /// the step that took it did, but the opened instances.
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
            wanted: open.wanted,
            checkpoint,
        };
        start.quote.check(start.checkpoint.profile())?;
        let mut h = Holder::started(start, id, open.mock, fixed);
        ensure!(
            h.open() == open,
            "an opening of another seed or spending secret"
        );
        if phase >= Phase::Termed {
            let base = Base::from_bytes(r.blob()?)?;
            let keys = h.check_terms(&base, ctx)?;
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
