//! The networked opening against a forging peer: each party refuses every
//! forged message of the other's, goes on as it was, and still opens on
//! the honest one; a holder given another hub's keys never funds.

use anyhow::{bail, ensure, Context as _, Result};
use bitcoincore_rpc::RpcApi;
use k256::Scalar;
use licash::base::{Base, Parts, Programs, ResponseKeys};
use licash::note::IssuerKeys;
use licash_contract::keys::HubKeys;
use licash_contract::opening::{self as machine, Context, Input, Out, Phase, Quote, Start, Trust};
use licash_contract::wire::{self, Envelope, Kind, Wanted};
use licash_regtest::native::{checkpoint, open_native_with, play, quote, Authority, Holder};
use licash_regtest::native_fixture::{point, SEED, SPEND};
use lichen_babe::setup::Instance;
use lngap_btc::bitcoin::hashes::Hash as _;
use lngap_btc::bitcoin::{Amount, OutPoint, ScriptBuf, Txid};
use lngap_btc::regtest::Regtest;
use std::collections::HashSet;

fn holder() -> Result<Holder> {
    Ok(Holder {
        keyset: IssuerKeys::random(12)?.keyset()?,
        seed: SEED,
        spend: Scalar::from(SPEND),
    })
}

/// The message `bytes` with its body decoded, changed by `f`, re-encoded.
fn forged<T>(
    bytes: &[u8],
    decode: fn(&[u8]) -> Result<T>,
    encode: fn(&T) -> Vec<u8>,
    f: impl FnOnce(&mut T),
) -> Result<Vec<u8>> {
    let e = Envelope::from_bytes(bytes)?;
    let mut m = decode(&e.body)?;
    f(&mut m);
    Ok(Envelope {
        body: encode(&m),
        ..e
    }
    .to_bytes())
}

fn base(bytes: &[u8], f: impl FnOnce(&mut Parts)) -> Result<Vec<u8>> {
    forged(
        bytes,
        |b| {
            let base = Base::from_bytes(b)?;
            Ok(base.parts().clone())
        },
        |p| Base::new(p.clone()).expect("a Base").to_bytes(),
        f,
    )
}

/// The same bytes, so the same commitment, split into other parts: the
/// last labels of the garbled circuit read as a first table entry.
fn repartitioned(k: &mut wire::Kept) {
    let mut x = Instance::from_bytes(&k.instance).expect("an instance");
    let commitment = x.commitment();
    let l = x.gc.ands.split_off(x.gc.ands.len() - 8);
    x.table
        .insert(0, [[l[0], l[1]], [l[2], l[3]], [l[4], l[5]], [l[6], l[7]]]);
    assert_eq!(x.commitment(), commitment);
    k.instance = x.to_bytes();
}

