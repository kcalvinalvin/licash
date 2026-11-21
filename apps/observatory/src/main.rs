//! `licash-observatory`: watch the Licash hubs and swap providers of a
//! network, and show what was seen on a web page.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use licash::spend::ChainProfile;
use licash_chain_client::Source;
use licash_nostr::Route;
use licash_observatory::{
    mover::{Mover, Moving},
    observer::{self, Watching},
    probe::Dialer,
    store::Store,
    web::{self, App},
};
use licash_tor::Tor;
use licash_transport::Http;
use licash_wallet::Wallet;
use rand_core::{OsRng, RngCore};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "licash-observatory",
    version,
    about = "Watch the Licash hubs and swap providers of a network, and show them on a web page"
)]
struct Cli {
    /// The observatory's directory: its records `observatory.sqlite`, its
    /// wallet `wallet.sqlite` and Tor's state.
    #[arg(
        long,
        global = true,
        env = "LICASH_OBSERVATORY_DATA",
        default_value = "data"
    )]
    data: PathBuf,
    /// Log lines on stderr as text or JSON, filtered by RUST_LOG.
    #[arg(long, global = true, env = "LICASH_OBSERVATORY_LOG", value_enum, default_value_t = Log::Text)]
    log: Log,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Log {
    Text,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Network {
    Bitcoin,
    Signet,
    Mutinynet,
    Regtest,
}

impl Network {
    fn profile(self) -> ChainProfile {
        match self {
            Network::Bitcoin => ChainProfile::Core30Mainnet,
            Network::Signet => ChainProfile::Core30Signet,
            Network::Mutinynet => ChainProfile::Mutinynet,
            Network::Regtest => ChainProfile::Core30DefaultRegtest,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Network::Bitcoin => "bitcoin",
            Network::Signet => "signet",
            Network::Mutinynet => "mutinynet",
            Network::Regtest => "regtest",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Watch, move within the budget, and serve the page.
    Run(Run),
}

#[derive(Args)]
struct Run {
    /// The network whose services are watched.
    #[arg(long, env = "LICASH_OBSERVATORY_NETWORK", value_enum)]
    network: Network,
    /// Where the page is served.
    #[arg(
        long,
        env = "LICASH_OBSERVATORY_LISTEN",
        default_value = "127.0.0.1:8350"
    )]
    listen: SocketAddr,
    /// Nostr relays to find services on; the usual ones if none.
    /// Repeat it, or separate them with commas.
    #[arg(
        long = "relay",
        env = "LICASH_OBSERVATORY_RELAYS",
        value_delimiter = ','
    )]
    relays: Vec<String>,
    /// A hub to watch besides those announced. Repeat it, or separate
    /// them with commas.
    #[arg(long = "hub", env = "LICASH_OBSERVATORY_HUBS", value_delimiter = ',')]
    hubs: Vec<String>,
    /// A swap provider to watch besides those announced.
    #[arg(
        long = "provider",
        env = "LICASH_OBSERVATORY_PROVIDERS",
        value_delimiter = ','
    )]
    providers: Vec<String>,
    /// Reach services and relays directly rather than over Tor: on
    /// regtest, or behind a VPN of one's own. Onion services are
    /// skipped, and services see the observatory's address.
    #[arg(long, env = "LICASH_OBSERVATORY_DIRECT")]
    direct: bool,
    /// Seconds between two probes of a service.
    #[arg(long, env = "LICASH_OBSERVATORY_PROBE_EVERY", default_value_t = 300,
          value_parser = clap::value_parser!(u64).range(10..))]
    probe_every: u64,
    /// Sat a month the observatory may spend on moving its balance
    /// between services, fees and losses included; 0 moves nothing,
    /// and makes no wallet. Cheap moves are made more often than dear
    /// ones, all as often as this allows.
    #[arg(long, env = "LICASH_OBSERVATORY_BUDGET", default_value_t = 0)]
    budget: u64,
    /// Sat a move moves.
    #[arg(long, env = "LICASH_OBSERVATORY_AMOUNT", default_value_t = 2_000,
          value_parser = clap::value_parser!(u64).range(1..))]
    amount: u64,
    /// The chain to follow payouts on and broadcast their claims to:
    /// Bitcoin Core's JSON-RPC, `http[s]://[user:pass@]host:port`, or
    /// Esplora, `esplora+https://host/api`. With it, a move on Bitcoin, out
    /// of a hub and into one, comes at most once a day; it is read directly,
    /// not over Tor.
    #[arg(long, env = "LICASH_OBSERVATORY_CHAIN")]
    chain: Option<Source>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging(cli.log);
    match command(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn logging(format: Log) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(match format {
            Log::Text => "info,licash_wallet=warn,nostr_sdk=off,arti_client=warn,tor_=warn",
            Log::Json => "info",
        })
    });
    let logs = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    match format {
        Log::Text => logs.without_time().with_target(false).init(),
        Log::Json => logs.json().init(),
    }
}

