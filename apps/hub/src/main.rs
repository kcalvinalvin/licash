//! `licash-hub`: create, run and watch a Licash hub.

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
use licash_chain_client::Source;
use licash_hub::{Fee, Hub, Ledger, Policy, Timing};
use licash_hub_service::{
    notify::{self, Notifier},
    watch::watch,
};
use licash_notify::{Message, Priority, Target};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "licash-hub",
    version,
    about = "Create, run and watch a Licash hub"
)]
struct Cli {
    /// The hub's directory: its database `hub.sqlite`, and its seed unless
    /// `--seed` says otherwise.
    #[arg(long, global = true, env = "LICASH_HUB_DATA", default_value = "data")]
    data: PathBuf,
    /// The seed file, the hub's one secret [default: <data>/seed].
    #[arg(long, global = true, env = "LICASH_HUB_SEED")]
    seed: Option<PathBuf>,
    /// Log lines on stderr as text or JSON, filtered by RUST_LOG.
    #[arg(long, global = true, env = "LICASH_HUB_LOG", value_enum, default_value_t = Log::Text)]
    log: Log,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Notify {
    /// Notify of the hub's events at this Apprise-style URL: gotify://,
    /// tgram://, ntfy://, json:// or apprise:// (see the README). Repeat it,
    /// or separate URLs with commas, for more targets.
    #[arg(
        long = "notify",
        value_name = "URL",
        env = "LICASH_HUB_NOTIFY",
        value_delimiter = ',',
        hide_env_values = true
    )]
    urls: Vec<String>,
    /// The name heading notification titles, to tell hubs apart.
    #[arg(long, env = "LICASH_HUB_NAME", default_value = "licash-hub")]
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

/// The chain the hub follows, for locks.
#[derive(Args)]
struct Chain {
    /// Follow Bitcoin at this URL: a node's JSON-RPC,
    /// `http[s]://[user:pass@]host:port[?cookie=PATH]`, or an Esplora API,
    /// `esplora+http[s]://host[:port][/path]`. Without it, locks are refused.
    #[arg(
        long = "chain",
        value_name = "URL",
        env = "LICASH_HUB_CHAIN",
        hide_env_values = true
    )]
    url: Option<String>,
    /// Seconds between rounds of the scan, below `--max-lag`.
    #[arg(long, env = "LICASH_HUB_CHAIN_POLL", default_value_t = 10,
          value_parser = clap::value_parser!(u64).range(1..))]
    chain_poll: u64,
}

impl Chain {
    fn source(&self, policy: &Policy) -> anyhow::Result<Option<(Source, Duration)>> {
        let Some(url) = &self.url else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.chain_poll < policy.timing.max_lag,
            "--chain-poll must be below --max-lag"
        );
        let source = url.parse().context("--chain")?;
        Ok(Some((source, Duration::from_secs(self.chain_poll))))
    }
}

/// What the hub charges and the locks it admits.
#[derive(Args)]
struct PolicyArgs {
    /// The fee on each spend, in the smallest unit.
    #[arg(long, env = "LICASH_HUB_SPEND_FEE_BASE", default_value_t = 0)]
    spend_fee_base: u64,
    /// The fee on each spend, in parts per million of its inputs.
    #[arg(long, env = "LICASH_HUB_SPEND_FEE_PPM", default_value_t = 0, value_parser = ppm())]
    spend_fee_ppm: u64,
    /// The fee in a lock's pay branch, in the smallest unit.
    #[arg(long, env = "LICASH_HUB_LOCK_FEE_BASE", default_value_t = 0)]
    lock_fee_base: u64,
    /// The fee in a lock's pay branch, in parts per million of its inputs.
    #[arg(long, env = "LICASH_HUB_LOCK_FEE_PPM", default_value_t = 0, value_parser = ppm())]
    lock_fee_ppm: u64,
    /// The fee in a lock's refund branch, in the smallest unit.
    #[arg(long, env = "LICASH_HUB_REFUND_FEE_BASE", default_value_t = 0)]
    refund_fee_base: u64,
    /// The fee in a lock's refund branch, in parts per million of its inputs.
    #[arg(long, env = "LICASH_HUB_REFUND_FEE_PPM", default_value_t = 0, value_parser = ppm())]
    refund_fee_ppm: u64,
    /// A lock's expiry, depth and reaction fit within this many blocks of
    /// the scanned tip: the response window of the hub's contracts.
    #[arg(long, env = "LICASH_HUB_LOCK_WINDOW", default_value_t = Timing::default().window)]
    lock_window: u32,
    /// Blocks the hub needs to produce and confirm a response.
    #[arg(long, env = "LICASH_HUB_LOCK_REACTION", default_value_t = Timing::default().reaction)]
    lock_reaction: u32,
    /// Least activation depth of a lock. Keep it above 1 on mainnet.
    #[arg(long, env = "LICASH_HUB_LOCK_MIN_DEPTH", default_value_t = Timing::default().min_depth)]
    lock_min_depth: u32,
    /// Blocks from its checkpoint within which a lock is decided.
    #[arg(long, env = "LICASH_HUB_LOCK_MAX_HEADERS", default_value_t = Timing::default().max_headers)]
    lock_max_headers: u32,
    /// Confirmations of a block before it becomes a checkpoint.
    #[arg(long, env = "LICASH_HUB_CHECKPOINT_BURY", default_value_t = Timing::default().bury)]
    checkpoint_bury: u32,
    /// Seconds after the scan last reached the tip that the hub stops
    /// admitting locks.
    #[arg(long, env = "LICASH_HUB_MAX_LAG", default_value_t = Timing::default().max_lag)]
    max_lag: u64,
}

