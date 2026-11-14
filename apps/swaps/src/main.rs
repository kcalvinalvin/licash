//! `licash-swaps`: create, run and watch a Licash swap provider.

use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use licash_api::Fee;
use licash_notify::{Message, Priority, Target};
use licash_swaps::{Config, Provider};
use licash_swaps_service::notify::{self, Notifier};
use licash_transport::Http;
use licash_wallet::Wallet;
use rand_core::{OsRng, RngCore};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "licash-swaps",
    version,
    about = "Create, run and watch a Licash swap provider"
)]
struct Cli {
    /// The provider's directory: its wallet `wallet.sqlite` and its swaps
    /// `swaps.sqlite`.
    #[arg(long, global = true, env = "LICASH_SWAPS_DATA", default_value = "data")]
    data: PathBuf,
    /// Log lines on stderr as text or JSON, filtered by RUST_LOG.
    #[arg(long, global = true, env = "LICASH_SWAPS_LOG", value_enum, default_value_t = Log::Text)]
    log: Log,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Log {
    Text,
    Json,
}

#[derive(Args)]
struct Notify {
    /// Notify of the provider's events at this Apprise-style URL: gotify://,
    /// tgram://, ntfy://, json:// or apprise://. Repeat it, or separate URLs
    /// with commas, for more targets.
    #[arg(
        long = "notify",
        value_name = "URL",
        env = "LICASH_SWAPS_NOTIFY",
        value_delimiter = ',',
        hide_env_values = true
    )]
    urls: Vec<String>,
    /// The name heading notification titles.
    #[arg(long, env = "LICASH_SWAPS_NAME", default_value = "licash-swaps")]
    name: String,
}

impl Notify {
    /// The targets, refused without quoting their URLs, which hold tokens.
    fn targets(&self) -> anyhow::Result<Vec<Target>> {
        self.urls
            .iter()
            .enumerate()
            .filter(|(_, url)| !url.trim().is_empty())
            .map(|(i, url)| {
                url.parse()
                    .with_context(|| format!("notification target {}", i + 1))
            })
            .collect()
    }
}

/// What the provider swaps, and on what terms.
#[derive(Args)]
struct Terms {
    /// The fee on each swap, in the smallest unit.
    #[arg(long, env = "LICASH_SWAPS_FEE_BASE", default_value_t = Config::default().fee.base)]
    fee_base: u64,
    /// The fee on each swap, in parts per million of its amount.
    #[arg(long, env = "LICASH_SWAPS_FEE_PPM", default_value_t = Config::default().fee.ppm,
          value_parser = clap::value_parser!(u64).range(0..=1_000_000))]
    fee_ppm: u64,
    /// The least swapped at once.
    #[arg(long, env = "LICASH_SWAPS_MIN", default_value_t = Config::default().min)]
    min: u64,
    /// The most swapped at once.
    #[arg(long, env = "LICASH_SWAPS_MAX", default_value_t = Config::default().max)]
    max: u64,
    /// Blocks the provider's lock expires before the client's, at least:
    /// its time to claim once the client claimed.
    #[arg(long, env = "LICASH_SWAPS_MARGIN", default_value_t = Config::default().margin)]
    margin: u32,
}

impl Terms {
    fn config(&self) -> anyhow::Result<Config> {
        anyhow::ensure!(self.min <= self.max, "--min must not exceed --max");
        anyhow::ensure!(
            self.margin > licash_wallet::CLAIM_MARGIN + 2,
            "--margin must exceed {}",
            licash_wallet::CLAIM_MARGIN + 2
        );
        Ok(Config {
            fee: Fee {
                base: self.fee_base,
                ppm: self.fee_ppm,
            },
            min: self.min,
            max: self.max,
            margin: self.margin,
        })
    }
}

/// The provider's announcement on Nostr, for wallets to discover it.
#[derive(Args)]
struct AnnounceArgs {
    /// Announce the provider on this Nostr relay. Repeat it, or separate
    /// relays with commas, for more. Needs `--announce-url`.
    #[arg(
        long = "nostr-relay",
        value_name = "URL",
        env = "LICASH_SWAPS_NOSTR_RELAYS",
        value_delimiter = ','
    )]
    nostr_relays: Vec<String>,
    /// A URL wallets reach the provider at: `https://`, or an onion
    /// service's `http://`. Repeat it, up to 4.
    #[arg(
        long = "announce-url",
        value_name = "URL",
        env = "LICASH_SWAPS_ANNOUNCE_URLS",
        value_delimiter = ','
    )]
    announce_urls: Vec<String>,
    /// The name the announcement shows.
    #[arg(long = "announce-name", env = "LICASH_SWAPS_ANNOUNCE_NAME")]
    announce_name: Option<String>,
}

/// The relays, URLs and name of the announcement.
type Announcing = (Vec<String>, Vec<String>, Option<String>);