/// What the first message of each kind becomes: forgeries, then the
/// message itself (or, for a replay, the message twice).
fn forge(kind: Kind, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let id = |b: &[u8]| {
        let mut b = b.to_vec();
        b[7] ^= 1;
        b
    };
    let honest = bytes.to_vec();
    let witness = |f: &mut wire::Funded| f.funding.input.iter_mut().for_each(|i| i.witness.clear());
    Ok(match kind {
        Kind::Open => vec![honest.clone(), honest],
        Kind::Terms => {
            let mut version = honest.clone();
            version[4..6].copy_from_slice(&[0, 2]);
            vec![
                base(bytes, |p| p.recipe.holder_program = [12; 32])?,
                base(bytes, |p| p.graph.assert_height += 1)?,
                base(bytes, |p| p.spend_root = point(SPEND + 1))?,
                // What only the quote fixes: the windows, depths, fees, cap.
                base(bytes, |p| p.graph.holder_answer += 1)?,
                base(bytes, |p| p.graph.to_self_delay += 1)?,
                base(bytes, |p| p.chain.funding_depth += 1)?,
                base(bytes, |p| p.fees.opening_fee_quote += 1)?,
                base(bytes, |p| p.cap -= 1)?,
                id(bytes),
                version,
                honest,
            ]
        }
        Kind::Keys => {
            let e = Envelope::from_bytes(bytes)?;
            let mut body = e.body.clone();
            body.pop();
            let k =
                |f: fn(&mut wire::Keys)| forged(bytes, wire::Keys::decode, wire::Keys::encode, f);
            vec![
                Envelope { body, ..e }.to_bytes(),
                k(|k| {
                    k.corrected_b.pop();
                })?,
                k(|k| {
                    k.lamport[1].0.pop();
                })?,
                honest,
            ]
        }
        Kind::Commit => {
            let c = |f: fn(&mut wire::Commit)| {
                forged(bytes, wire::Commit::decode, wire::Commit::encode, f)
            };
            vec![
                c(|c| {
                    for o in &mut c.funding.output {
                        o.value += Amount::from_sat(1);
                    }
                })?,
                c(|c| {
                    for i in &mut c.funding.input {
                        i.previous_output.vout += 100;
                    }
                })?,
                c(|c| c.funding.input.extend(c.funding.input.clone()))?,
                c(|c| c.funding.input[0].script_sig = ScriptBuf::from_bytes(vec![0x51]))?,
                c(|c| c.d[0] ^= 1)?,
                c(|c| {
                    c.dealings[0].commitments[0][0].pop();
                })?,
                c(|c| c.publication[0].1.script_pubkey = ScriptBuf::from_bytes(vec![0x51]))?,
                c(|c| {
                    c.commitments[1].pop();
                })?,
                honest,
            ]
        }
        Kind::Choose => {
            let other = forged(bytes, wire::Choose::decode, wire::Choose::encode, |c| {
                c.kept[0] = if c.kept[0] == [0, 1] {
                    vec![2, 3]
                } else {
                    vec![0, 1]
                }
            })?;
            vec![honest.clone(), other, honest]
        }
        Kind::Opened => {
            let o = |f: fn(&mut wire::Opened)| {
                forged(bytes, wire::Opened::decode, wire::Opened::encode, f)
            };
            vec![
                o(|o| o.opened[0][0].shares[0][0][0] ^= 1)?,
                o(|o| o.opened[0][0].seed[0] ^= 1)?,
                o(|o| o.opened[0][0].index = 2)?,
                honest,
            ]
        }
        Kind::Instance => {
            let k =
                |f: fn(&mut wire::Kept)| forged(bytes, wire::Kept::decode, wire::Kept::encode, f);
            vec![
                k(|k| k.instance = vec![7; 64])?,
                k(|k| k.index = 0)?,
                k(|k| k.index = 3)?,
                k(|k| k.stage = 1)?,
                k(repartitioned)?,
                honest,
            ]
        }
        Kind::Consent => {
            let c = |f: fn(&mut wire::Consent)| {
                forged(bytes, wire::Consent::decode, wire::Consent::encode, f)
            };
            vec![
                c(|c| c.presigs.swap(0, 1))?,
                c(|c| {
                    c.sigs.remove("request");
                })?,
                c(|c| {
                    c.sigs.insert("wrongly_challenged_0".into(), vec![1; 64]);
                })?,
                c(|c| c.consent.0[0] ^= 1)?,
                c(|c| c.sigs.get_mut("request").expect("R signed")[0] ^= 1)?,
                honest,
            ]
        }
        Kind::Signed => {
            let s = |f: fn(&mut wire::Signed)| {
                forged(bytes, wire::Signed::decode, wire::Signed::encode, f)
            };
            vec![
                s(|s| {
                    let r = s.sigs["request"].clone();
                    s.sigs.insert("wrongly_challenged_0".into(), r);
                })?,
                s(|s| s.funding.output.push(s.funding.output[0].clone()))?,
                s(|s| {
                    for i in &mut s.funding.input {
                        i.witness.push([1]);
                    }
                })?,
                honest,
            ]
        }
        Kind::Funded => vec![
            forged(bytes, wire::Funded::decode, wire::Funded::encode, witness)?,
            id(bytes),
            honest,
        ],
    })
}

