//! `licash-wallet open-contract` and `contracts`: recovery contracts at
//! the wallet's hubs, kept beside its file, each paid from a coin of the
//! wallet's on Bitcoin. `approve-fees`: more than a hub charged as they
//! opened, agreed to. `force-close`, `recoveries` and
//! `give-up-recovery`, `allow-publication-fee`: one its hub left silent, closed by the wallet.
//! `recovery-statement` and `import-proof`: one its hub answered, proved
//! on another machine.

use std::path::Path;

use anyhow::Context;
use licash_contracts::holder::recovery::{self, Recovery};
use licash_contracts::holder::{self, Ask, Contract, Node};
use licash_transport::Http;
use licash_wallet::{Blocks as _, Wallet};
use serde_json::json;

use crate::show;

fn node(chain: Option<&str>) -> anyhow::Result<Node> {
    let chain = chain.context(
        "--chain: Bitcoin Core as http://user:password@127.0.0.1:8332, \
         or Esplora as esplora+https://esplora.example/api",
    )?;
    Ok(Node(chain.parse().context("--chain")?))
}

/// The directory of the wallet's file, where its contracts are kept.
fn dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// Hold each hub to the fees the wallet's contracts there opened under,
/// or its owner approved since. A wallet whose contracts cannot be read
/// holds no hub to any, as one with none.
pub fn agree(wallet: &Wallet<Http>, path: &Path) {
    if let Err(e) = holder::fees::agree(wallet, dir(path), None) {
        tracing::warn!("what the contracts' hubs may charge: {e:#}");
    }
}

/// What `hub` charges now beside what the wallet agreed to there, asked
/// again; with `yes`, agreed to as the wallet last heard it, without
/// asking: what was shown.
pub async fn approve_fees(
    wallet: &Wallet<Http>,
    path: &Path,
    hub: &str,
    yes: bool,
    json: bool,
) -> anyhow::Result<()> {
    let hub = hub.trim_end_matches('/');
    let agreed = wallet.fee_ceilings(hub)?;
    let charged = match yes {
        true => {
            let hubs = wallet.hubs()?;
            let known = hubs.iter().find(|(url, _)| url == hub);
            let (_, info) = known.with_context(|| format!("{hub} is no hub this wallet trusts"))?;
            info.fees
        }
        false => wallet.add_hub(hub).await?.fees,
    };
    let raised = agreed.is_some_and(|agreed| charged.above(&agreed));
    if yes && raised {
        holder::fees::approve(wallet, dir(path), hub, &charged, None)?;
    }
    let now = wallet.fee_ceilings(hub)?;
    let said = |fees: &licash_api::Fees| crate::say::fees(fees);
    let line = match (agreed, raised, yes) {
        (None, ..) => format!(
            "nothing to approve: no contract holds {hub} to its fees\ncharges  {}",
            said(&charged)
        ),
        (Some(agreed), false, _) => format!(
            "nothing to approve: {hub} charges no more than agreed\ncharges  {}\nagreed   {}",
            said(&charged),
            said(&agreed)
        ),
        (Some(agreed), true, false) => format!(
            "{hub} charges more than agreed: `approve-fees {hub} --yes` agrees to it\ncharges  {}\nagreed   {}",
            said(&charged),
            said(&agreed)
        ),
        (Some(agreed), true, true) => format!(
            "approved\ncharges  {}\nagreed   {}\nbefore   {}",
            said(&charged),
            now.as_ref().map_or_else(|| "nothing".into(), said),
            said(&agreed)
        ),
    };
    show(
        json,
        json!({
            "hub": hub, "charged": charged, "agreed_before": agreed, "agreed": now,
            "raised": raised, "approved": yes && raised,
        }),
        line,
    );
    Ok(())
}

/// What `open-contract` asks of a hub, as given.
pub struct Asked {
    pub blocks: Option<u32>,
    pub max_price: Option<u64>,
    pub cap: Option<u32>,
    pub coin: Option<bitcoin::OutPoint>,
    /// At a hub charging more than is sane too.
    pub any_fees: bool,
}

