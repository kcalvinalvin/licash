//! `licash-wallet`: pay and get paid through Licash hubs.

mod http;

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use licash::{address::Address, spend::lock_hash};
use licash_api::Payment;
use licash_nostr::Role;
use licash_wallet::{Error, Invoice, InvoiceState, Lock, LockState, State, Wallet};
use rand_core::{OsRng, RngCore};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use http::Http;

/// The exit status of a payment recorded but not finished: retry later.
const PENDING: u8 = 75;

#[derive(Parser)]
#[command(
    name = "licash-wallet",
    version,
    about = "Pay and get paid through Licash hubs"
)]
struct Cli {
    /// The wallet file [default: $XDG_DATA_HOME/licash/wallet.sqlite].
    #[arg(long, global = true, env = "LICASH_WALLET")]
    wallet: Option<PathBuf>,
    /// Reach hubs through this proxy, such as socks5h://127.0.0.1:9050 for
    /// Tor's.
    #[arg(long, global = true, env = "LICASH_WALLET_PROXY")]
    proxy: Option<String>,
    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Log lines on stderr as text or JSON, filtered by RUST_LOG (default
    /// warn).
    #[arg(long, global = true, env = "LICASH_WALLET_LOG", value_enum, default_value_t = Log::Text)]
    log: Log,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Log {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Create a wallet, with a new seed. Back up its file: the seed alone
    /// does not restore its notes.
    Init,
    /// Where others pay this wallet.
    Address,
    /// Trust the hub at URL, or learn again what it charges.
    AddHub { url: String },
    /// The hubs trusted, and their fees.
    Hubs,
    /// What the wallet holds, by hub.
    Balance,
    /// Pay AMOUNT to ADDRESS, the hub's fee on top, and print the payment
    /// to hand the payee (its id on stderr).
    Send {
        address: String,
        amount: u64,
        /// Through this hub [default: the one holding the most].
        #[arg(long)]
        hub: Option<String>,
        /// Write the payment here instead of to stdout.
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Take a payment from a file, or from stdin, and unlink its notes.
    Receive {
        #[arg(default_value = "-")]
        payment: PathBuf,
        /// Trust the hub the payer names if it is not trusted yet.
        #[arg(long)]
        trust: bool,
    },
    /// Finish the payments under way and unlink the notes still linked.
    Resume,
    /// The payments made, newest first.
    History,
    /// The payments taken, newest first.
    Received,
    /// Drop the notes a copy of this wallet spent, as after restoring an
    /// old backup of its file. Each hub learns which notes are this
    /// wallet's.
    Check,
    /// A payment made, to hand its payee again.
    Payment {
        id: String,
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Give up a payment its hub never signed for, freeing its notes.
    Abandon { id: String },
    /// A new secret to lock payments to: its preimage, to keep, and its
    /// hash, to hand the payer.
    Secret,
    /// Lock AMOUNT to ADDRESS, paid on the preimage of HASH until the
    /// lock expires, else refunded; the hub's fee on top. Prints the lock,
    /// whose id the payee claims it with.
    Lock {
        address: String,
        amount: u64,
        hash: String,
        /// Through this hub [default: the one holding the most].
        #[arg(long)]
        hub: Option<String>,
        /// The height the lock expires at [default: the latest the hub
        /// admits].
        #[arg(long)]
        expiry: Option<u32>,
    },
    /// The locks made, newest first.
    Locks,
    /// Take what lock ID pays this wallet, revealing PREIMAGE.
    Claim {
        id: String,
        preimage: String,
        /// The hub holding the lock [default: the only one trusted].
        #[arg(long)]
        hub: Option<String>,
    },
    /// Unlink the notes held from the payments they came in.
    Unlink,
    /// Pay a Lightning invoice through a hub, its fee on top: the hub pays
    /// it for a lock of notes, refunded if the payment fails.
    PayInvoice {
        invoice: String,
        /// Through this hub [default: the one holding the most].
        #[arg(long)]
        hub: Option<String>,
        /// Seconds to wait for the payment to end; `resume` follows it
        /// after.
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// A Lightning invoice paying this wallet AMOUNT through a hub, its fee
    /// on top, to hand the payer (its hash on stderr). `resume` takes the
    /// payment once made.
    Invoice {
        amount: u64,
        /// Through this hub [default: the only one trusted].
        #[arg(long)]
        hub: Option<String>,
    },
    /// The Lightning invoices asked for, newest first.
    Invoices,
    /// Hubs announced on Nostr relays, at the URLs that answer as the hub
    /// that announced them. Needs no wallet.
    Discover {
        /// A relay to ask, such as `wss://relay.example`. Repeat it, or
        /// separate relays with commas, for more.
        #[arg(
            long = "relay",
            value_name = "URL",
            env = "LICASH_WALLET_RELAYS",
            value_delimiter = ',',
            required = true
        )]
        relays: Vec<String>,
        /// The Bitcoin network of the hubs: bitcoin, testnet4, signet or
        /// regtest.
        #[arg(long, default_value = "bitcoin")]
        network: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging(cli.log);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let pending = matches!(e.downcast_ref(), Some(Error::Pending { .. }));
            eprintln!("error: {e:#}");
            if pending {
                ExitCode::from(PENDING)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn logging(format: Log) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    match format {
        Log::Text => logs.init(),
        Log::Json => logs.json().init(),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let path = match cli.wallet {
        Some(path) => path,
        None => default_path()?,
    };
    let transport = Http::new(cli.proxy.as_deref())?;
    let json = cli.json;
    match cli.command {
        Command::Init => return init(&path, transport, json),
        Command::Discover { relays, network } => {
            return discover(transport, cli.proxy.as_deref(), &relays, &network, json).await
        }
        _ => {}
    }
    if !path.exists() {
        bail!(
            "no wallet at {}: `licash-wallet init` creates one",
            path.display()
        );
    }
    let wallet = Wallet::open(&path, transport)?;
    match cli.command {
        Command::Init | Command::Discover { .. } => unreachable!("handled above"),
        Command::Address => {
            let address = encode(&wallet.address());
            show(json, json!({ "address": address }), address.clone());
        }
        Command::AddHub { url } => {
            let info = wallet.add_hub(&url).await?;
            let fee = info.fees.spend;
            show(
                json,
                json!({
                    "fees": info.fees, "keysets": info.keysets.len(), "locks": info.locks.is_some(),
                    "lightning": info.lightning,
                }),
                format!(
                    "trusted; it charges {} + {} ppm a payment",
                    fee.base, fee.ppm
                ),
            );
        }
        Command::Hubs => {
            let hubs = wallet.hubs()?;
            let rows = hubs.iter().map(|(url, info)| {
                json!({
                    "url": url, "fees": info.fees, "locks": info.locks.is_some(),
                    "lightning": info.lightning,
                })
            });
            let lines = hubs.iter().map(|(url, info)| {
                let fee = info.fees.spend;
                let lightning = if info.lightning.is_some() {
                    "  lightning"
                } else {
                    ""
                };
                format!("{url}  {} + {} ppm{lightning}", fee.base, fee.ppm)
            });
            show(json, rows.collect(), lines.collect::<Vec<_>>().join("\n"));
        }
        Command::Balance => {
            let balance = wallet.balance()?;
            let rows = balance
                .iter()
                .map(|b| json!({ "hub": b.hub, "available": b.available, "pending": b.pending }));
            let total: u64 = balance.iter().map(|b| b.available).sum();
            let mut lines: Vec<_> = balance
                .iter()
                .map(|b| match b.pending {
                    0 => format!("{}  {}", b.hub, b.available),
                    p => format!("{}  {} ({p} pending)", b.hub, b.available),
                })
                .collect();
            lines.push(format!("total  {total}"));
            show(json, rows.collect(), lines.join("\n"));
        }
        Command::Send {
            address,
            amount,
            hub,
            out,
        } => {
            let payee = decode(&address)?;
            let hub = match hub {
                Some(hub) => hub,
                None => richest(&wallet)?,
            };
            let paid = wallet.send(&hub, &payee, amount).await?;
            eprintln!("payment {}", paid.id);
            write_payment(&paid.payment, out.as_deref())?;
        }
        Command::Receive { payment, trust } => {
            let text = read(&payment)?;
            let payment: Payment = serde_json::from_str(&text).context("not a payment")?;
            let received = match wallet.receive(&payment) {
                Err(Error::UnknownHub(hub)) if trust => {
                    wallet.add_hub(&hub).await?;
                    wallet.receive(&payment)?
                }
                Err(e @ Error::UnknownHub(_)) => bail!("{e}, or receive with --trust"),
                received => received?,
            };
            let unlinked = unlink(&wallet).await;
            let amount = received.amount;
            show(
                json,
                json!({ "id": received.id, "hub": received.hub, "amount": amount, "unlinked": unlinked }),
                format!("received {amount}"),
            );
        }
        Command::Resume => resume(&wallet, json).await?,
        Command::History => {
            let sends = wallet.sends()?;
            let rows = sends.iter().map(|s| {
                json!({
                    "id": s.id, "hub": s.hub, "payee": encode(&s.payee), "amount": s.amount,
                    "fee": s.fee, "state": state(&s.state), "created_at": s.created_at,
                })
            });
            let lines = sends.iter().map(|s| {
                format!(
                    "{}  {}  {} + {}  via {}  {}",
                    s.created_at,
                    state(&s.state),
                    s.amount,
                    s.fee,
                    s.hub,
                    s.id
                )
            });
            show(json, rows.collect(), lines.collect::<Vec<_>>().join("\n"));
        }
        Command::Received => {
            let receipts = wallet.receipts()?;
            let rows = receipts.iter().map(|r| {
                json!({ "id": r.id, "hub": r.hub, "amount": r.amount, "received_at": r.received_at })
            });
            let lines = receipts
                .iter()
                .map(|r| format!("{}  {}  via {}  {}", r.received_at, r.amount, r.hub, r.id));
            show(json, rows.collect(), lines.collect::<Vec<_>>().join("\n"));
        }
        Command::Check => {
            let dropped = wallet.check().await?;
            let text = match dropped {
                0 => "no note was spent elsewhere".into(),
                d => format!("dropped {d} spent by a copy of this wallet"),
            };
            show(json, json!({ "dropped": dropped }), text);
        }
        Command::Payment { id, out } => write_payment(&wallet.payment(&id)?, out.as_deref())?,
        Command::Abandon { id } => {
            wallet.abandon(&id)?;
            show(
                json,
                json!({ "abandoned": id }),
                "abandoned; its notes are free".into(),
            );
        }
        Command::Secret => {
            let mut preimage = [0; 32];
            OsRng.fill_bytes(&mut preimage);
            let hash = hex::encode(lock_hash(&preimage));
            let preimage = hex::encode(preimage);
            show(
                json,
                json!({ "preimage": preimage, "hash": hash }),
                format!("preimage {preimage}\nhash {hash}"),
            );
        }
        Command::Lock {
            address,
            amount,
            hash,
            hub,
            expiry,
        } => {
            let payee = decode(&address)?;
            let hub = match hub {
                Some(hub) => hub,
                None => richest(&wallet)?,
            };
            let lock = wallet
                .lock(&hub, &payee, amount, bytes(&hash)?, expiry)
                .await?;
            show(json, lock_json(&lock), lock_line(&lock));
        }
        Command::Locks => {
            let locks = wallet.locks()?;
            let lines = locks.iter().map(lock_line).collect::<Vec<_>>();
            show(
                json,
                locks.iter().map(lock_json).collect(),
                lines.join("\n"),
            );
        }
        Command::Claim { id, preimage, hub } => {
            let hub = match hub {
                Some(hub) => hub,
                None => only_hub(&wallet)?,
            };
            let received = wallet.claim(&hub, &id, &bytes(&preimage)?).await?;
            let unlinked = unlink(&wallet).await;
            let amount = received.amount;
            show(
                json,
                json!({ "id": received.id, "hub": received.hub, "amount": amount, "unlinked": unlinked }),
                format!("received {amount}"),
            );
        }
        Command::Unlink => {
            let unlinked = wallet.unlink().await?;
            show(
                json,
                json!({ "unlinked": unlinked }),
                format!("unlinked {unlinked} notes"),
            );
        }
        Command::PayInvoice { invoice, hub, wait } => {
            let hub = match hub {
                Some(hub) => hub,
                None => richest(&wallet)?,
            };
            let lock = wallet.pay_invoice(&hub, &invoice).await?;
            return pay_invoice(&wallet, lock, wait, json).await;
        }
        Command::Invoice { amount, hub } => {
            let hub = match hub {
                Some(hub) => hub,
                None => only_hub(&wallet)?,
            };
            let invoice = wallet.invoice(&hub, amount).await?;
            eprintln!("invoice {}", invoice.hash);
            let request = invoice.request.clone().unwrap_or_default();
            show(json, invoice_json(&invoice), request);
        }
        Command::Invoices => {
            let invoices = wallet.invoices()?;
            let lines = invoices.iter().map(invoice_line).collect::<Vec<_>>();
            show(
                json,
                invoices.iter().map(invoice_json).collect(),
                lines.join("\n"),
            );
        }
    }
    Ok(())
}

/// Follow `lock`, paying an invoice, for up to `wait` seconds: done once
/// paid, failed once refunded, pending while under way.
async fn pay_invoice(
    wallet: &Wallet<Http>,
    mut lock: Lock,
    wait: u64,
    json: bool,
) -> anyhow::Result<()> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    let mut pending = None;
    while matches!(lock.state, LockState::Requested | LockState::Locked)
        && std::time::Instant::now() < until
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        match wallet.follow_lock(&lock.id).await {
            Ok(followed) => (lock, pending) = (followed, None),
            Err(Error::Pending { reason, .. }) => pending = Some(reason),
            Err(e) => return Err(e.into()),
        }
    }
    let text = match &lock.state {
        LockState::Paid(preimage) => format!(
            "paid {} + {}\npreimage {}",
            lock.amount,
            lock.fee,
            hex::encode(preimage)
        ),
        _ => lock_line(&lock),
    };
    show(json, lock_json(&lock), text);
    match (&lock.state, lock.failure) {
        (LockState::Paid(_), _) => Ok(()),
        (LockState::Requested | LockState::Locked, _) => Err(Error::Pending {
            id: lock.id,
            reason: pending.unwrap_or_else(|| "the payment is under way".into()),
        }
        .into()),
        (LockState::Refunded, Some(failure)) => {
            bail!("the invoice was not paid ({failure}); its lock was refunded")
        }
        (state, failure) => bail!(
            "the invoice was not paid: its lock {}{}",
            lock_state(state),
            failure.map(|f| format!(" ({f})")).unwrap_or_default()
        ),
    }
}

/// Print the hubs announced on `relays` for `network`, each at the URLs
/// whose info names the key that announced it.
async fn discover(
    http: Http,
    proxy: Option<&str>,
    relays: &[String],
    network: &str,
    json: bool,
) -> anyhow::Result<()> {
    let socks = proxy.map(socks_address).transpose()?;
    let found = licash_nostr::discover(relays, socks, Role::Hub, network).await?;
    let http = Arc::new(http);
    let mut checks = tokio::task::JoinSet::new();
    for (i, hub) in found.iter().enumerate() {
        for url in &hub.announcement.urls {
            if is_onion(url) && proxy.is_none() {
                tracing::warn!(%url, "skipped: an onion service needs --proxy");
                continue;
            }
            let (http, url) = (http.clone(), url.clone());
            checks.spawn(async move {
                let info = tokio::time::timeout(CHECK_TIMEOUT, info_at(&http, &url)).await;
                let info = info.unwrap_or_else(|_| Err("no answer in time".into()));
                (i, url, info)
            });
        }
    }
    let mut answered: Vec<Vec<(String, licash_api::Info)>> = vec![Vec::new(); found.len()];
    while let Some(checked) = checks.join_next().await {
        let (i, url, info) = checked?;
        match info {
            Ok(info) if found[i].named_by(info.nostr.as_deref()) => answered[i].push((url, info)),
            Ok(_) => tracing::warn!(%url, "skipped: not the hub that announced it"),
            Err(e) => tracing::warn!(%url, "skipped: {e}"),
        }
    }
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for (hub, mut at) in found.iter().zip(answered) {
        if at.is_empty() {
            continue;
        }
        // In the order announced.
        let urls = &hub.announcement.urls;
        at.sort_by_key(|(url, _)| urls.iter().position(|u| u == url));
        let info = &at[0].1;
        let urls: Vec<&str> = at.iter().map(|(url, _)| url.as_str()).collect();
        let name = hub.announcement.name.as_deref().unwrap_or_default();
        let fee = info.fees.spend;
        let lightning = if info.lightning.is_some() {
            "  lightning"
        } else {
            ""
        };
        lines.push(format!(
            "{}  {} + {} ppm{lightning}  {name}",
            urls.join(" "),
            fee.base,
            fee.ppm
        ));
        rows.push(json!({
            "key": hub.key, "name": hub.announcement.name, "urls": urls, "fees": info.fees,
            "locks": info.locks.is_some(), "lightning": info.lightning,
        }));
    }
    show(json, rows.into(), lines.join("\n"));
    Ok(())
}

/// How long a hub announced has to answer.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What the hub at `url` says of itself.
async fn info_at(http: &Http, url: &str) -> Result<licash_api::Info, String> {
    let at = format!("{}{}", url.trim_end_matches('/'), licash_api::INFO);
    let response = licash_wallet::Transport::request(http, &at, None).await?;
    if response.status != 200 {
        return Err(format!("status {}", response.status));
    }
    serde_json::from_slice(&response.body).map_err(|e| format!("not a hub's info: {e}"))
}

fn is_onion(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split(['/', ':']).next().unwrap_or(host);
    host.ends_with(".onion")
}

/// The address of a `socks5[h]://host:port` proxy, which Nostr relays are
/// reached through.
fn socks_address(proxy: &str) -> anyhow::Result<std::net::SocketAddr> {
    let at = proxy
        .strip_prefix("socks5h://")
        .or_else(|| proxy.strip_prefix("socks5://"))
        .context("Nostr relays are reached through a socks5h:// proxy only")?;
    std::net::ToSocketAddrs::to_socket_addrs(at.trim_end_matches('/'))
        .with_context(|| format!("the proxy {at}"))?
        .next()
        .with_context(|| format!("the proxy {at}: no address"))
}

fn init(path: &Path, transport: Http, json: bool) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        let mut create = std::fs::DirBuilder::new();
        create.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut create, 0o700);
        create
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut seed = [0; 32];
    OsRng.fill_bytes(&mut seed);
    let wallet = Wallet::create(path, &seed, transport)?;
    let address = encode(&wallet.address());
    show(
        json,
        json!({ "wallet": path, "address": address }),
        format!(
            "created {}\naddress {address}\nBack up this file: the seed alone does not restore its notes.",
            path.display()
        ),
    );
    Ok(())
}

async fn resume(wallet: &Wallet<Http>, json: bool) -> anyhow::Result<()> {
    let resumed = wallet.resume().await?;
    let unlinked = unlink(wallet).await;
    let mut lines = Vec::new();
    for paid in &resumed.paid {
        let id = &paid.id;
        lines.push(format!(
            "done {id}: `licash-wallet payment {id}` prints its payment"
        ));
    }
    lines.extend(resumed.locks.iter().map(lock_line));
    lines.extend(resumed.invoices.iter().map(invoice_line));
    for (id, reason) in &resumed.abandoned {
        lines.push(format!("abandoned {id}: {reason}"));
    }
    for (id, reason) in &resumed.pending {
        lines.push(format!("pending {id}: {reason}"));
    }
    let paid = resumed
        .paid
        .iter()
        .map(|p| json!({ "id": p.id, "payment": p.payment }));
    let abandoned = resumed
        .abandoned
        .iter()
        .map(|(id, r)| json!({ "id": id, "reason": r }));
    let pending = resumed
        .pending
        .iter()
        .map(|(id, r)| json!({ "id": id, "reason": r }));
    show(
        json,
        json!({
            "paid": paid.collect::<Vec<_>>(),
            "locks": resumed.locks.iter().map(lock_json).collect::<Vec<_>>(),
            "invoices": resumed.invoices.iter().map(invoice_json).collect::<Vec<_>>(),
            "abandoned": abandoned.collect::<Vec<_>>(),
            "pending": pending.collect::<Vec<_>>(),
            "unlinked": unlinked,
        }),
        lines.join("\n"),
    );
    match resumed.pending.into_iter().next() {
        Some((id, reason)) => Err(Error::Pending { id, reason }.into()),
        None => Ok(()),
    }
}

/// Unlink what can be: a failure leaves notes linked until the next try.
async fn unlink(wallet: &Wallet<Http>) -> Option<usize> {
    match wallet.unlink().await {
        Ok(unlinked) => Some(unlinked),
        Err(e) => {
            eprintln!("warning: some notes stay linked for now ({e}); resume unlinks them");
            None
        }
    }
}

/// The hub holding the most.
fn richest(wallet: &Wallet<Http>) -> anyhow::Result<String> {
    let balance = wallet.balance()?;
    let richest = balance.into_iter().max_by_key(|b| b.available);
    Ok(richest
        .context("no hub: `licash-wallet add-hub URL` trusts one")?
        .hub)
}

/// The only hub trusted.
fn only_hub(wallet: &Wallet<Http>) -> anyhow::Result<String> {
    let hubs = wallet.hubs()?;
    match &hubs[..] {
        [(url, _)] => Ok(url.clone()),
        [] => bail!("no hub: `licash-wallet add-hub URL` trusts one"),
        _ => bail!("several hubs trusted: name one with --hub"),
    }
}

fn lock_state(state: &LockState) -> String {
    match state {
        LockState::Requested => "requested".into(),
        LockState::Locked => "locked".into(),
        LockState::Paid(_) => "paid".into(),
        LockState::Refunded => "refunded".into(),
        LockState::Abandoned(reason) => format!("abandoned ({reason})"),
        _ => "unknown".into(),
    }
}

fn lock_json(lock: &Lock) -> serde_json::Value {
    let preimage = match &lock.state {
        LockState::Paid(preimage) => Some(hex::encode(preimage)),
        _ => None,
    };
    json!({
        "id": lock.id, "hub": lock.hub, "payee": encode(&lock.payee), "amount": lock.amount,
        "fee": lock.fee, "hash": hex::encode(lock.hash), "expiry": lock.expiry,
        "state": lock_state(&lock.state), "preimage": preimage, "invoice": lock.invoice,
        "failure": lock.failure, "created_at": lock.created_at,
    })
}

fn lock_line(lock: &Lock) -> String {
    let state = match &lock.failure {
        Some(failure) => format!("{} ({failure})", lock_state(&lock.state)),
        None => lock_state(&lock.state),
    };
    format!(
        "{}  lock {}  {} + {}  until {}  via {}  {}",
        lock.created_at, state, lock.amount, lock.fee, lock.expiry, lock.hub, lock.id
    )
}

fn invoice_state(state: &InvoiceState) -> String {
    match state {
        InvoiceState::Open => "open".into(),
        InvoiceState::Received => "received".into(),
        InvoiceState::Canceled(reason) => format!("canceled ({reason})"),
        _ => "unknown".into(),
    }
}

fn invoice_json(invoice: &Invoice) -> serde_json::Value {
    json!({
        "hash": invoice.hash, "hub": invoice.hub, "amount": invoice.amount,
        "request": invoice.request, "state": invoice_state(&invoice.state),
        "created_at": invoice.created_at,
    })
}

fn invoice_line(invoice: &Invoice) -> String {
    format!(
        "{}  invoice {}  {}  via {}  {}",
        invoice.created_at,
        invoice_state(&invoice.state),
        invoice.amount,
        invoice.hub,
        invoice.hash
    )
}

/// 32 bytes, in hex.
fn bytes(hex: &str) -> anyhow::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hex.trim(), &mut bytes).context("not 32 bytes in hex")?;
    Ok(bytes)
}

