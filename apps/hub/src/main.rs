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
use clap::{Parser, Subcommand, ValueEnum};
use licash_hub::{Fee, Hub, Ledger, Policy};
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
        /// The fee on each spend, in the smallest unit.
        #[arg(long, env = "LICASH_HUB_SPEND_FEE_BASE", default_value_t = 0)]
        spend_fee_base: u64,
        /// The fee on each spend, in parts per million of its inputs.
        #[arg(long, env = "LICASH_HUB_SPEND_FEE_PPM", default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=1_000_000))]
        spend_fee_ppm: u64,
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
            spend_fee_base,
            spend_fee_ppm,
        } => {
            let policy = Policy {
                spend: Fee {
                    base: spend_fee_base,
                    ppm: spend_fee_ppm,
                },
            };
            let hub = Hub::open(&seed, &db, policy).context("open the hub")?;
            tokio::runtime::Runtime::new()?.block_on(serve(Arc::new(hub), listen, policy))
        }
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

async fn serve(hub: Arc<Hub>, listen: SocketAddr, policy: Policy) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    tracing::info!(
        listen = %listener.local_addr()?,
        spend_fee_base = policy.spend.base,
        spend_fee_ppm = policy.spend.ppm,
        "serving"
    );
    axum::serve(listener, licash_hub_service::router(hub))
        .with_graceful_shutdown(shutdown())
        .await?;
    tracing::info!("stopped");
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
        ("spent", stats.spent),
        ("issued", stats.issued),
        ("unlinked", stats.unlinked),
        ("income", stats.income),
        ("events", stats.events),
    ] {
        println!("{name:<9} {value}");
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
