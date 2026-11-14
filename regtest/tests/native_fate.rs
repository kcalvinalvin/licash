//! An opening on the chain, each party stalled at a step: what spends F's
//! inputs tells whether it stands or never can. Short of its signature a
//! party may leave; from it on it gives up only once its gate lapsed, by
//! spending a coin of its own of F, and F confirming first still stands.
//! A hub restarted reserves its coins of F again; a deposit owed no change
//! opens paying none.

use anyhow::{bail, ensure, Context as _, Result};
use k256::Scalar;
use licash::note::IssuerKeys;
use licash_contract::opening::{self as machine, Context, Fate, HubPhase, Input, Out, Phase};
use licash_contract::wire::{self, Envelope, Kind};
use licash_regtest::native::{
    agree, cancel, checkpoint, quote, reclaim, release, reserve, reserved, run, spent, Authority,
    Holder, Run,
};
use licash_regtest::native_fixture::{SEED, SPEND};
use lngap_btc::bitcoin::{Amount, OutPoint};
use lngap_btc::regtest::Regtest;
use std::collections::HashSet;

/// Assertion deadline and response window: her gate lapses some 25
/// blocks in.
const DEADLINES: (u32, u16) = (40, 6);

fn holder() -> Result<Holder> {
    Ok(Holder {
        keyset: IssuerKeys::random(12)?.keyset()?,
        seed: SEED,
        spend: Scalar::from(SPEND),
    })
}

fn context(rt: &Regtest) -> Result<Context> {
    Ok(Context {
        tip: rt.height()?,
        trust: Authority::mock().trust(),
    })
}

/// An opening of a 500 000 sat deposit whose messages of `kind` are lost:
/// it stalls there, refusing nothing.
fn stalled(rt: &Regtest, label: &str, kind: Kind) -> Result<Run> {
    let mut lose = |k: Kind, m: Vec<u8>| if k == kind { vec![] } else { vec![m] };
    let deposit = Amount::from_sat(500_000);
    let run = run(
        rt,
        label,
        DEADLINES,
        holder()?,
        Authority::mock(),
        deposit,
        &mut lose,
    )?;
    ensure!(!run.funded, "funded without {kind:?}");
    ensure!(run.refused.is_empty(), "refused {:?}", run.refused);
    Ok(run)
}

/// Her F's fate, as the node shows what spends its inputs.
fn hers(rt: &Regtest, run: &Run) -> Result<Fate> {
    let fate = run.alice.fate(|o| spent(rt, o, run.from))?;
    fate.context("her F unknown")
}

/// The hub's F's fate likewise.
fn its(rt: &Regtest, hub: &machine::Hub, from: u32) -> Result<Fate> {
    hub.fate(|o| spent(rt, o, from))?.context("its F unknown")
}

/// Mine until `lapsed` at the tip, F's fate `open` all along.
fn until_lapsed(
    rt: &Regtest,
    lapsed: impl Fn(u32) -> bool,
    open: impl Fn() -> Result<Fate>,
) -> Result<()> {
    for _ in 0..100 {
        if lapsed(rt.height()?) {
            return Ok(());
        }
        ensure!(open()? == Fate::Open, "F's fate before the gate lapsed");
        rt.mine(1)?;
    }
    bail!("no gate lapsed in 100 blocks")
}

/// The hub's coins of its F.
fn coins(hub: &machine::Hub) -> Result<HashSet<OutPoint>> {
    let f = hub.funding().context("its F unknown")?;
    Ok(f.hub_coins().into_iter().map(|(o, _)| o).collect())
}

/// The hub signed, its signatures lost: she leaves, as she may. Restarted,
/// it reserves its coins of F again, and only those; once its gate lapsed
/// it cancels by spending one, F dead once that is buried, and releases
/// the rest. Her deposit is hers all along.
#[test]
fn a_hub_whose_holder_left_cancels_with_its_own_coin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let run = stalled(&rt, "native-fate-left", Kind::Signed)?;
    let phases = (run.alice.phase(), run.hub.phase());
    ensure!(
        phases == (Phase::Consented, Some(HubPhase::Signed)),
        "{phases:?}"
    );
    ensure!(run.alice.abandonable() && !run.hub.abandonable());
    let ours = coins(&run.hub)?;
    ensure!(
        !ours.is_empty() && reserved(&rt)? == ours,
        "its coins of F unreserved"
    );

    let stored = &run.hubs[&HubPhase::Signed];
    let hub = machine::Hub::load(stored, &context(&rt)?)?;
    reserve(&rt, &[&hub])?;
    ensure!(reserved(&rt)? == ours, "restarted, it reserves otherwise");
    ensure!(!hub.lapsed(rt.height()?), "lapsed at once");
    until_lapsed(&rt, |tip| hub.lapsed(tip), || its(&rt, &hub, run.from))?;
    let f = hub.funding().context("its F")?;
    rt.send_raw(&cancel(&rt, &f)?)?;
    ensure!(
        its(&rt, &hub, run.from)? == Fate::Open,
        "dead before its cancel is buried"
    );
    rt.mine(1)?;
    ensure!(
        its(&rt, &hub, run.from)? == Fate::Dead,
        "canceled, F lives on"
    );
    release(&rt, &f)?;
    ensure!(
        reserved(&rt)?.is_empty(),
        "coins of F reserved past its end"
    );

    ensure!(
        spent(&rt, run.deposit.0, run.from)?.is_none(),
        "her deposit spent"
    );
    rt.send_and_confirm(&reclaim(&rt, &run)?)?;
    Ok(())
}