fn state(state: &State) -> String {
    match state {
        State::Offered => "offered".into(),
        State::Signed => "signed".into(),
        State::Done => "done".into(),
        State::Abandoned(reason) => format!("abandoned ({reason})"),
        _ => "unknown".into(),
    }
}

fn show(json: bool, value: serde_json::Value, text: String) {
    if json {
        println!("{value}");
    } else if !text.is_empty() {
        println!("{text}");
    }
}

fn write_payment(payment: &Payment, out: Option<&Path>) -> anyhow::Result<()> {
    let text = serde_json::to_string(payment)?;
    match out {
        Some(out) => std::fs::write(out, text + "\n")
            .with_context(|| format!("writing {}", out.display()))?,
        None => writeln!(std::io::stdout(), "{text}")?,
    }
    Ok(())
}

fn read(path: &Path) -> anyhow::Result<String> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    }
    Ok(text)
}

fn encode(address: &Address) -> String {
    hex::encode(address.to_bytes())
}

fn decode(address: &str) -> anyhow::Result<Address> {
    let bytes = hex::decode(address.trim()).context("an address is hex")?;
    Address::from_bytes(&bytes).context("not an address")
}

fn default_path() -> anyhow::Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
        .context("no wallet file: give --wallet")?;
    Ok(data.join("licash").join("wallet.sqlite"))
}
