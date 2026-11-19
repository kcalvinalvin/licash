//! `licash-wallet cold`: cold storage, an external wallet notes are
//! withdrawn to now and then by its extended public key, each time to an
//! address of its own, and what it holds, looked for in the chain's blocks.

use anyhow::Context;
use clap::Subcommand;
use licash_chain_client::Source;
use licash_nostr::Route;
use licash_transport::Http;
use licash_wallet::{Blocks as _, Cold, ColdRules, ColdScript, Wallet};
use serde_json::json;

use crate::{onchain, say, show};

/// Blocks looked in for cold storage's coins at a time.
const BATCH: u32 = 1_000;

#[derive(Subcommand)]
pub enum ColdCommand {
    /// Withdraw to the wallet of KEY, an extended public key of its
    /// account: a zpub (vpub off mainnet) for native segwit addresses, or
    /// an xpub (tpub) with --script. Once the notes exceed --threshold, all
    /// but --keep is paid out to its next address unused, through a swap
    /// provider charging at most --max-fee-percent of it, by `cold run`.
    Set {
        key: String,
        /// Its addresses' kind, for an xpub or tpub: wpkh (native segwit)
        /// or tr (taproot).
        #[arg(long, value_parser = ["wpkh", "tr"])]
        script: Option<String>,
        /// The notes held, in sat, past which a withdrawal is due.
        #[arg(long)]
        threshold: u64,
        /// The notes kept, in sat, when withdrawing.
        #[arg(long, default_value_t = 0)]
        keep: u64,
        /// The most a withdrawal may cost, in percent of it.
        #[arg(long, default_value_t = 1.0)]
        max_fee_percent: f64,
        /// The block to look for its coins from [default: the chain's
        /// height, with --chain].
        #[arg(long)]
        birthday: Option<u32>,
    },
    /// Cold storage as configured, and what it holds: with --chain, looked
    /// for first up to the chain's height.
    Show,
    /// Look for what cold storage holds, follow the payouts, and withdraw
    /// if due: through the swap provider announced on the hubs' relays
    /// asking least. Needs --chain.
    Run,
    /// Stop withdrawing to cold storage: what it holds stays there.
    Clear,
}

pub async fn run(
    wallet: &Wallet<Http>,
    chain: Option<String>,
    route: Route,
    command: ColdCommand,
    json: bool,
) -> anyhow::Result<()> {
    let source = |chain: &Option<String>| -> anyhow::Result<Source> {
        let chain = chain.as_deref().context(
            "--chain: Bitcoin Core as http://user:password@127.0.0.1:8332, \
             or Esplora as esplora+https://esplora.example/api",
        )?;
        chain.parse().context("--chain")
    };
    match command {
        ColdCommand::Set {
            key,
            script,
            threshold,
            keep,
            max_fee_percent,
            birthday,
        } => {
            let script = script.map(|s| s.parse::<ColdScript>()).transpose()?;
            let birthday = match birthday {
                Some(birthday) => birthday,
                None => {
                    let blocks = onchain::Blocks(source(&chain).context("--birthday or --chain")?);
                    blocks.height().await.map_err(anyhow::Error::msg)?
                }
            };
            let rules = ColdRules {
                threshold,
                keep,
                max_fee_percent,
            };
            let cold = wallet.set_cold(&key, script, rules, birthday)?;
            show(json, cold_json(&cold), cold_text(&cold));
        }
        ColdCommand::Show => {
            let cold = match &chain {
                Some(_) => scan(wallet, &source(&chain)?).await?,
                None => wallet.cold()?,
            };
            let cold = cold.context("no cold storage: `cold set` configures it")?;
            show(json, cold_json(&cold), cold_text(&cold));
        }
        ColdCommand::Run => {
            let source = source(&chain)?;
            crate::payout::follow_all(wallet, &source).await?;
            let cold = scan(wallet, &source).await?;
            cold.context("no cold storage: `cold set` configures it")?;
            let mut text = Vec::new();
            let mut withdrawn = None;
            if let Some(due) = wallet.cold_due()? {
                let rate = onchain::rate(&source, None).await?;
                let route_to = (due.from.as_str(), due.to.as_str());
                let limits = (due.max_fee, rate);
                match crate::payout::announced(wallet, &route, route_to, due.amount, limits).await {
                    Ok(payout) => {
                        text.push(format!(
                            "withdrawing {} from {} to {} (payout {})",
                            say::sat(due.amount),
                            due.from,
                            due.to,
                            payout.hash
                        ));
                        withdrawn = Some(payout.hash);
                        crate::payout::follow_all(wallet, &source).await?;
                    }
                    Err(e) => text.push(format!("a withdrawal is due but not made: {e:#}")),
                }
            }
            let cold = wallet.cold()?.context("cold storage cleared meanwhile")?;
            text.push(cold_text(&cold));
            let mut shown = cold_json(&cold);
            shown["withdrawn"] = json!(withdrawn);
            show(json, shown, text.join("\n"));
        }
        ColdCommand::Clear => wallet.clear_cold()?,
    }
    Ok(())
}

/// Look for cold storage's coins up to the chain's height.
async fn scan(wallet: &Wallet<Http>, source: &Source) -> anyhow::Result<Option<Cold>> {
    let blocks = onchain::Blocks(source.clone());
    let tip = blocks.height().await.map_err(anyhow::Error::msg)?;
    loop {
        let cold = wallet.scan_cold(&blocks, BATCH).await?;
        match &cold {
            Some(c) if c.scanned.is_some_and(|s| s < tip) => continue,
            _ => return Ok(cold),
        }
    }
}

fn cold_json(cold: &Cold) -> serde_json::Value {
    json!({
        "key": cold.key, "script": cold.script.as_str(),
        "threshold": cold.rules.threshold, "keep": cold.rules.keep,
        "max_fee_percent": cold.rules.max_fee_percent,
        "birthday": cold.birthday, "scanned": cold.scanned,
        "balance": cold.balance, "coins": cold.coins,
        "next": cold.next, "next_index": cold.next_index, "pending": cold.pending,
    })
}

fn cold_text(cold: &Cold) -> String {
    let scanned = match cold.scanned {
        Some(height) => format!("to block {height}"),
        None => "not looked for yet: `cold show --chain`".into(),
    };
    let mut lines = vec![
        format!(
            "cold storage  {} ({} {})",
            say::sat(cold.balance),
            cold.coins,
            match cold.coins {
                1 => "coin",
                _ => "coins",
            }
        ),
        format!("looked for  from block {} {scanned}", cold.birthday),
        format!(
            "withdraws  past {}, keeping {}, at most {}% in fees",
            say::sat(cold.rules.threshold),
            say::sat(cold.rules.keep),
            cold.rules.max_fee_percent
        ),
        format!("next  {} (address {})", cold.next, cold.next_index),
    ];
    if let Some(hash) = &cold.pending {
        lines.push(format!("under way  payout {hash}"));
    }
    lines.push(
        "to bring funds back, pay an address of `licash-wallet code --onchain` from the cold wallet"
            .into(),
    );
    lines.join("\n")
}
