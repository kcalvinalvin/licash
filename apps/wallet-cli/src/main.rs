//! `licash-wallet`: pay and get paid through Licash hubs.

use std::{
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use licash::{
    address::Address,
    spend::{lock_hash, ChainProfile},
};
use licash_api::Payment;
use licash_nostr::{wallet::swap_providers, Role, Route};
use licash_transport::Http;
use licash_wallet::{
    Deposit, DepositState, Error, Invoice, InvoiceState, Lock, LockState, PaymentCode, State, Swap,
    SwapLimits, SwapState, Wallet,
};
use rand_core::{OsRng, RngCore};
use serde_json::json;
use tracing_subscriber::EnvFilter;

mod contract;
mod onchain;
mod payout;
mod say;

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
    /// Reach hubs, swap providers and Nostr relays through this proxy, such
    /// as socks5h://127.0.0.1:9050 for Tor's.
    #[arg(long, global = true, env = "LICASH_WALLET_PROXY")]
    proxy: Option<String>,
    /// Reach hubs, swap providers and Nostr relays through a Tor client of
    /// the wallet's own, instead of --proxy, keeping its state in `tor`
    /// next to the wallet file.
    #[arg(long, global = true, env = "LICASH_WALLET_TOR", conflicts_with = "proxy",
          value_parser = clap::builder::BoolishValueParser::new())]
    tor: bool,
    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Renew notes a hub no longer unlinks since it restarted, paying them
    /// to the wallet itself, only if its fee on them is at most this many
    /// parts per million of their value, its base fee included: 0 renews
    /// them only for free.
    #[arg(long, global = true, env = "LICASH_WALLET_RENEW_PPM",
          default_value_t = licash_wallet::RENEW_PPM)]
    renew_ppm: u32,
    /// Diagnostic logs on stderr as text or JSON (default: off). RUST_LOG
    /// also enables diagnostics; explicit --log defaults to warn.
    #[arg(long, global = true, env = "LICASH_WALLET_LOG", value_enum)]
    log: Option<Log>,
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
    Init {
        /// The network of the hubs it trusts: bitcoin, regtest, signet or
        /// mutinynet.
        #[arg(long, env = "LICASH_WALLET_NETWORK", default_value_t = ChainProfile::Core30Mainnet)]
        network: ChainProfile,
    },
    /// Where others pay this wallet.
    Address,
    /// A payment code to show as text or a QR code: payers pay it in notes
    /// at a hub it names that they trust, else on Bitcoin to its silent
    /// payment address, if it has one.
    Code {
        /// A hub it is paid at, preferred first; repeat it for more
        /// [default: the two trusted holding the most].
        #[arg(long)]
        hub: Vec<String>,
        /// With the wallet's silent payment address and a fallback address,
        /// for payers to pay on Bitcoin: `onchain scan` finds what they pay.
        #[arg(long)]
        sp: bool,
        /// In upper case, which a QR code holds in fewer modules.
        #[arg(long)]
        qr: bool,
    },
    /// Pay the payee of a payment code AMOUNT, the hub's fee on top, at a
    /// hub it names that this wallet trusts and holds enough at, else once
    /// a swap through a provider announced on Nostr brought it there: sent
    /// to the payee's mailbox, else printed to hand over.
    Pay {
        code: String,
        /// In satoshis [default: what the code asks for].
        amount: Option<u64>,
        /// Write the payment here instead of to stdout, if not sent.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// All its fees at most, in sat, a swap's included [default: any,
        /// but a payment needing a swap is refused, saying its fees].
        #[arg(long)]
        max_fee: Option<u64>,
        /// Also show the payment ID and the hub's full URL.
        #[arg(long)]
        details: bool,
    },
    /// Receive the payments sent to the wallet's mailbox, on the relays its
    /// hubs name, and unlink their notes.
    Mailbox,
    /// Trust the hub at URL, or learn again what it charges.
    AddHub { url: String },
    /// The hubs trusted, and their fees.
    Hubs,
    /// Available balance; --details shows where it is held.
    Balance {
        #[arg(long)]
        details: bool,
    },
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
    /// Get AMOUNT in notes for coins paid on Bitcoin: the address and the
    /// exact amount to pay, the hub's fee on top, as a BIP21 URI (the
    /// deposit's hash on stderr). `resume` takes the notes once the
    /// payment confirmed.
    Deposit {
        amount: u64,
        /// At this hub [default: the only one trusted].
        #[arg(long)]
        hub: Option<String>,
    },
    /// The deposits asked for, newest first.
    Deposits,
    /// Take back the coins paid to deposit HASH once its timeout passed,
    /// its notes never received.
    Refund {
        hash: String,
        /// Pay the coins to this address.
        #[arg(long)]
        to: String,
        /// The chain to find the payment on and broadcast to: Esplora as
        /// `esplora+https://esplora.example/api`, or a Bitcoin Core node
        /// as `http://user:password@127.0.0.1:8332`.
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: String,
        /// In sat/vB [default: the chain's estimate within 6 blocks, at
        /// least 1].
        #[arg(long)]
        fee_rate: Option<u64>,
    },
    /// Swap notes at one hub for AMOUNT at another through a swap provider,
    /// its fee and the hub's on top. `resume` takes a pending swap on.
    Swap {
        amount: u64,
        /// The hub to get AMOUNT at.
        #[arg(long)]
        to: String,
        /// The swap provider, by its URL [default: of those announced on
        /// the relays that swap AMOUNT within the limits, one swapped
        /// through before, else the cheapest].
        #[arg(long, env = "LICASH_WALLET_PROVIDER")]
        provider: Option<String>,
        /// A relay to find a provider on [default: those the wallet's hubs
        /// name]. Repeat it, or separate relays with commas, for more.
        #[arg(
            long = "relay",
            value_name = "URL",
            env = "LICASH_WALLET_RELAYS",
            value_delimiter = ',',
            conflicts_with = "provider"
        )]
        relays: Vec<String>,
        /// The hub to pay at [default: the other one holding the most].
        #[arg(long)]
        from: Option<String>,
        /// The provider's fee at most, in sat [default: 1% and 100].
        #[arg(long)]
        max_fee: Option<u64>,
        /// The provider's margin at most, in blocks: how much longer the
        /// notes stay locked should it never pay.
        #[arg(long, default_value_t = SwapLimits::usual(0).margin)]
        max_margin: u32,
    },
    /// The swaps made, newest first.
    Swaps,
    /// Pay AMOUNT to TO, an address on Bitcoin, from notes at a hub
    /// through a swap provider: the wallet locks the amount and the fees to
    /// the provider, which locks a coin on chain the wallet claims, paying
    /// TO exactly AMOUNT. `payouts` takes it on until done.
    Payout {
        amount: u64,
        #[arg(long)]
        to: String,
        /// The swap provider, by its URL [default: of those announced on
        /// the relays that pay AMOUNT out within --max-fee, one paid out
        /// through before, else the cheapest].
        #[arg(long, env = "LICASH_WALLET_PROVIDER")]
        provider: Option<String>,
        /// A relay to find a provider on [default: those the wallet's hubs
        /// name]. Repeat it, or separate relays with commas, for more.
        #[arg(
            long = "relay",
            value_name = "URL",
            env = "LICASH_WALLET_RELAYS",
            value_delimiter = ',',
            conflicts_with = "provider"
        )]
        relays: Vec<String>,
        /// The hub to pay at [default: the one holding the most].
        #[arg(long)]
        from: Option<String>,
        /// What the payout costs at most on top of AMOUNT, in sat: the
        /// claim's fee, the provider's and the hub's [default: 1% and
        /// 2000].
        #[arg(long)]
        max_fee: Option<u64>,
        /// The fee rate the claim is budgeted at least, in sat/vB
        /// [default: the chain's estimate within 6 blocks, at least 1].
        #[arg(long)]
        fee_rate: Option<u64>,
        /// The chain to find the provider's coin on and broadcast the claim
        /// to: a Bitcoin Core node as `http://user:password@127.0.0.1:8332`,
        /// or Esplora as `esplora+https://esplora.example/api`.
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: String,
    },
    /// The payouts made, newest first: with --chain, those under way taken
    /// on first and their claims broadcast.
    Payouts {
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
    /// The coins paid on Bitcoin to the wallet's silent payment and
    /// fallback addresses: found, spent, or moved to notes.
    Onchain {
        /// The chain to read and broadcast to: a Bitcoin Core node as
        /// `http://user:password@127.0.0.1:8332`, which with
        /// `-blockfilterindex` spares fetching blocks that pay the wallet
        /// nothing, or Esplora as `esplora+https://esplora.example/api`.
        #[arg(long, global = true, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
        #[command(subcommand)]
        command: onchain::Onchain,
    },
    /// Open a recovery contract at HUB, a hub the wallet trusts: its
    /// collateral protects the wallet's notes there, up to its cap, until
    /// the contract's deadline. Paid from a coin of the wallet's on
    /// Bitcoin, its change back to it. On regtest under the mock authority,
    /// protecting nothing; on signet under the genuine programs, a trial:
    /// `force-close` recovers from it, not yet tried on signet itself. No
    /// other network for now.
    OpenContract {
        hub: String,
        /// Blocks from now to its close [default: as many as the hub
        /// allows].
        #[arg(long)]
        blocks: Option<u32>,
        /// The most to pay to open it, in sat [default: the price the
        /// hub's info said].
        #[arg(long)]
        max_price: Option<u64>,
        /// The most notes it protects, in sat, if the hub lets holders
        /// choose (its info's range), over --blocks [default: the hub's
        /// fixed terms].
        #[arg(long, requires = "blocks")]
        cap: Option<u32>,
        /// The coin on Bitcoin to pay it from, as TXID:VOUT [default: the
        /// smallest free that pays it].
        #[arg(long)]
        coin: Option<bitcoin::OutPoint>,
        /// The chain to check the hub's quote on and broadcast to.
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
    /// The wallet's contracts, the newest first. With --chain, those
    /// under way carried on, and where each one's funding stands.
    Contracts {
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
    /// Force contract ID (its id, or the start of it) closed when its hub
    /// stopped answering: the wallet's notes at the hub, up to its cap,
    /// claimed on Bitcoin, and once the hub's window to answer passed
    /// unanswered, paid to the wallet on Bitcoin with both reserves. Past
    /// its first step, irrevocable: those notes leave the wallet's balance
    /// for good. Run `recoveries --chain` until it is done.
    ForceClose {
        id: String,
        /// Its fees' rate, in sat/vB [default: the chain's estimate].
        #[arg(long)]
        fee_rate: Option<u64>,
        /// The chain to read and broadcast to.
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
    /// The contracts the wallet forced closed. With --chain, each carried
    /// on as far as the chain lets it.
    Recoveries {
        /// Its fees' rate, in sat/vB [default: the chain's estimate].
        #[arg(long)]
        fee_rate: Option<u64>,
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
    /// Give up forcing contract ID closed, its notes back to the wallet:
    /// only before its request is signed.
    GiveUpRecovery { id: String },
    /// The statement to prove of contract ID, forced closed and answered
    /// by its hub: written to a file, with how to prove it on another
    /// machine and bring the proof back with `import-proof`.
    RecoveryStatement {
        id: String,
        /// Where to write it [default: ID.statement.input here].
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Bring back the proof of contract ID's claim from FILE, as
    /// `licash-r0 native-wrap` wrote it: checked, kept, and asserted on
    /// Bitcoin at once. Run `recoveries --chain` until it is done.
    ImportProof {
        id: String,
        file: PathBuf,
        /// Its fees' rate, in sat/vB [default: the chain's estimate].
        #[arg(long)]
        fee_rate: Option<u64>,
        #[arg(long, env = "LICASH_WALLET_CHAIN")]
        chain: Option<String>,
    },
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
        /// The network of the hubs: bitcoin, regtest, signet or mutinynet
        /// [default: the wallet's, else bitcoin].
        #[arg(long)]
        network: Option<ChainProfile>,
        /// Swap providers instead of hubs.
        #[arg(long)]
        swaps: bool,
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

fn log_filter(format: Option<Log>, rust_log: Option<&str>) -> EnvFilter {
    rust_log
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| {
            EnvFilter::new(if format.is_some() {
                "warn"
            } else {
                "off,licash_wallet=warn"
            })
        })
}

fn logging(format: Option<Log>) {
    let filter = log_filter(format, std::env::var("RUST_LOG").ok().as_deref());
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    match format.unwrap_or(Log::Text) {
        Log::Text => logs.without_time().with_target(false).init(),
        Log::Json => logs.json().init(),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let path = match cli.wallet {
        Some(path) => path,
        None => default_path()?,
    };
    let tor = match cli.tor {
        true => {
            let dir = path.parent().unwrap_or(Path::new(".")).join("tor");
            Some(licash_tor::Tor::new(&dir).context("a Tor client")?)
        }
        false => None,
    };
    let transport = match &tor {
        Some(tor) => Http::tor(tor.clone()),
        None => Http::new(cli.proxy.as_deref())?,
    };
    let json = cli.json;
    match cli.command {
        Command::Init { network } => return init(&path, network, transport, json),
        Command::Discover {
            relays,
            network,
            swaps,
        } => {
            let network = match network {
                Some(network) => network,
                // Opening it reaches nothing.
                None if path.exists() => Wallet::open(&path, Http::new(None)?)?.network(),
                None => ChainProfile::Core30Mainnet,
            };
            let route = route(tor, cli.proxy.as_deref())?;
            return discover(transport, route, &relays, network.name(), swaps, json).await;
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
    wallet.set_renew_limit(cli.renew_ppm);
    match cli.command {
        Command::Init { .. } | Command::Discover { .. } => unreachable!("handled above"),
        Command::Address => {
            let address = encode(&wallet.address());
            show(json, json!({ "address": address }), address.clone());
        }
        Command::Code { hub, sp, qr } => {
            let hubs = match hub.is_empty() {
                false => hub,
                true => {
                    let mut balance = wallet.balance()?;
                    balance.sort_by_key(|b| std::cmp::Reverse(b.available));
                    balance.into_iter().take(2).map(|b| b.hub).collect()
                }
            };
            if hubs.is_empty() {
                bail!("no hub: `licash-wallet add-hub URL` trusts one");
            }
            let code = wallet.payment_code(&hubs, sp)?;
            let mailbox = code.licash.as_ref().is_some_and(|c| c.mailbox.is_some());
            let text = if qr { code.qr() } else { code.to_string() };
            if !mailbox {
                eprintln!(
                    "warning: none of its hubs names mailbox relays: payments are handed over"
                );
            }
            show(
                json,
                json!({ "code": text, "hubs": hubs, "mailbox": mailbox, "sp": sp }),
                text.clone(),
            );
        }
        Command::Pay {
            code,
            amount,
            out,
            max_fee,
            details,
        } => {
            let route = route(tor.clone(), cli.proxy.as_deref())?;
            let out = out.as_deref();
            pay(&wallet, &route, &code, amount, max_fee, out, json, details).await?;
        }
        Command::Mailbox => {
            let route = route(tor.clone(), cli.proxy.as_deref())?;
            mailbox(&wallet, &route, json).await?;
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
                match (fee.base, fee.ppm) {
                    (0, 0) => "trusted: it charges no fee".to_owned(),
                    _ => format!(
                        "trusted: it charges {} a payment",
                        say::fee(fee.base, fee.ppm)
                    ),
                },
            );
        }
        Command::Hubs => {
            let hubs = wallet.hubs()?;
            let rows = hubs.iter().map(|(url, info)| {
                json!({
                    "url": url, "fees": info.fees, "locks": info.locks.is_some(),
                    "lightning": info.lightning, "deposits": info.deposits,
                })
            });
            let lines = hubs.iter().map(|(url, info)| {
                let fee = info.fees.spend;
                let lightning = if info.lightning.is_some() {
                    "  lightning"
                } else {
                    ""
                };
                let deposits = match &info.deposits {
                    Some(d) => format!("  deposits {}", say::between(d.min, d.max)),
                    None => String::new(),
                };
                format!(
                    "{url}  {}{lightning}{deposits}",
                    say::fee(fee.base, fee.ppm)
                )
            });
            show(json, rows.collect(), lines.collect::<Vec<_>>().join("\n"));
        }
        Command::Balance { details } => {
            let balance = wallet.balance()?;
            let rows = balance
                .iter()
                .map(|b| json!({ "hub": b.hub, "available": b.available, "pending": b.pending }));
            let total: u64 = balance.iter().map(|b| b.available).sum();
            let mut lines = vec![format!("balance  {}", say::sat(total))];
            let pending: u64 = balance.iter().map(|b| b.pending).sum();
            if pending > 0 {
                lines.push(format!("pending  {}", say::sat(pending)));
            }
            if details {
                lines.extend(balance.iter().map(|b| match b.pending {
                    0 => format!("{}  {}", b.hub, say::sat(b.available)),
                    p => format!(
                        "{}  {} ({} pending)",
                        b.hub,
                        say::sat(b.available),
                        say::sat(p)
                    ),
                }));
            }
            // Left at hubs visited to pay there, apart from the total (and
            // from the JSON rows, which sum to it) until added.
            for b in wallet.visited_balance()? {
                if b.available + b.pending > 0 {
                    let held = b.available + b.pending;
                    lines.push(format!(
                        "not counted  {} at {} (hub not added: add-hub to count it)",
                        say::sat(held),
                        b.hub
                    ));
                }
            }
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
            // Its change, linked to it until then.
            unlink(&wallet).await;
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
                format!("received {}", say::sat(amount)),
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
                    "{}  {}  {}  via {}  {}",
                    say::when(s.created_at),
                    state(&s.state),
                    say::plus(s.amount, s.fee),
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
            let lines = receipts.iter().map(|r| {
                let (at, amount) = (say::when(r.received_at), say::sat(r.amount));
                format!("{at}  {amount}  via {}  {}", r.hub, r.id)
            });
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
                format!("received {}", say::sat(amount)),
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
        Command::Deposit { amount, hub } => {
            let hub = match hub {
                Some(hub) => hub,
                None => only_hub(&wallet)?,
            };
            let deposit = wallet.deposit(&hub, amount).await?;
            let text = match (&deposit.terms, &deposit.state) {
                (Some(terms), DepositState::Open) => {
                    eprintln!(
                        "deposit {}: pay exactly {} sat, confirmed by block {}; refundable after block {}",
                        deposit.hash, terms.pays, terms.fund_by, terms.timeout
                    );
                    bip21(&terms.address, terms.pays)
                }
                _ => {
                    show(json, deposit_json(&deposit), deposit_line(&deposit));
                    bail!(
                        "deposit {}: {}",
                        deposit.hash,
                        deposit_state(&deposit.state)
                    )
                }
            };
            show(json, deposit_json(&deposit), text);
        }
        Command::Deposits => {
            let deposits = wallet.deposits()?;
            let lines = deposits.iter().map(deposit_line).collect::<Vec<_>>();
            show(
                json,
                deposits.iter().map(deposit_json).collect(),
                lines.join("\n"),
            );
        }
        Command::Refund {
            hash,
            to,
            chain,
            fee_rate,
        } => {
            let txid = refund(&wallet, &hash, &to, &chain, fee_rate).await?;
            show(
                json,
                json!({ "hash": hash, "txid": txid.to_string() }),
                txid.to_string(),
            );
        }
        Command::Swap {
            amount,
            to,
            provider,
            relays,
            from,
            max_fee,
            max_margin,
        } => {
            let from = match from {
                Some(from) => from,
                None => richest_but(&wallet, &to)?,
            };
            let limits = SwapLimits {
                fee: max_fee.unwrap_or(SwapLimits::usual(amount).fee),
                margin: max_margin,
            };
            let (provider, found) = match provider {
                Some(provider) => (provider, false),
                None => {
                    let route = route(tor.clone(), cli.proxy.as_deref())?;
                    let pair = (from.as_str(), to.as_str());
                    let found = find_provider(&wallet, relays, &route, pair, amount, limits);
                    (found.await?, true)
                }
            };
            let swap = wallet.swap(&provider, &from, &to, amount, limits).await?;
            let unlinked = unlink(&wallet).await;
            let mut shown = swap_json(&swap);
            shown["unlinked"] = json!(unlinked);
            match &swap.state {
                SwapState::Failed(reason) => {
                    show(json, shown, swap_line(&swap));
                    bail!("the swap failed ({reason}): its lock returns once expired")
                }
                _ => {
                    let through = match found {
                        true => format!(" through {provider}"),
                        false => String::new(),
                    };
                    show(
                        json,
                        shown,
                        format!(
                            "swapped {} at {} for {} at {}{through}",
                            say::plus(swap.amount, swap.fee),
                            swap.from,
                            say::sat(swap.amount),
                            swap.to
                        ),
                    )
                }
            }
        }
        Command::Onchain { chain, command } => {
            return onchain::run(&wallet, chain, command, json).await;
        }
        Command::Payout {
            amount,
            to,
            provider,
            relays,
            from,
            max_fee,
            fee_rate,
            chain,
        } => {
            let from = match from {
                Some(from) => from,
                None => richest_but(&wallet, "")?,
            };
            let provider = match provider {
                Some(url) => payout::Provider::At(url),
                None => payout::Provider::Announced(relays, route(tor, cli.proxy.as_deref())?),
            };
            let route = (provider, from.as_str(), to.as_str());
            return payout::pay(&wallet, &chain, route, amount, (max_fee, fee_rate), json).await;
        }
        Command::Payouts { chain } => return payout::list(&wallet, chain.as_deref(), json).await,
        Command::OpenContract {
            hub,
            blocks,
            max_price,
            cap,
            coin,
            chain,
        } => {
            let ask = contract::Asked {
                blocks,
                max_price,
                cap,
                coin,
            };
            return contract::open(&wallet, &path, chain.as_deref(), &hub, ask, json).await;
        }
        Command::Contracts { chain } => {
            return contract::list(&wallet, &path, chain.as_deref(), json).await;
        }
        Command::ForceClose {
            id,
            fee_rate,
            chain,
        } => {
            return contract::force_close(&wallet, &path, chain.as_deref(), &id, fee_rate, json)
                .await;
        }
        Command::Recoveries { fee_rate, chain } => {
            return contract::recoveries(&wallet, &path, chain.as_deref(), fee_rate, json).await;
        }
        Command::GiveUpRecovery { id } => contract::give_up(&wallet, &path, &id)?,
        Command::RecoveryStatement { id, out } => {
            contract::statement(&path, &id, out.as_deref(), json)?
        }
        Command::ImportProof {
            id,
            file,
            fee_rate,
            chain,
        } => {
            let chain = chain.as_deref();
            return contract::import_proof(&wallet, &path, chain, &id, &file, fee_rate, json).await;
        }
        Command::Swaps => {
            let swaps = wallet.swaps()?;
            let lines = swaps.iter().map(swap_line).collect::<Vec<_>>();
            show(
                json,
                swaps.iter().map(swap_json).collect(),
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
            "paid {}\npreimage {}",
            say::plus(lock.amount, lock.fee),
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

/// Print the hubs, or swap providers, announced on `relays` for `network`,
/// each at the URLs whose info names the key that announced it.
async fn discover(
    http: Http,
    route: Route,
    relays: &[String],
    network: &str,
    swaps: bool,
    json: bool,
) -> anyhow::Result<()> {
    // Onion services are reached the way relays are, if through Tor.
    let onions = !matches!(route, Route::Direct);
    let role = if swaps { Role::Swaps } else { Role::Hub };
    let found = licash_nostr::discover(relays, &route, role, network).await?;
    let http = Arc::new(http);
    let mut checks = tokio::task::JoinSet::new();
    for (i, service) in found.iter().enumerate() {
        for url in &service.announcement.urls {
            if is_onion(url) && !onions {
                tracing::warn!(%url, "skipped: an onion service needs --proxy or --tor");
                continue;
            }
            let (http, url) = (http.clone(), url.clone());
            checks.spawn(async move {
                let info = tokio::time::timeout(CHECK_TIMEOUT, info_at(&http, &url, swaps)).await;
                let info = info.unwrap_or_else(|_| Err("no answer in time".into()));
                (i, url, info)
            });
        }
    }
    let mut answered: Vec<Vec<(String, Answer)>> = Vec::new();
    answered.resize_with(found.len(), Vec::new);
    while let Some(checked) = checks.join_next().await {
        let (i, url, info) = checked?;
        match info {
            Ok(info) if found[i].named_by(info.nostr()) => answered[i].push((url, info)),
            Ok(_) => tracing::warn!(%url, "skipped: not the service that announced it"),
            Err(e) => tracing::warn!(%url, "skipped: {e}"),
        }
    }
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for (service, mut at) in found.iter().zip(answered) {
        if at.is_empty() {
            continue;
        }
        // In the order announced.
        let announced = &service.announcement.urls;
        at.sort_by_key(|(url, _)| announced.iter().position(|u| u == url));
        let urls: Vec<&str> = at.iter().map(|(url, _)| url.as_str()).collect();
        let name = service.announcement.name.as_deref().unwrap_or_default();
        let (key, shown) = (&service.key, &service.announcement.name);
        match &at[0].1 {
            Answer::Hub(info) => {
                let fee = info.fees.spend;
                let lightning = if info.lightning.is_some() {
                    "  lightning"
                } else {
                    ""
                };
                lines.push(format!(
                    "{}  {}{lightning}  {name}",
                    urls.join(" "),
                    say::fee(fee.base, fee.ppm)
                ));
                rows.push(json!({
                    "key": key, "name": shown, "urls": urls, "fees": info.fees,
                    "locks": info.locks.is_some(), "lightning": info.lightning,
                }));
            }
            Answer::Swaps(info) => {
                let fee = info.fee;
                lines.push(format!(
                    "{}  {}  {}  {} hubs  {name}",
                    urls.join(" "),
                    say::fee(fee.base, fee.ppm),
                    say::between(info.min, info.max),
                    info.hubs.len()
                ));
                let hubs = info.hubs.iter().map(|hub| {
                    json!({
                        "address": encode(&hub.address), "max": hub.max,
                        "margin": info.margin_at(hub),
                    })
                });
                rows.push(json!({
                    "key": key, "name": shown, "urls": urls, "fee": fee, "min": info.min,
                    "max": info.max, "margin": info.margin, "hubs": hubs.collect::<Vec<_>>(),
                }));
            }
        }
    }
    show(json, rows.into(), lines.join("\n"));
    Ok(())
}

/// How long a service announced has to answer.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What a service announced says of itself.
enum Answer {
    Hub(Box<licash_api::Info>),
    Swaps(Box<licash_api::swaps::Info>),
}

impl Answer {
    /// The key it announces itself under.
    fn nostr(&self) -> Option<&str> {
        match self {
            Self::Hub(info) => info.nostr.as_deref(),
            Self::Swaps(info) => info.nostr.as_deref(),
        }
    }
}

/// What the hub, or the swap provider, at `url` says of itself.
async fn info_at(http: &Http, url: &str, swaps: bool) -> Result<Answer, String> {
    let path = if swaps {
        licash_api::swaps::SWAPS_INFO
    } else {
        licash_api::INFO
    };
    let at = format!("{}{path}", url.trim_end_matches('/'));
    let response = licash_wallet::Transport::request(http, &at, None).await?;
    if response.status != 200 {
        return Err(format!("status {}", response.status));
    }
    let body = &response.body;
    let answer = if swaps {
        serde_json::from_slice(body).map(|info| Answer::Swaps(Box::new(info)))
    } else {
        serde_json::from_slice(body).map(|info| Answer::Hub(Box::new(info)))
    };
    answer.map_err(|e| format!("not a service's info: {e}"))
}

fn is_onion(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split(['/', ':']).next().unwrap_or(host);
    host.ends_with(".onion")
}

fn init(path: &Path, network: ChainProfile, transport: Http, json: bool) -> anyhow::Result<()> {
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
    let wallet = Wallet::create(path, &seed, network, transport)?;
    let address = encode(&wallet.address());
    show(
        json,
        json!({ "wallet": path, "address": address, "network": network.name() }),
        format!(
            "created {} for {network}\naddress {address}\nBack up this file: the seed alone does not restore its notes.",
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
    lines.extend(resumed.swaps.iter().map(swap_line));
    lines.extend(resumed.deposits.iter().map(deposit_line));
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
            "swaps": resumed.swaps.iter().map(swap_json).collect::<Vec<_>>(),
            "deposits": resumed.deposits.iter().map(deposit_json).collect::<Vec<_>>(),
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

/// Pay the payee of payment code `text` `amount`, or what the code asks
/// for, at a hub it names that the wallet trusts and holds enough at, else
/// once a swap brought it there, fees of `max_fee` at most: sent to its
/// mailbox on the relays that hub names, if it has one, else written to
/// `out` or stdout to hand over. A swap is only made with a `max_fee`.
#[allow(clippy::too_many_arguments)]
async fn pay(
    wallet: &Wallet<Http>,
    route: &Route,
    text: &str,
    amount: Option<u64>,
    max_fee: Option<u64>,
    out: Option<&Path>,
    json: bool,
    details: bool,
) -> anyhow::Result<()> {
    let code: PaymentCode = text.parse()?;
    let progress = !json && std::io::stderr().is_terminal();
    if progress {
        eprintln!("Preparing payment…");
    }
    let plan = licash_nostr::wallet::plan(wallet, &code, amount, route).await?;
    let fees = plan.fees();
    if let Some(swap) = &plan.swap {
        let (provider, quote) = (&swap.provider, &swap.provider.quote);
        let technical = format!(
            "paying {} at {} swaps {} from {} through {} first",
            plan.amount, plan.hub, quote.amount, swap.from, provider.url
        );
        let swapping = if json || details {
            technical
        } else {
            format!("A swap is needed to send {}", say::sat(plan.amount))
        };
        let swapping = if plan.trusted {
            swapping
        } else {
            format!("{swapping}; {} is not a hub you added: what the swap brings past the payment stays there", plan.hub)
        };
        if max_fee.is_none() {
            if json {
                bail!("{swapping}, {fees} sat in fees at most in all: pay again with --max-fee {fees}");
            }
            bail!(
                "{swapping}; fee up to {}: pay again with --max-fee {fees}",
                say::fee_of(fees, plan.amount)
            );
        }
        if json || details || !plan.trusted {
            eprintln!("{swapping}");
        }
    }
    if let Some(max) = max_fee.filter(|max| fees > *max) {
        if json {
            bail!("paying costs {fees} sat in fees at most, more than --max-fee {max}");
        }
        bail!(
            "fee up to {} exceeds --max-fee {max}",
            say::fee_of(fees, plan.amount)
        );
    }
    if progress {
        eprintln!("Sending {}…", say::sat(plan.amount));
    }
    let paid = licash_nostr::wallet::pay_planned(wallet, &code, &plan, || Ok(route.clone()));
    let paid = paid.await?;
    let (id, hub, amount) = (&paid.paid.id, &paid.hub, paid.amount);
    if json || details {
        eprintln!("payment {id}");
    }
    if let Some(reason) = &paid.undelivered {
        eprintln!("warning: not sent to the payee's mailbox ({reason}): hand it over");
    }
    match paid.delivered {
        Some(relays) => show(
            json,
            json!({ "id": id, "hub": hub, "amount": amount, "relays": relays }),
            format!(
                "sent to mailbox  {}\nfee              {}{}",
                say::sat(amount),
                if fees == 0 {
                    String::new()
                } else {
                    "up to ".into()
                },
                say::fee_of(fees, amount)
            ) + &if details {
                format!("\nhub              {hub}")
            } else {
                String::new()
            },
        ),
        None => {
            write_payment(&paid.paid.payment, out)?;
            if !json {
                eprintln!(
                    "payment ready  {}\nfee            {}{}\nHand the payment to the recipient.",
                    say::sat(amount),
                    if fees == 0 { "" } else { "up to " },
                    say::fee_of(fees, amount)
                );
            }
        }
    }
    // Its change, linked to it until then.
    unlink(wallet).await;
    Ok(())
}

/// Receive the payments in the wallet's mailbox, on the relays its hubs
/// name: each once, of hubs it trusts only.
async fn mailbox(wallet: &Wallet<Http>, route: &Route, json: bool) -> anyhow::Result<()> {
    let collected = licash_nostr::wallet::collect(wallet, route).await?;
    for (id, reason) in &collected.skipped {
        eprintln!("warning: {id} in the mailbox {reason}");
    }
    let received = collected.received;
    let unlinked = match received.is_empty() {
        true => Some(0),
        false => unlink(wallet).await,
    };
    let rows: Vec<_> = received
        .iter()
        .map(|r| json!({ "id": r.id, "hub": r.hub, "amount": r.amount }))
        .collect();
    let mut lines: Vec<String> = received
        .iter()
        .map(|r| format!("received {} at {}", say::sat(r.amount), r.hub))
        .collect();
    if lines.is_empty() {
        lines.push("nothing new".into());
    }
    show(
        json,
        json!({ "received": rows, "unlinked": unlinked }),
        lines.join("\n"),
    );
    Ok(())
}

/// How to reach Nostr relays: through the wallet's own Tor client, else
/// `proxy`, else directly.
fn route(tor: Option<licash_tor::Tor>, proxy: Option<&str>) -> anyhow::Result<Route> {
    Ok(match (tor, proxy) {
        (Some(tor), _) => Route::Tor(tor),
        (None, Some(proxy)) => Route::socks(proxy)?,
        (None, None) => Route::Direct,
    })
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

/// The hub holding the most, other than `other`.
fn richest_but(wallet: &Wallet<Http>, other: &str) -> anyhow::Result<String> {
    let other = other.trim_end_matches('/');
    let balance = wallet.balance()?;
    let richest = balance
        .into_iter()
        .filter(|b| b.hub != other)
        .max_by_key(|b| b.available);
    Ok(richest
        .context("no other hub to pay at: `licash-wallet add-hub URL` trusts one")?
        .hub)
}

/// The provider to swap `amount` from hub `from` to hub `to` through
/// within `limits`: the first [`swap_providers`] finds on `relays`, else
/// on those the wallet's hubs name.
async fn find_provider(
    wallet: &Wallet<Http>,
    relays: Vec<String>,
    route: &Route,
    (from, to): (&str, &str),
    amount: u64,
    limits: SwapLimits,
) -> anyhow::Result<String> {
    let relays = match relays {
        relays if !relays.is_empty() => relays,
        _ => licash_nostr::wallet::hub_relays(wallet)?,
    };
    if relays.is_empty() {
        bail!("no relay to find a provider on: name one with --relay, or a provider");
    }
    let found = swap_providers(wallet, &relays, route, (from, to), amount, limits).await?;
    // Of those swapping that much exactly, not their least if more.
    let found = found.into_iter().find(|p| p.quote.amount == amount);
    let found = found.with_context(|| {
        format!("no provider announced swaps {amount} from {from} to {to} within the limits")
    })?;
    let (fee, known) = (found.quote.fee, found.known);
    tracing::info!(provider = %found.url, fee, known, "provider found");
    Ok(found.url)
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
        "{}  lock {}  {}  until block {}  via {}  {}",
        say::when(lock.created_at),
        state,
        say::plus(lock.amount, lock.fee),
        lock.expiry,
        lock.hub,
        lock.id
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
        say::when(invoice.created_at),
        invoice_state(&invoice.state),
        say::sat(invoice.amount),
        invoice.hub,
        invoice.hash
    )
}

fn deposit_state(state: &DepositState) -> String {
    match state {
        DepositState::Open => "open".into(),
        DepositState::Received => "received".into(),
        DepositState::Canceled(reason) => format!("canceled ({reason})"),
        DepositState::Refunded(txid) => format!("refunded ({txid})"),
        _ => "unknown".into(),
    }
}

fn deposit_json(deposit: &Deposit) -> serde_json::Value {
    let terms = deposit.terms.as_ref().map(|t| {
        json!({
            "address": t.address, "pays": t.pays, "fund_by": t.fund_by, "timeout": t.timeout,
            "uri": bip21(&t.address, t.pays),
        })
    });
    let funding = deposit
        .funding
        .map(|(outpoint, height)| json!({ "outpoint": outpoint.to_string(), "height": height }));
    json!({
        "hash": deposit.hash, "hub": deposit.hub, "amount": deposit.amount, "terms": terms,
        "funding": funding, "state": deposit_state(&deposit.state),
        "created_at": deposit.created_at,
    })
}

fn deposit_line(deposit: &Deposit) -> String {
    let terms = match &deposit.terms {
        Some(t) => format!(
            "  pay {} to {} by block {}",
            say::plus(deposit.amount, t.pays.saturating_sub(deposit.amount)),
            t.address,
            t.fund_by
        ),
        None => String::new(),
    };
    format!(
        "{}  deposit {}  {}  at {}  {}{terms}",
        say::when(deposit.created_at),
        deposit_state(&deposit.state),
        say::sat(deposit.amount),
        deposit.hub,
        deposit.hash
    )
}

/// A BIP21 URI paying `sat` to `address`.
fn bip21(address: &str, sat: u64) -> String {
    let btc = bitcoin::Amount::from_sat(sat).to_string_in(bitcoin::Denomination::Bitcoin);
    format!("bitcoin:{address}?amount={btc}")
}

/// Refund deposit `hash` to `to` from what `chain` shows paid to it, once
/// its timeout passed: the refund's id, broadcast.
async fn refund(
    wallet: &Wallet<Http>,
    hash: &str,
    to: &str,
    chain: &str,
    fee_rate: Option<u64>,
) -> anyhow::Result<bitcoin::Txid> {
    let network = licash_chain_client::network(wallet.network());
    let to = to
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .context("--to")?
        .require_network(network)
        .with_context(|| format!("--to: not an address on {}", wallet.network()))?;
    let source: licash_chain_client::Source = chain.parse().context("--chain")?;
    let deposit = wallet.deposit_of(hash)?;
    if deposit.state == DepositState::Received {
        bail!("its notes were received: the coins are its hub's");
    }
    let terms = deposit
        .terms
        .as_ref()
        .context("its hub named no address: nothing was paid to it")?;
    let address = terms
        .address
        .parse::<bitcoin::Address<_>>()?
        .assume_checked();
    let tip = source.tip().await?.height;
    if tip < terms.timeout {
        bail!("refundable after block {}, the tip is {tip}", terms.timeout);
    }
    let found = source.unspent(&address).await?;
    let Some(paid) = found.first() else {
        bail!("nothing unspent pays {address}: claimed or refunded already, or never paid");
    };
    if found.len() > 1 {
        tracing::warn!(
            count = found.len(),
            "several payments to the deposit: refunding the oldest, again for the others"
        );
    }
    let rate = onchain::rate(&source, fee_rate).await?;
    let tx = wallet.refund_deposit(hash, paid.outpoint, paid.value, to.script_pubkey(), rate)?;
    source.broadcast(&tx).await
}

fn swap_state(state: &SwapState) -> String {
    match state {
        SwapState::Open => "open".into(),
        SwapState::Done => "done".into(),
        SwapState::Failed(reason) => format!("failed ({reason})"),
        _ => "unknown".into(),
    }
}

fn swap_json(swap: &Swap) -> serde_json::Value {
    json!({
        "hash": swap.hash, "provider": swap.provider, "from": swap.from, "to": swap.to,
        "amount": swap.amount, "fee": swap.fee, "lock": swap.lock, "theirs": swap.theirs,
        "state": swap_state(&swap.state), "created_at": swap.created_at,
    })
}

fn swap_line(swap: &Swap) -> String {
    format!(
        "{}  swap {}  {}  {} → {}  {}",
        say::when(swap.created_at),
        swap_state(&swap.state),
        say::plus(swap.amount, swap.fee),
        swap.from,
        swap.to,
        swap.hash
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

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn diagnostics_are_opt_in_and_keep_explicit_filters() {
        let quiet = log_filter(None, None).to_string();
        assert!(
            quiet.contains("off") && quiet.contains("licash_wallet=warn"),
            "{quiet}"
        );
        assert_eq!(log_filter(Some(Log::Text), None).to_string(), "warn");
        assert_eq!(log_filter(Some(Log::Json), None).to_string(), "warn");
        assert_eq!(
            log_filter(None, Some("nostr_sdk=debug")).to_string(),
            "nostr_sdk=debug"
        );
    }

    #[test]
    fn relay_noise_is_hidden_but_wallet_warnings_are_not() {
        use std::sync::{Arc, Mutex};
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let capture = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(log_filter(None, None))
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || Capture(capture.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "nostr_sdk::relay::inner", "Connection reset");
            tracing::warn!(target: "licash_wallet::payout", "claim not broadcast: retry required");
        });
        let bytes = buffer.lock().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("claim not broadcast: retry required"),
            "{text}"
        );
        assert!(!text.contains("Connection reset"), "{text}");
    }
}
