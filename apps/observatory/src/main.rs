//! `licash-observatory`: watch the Licash hubs and swap providers of a
//! network, and show what was seen on a web page.

use std::{
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use licash_nostr::Route;
use licash_observatory::{
    observer::{self, Watching},
    probe::Dialer,
    store::Store,
    web::{self, App},
};
use licash_tor::Tor;
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
    /// Watch, and serve the page.
    Run {
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
    },
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
        Command::Run {
            network,
            listen,
            relays,
            hubs,
            providers,
            direct,
            probe_every,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(run(
                cli.data,
                network,
                listen,
                relays,
                hubs,
                providers,
                direct,
                Duration::from_secs(probe_every),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    data: PathBuf,
    network: Network,
    listen: SocketAddr,
    relays: Vec<String>,
    hubs: Vec<String>,
    providers: Vec<String>,
    direct: bool,
    probe_every: Duration,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&data).with_context(|| format!("{}", data.display()))?;
    let store = Arc::new(Store::open(&data.join("observatory.sqlite"))?);
    let (route, dialer) = if direct {
        (Route::Direct, Dialer::direct())
    } else {
        let tor = Tor::new(&data.join("tor"))?;
        let bootstrapping = tor.clone();
        tokio::spawn(async move {
            match bootstrapping.bootstrap().await {
                Ok(()) => tracing::info!("connected to Tor"),
                Err(e) => tracing::warn!("connecting to Tor: {e}"),
            }
        });
        (Route::Tor(tor.clone()), Dialer::Tor(tor))
    };
    let relays = if relays.is_empty() {
        licash_nostr::default_relays(network.name(), !direct)
    } else {
        relays
    };
    let watching = Watching {
        network: network.name().to_owned(),
        relays,
        route,
        dialer,
        hubs,
        providers,
        probe_every,
        discover_every: Duration::from_secs(600),
        timeout: Duration::from_secs(if direct { 20 } else { 90 }),
    };
    let budget = Arc::new(Mutex::new(None));
    let app = Arc::new(App::new(
        store.clone(),
        network.name().to_owned(),
        !direct,
        probe_every * 3,
        budget,
    ));
    let watching = tokio::spawn(observer::run(store, watching));
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening at {listen}"))?;
    tracing::info!(
        network = network.name(),
        "serving the observatory at http://{listen}"
    );
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    watching.abort();
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
    tracing::info!("stopping");
}
