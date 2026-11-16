//! `licash-wallet open-contract` and `contracts`: recovery contracts at
//! the wallet's hubs, kept beside its file, each paid from a coin of the
//! wallet's on Bitcoin. `force-close`, `recoveries` and
//! `give-up-recovery`: one its hub left silent, closed by the wallet.
//! `recovery-statement` and `import-proof`: one its hub answered, proved
//! on another machine.

use std::path::Path;

use anyhow::Context;
use licash_contracts::holder::recovery::{self, Recovery};
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
    let forced = recovery::force_close(wallet, &chain, dir(path), &id, rate).await?;
    show(json, recovery_json(&forced), recovery_line(&forced));
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

/// The statement to prove of contract `id`, written to `out` [default:
/// `<id>.statement.input` here, its id's first 16 digits], and how to
/// prove it elsewhere and bring the proof back.
pub fn statement(path: &Path, id: &str, out: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let id = contract_id(path, id)?;
    let bytes = recovery::statement(dir(path), &id)?;
    let short = &licash_contracts::hex_id(&id)[..16];
    let out = out.map_or_else(
        || std::path::PathBuf::from(format!("{short}.statement.input")),
        Path::to_path_buf,
    );
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    let mut file = file
        .open(&out)
        .with_context(|| format!("{}", out.display()))?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    let deadline = recovery::recoveries(dir(path))?
        .into_iter()
        .find(|r| r.id == id)
        .and_then(|r| r.deadline);
    let by = deadline.map_or_else(String::new, |d| format!(" before block {d}"));
    let at = out.display();
    let text = format!(
        "wrote {at} ({} bytes), the statement the wallet's proof is of. It shows the wallet's \
         notes: keep it private.\n\
         Prove it on a machine with RISC Zero's prover (16 GB of memory; x86 with Docker for \
         the second step), each step carried on where it stopped if run again:\n  \
         licash-r0 native-prove-succinct {at}\n  \
         licash-r0 native-wrap {at}.proof/receipt proof.json\n\
         Then bring the proof back{by}:\n  \
         licash-wallet import-proof {short} proof.json --chain CHAIN",
        bytes.len(),
    );
    let value = json!({
        "id": licash_contracts::hex_id(&id),
        "statement": at.to_string(),
        "bytes": bytes.len(),
        "deadline": deadline,
    });
    show(json, value, text);
    Ok(())
}

/// Bring back the proof of contract `id`'s claim from `file`: checked,
/// kept, and asserted on Bitcoin at once, its fees at `fee_rate` sat/vB
/// [default: the chain's estimate].
pub async fn import_proof(
    wallet: &Wallet<Http>,
    path: &Path,
    chain: Option<&str>,
    id: &str,
    file: &Path,
    fee_rate: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let chain = node(chain)?;
    let id = contract_id(path, id)?;
    let export = std::fs::read(file).with_context(|| format!("{}", file.display()))?;
    let rate = crate::onchain::rate(&chain.0, fee_rate).await?;
    let r = recovery::import_proof(wallet, &chain, dir(path), &id, &export, rate).await?;
    show(json, recovery_json(&r), recovery_line(&r));
    Ok(())
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
    if r.stage == recovery::Stage::Proving {
        let by = r
            .deadline
            .map_or_else(String::new, |d| format!(" by block {d}"));
        line.push_str(&format!("  its proof needed{by}: see recovery-statement"));
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