/// Open a contract at `hub` closing `blocks` from now [default: as late
/// as the hub allows], paying at most `max_price` [default: what its info
/// said, or its range prices the terms chosen at], protecting `cap` sat
/// if chosen [default: the hub's fixed terms], paid from `coin` [default:
/// the smallest free that pays it].
pub async fn open(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    hub: &str,
    asked: Asked,
    json: bool,
) -> anyhow::Result<()> {
    let chain = node(chain)?;
    let hub = hub.trim_end_matches('/');
    let hubs = wallet.hubs()?;
    let (_, info) = hubs
        .iter()
        .find(|(url, _)| url == hub)
        .with_context(|| format!("{hub} is no hub this wallet trusts: add-hub first"))?;
    let offered = info
        .contracts
        .with_context(|| format!("{hub} opens no contracts"))?;
    let blocks = asked.blocks.unwrap_or(offered.blocks);
    let price = match (asked.cap, offered.range) {
        (Some(cap), Some(range)) => range.price(cap, blocks),
        (Some(_), None) => anyhow::bail!("{hub} offers no contracts of other caps"),
        (None, _) => offered.price,
    };
    let ask = Ask {
        blocks,
        max_price: asked.max_price.unwrap_or(price),
        cap: asked.cap,
        deposit: asked.coin,
    };
    // Her coin turned whole into the contract and the notes it protects:
    // never change left on Bitcoin.
    let (dir, change) = (dir(path), holder::Change::Notes);
    let contract = match asked.any_fees {
        true => holder::open_anyway(wallet, &chain, dir, hub, ask, change).await,
        false => holder::open_with(wallet, &chain, dir, hub, ask, change).await,
    }?;
    show(json, contract_json(&contract), contract_line(&contract));
    Ok(())
}

/// The wallet's contracts, those under way carried on and their F watched.
pub async fn list(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let (contracts, claims) = match chain {
        Some(_) => {
            let node = node(chain)?;
            let contracts = holder::resume(wallet, &node, dir(path)).await?;
            let tip = node.height().await.map_err(|e| anyhow::anyhow!(e))?;
            let each = holder::coverage(wallet, dir(path), &contracts, tip)?;
            let claims = |c: &Contract| c.live(tip).then(|| each.get(&c.id).copied().unwrap_or(0));
            let claims: Vec<_> = contracts.iter().map(claims).collect();
            (contracts, claims)
        }
        None => {
            let contracts = holder::list(dir(path))?;
            let none = vec![None; contracts.len()];
            (contracts, none)
        }
    };
    let lines: Vec<String> = (contracts.iter().zip(&claims))
        .map(|(c, claims)| match claims {
            Some(sat) => format!("{}  claims {} now", contract_line(c), crate::say::sat(*sat)),
            None => contract_line(c),
        })
        .collect();
    let values: serde_json::Value = (contracts.iter().zip(&claims))
        .map(|(c, claims)| {
            let mut value = contract_json(c);
            value["claims"] = json!(claims);
            value
        })
        .collect();
    show(json, values, lines.join("\n"));
    Ok(())
}

fn state(c: &Contract) -> String {
    if let Some(fold) = c.folded {
        return format!("closed early ({fold})");
    }
    match (&c.abandoned, c.fate) {
        (Some(why), _) => format!("given up ({why})"),
        (None, Some(fate)) => format!("{fate:?}").to_lowercase(),
        (None, None) => format!("opening ({:?})", c.phase).to_lowercase(),
    }
}

fn contract_line(c: &Contract) -> String {
    let mock = if c.mock {
        "  MOCK, protects nothing"
    } else {
        ""
    };
    format!(
        "{}  {}  {}  covers {}  until block {}  closes at block {}  deposit {}  price {}{share}{mock}",
        &licash_contracts::hex_id(&c.id)[..16],
        c.hub,
        state(c),
        crate::say::sat(c.cap.into()),
        c.assert_height,
        c.close_height,
        crate::say::sat(c.value),
        crate::say::sat(c.price),
        share = crate::say::share(c.price, c.cap.into())
            .map(|s| format!(" ({s} of what it covers)"))
            .unwrap_or_default(),
    )
}

fn contract_json(c: &Contract) -> serde_json::Value {
    json!({
        "id": licash_contracts::hex_id(&c.id),
        "hub": c.hub,
        "state": state(c),
        "phase": format!("{:?}", c.phase).to_lowercase(),
        "fate": c.fate.map(|f| format!("{f:?}").to_lowercase()),
        "mock": c.mock,
        "cap": c.cap,
        "assert_height": c.assert_height,
        "close_height": c.close_height,
        "deposit": c.deposit.to_string(),
        "value": c.value,
        "price": c.price,
        "funding": c.funding.map(|t| t.to_string()),
        "abandoned": c.abandoned,
        "folded": c.folded.map(|t| t.to_string()),
        "created_at": c.created_at,
    })
}

/// The contract of the wallet's whose id is or starts with `id`, in hex.
fn contract_id(path: &Path, id: &str) -> anyhow::Result<[u8; 32]> {
    let id = id.to_lowercase();
    let mut found = holder::list(dir(path))?
        .into_iter()
        .map(|c| c.id)
        .filter(|c| licash_contracts::hex_id(c).starts_with(&id));
    let one = found.next().with_context(|| format!("no contract {id}"))?;
    anyhow::ensure!(found.next().is_none(), "more than one contract starts {id}");
    Ok(one)
}