fn ppm() -> clap::builder::RangedU64ValueParser<u64> {
    clap::value_parser!(u64).range(0..=1_000_000)
}

impl PolicyArgs {
    fn policy(&self) -> Policy {
        let fee = |base, ppm| Fee { base, ppm };
        Policy {
            spend: fee(self.spend_fee_base, self.spend_fee_ppm),
            lock: fee(self.lock_fee_base, self.lock_fee_ppm),
            refund: fee(self.refund_fee_base, self.refund_fee_ppm),
            timing: Timing {
                window: self.lock_window,
                reaction: self.lock_reaction,
                min_depth: self.lock_min_depth,
                max_headers: self.lock_max_headers,
                bury: self.checkpoint_bury,
                max_lag: self.max_lag,
            },
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Log {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Create the hub: a fresh seed and an empty ledger. Never replaces
    /// either.
    Init {
        /// The largest denomination, as a power of two.
        #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u32).range(0..=licash_hub::MAX_POW as i64))]
        max_pow: u32,
    },
    /// Serve the hub's HTTP API until SIGINT or SIGTERM.
    Run {
        #[arg(long, env = "LICASH_HUB_LISTEN", default_value = "127.0.0.1:8338")]
        listen: SocketAddr,
        #[command(flatten)]
        policy: PolicyArgs,
        #[command(flatten)]
        chain: Chain,
        #[command(flatten)]
        notify: Notify,
        /// Notify only of these kinds of event [default: all].
        #[arg(long, value_name = "KIND", env = "LICASH_HUB_NOTIFY_KINDS", value_delimiter = ',',
              value_parser = clap::builder::PossibleValuesParser::new(notify::KINDS))]
        notify_kinds: Vec<String>,
    },
    /// Send a test notification to each `--notify` target. Fails if any
    /// target does not take it.
    TestNotify {
        #[command(flatten)]
        notify: Notify,
    },
    /// Counts over the ledger, beside the running hub or without it.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The hub's events as JSON lines, oldest first.
    Events {
        /// Only events after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// At most this many events, or per poll with `--follow`.
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
    let db = cli.data.join("hub.sqlite");
    let seed = cli.seed.unwrap_or_else(|| cli.data.join("seed"));
    match cli.command {
        Command::Init { max_pow } => init(&cli.data, &seed, &db, max_pow),
        Command::Run {
            listen,
            policy,
            chain,
            notify,
            notify_kinds,
        } => {
            let policy = policy.policy();
            let chain = chain.source(&policy)?;
            let targets = notify.targets()?;
            // Before the hub opens, so that new targets hear of its start.
            let notifier = if targets.is_empty() {
                None
            } else {
                let cursors = cli.data.join("notify.json");
                let notifier = Notifier::new(&db, &notify.name, targets, &cursors, notify_kinds)
                    .context("set up notifications")?;
                Some(notifier)
            };
            let hub = Hub::open(&seed, &db, policy).context("open the hub")?;
            let hub = Arc::new(hub);
            tokio::runtime::Runtime::new()?.block_on(serve(hub, listen, policy, chain, notifier))
        }
        Command::TestNotify { notify } => test_notify(&notify),
        Command::Status { json } => status(&db, json),
        Command::Events {
            after,
            limit,
            follow,
        } => events(&db, after, limit, follow),
    }
}

fn init(data: &Path, seed: &Path, db: &Path, max_pow: u32) -> anyhow::Result<()> {
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(data)
        .with_context(|| format!("create {}", data.display()))?;
    Hub::init(seed, db, max_pow).context("create the hub")?;
    tracing::info!(db = %db.display(), seed = %seed.display(), max_pow, "hub created: keep a copy of the seed");
    Ok(())
}

async fn serve(
    hub: Arc<Hub>,
    listen: SocketAddr,
    policy: Policy,
    chain: Option<(Source, Duration)>,
    notifier: Option<Notifier>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    tracing::info!(
        listen = %listener.local_addr()?,
        policy = %serde_json::to_string(&policy)?,
        "serving"
    );
    let notifying = notifier.clone().map(|notifier| {
        let targets: Vec<_> = notifier.targets().iter().map(Target::to_string).collect();
        tracing::info!(targets = %targets.join(", "), "notifying");
        tokio::spawn(notifier.run(hub.clone()))
    });
    let watching = chain.map(|(source, every)| tokio::spawn(watch(hub.clone(), source, every)));
    if watching.is_none() {
        tracing::warn!("following no chain (--chain): locks are refused");
    }
    axum::serve(listener, licash_hub_service::router(hub))
        .with_graceful_shutdown(shutdown())
        .await?;
    if let Some(watching) = watching {
        watching.abort();
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
            "Notifications of this hub reach you here.",
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

fn status(db: &Path, json: bool) -> anyhow::Result<()> {
    let stats = Ledger::open(db).context("read the hub")?.stats()?;
    if json {
        println!("{}", serde_json::to_string(&stats)?);
        return Ok(());
    }
    for (name, value) in [
        ("offers", stats.offers),
        ("released", stats.released),
        ("locks", stats.locks),
        ("open_locks", stats.open_locks),
        ("spent", stats.spent),
        ("issued", stats.issued),
        ("unlinked", stats.unlinked),
        ("income", stats.income),
        ("events", stats.events),
    ] {
        println!("{name:<10} {value}");
    }
    Ok(())
}

fn events(db: &Path, mut after: u64, limit: u32, follow: bool) -> anyhow::Result<()> {
    let ledger = Ledger::open(db).context("read the hub")?;
    let mut out = std::io::stdout().lock();
    loop {
        let page = ledger.events(after, limit)?;
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