/// Every first message of each kind arrives forged several ways before
/// the honest one: each forgery is refused by its addressee, which goes on
/// as it was, and the opening funds on the honest messages.
#[test]
fn forged_messages_are_refused_and_the_opening_goes_on() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let mut seen = HashSet::new();
    let mut tamper = |kind: Kind, m: Vec<u8>| {
        if seen.insert(kind as u8) {
            forge(kind, &m).expect("a forgery")
        } else {
            vec![m]
        }
    };
    let (c, _, refused) = open_native_with(
        &rt,
        "native-forged",
        (60, 6),
        holder()?,
        Authority::mock(),
        &mut tamper,
    )?;
    use licash_regtest::native::Side::{Holder as H, Hub};
    // In delivery order: each refusal's party, kind and reason.
    let expected = [
        (Hub, Kind::Open, "Open again"),
        (H, Kind::Terms, "programs she does not trust"),
        (H, Kind::Terms, "deadlines she did not ask for"),
        (H, Kind::Terms, "not her spending root"),
        (H, Kind::Terms, "terms she was not quoted"),
        (H, Kind::Terms, "terms she was not quoted"),
        (H, Kind::Terms, "terms she was not quoted"),
        (H, Kind::Terms, "terms she was not quoted"),
        (H, Kind::Terms, "terms she was not quoted"),
        (H, Kind::Terms, "a message of another opening"),
        (H, Kind::Terms, "message of version 2"),
        (Hub, Kind::Keys, "count past the message"),
        (Hub, Kind::Keys, "a key of 9 chains, not 10"),
        (Hub, Kind::Keys, "253 digits, not 254"),
        (H, Kind::Commit, "F pays the graph 0 times"),
        (H, Kind::Commit, "F spends her deposit 0 times"),
        (H, Kind::Commit, "F spends her deposit 2 times"),
        (H, Kind::Commit, "F signed before the graph"),
        (H, Kind::Commit, "D not hers"),
        (H, Kind::Commit, "a dealing of another shape"),
        (H, Kind::Commit, "a publication coin not the graph's"),
        (H, Kind::Commit, "3 commitments"),
        // The same choice again is answered again.
        (Hub, Kind::Choose, "a second choice"),
        (H, Kind::Opened, "shares off the dealing"),
        (H, Kind::Opened, "an opened instance not its commitment"),
        (H, Kind::Opened, "opened instances she did not leave"),
        (H, Kind::Instance, "not an instance"),
        (H, Kind::Instance, "an instance she did not keep"),
        (H, Kind::Instance, "an instance not its commitment"),
        (H, Kind::Instance, "an instance not its commitment"),
        (
            H,
            Kind::Instance,
            "an instance of another shape than the opened",
        ),
        // The answer to the repeated choice.
        (H, Kind::Opened, "Opened again"),
        (
            Hub,
            Kind::Consent,
            "pre-signatures of assertion 0 not on its dealing",
        ),
        (Hub, Kind::Consent, "no signature of request"),
        (Hub, Kind::Consent, "signatures of what is not to sign"),
        (Hub, Kind::Consent, "signature does not verify"),
        (Hub, Kind::Consent, "request: signature failed verification"),
        (
            H,
            Kind::Signed,
            "wrongly_challenged_0: not an ALL|ANYONECANPAY signature",
        ),
        (H, Kind::Signed, "F not as frozen"),
        (H, Kind::Signed, "her input of F signed by another"),
        (Hub, Kind::Funded, "F not signed whole"),
        (Hub, Kind::Funded, "a message of another opening"),
    ];
    let matches = refused.len() == expected.len()
        && refused.iter().zip(&expected).all(|(r, (by, kind, why))| {
            r.by == *by && r.kind == Some(*kind) && r.error.contains(why)
        });
    ensure!(
        matches,
        "refused {:#?}",
        refused
            .iter()
            .map(|r| format!("{:?} {:?}: {}", r.by, r.kind, r.error))
            .collect::<Vec<_>>()
    );
    let d = c.deposit;
    ensure!(
        rt.rpc.get_tx_out(&d.txid, d.vout, Some(false))?.is_none(),
        "F spends the deposit"
    );
    Ok(())
}

/// A man in the middle swaps the hub's response keys in Base: the holder
/// answers it, but refuses the hub's F, which pays another graph under
/// another D, and the opening stalls before anyone signs F.
#[test]
fn a_holder_given_another_hubs_keys_never_funds() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let other = ResponseKeys::new(HubKeys::from_secret(&[9; 32]).sealed.public())?;
    let mut tamper = |kind: Kind, m: Vec<u8>| {
        if kind != Kind::Terms {
            return vec![m];
        }
        vec![base(&m, |p| p.response_keys = other.clone()).expect("a Base")]
    };
    let opened = open_native_with(
        &rt,
        "native-swapped",
        (60, 6),
        holder()?,
        Authority::mock(),
        &mut tamper,
    );
    let error = match opened {
        Ok(_) => bail!("opened on another hub's keys"),
        Err(e) => format!("{e:#}"),
    };
    ensure!(
        error.contains("stalled") && error.contains("by: Holder, kind: Some(Commit)"),
        "{error}"
    );
    Ok(())
}