/// She signed F, which the hub never had: once her gate lapsed, F
/// unconfirmed, she spends her deposit otherwise. F is dead to both once
/// that is buried, and the hub releases its coins.
#[test]
fn past_her_gate_she_takes_back_the_deposit_of_an_f_she_signed() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let run = stalled(&rt, "native-fate-late", Kind::Funded)?;
    let phases = (run.alice.phase(), run.hub.phase());
    ensure!(
        phases == (Phase::Funded, Some(HubPhase::Signed)),
        "{phases:?}"
    );
    ensure!(!run.alice.abandonable() && !run.hub.abandonable());
    until_lapsed(&rt, |tip| run.alice.lapsed(tip), || hers(&rt, &run))?;
    ensure!(
        spent(&rt, run.deposit.0, run.from)?.is_none(),
        "F confirmed"
    );
    rt.send_raw(&reclaim(&rt, &run)?)?;
    ensure!(
        hers(&rt, &run)? == Fate::Open,
        "dead before her spend is buried"
    );
    rt.mine(1)?;
    ensure!(
        hers(&rt, &run)? == Fate::Dead,
        "her deposit spent, F lives on"
    );
    ensure!(
        its(&rt, &run.hub, run.from)? == Fate::Dead,
        "F lives on for the hub"
    );
    ensure!(
        reserved(&rt)? == coins(&run.hub)?,
        "its coins of F unreserved"
    );
    release(&rt, &run.hub.funding().context("its F")?)?;
    ensure!(
        reserved(&rt)?.is_empty(),
        "coins of F reserved past its end"
    );
    Ok(())
}

/// The hub gives up before signing, as it may, and spends a coin of F
/// otherwise: F is dead to her once that is buried, and she leaves, her
/// deposit hers.
#[test]
fn a_hub_leaving_unsigned_spends_its_coin_and_ends_hers() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let run = stalled(&rt, "native-fate-reused", Kind::Consent)?;
    let phases = (run.alice.phase(), run.hub.phase());
    ensure!(
        phases == (Phase::Consented, Some(HubPhase::Chosen)),
        "{phases:?}"
    );
    ensure!(run.alice.abandonable() && run.hub.abandonable());
    let f = run.hub.funding().context("its F")?;
    release(&rt, &f)?;
    ensure!(
        reserved(&rt)?.is_empty(),
        "coins of F reserved once it left"
    );
    rt.send_raw(&cancel(&rt, &f)?)?;
    ensure!(
        hers(&rt, &run)? == Fate::Open,
        "dead before its spend is buried"
    );
    rt.mine(1)?;
    ensure!(hers(&rt, &run)? == Fate::Dead, "its coin spent, F lives on");
    ensure!(
        spent(&rt, run.deposit.0, run.from)?.is_none(),
        "her deposit spent"
    );
    rt.send_and_confirm(&reclaim(&rt, &run)?)?;
    Ok(())
}

/// Both gave up once their gates lapsed, but F, signed whole, which she
/// broadcast herself, confirmed first: their spends are refused, and F
/// stands for both. The hub, never sent it, takes F from the chain as her
/// signed F, and keeps its side.
#[test]
fn f_confirmed_first_stands_though_both_gave_up() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let run = stalled(&rt, "native-fate-first", Kind::Funded)?;
    let lapsed = |tip| run.alice.lapsed(tip) && run.hub.lapsed(tip);
    until_lapsed(&rt, lapsed, || hers(&rt, &run))?;
    let f = run.hub.funding().context("its F")?;
    let gave_up = [reclaim(&rt, &run)?, cancel(&rt, &f)?];
    let whole = run.alice.funding().context("her F")?.tx.clone();
    rt.send_and_confirm(&whole)?;
    for tx in &gave_up {
        let txid = tx.compute_txid();
        ensure!(rt.send_raw(tx).is_err(), "{txid} spent a coin of F after F");
    }
    ensure!(hers(&rt, &run)? == Fate::Live, "F confirmed, dead to her");
    ensure!(
        its(&rt, &run.hub, run.from)? == Fate::Live,
        "F confirmed, dead to it"
    );

    let mut hub = run.hub;
    let funded = Envelope {
        kind: Kind::Funded,
        id: run.alice.id(),
        body: wire::Funded {
            funding: whole.clone(),
        }
        .encode(),
    };
    let out = hub.step(Input::Message(funded.to_bytes()), &context(&rt)?)?;
    ensure!(out == vec![Out::Broadcast(whole)], "{out:?}");
    agree(&run.alice.opening()?, &hub.opening()?)
}

/// Her deposit owed less change than dust: F pays her none, the rest its
/// fee, and opens.
#[test]
fn a_deposit_owed_no_change_opens_paying_none() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let holder = holder()?;
    let q = quote(holder.keyset.clone(), checkpoint(&rt)?);
    let price = q.graph.reserve_alice + q.fees.opening_fee_quote;
    let value = Amount::from_sat(price + q.graph.dust - 1);
    let mut honest = |_: Kind, m: Vec<u8>| vec![m];
    let mock = Authority::mock();
    let run = run(
        &rt,
        "native-fate-exact",
        DEADLINES,
        holder,
        mock,
        value,
        &mut honest,
    )?;
    ensure!(
        run.funded && run.refused.is_empty(),
        "refused {:?}",
        run.refused
    );
    let f = run.alice.funding().context("her F")?.tx;
    ensure!(
        f.output.iter().all(|o| o.script_pubkey != run.change),
        "change paid out of {value}"
    );
    ensure!(hers(&rt, &run)? == Fate::Live, "F funded, not live");
    agree(&run.alice.opening()?, &run.hub.opening()?)
}