impl AnnounceArgs {
    fn announcing(self) -> anyhow::Result<Option<Announcing>> {
        let relays: Vec<String> = self
            .nostr_relays
            .into_iter()
            .filter(|r| !r.is_empty())
            .collect();
        let urls: Vec<String> = self
            .announce_urls
            .into_iter()
            .filter(|u| !u.is_empty())
            .collect();
        if relays.is_empty() {
            anyhow::ensure!(urls.is_empty(), "--announce-url needs --nostr-relay");
            return Ok(None);
        }
        let announcement = licash_nostr::Announcement {
            urls,
            network: String::new(),
            name: self.announce_name,
        };
        announcement.check().context("--announce-url")?;
        Ok(Some((relays, announcement.urls, announcement.name)))
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create the provider: a wallet with a fresh seed, and no swaps. Never
    /// replaces either. Add hubs and fund it with `licash-wallet --wallet
    /// <data>/wallet.sqlite`.
    Init,
    /// Serve the provider's HTTP API, and take its swaps on, until SIGINT
    /// or SIGTERM.
    Run {
        #[arg(long, env = "LICASH_SWAPS_LISTEN", default_value = "127.0.0.1:8340")]
        listen: SocketAddr,
        #[command(flatten)]
        terms: Terms,
        /// Seconds between rounds over the swaps under way.
        #[arg(long, env = "LICASH_SWAPS_STEP", default_value_t = 10,
              value_parser = clap::value_parser!(u64).range(1..))]
        step: u64,
        /// Reach hubs through this proxy, such as socks5h://127.0.0.1:9050
        /// for Tor's.
        #[arg(long, env = "LICASH_SWAPS_PROXY")]
        proxy: Option<String>,
        #[command(flatten)]
        notify: Notify,
        /// Notify only of these kinds of event [default: all].
        #[arg(long, value_name = "KIND", env = "LICASH_SWAPS_NOTIFY_KINDS", value_delimiter = ',',
              value_parser = clap::builder::PossibleValuesParser::new(notify::KINDS))]
        notify_kinds: Vec<String>,
        #[command(flatten)]
        announce: AnnounceArgs,
    },
    /// Send a test notification to each `--notify` target.
    TestNotify {
        #[command(flatten)]
        notify: Notify,
    },
    /// What the provider holds, its counts and its latest swaps.
    Status {
        #[arg(long)]
        json: bool,
        /// The latest swaps shown.
        #[arg(long, default_value_t = 10)]
        swaps: u32,
    },
    /// The provider's events as JSON lines, oldest first.
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Keep printing new events as they come.
        #[arg(long)]
        follow: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging(cli.log);
    match command(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn logging(format: Log) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    match format {
        Log::Text => logs.init(),
        Log::Json => logs.json().init(),
    }
}

fn command(cli: Cli) -> anyhow::Result<()> {
    let data = cli.data;
    match cli.command {
        Command::Init => init(&data),
        Command::Run {
            listen,
            terms,
            step,
            proxy,
            notify,
            notify_kinds,
            announce,
        } => {
            let config = terms.config()?;
            let announcing = announce.announcing()?;
            let targets = notify.targets()?;
            let provider = open(&data, Http::new(proxy.as_deref())?, config)?;
            let provider = Arc::new(provider);
            let notifier = if targets.is_empty() {
                None
            } else {
                let cursors = data.join("notify.json");
                let notifier = Notifier::new(
                    provider.clone(),
                    &notify.name,
                    targets,
                    &cursors,
                    notify_kinds,
                )
                .context("set up notifications")?;
                Some(notifier)
            };
            let every = Duration::from_secs(step);
            let serving = serve(provider, listen, every, notifier, announcing);
            tokio::runtime::Runtime::new()?.block_on(serving)
        }
        Command::TestNotify { notify } => test_notify(&notify),
        Command::Status { json, swaps } => status(&data, json, swaps),
        Command::Events {
            after,
            limit,
            follow,
        } => events(&data, after, limit, follow),
    }
}

fn init(data: &Path) -> anyhow::Result<()> {
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(data)
        .with_context(|| format!("create {}", data.display()))?;
    let (wallet, swaps) = (data.join("wallet.sqlite"), data.join("swaps.sqlite"));
    anyhow::ensure!(
        !swaps.exists(),
        "{} exists already: never replaced",
        swaps.display()
    );
    let mut seed = [0; 32];
    OsRng.fill_bytes(&mut seed);
    let wallet = Wallet::create(&wallet, &seed, Http::new(None)?).context("create the wallet")?;
    let address = hex::encode(wallet.address().to_bytes());
    Provider::create(&swaps, wallet, Config::default()).context("create the swaps file")?;
    tracing::info!(data = %data.display(), %address, "provider created: back up its wallet file");
    println!("{address}");
    Ok(())
}

fn open(data: &Path, transport: Http, config: Config) -> anyhow::Result<Provider<Http>> {
    let wallet = Wallet::open(&data.join("wallet.sqlite"), transport).context("open the wallet")?;
    Provider::open(&data.join("swaps.sqlite"), wallet, config).context("open the swaps file")
}