/// A change to a quote.
type Change = fn(&mut Quote);

/// A holder opens on no quote under her network's floors: too little time
/// to answer a challenge, too short a delay, too shallow a depth.
#[test]
fn a_quote_under_her_floors_is_refused() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let checkpoint = checkpoint(&rt)?;
    let ctx = Context {
        tip: rt.height()?,
        trust: Authority::mock().trust(),
    };
    let keyset = holder()?.keyset;
    let start = |f: Change| {
        let mut quote = quote(keyset.clone(), checkpoint.clone());
        f(&mut quote);
        Start {
            seed: SEED,
            spend: Scalar::from(SPEND),
            quote,
            deposit: OutPoint::null(),
            wanted: Wanted {
                assert_height: 200,
                close_height: 400,
                response_window: 6,
            },
            checkpoint: checkpoint.clone(),
        }
    };
    let under: [(Change, &str); 5] = [
        (
            |q| q.graph.holder_answer = 5,
            "5 blocks to answer, not 6 at least",
        ),
        (
            |q| q.graph.to_self_delay = 5,
            "a delay of 5 blocks, not 6 at least",
        ),
        (
            |q| q.chain.funding_depth = 0,
            "F 0 blocks deep, not 1 at least",
        ),
        (|q| q.chain.request_depth = 0, "a request 0 blocks deep"),
        (
            |q| q.chain.publication_depth = 0,
            "a publication 0 blocks deep",
        ),
    ];
    for (f, why) in under {
        match machine::Holder::new(start(f), &ctx) {
            Ok(_) => bail!("opened on a quote under the floors: {why}"),
            Err(e) => ensure!(format!("{e:#}").contains(why), "{e:#}"),
        }
    }
    ensure!(
        machine::Holder::new(start(|_| ()), &ctx).is_ok(),
        "the floors themselves"
    );
    Ok(())
}

/// Refused, with a reason saying `why`.
fn refused<T>(result: Result<T>, why: &str) -> Result<()> {
    match result {
        Ok(_) => bail!("not refused: {why}"),
        Err(e) => {
            let e = format!("{e:#}");
            ensure!(e.contains(why), "refused otherwise than {why:?}: {e}");
            Ok(())
        }
    }
}

/// Her stored opening loads again only as hers: of this format,
/// derivation and side, whole, under her seed, her spending secret and a
/// trust of its kind and programs.
#[test]
fn a_stored_opening_loads_only_as_hers() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let checkpoint = checkpoint(&rt)?;
    let ctx = Context {
        tip: rt.height()?,
        trust: Authority::mock().trust(),
    };
    let quote = quote(holder()?.keyset, checkpoint.clone());
    let h = rt.height()?;
    let start = Start {
        seed: SEED,
        spend: Scalar::from(SPEND),
        quote: quote.clone(),
        // Never funded: a coin of no transaction.
        deposit: OutPoint {
            txid: Txid::from_byte_array([3; 32]),
            vout: 0,
        },
        wanted: Wanted {
            assert_height: h + 60,
            close_height: h + 200,
            response_window: 6,
        },
        checkpoint,
    };
    let (mut alice, out) = machine::Holder::new(start, &ctx)?;
    let started = alice.to_bytes();
    let mut hub = machine::Hub::with_secret(&[7; 32], quote);
    let [Out::Send(open)] = &out[..] else {
        bail!("{out:?}")
    };
    let out = hub.step(Input::Message(open.clone()), &ctx)?;
    let [Out::Send(terms)] = &out[..] else {
        bail!("{out:?}")
    };
    alice.step(Input::Message(terms.clone()), &ctx)?;
    ensure!(alice.phase() == Phase::Termed, "{:?}", alice.phase());
    let termed = alice.to_bytes();
    let load = |bytes: &[u8], seed, spend, ctx: &Context| {
        machine::Holder::load(bytes, seed, Scalar::from(spend), ctx)
    };
    for bytes in [&started, &termed] {
        let loaded = load(bytes, SEED, SPEND, &ctx)?;
        ensure!(loaded.to_bytes() == *bytes, "stored otherwise once loaded");
    }
    let changed = |at: usize, byte: u8| {
        let mut b = termed.clone();
        b[at] = byte;
        b
    };
    let genuine = Context {
        trust: Trust {
            mock: false,
            ..ctx.trust
        },
        ..ctx
    };
    let distrusted = Context {
        trust: Trust {
            programs: Programs {
                holder: [1; 32],
                ..ctx.trust.programs
            },
            ..ctx.trust
        },
        ..ctx
    };
    let damaged: [(Vec<u8>, &str); 9] = [
        (changed(0, b'X'), "not an opening's state"),
        (changed(5, 2), "of format 2"),
        (changed(6, 2), "keys of derivation 2"),
        (changed(7, 1), "not a holder's opening"),
        (changed(8, 7), "phase 7"),
        // Termed's sections under another phase.
        (changed(8, 0), "bytes past an opening's state"),
        (changed(8, 2), "cut short"),
        (
            [termed.clone(), vec![0]].concat(),
            "bytes past an opening's state",
        ),
        (termed[..termed.len() - 1].to_vec(), "past the message"),
    ];
    for (bytes, why) in damaged {
        refused(load(&bytes, SEED, SPEND, &ctx), why)?;
    }
    let other = "an opening of another seed or spending secret";
    refused(load(&started, [9; 32], SPEND, &ctx), other)?;
    refused(load(&termed, [9; 32], SPEND, &ctx), other)?;
    refused(load(&termed, SEED, SPEND + 1, &ctx), other)?;
    refused(
        load(&termed, SEED, SPEND, &genuine),
        "an opening under mock proofs, not genuine",
    )?;
    refused(
        load(&termed, SEED, SPEND, &distrusted),
        "programs she does not trust",
    )?;
    Ok(())
}