/// Force contract `id` closed, its hub silent: the wallet's notes at the
/// hub claimed on Bitcoin, paid back on Bitcoin once the hub's window
/// passed, its fees at `fee_rate` sat/vB [default: the chain's estimate].
pub async fn force_close(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    id: &str,
    fee_rate: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let chain = node(chain)?;
    let id = contract_id(path, id)?;
    let rate = crate::onchain::rate(&chain.0, fee_rate).await?;
    let forcing = recovery::recoveries(dir(path))?.iter().all(|r| r.id != id);
    if forcing {
        if let Some(need) = recovery::challenge_shortfall(wallet, dir(path), rate)? {
            eprintln!(
                "warning: no coin of {} sat on Bitcoin besides the others to set aside for a \
                 challenge's fee: should the hub prove falsely, a challenge pays what the \
                 coins hold, and may come too late",
                need.to_sat()
            );
        }
    }
    let forced = recovery::force_close(wallet, &chain, dir(path), &id, rate).await?;
    show(json, recovery_json(&forced), recovery_line(&forced));
    Ok(())
}

/// Close contract `id` early with its hub, cooperatively: the wallet's
/// reserve back on Bitcoin at once, the protection over.
pub async fn close(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let chain = node(chain)?;
    let id = contract_id(path, id)?;
    let txid = licash_contracts::holder::close(wallet, &chain, dir(path), &id).await?;
    let hex = licash_contracts::hex_id(&id);
    show(
        json,
        json!({"id": hex, "fold": txid.to_string()}),
        format!("{}  closed early by {txid}", &hex[..16]),
    );
    Ok(())
}

/// The contracts the wallet forced closed: with --chain, each carried on
/// as far as the chain lets it.
pub async fn recoveries(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    fee_rate: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let all = match chain {
        Some(_) => {
            let chain = node(chain)?;
            let rate = crate::onchain::rate(&chain.0, fee_rate).await?;
            recovery::advance(wallet, &chain, dir(path), rate).await?
        }
        None => recovery::recoveries(dir(path))?,
    };
    let lines: Vec<String> = all.iter().map(recovery_line).collect();
    show(
        json,
        all.iter().map(recovery_json).collect(),
        lines.join("\n"),
    );
    Ok(())
}

/// Give up forcing contract `id` closed, the wallet's notes back to it:
/// only before its request is signed.
pub fn give_up(wallet: &Wallet<Http>, path: &Path, id: &str) -> anyhow::Result<()> {
    let id = contract_id(path, id)?;
    recovery::give_up(wallet, dir(path), &id)
}

pub fn allow_publication_fee(path: &Path, id: &str, sats: u64) -> anyhow::Result<()> {
    let id = contract_id(path, id)?;
    recovery::allow_publication_fee(dir(path), &id, sats)
}

fn recovery_line(r: &Recovery) -> String {
    let mut line = format!(
        "{}  {}  claims {} sat in {} notes",
        &licash_contracts::hex_id(&r.id)[..16],
        r.stage.as_str(),
        r.claimed,
        r.notes,
    );
    if let Some(at) = r.next_at {
        line.push_str(&format!("  next step at block {at}"));
    }
    if r.stage == recovery::Stage::Answered {
        let by = r
            .deadline
            .map_or_else(String::new, |d| format!(" by block {d}"));
        let owed = r
            .corrected
            .map_or_else(String::new, |c| format!(" {c} sat owed,"));
        line.push_str(&format!("  the hub says{owed} to prove{by}"));
    }
    if let Some(error) = &r.error {
        line.push_str(&format!("  ({error})"));
    }
    line
}

fn recovery_json(r: &Recovery) -> serde_json::Value {
    let txid = |t: Option<bitcoin::Txid>| t.map(|t| t.to_string());
    json!({
        "id": licash_contracts::hex_id(&r.id),
        "stage": r.stage.as_str(),
        "irrevocable": r.irrevocable,
        "claimed": r.claimed,
        "notes": r.notes,
        "request": txid(r.request),
        "publication": txid(r.publication),
        "requested_at": r.requested_at,
        "timeout": txid(r.timeout),
        "timed_out_at": r.timed_out_at,
        "sweep": txid(r.sweep),
        "swept_at": r.swept_at,
        "next_at": r.next_at,
        "error": r.error,
        "response": txid(r.response),
        "deadline": r.deadline,
        "proved": r.proved,
        "assertion": txid(r.assertion),
        "asserted_at": r.asserted_at,
        "challenge": txid(r.challenge),
        "payout": txid(r.payout),
        "paid_at": r.paid_at,
    })
}
