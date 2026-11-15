//! `licash-wallet open-contract` and `contracts`: recovery contracts at
//! the wallet's hubs, kept beside its file, each paid from a coin of the
//! wallet's on Bitcoin.

use std::path::Path;

use anyhow::Context;
use licash_contracts::holder::{self, Ask, Contract, Node};
use licash_transport::Http;
use licash_wallet::Wallet;
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

/// What `open-contract` asks of a hub, as given.
pub struct Asked {
    pub blocks: Option<u32>,
    pub max_price: Option<u64>,
    pub cap: Option<u32>,
    pub coin: Option<bitcoin::OutPoint>,
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
    let contract = holder::open(wallet, &chain, dir(path), hub, ask).await?;
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
    let contracts = match chain {
        Some(_) => holder::resume(wallet, &node(chain)?, dir(path)).await?,
        None => holder::list(dir(path))?,
    };
    let lines: Vec<String> = contracts.iter().map(contract_line).collect();
    show(
        json,
        contracts.iter().map(contract_json).collect(),
        lines.join("\n"),
    );
    Ok(())
}

fn state(c: &Contract) -> String {
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
        "{}  {}  {}  cap {}  until block {}  closes at {}  deposit {} price {}{mock}",
        &licash_contracts::hex_id(&c.id)[..16],
        c.hub,
        state(c),
        c.cap,
        c.assert_height,
        c.close_height,
        c.value,
        c.price,
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
        "created_at": c.created_at,
    })
}