/// Her gate runs again on the hub's signatures: blocks past it while they
/// are on their way, she signs no F, and the opening stalls.
#[test]
fn past_her_deadline_she_signs_no_f() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let mut tamper = |kind: Kind, m: Vec<u8>| {
        if kind == Kind::Signed {
            rt.mine(60).expect("blocks past her deadline");
        }
        vec![m]
    };
    let played = play(
        &rt,
        "native-late",
        (60, 6),
        holder()?,
        Authority::mock(),
        &mut tamper,
    );
    let error = match played {
        Ok(_) => bail!("F signed past her deadline"),
        Err(e) => format!("{e:#}"),
    };
    ensure!(
        error.contains("stalled")
            && error.contains("by: Holder, kind: Some(Signed)")
            && error.contains("before its deadline"),
        "{error}"
    );
    Ok(())
}

/// Her driver keeps her instances: each reads back into her setups only
/// as she kept it, not lost, not another's, not changed.
#[test]
fn her_kept_instances_read_back_only_as_she_kept_them() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let played = play(
        &rt,
        "native-kept",
        (60, 6),
        holder()?,
        Authority::mock(),
        &mut |_, m| vec![m],
    )?;
    let o = &played.opening;
    let read = |_: usize, _: usize, c: &[u8; 32]| {
        played.kept.get(c).cloned().context("a kept instance lost")
    };
    ensure!(
        o.instances.iter().all(|k| k.len() == 2),
        "{:?}",
        o.instances
    );
    let [a, b] = [o.instances[0][0], o.instances[1][1]];
    o.setups(read)?;
    refused(
        o.setups(|s, i, c| match (s, i) {
            (1, 3) => bail!("a kept instance lost"),
            _ => read(s, i, c),
        }),
        "a kept instance lost",
    )?;
    // Another of hers in its place: of her other assertion, or this one.
    refused(
        o.setups(|s, i, c| match (s, i) {
            (0, 3) => read(0, a.index, &a.commitment),
            _ => read(s, i, c),
        }),
        "instance 3 of assertion 0 not the one she kept",
    )?;
    refused(
        o.setups(|s, i, c| match (s, i) {
            (0, 3) => read(1, b.index, &b.commitment),
            _ => read(s, i, c),
        }),
        "instance 3 of assertion 0 not the one she kept",
    )?;
    refused(
        o.setups(|s, i, c| {
            let mut bytes = read(s, i, c)?;
            if (s, i) == (1, 2) {
                let n = bytes.len() / 2;
                bytes[n] ^= 1;
            }
            Ok(bytes)
        }),
        "instance",
    )?;
    Ok(())
}