fn command(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(run) => tokio::runtime::Runtime::new()?.block_on(serve(cli.data, run)),
    }
}

async fn serve(data: PathBuf, run: Run) -> anyhow::Result<()> {
    std::fs::create_dir_all(&data).with_context(|| format!("{}", data.display()))?;
    let store = Arc::new(Store::open(&data.join("observatory.sqlite"))?);
    let network = run.network;
    let tor = match run.direct {
        true => None,
        false => {
            let tor = Tor::new(&data.join("tor"))?;
            let bootstrapping = tor.clone();
            tokio::spawn(async move {
                match bootstrapping.bootstrap().await {
                    Ok(()) => tracing::info!("connected to Tor"),
                    Err(e) => tracing::warn!("connecting to Tor: {e}"),
                }
            });
            Some(tor)
        }
    };
    let (route, dialer) = match &tor {
        None => (Route::Direct, Dialer::direct()),
        Some(tor) => (Route::Tor(tor.clone()), Dialer::Tor(tor.clone())),
    };
    let relays = if run.relays.is_empty() {
        licash_nostr::default_relays(network.name(), !run.direct)
    } else {
        run.relays
    };
    let fresh = run.probe_every * 3;
    let watching = Watching {
        network: network.name().to_owned(),
        relays,
        route,
        dialer,
        hubs: run.hubs,
        providers: run.providers,
        probe_every: Duration::from_secs(run.probe_every),
        discover_every: Duration::from_secs(600),
        timeout: Duration::from_secs(if run.direct { 20 } else { 90 }),
    };
    let budget = Arc::new(Mutex::new(None));
    let app = Arc::new(App::new(
        store.clone(),
        network.name().to_owned(),
        !run.direct,
        Duration::from_secs(fresh),
        budget.clone(),
    ));
    let mut tasks = vec![tokio::spawn(observer::run(store.clone(), watching))];
    if run.budget > 0 {
        let transport = match tor {
            Some(tor) => Http::tor(tor),
            None => Http::new(None)?,
        };
        let wallet = wallet(&data.join("wallet.sqlite"), network, transport)?;
        let moving = Moving {
            store,
            monthly: run.budget,
            amount: run.amount,
            fresh: (fresh * 1000) as i64,
            shown: budget,
            chain: run.chain,
        };
        tasks.push(tokio::spawn(Mover::new(wallet, moving)?.run()));
    }
    let listener = tokio::net::TcpListener::bind(run.listen)
        .await
        .with_context(|| format!("listening at {}", run.listen))?;
    tracing::info!(
        network = network.name(),
        "serving the observatory at http://{}",
        run.listen
    );
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    for task in tasks {
        task.abort();
    }
    tracing::info!("stopped");
    Ok(())
}

/// The observatory's wallet, made with a seed of its own on its first run.
fn wallet(path: &Path, network: Network, transport: Http) -> anyhow::Result<Wallet<Http>> {
    if path.exists() {
        let wallet = Wallet::open(path, transport)?;
        anyhow::ensure!(
            wallet.network() == network.profile(),
            "{} is a wallet for {}, not {}",
            path.display(),
            wallet.network().name(),
            network.name()
        );
        return Ok(wallet);
    }
    let mut seed = [0; 32];
    OsRng.fill_bytes(&mut seed);
    let wallet = Wallet::create(path, &seed, network.profile(), transport)?;
    tracing::info!(
        wallet = %path.display(),
        "a wallet made; fund it at a hub with `licash-wallet` (LICASH_WALLET={})",
        path.display()
    );
    Ok(wallet)
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
    tracing::info!("stopping");
}