async fn serve(
    provider: Arc<Provider<Http>>,
    listen: SocketAddr,
    every: Duration,
    notifier: Option<Notifier>,
    announcing: Option<Announcing>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    let config = provider.config();
    tracing::info!(
        listen = %listener.local_addr()?,
        address = %hex::encode(provider.wallet().address().to_bytes()),
        config = %serde_json::to_string(config)?,
        "serving"
    );
    let hubs = provider.wallet().hubs()?;
    if hubs.len() < 2 {
        tracing::warn!(
            hubs = hubs.len(),
            "swapping needs two hubs: add them with licash-wallet"
        );
    }
    let notifying = notifier.clone().map(|notifier| {
        let targets: Vec<_> = notifier.targets().iter().map(Target::to_string).collect();
        tracing::info!(targets = %targets.join(", "), "notifying");
        tokio::spawn(notifier.run())
    });
    let stepping = tokio::spawn(licash_swaps_service::step(provider.clone(), every));
    let nostr = match &announcing {
        Some(_) => Some(licash_nostr::public_key(&provider.nostr_secret()?)?),
        None => None,
    };
    let announcing = announcing.map(|(relays, urls, name)| {
        tracing::info!(relays = %relays.join(", "), urls = %urls.join(", "), "announcing on Nostr");
        tokio::spawn(licash_swaps_service::announce(
            provider.clone(),
            relays,
            urls,
            name,
        ))
    });
    axum::serve(listener, licash_swaps_service::router(provider, nostr))
        .with_graceful_shutdown(shutdown())
        .await?;
    for task in [Some(stepping), announcing].into_iter().flatten() {
        task.abort();
    }
    if let (Some(notifying), Some(notifier)) = (notifying, notifier) {
        // Events not yet sent go out after the next start.
        notifying.abort();
        notifier.stopped().await;
    }
    tracing::info!("stopped");
    Ok(())
}

fn test_notify(notify: &Notify) -> anyhow::Result<()> {
    let targets = notify.targets()?;
    anyhow::ensure!(!targets.is_empty(), "no target: give --notify URL");
    let client = licash_notify::client()?;
    let message = notify::titled(
        &notify.name,
        Message::new(
            Priority::Normal,
            "Test",
            "Notifications of this swap provider reach you here.",
        ),
    );
    let failed = tokio::runtime::Runtime::new()?.block_on(async {
        let mut failed = 0;
        for target in &targets {
            match target.send(&client, &message).await {
                Ok(()) => println!("sent    {target}"),
                Err(e) => {
                    println!("failed  {target}: {e:#}");
                    failed += 1;
                }
            }
        }
        failed
    });
    anyhow::ensure!(failed == 0, "{failed} of {} targets failed", targets.len());
    Ok(())
}

async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
    tracing::info!("stopping: finishing the requests in flight");
}

fn status(data: &Path, json: bool, latest: u32) -> anyhow::Result<()> {
    let provider = open(data, Http::new(None)?, Config::default())?;
    let stats = provider.stats()?;
    let balance = provider.wallet().balance()?;
    let swaps = provider.records(latest)?;
    let address = hex::encode(provider.wallet().address().to_bytes());
    if json {
        let balance: Vec<_> = balance
            .iter()
            .map(|b| serde_json::json!({"hub": b.hub, "available": b.available, "pending": b.pending}))
            .collect();
        let status = serde_json::json!({
            "address": address,
            "stats": stats,
            "balance": balance,
            "swaps": swaps,
        });
        println!("{status}");
        return Ok(());
    }
    println!("address     {address}");
    for (name, value) in [
        ("open", stats.open),
        ("done", stats.done),
        ("refunded", stats.refunded),
        ("failed", stats.failed),
        ("lost", stats.lost),
        ("earned", stats.earned),
        ("lost_amount", stats.lost_amount),
        ("events", stats.events),
    ] {
        println!("{name:<11} {value}");
    }
    for b in &balance {
        println!(
            "hub         {} available {} pending {}",
            b.hub, b.available, b.pending
        );
    }
    for s in &swaps {
        let stage = serde_json::to_value(s.stage)?;
        let stage = stage.as_str().unwrap_or_default();
        let reason = s
            .reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        println!(
            "swap        {} {stage:<8} {} fee {} {} -> {}{reason}",
            &s.id[..16],
            s.amount,
            s.fee,
            s.from,
            s.to
        );
    }
    Ok(())
}

fn events(data: &Path, mut after: u64, limit: u32, follow: bool) -> anyhow::Result<()> {
    let provider = open(data, Http::new(None)?, Config::default())?;
    let mut out = std::io::stdout().lock();
    loop {
        let page = provider.events(after, limit)?;
        for event in &page {
            let printed =
                writeln!(out, "{}", serde_json::to_string(event)?).and_then(|()| out.flush());
            match printed {
                // The reader is gone, as `events --follow | head` leaves it.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                printed => printed?,
            }
            after = event.seq;
        }
        if !follow {
            return Ok(());
        }
        if page.is_empty() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
