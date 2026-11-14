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
use licash::spend::ChainProfile;
use licash_chain_client::Source;
use licash_hub::{Fee, Hub, Ledger, Policy, Timing};
use licash_hub_service::{
    announce::announce,
    lightning::{lnd::Lnd, Config, Gateway},
    notify::{self, Notifier},
    tweaks::{self, Tweaks},
    watch::watch,
    Services,
};
use licash_nostr::{Announcement, Route};
use licash_notify::{Message, Priority, Target};
use licash_tor::{Onion, Tor};
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
    /// The seed file, the hub's one secret: `<data>/seed` unless given.
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
    /// The network the chain must be: bitcoin, regtest, signet or mutinynet.
    #[arg(long, env = "LICASH_HUB_NETWORK", default_value_t = ChainProfile::Core30Mainnet)]
    network: ChainProfile,
    /// Seconds between rounds of the scan, below `--max-lag`.
    #[arg(long, env = "LICASH_HUB_CHAIN_POLL", default_value_t = 10,
          value_parser = clap::value_parser!(u64).range(1..))]
    chain_poll: u64,
    /// Index the silent payment tweaks of the chain's blocks, in
    /// `<data>/tweaks.sqlite`, and serve them to light wallets.
    #[arg(long, env = "LICASH_HUB_TWEAKS",
          value_parser = clap::builder::BoolishValueParser::new())]
    tweaks: bool,
    /// The first block to index the tweaks of, when the index is new
    /// [default: the tip]. Delete the index to start from another.
    #[arg(
        long,
        env = "LICASH_HUB_TWEAKS_FROM",
        value_name = "HEIGHT",
        requires = "tweaks"
    )]
    tweaks_from: Option<u32>,
}

impl Chain {
    fn source(&self, policy: &Policy) -> anyhow::Result<Option<(Source, ChainProfile, Duration)>> {
        let Some(url) = &self.url else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.chain_poll < policy.timing.max_lag,
            "--chain-poll must be below --max-lag"
        );
        let source = url.parse().context("--chain")?;
        let every = Duration::from_secs(self.chain_poll);
        Ok(Some((source, self.network, every)))
    }
}

/// The hub's announcement on Nostr, for wallets to discover it.
#[derive(Args)]
struct AnnounceArgs {
    /// Announce the hub on this Nostr relay, such as `wss://relay.example`.
    /// Repeat it, or separate relays with commas, for more. Needs
    /// `--announce-url` and `--chain`, whose network it announces.
    #[arg(
        long = "nostr-relay",
        value_name = "URL",
        env = "LICASH_HUB_NOSTR_RELAYS",
        value_delimiter = ','
    )]
    nostr_relays: Vec<String>,
    /// A URL wallets reach the hub at: `https://`, or an onion service's
    /// `http://`. Repeat it, up to 4 with `--onion`'s.
    #[arg(
        long = "announce-url",
        value_name = "URL",
        env = "LICASH_HUB_ANNOUNCE_URLS",
        value_delimiter = ','
    )]
    announce_urls: Vec<String>,
    /// The name the announcement shows.
    #[arg(long = "announce-name", env = "LICASH_HUB_ANNOUNCE_NAME")]
    announce_name: Option<String>,
}

impl AnnounceArgs {
    /// The relays and the announcement's URLs and name, if it announces:
    /// with `onion`, the onion service's URL is added once it runs.
    fn announcing(self, onion: bool) -> anyhow::Result<Option<Announcing>> {
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
        // `--onion`'s URL joins them once its service runs.
        if !(onion && urls.is_empty()) {
            check(&urls).context("--announce-url")?;
        }
        Ok(Some(Announcing {
            relays,
            route: Route::Direct,
            urls,
            name: self.announce_name,
        }))
    }
}

/// Whether wallets can use the announced `urls`.
fn check(urls: &[String]) -> anyhow::Result<()> {
    let announcement = Announcement {
        urls: urls.to_vec(),
        network: String::new(),
        name: None,
    };
    announcement.check()
}

/// Announce through `tor`, and at `onion`'s URL too, if the service
/// announces.
fn announce_over(
    announcing: &mut Option<Announcing>,
    tor: &Tor,
    onion: Option<&Onion>,
) -> anyhow::Result<()> {
    let Some(announcing) = announcing else {
        return Ok(());
    };
    announcing.route = Route::Tor(tor.clone());
    if let Some(onion) = onion {
        announcing.urls.push(onion.url().to_owned());
        check(&announcing.urls).context("--onion")?;
    }
    Ok(())
}

/// The hub's announcement: the relays, how it reaches them, and the URLs
/// and name it announces.
struct Announcing {
    relays: Vec<String>,
    route: Route,
    urls: Vec<String>,
    name: Option<String>,
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

/// The hub's Lightning gateway, over an LND node.
#[derive(Args)]
struct LightningArgs {
    /// Run a Lightning gateway over the LND REST API at this URL, such as
    /// `https://127.0.0.1:8080`.
    #[arg(long, value_name = "URL", env = "LICASH_HUB_LND")]
    lnd: Option<String>,
    /// The macaroon the gateway calls LND with: it pays, and makes, settles
    /// and cancels invoices.
    #[arg(
        long,
        value_name = "FILE",
        env = "LICASH_HUB_LND_MACAROON",
        requires = "lnd"
    )]
    lnd_macaroon: Option<PathBuf>,
    /// LND's TLS certificate, pinned. Without it, the URL's certificate
    /// must validate against the system's roots.
    #[arg(
        long,
        value_name = "FILE",
        env = "LICASH_HUB_LND_CERT",
        requires = "lnd"
    )]
    lnd_cert: Option<PathBuf>,
    /// The fee on each invoice paid, routing fees included, in satoshis.
    #[arg(long, env = "LICASH_HUB_LN_PAY_FEE_BASE", default_value_t = Config::default().pay.base)]
    ln_pay_fee_base: u64,
    /// The fee on each invoice paid, in parts per million of its amount.
    #[arg(long, env = "LICASH_HUB_LN_PAY_FEE_PPM", default_value_t = Config::default().pay.ppm,
          value_parser = ppm())]
    ln_pay_fee_ppm: u64,
    /// The fee on each payment received, in satoshis.
    #[arg(long, env = "LICASH_HUB_LN_RECEIVE_FEE_BASE", default_value_t = 0)]
    ln_receive_fee_base: u64,
    /// The fee on each payment received, in parts per million.
    #[arg(long, env = "LICASH_HUB_LN_RECEIVE_FEE_PPM", default_value_t = 0, value_parser = ppm())]
    ln_receive_fee_ppm: u64,
    /// The largest invoice paid, in satoshis.
    #[arg(long, env = "LICASH_HUB_LN_MAX_PAY", default_value_t = Config::default().max_pay)]
    ln_max_pay: u64,
    /// The least received, in satoshis.
    #[arg(long, env = "LICASH_HUB_LN_MIN_RECEIVE", default_value_t = Config::default().min_receive)]
    ln_min_receive: u64,
    /// The most received, in satoshis.
    #[arg(long, env = "LICASH_HUB_LN_MAX_RECEIVE", default_value_t = Config::default().max_receive)]
    ln_max_receive: u64,
    /// Blocks between the end of a payment's route timelock and the
    /// expiry of the lock paying for it.
    #[arg(long, env = "LICASH_HUB_LN_PAY_SLACK", default_value_t = Config::default().pay_slack)]
    ln_pay_slack: u32,
    /// LND's `holdexpirydelta`: blocks before an HTLC's expiry at which
    /// LND cancels it while held.
    #[arg(long, env = "LICASH_HUB_LN_HOLD_DELTA", default_value_t = Config::default().hold_delta)]
    ln_hold_delta: u32,
    /// Blocks the gateway may be down while a payment received is locked.
    #[arg(long, env = "LICASH_HUB_LN_DOWNTIME", default_value_t = Config::default().downtime)]
    ln_downtime: u32,
    /// Blocks a payment received stays locked to its payee.
    #[arg(long, env = "LICASH_HUB_LN_RECEIVE_BLOCKS", default_value_t = Config::default().receive_blocks)]
    ln_receive_blocks: u32,
    /// Seconds an invoice to receive stays payable.
    #[arg(long, env = "LICASH_HUB_LN_INVOICE_EXPIRY", default_value_t = Config::default().invoice_expiry)]
    ln_invoice_expiry: u64,
    /// Seconds LND tries to route a payment.
    #[arg(long, env = "LICASH_HUB_LN_PAY_TIMEOUT", default_value_t = Config::default().pay_timeout)]
    ln_pay_timeout: u32,
}

impl LightningArgs {
    /// The node and the gateway's configuration, if it runs one.
    fn gateway(&self) -> anyhow::Result<Option<(Lnd, Config)>> {
        let Some(url) = &self.lnd else {
            return Ok(None);
        };
        let macaroon = self
            .lnd_macaroon
            .as_ref()
            .context("--lnd needs --lnd-macaroon")?;
        let macaroon =
            std::fs::read(macaroon).with_context(|| format!("read {}", macaroon.display()))?;
        let cert = match &self.lnd_cert {
            Some(path) => {
                Some(std::fs::read(path).with_context(|| format!("read {}", path.display()))?)
            }
            None => None,
        };
        let lnd = Lnd::new(url, &macaroon, cert.as_deref()).context("--lnd")?;
        let fee = |base, ppm| Fee { base, ppm };
        let config = Config {
            pay: fee(self.ln_pay_fee_base, self.ln_pay_fee_ppm),
            receive: fee(self.ln_receive_fee_base, self.ln_receive_fee_ppm),
            max_pay: self.ln_max_pay,
            min_receive: self.ln_min_receive,
            max_receive: self.ln_max_receive,
            pay_slack: self.ln_pay_slack,
            hold_delta: self.ln_hold_delta,
            downtime: self.ln_downtime,
            receive_blocks: self.ln_receive_blocks,
            invoice_expiry: self.ln_invoice_expiry,
            pay_timeout: self.ln_pay_timeout,
            ..Config::default()
        };
        Ok(Some((lnd, config)))
    }
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
        chain: Box<Chain>,
        #[command(flatten)]
        notify: Notify,
        #[command(flatten)]
        lightning: Box<LightningArgs>,
        #[command(flatten)]
        announce: Box<AnnounceArgs>,
        /// Also serve the API as an onion service of a Tor client of the
        /// hub's own, at the same address on every start (its key in
        /// `<data>/tor`), and announce it, reaching relays through Tor too.
        #[arg(long, env = "LICASH_HUB_ONION",
              value_parser = clap::builder::BoolishValueParser::new())]
        onion: bool,
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
            lightning,
            announce,
            onion,
            notify_kinds,
        } => {
            let policy = policy.policy();
            let (indexing, tweaks_from) = (chain.tweaks, chain.tweaks_from);
            let chain = chain.source(&policy)?;
            anyhow::ensure!(
                !indexing || chain.is_some(),
                "indexing tweaks (--tweaks) needs --chain"
            );
            let lightning = lightning.gateway()?;
            anyhow::ensure!(
                lightning.is_none() || chain.is_some(),
                "a Lightning gateway (--lnd) needs --chain"
            );
            let mut announcing = (*announce).announcing(onion)?;
            anyhow::ensure!(
                announcing.is_none() || chain.is_some(),
                "announcing on Nostr (--nostr-relay) needs --chain"
            );
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
            let gateway = match lightning {
                Some((lnd, config)) => {
                    let path = cli.data.join("lightning.sqlite");
                    let gateway = Gateway::open(hub.clone(), Arc::new(lnd), config, &path)
                        .context("open the Lightning gateway")?;
                    Some(Arc::new(gateway))
                }
                None => None,
            };
            let tweaks = match indexing {
                true => {
                    let path = cli.data.join("tweaks.sqlite");
                    let index = Tweaks::open(&path).context("open the tweak index")?;
                    Some(Arc::new(index))
                }
                false => None,
            };
            let services = Services {
                gateway: gateway.clone(),
                tweaks: tweaks.clone(),
            };
            let router = licash_hub_service::router(hub.clone(), services);
            let runtime = tokio::runtime::Runtime::new()?;
            let _entered = runtime.enter();
            // Served until the hub stops.
            let _onion = match onion {
                true => {
                    let tor = Tor::new(&cli.data.join("tor")).context("a Tor client")?;
                    let onion = tor.serve("licash-hub", router.clone())?;
                    tracing::info!(url = onion.url(), "serving as an onion service");
                    announce_over(&mut announcing, &tor, Some(&onion))?;
                    Some(onion)
                }
                false => None,
            };
            let tasks = Tasks {
                chain,
                notifier,
                gateway,
                announcing,
                tweaks: tweaks.map(|index| (index, tweaks_from)),
            };
            let serving = serve(hub, router, listen, policy, tasks);
            runtime.block_on(serving)
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

/// What runs beside the API, if configured.
struct Tasks {
    chain: Option<(Source, ChainProfile, Duration)>,
    notifier: Option<Notifier>,
    gateway: Option<Arc<Gateway>>,
    announcing: Option<Announcing>,
    /// With the first block to index if the index is new.
    tweaks: Option<(Arc<Tweaks>, Option<u32>)>,
}

async fn serve(
    hub: Arc<Hub>,
    router: axum::Router,
    listen: SocketAddr,
    policy: Policy,
    tasks: Tasks,
) -> anyhow::Result<()> {
    let Tasks {
        chain,
        notifier,
        gateway,
        announcing,
        tweaks,
    } = tasks;
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
    let indexing = match (&chain, tweaks) {
        (Some((source, network, every)), Some((index, from))) => {
            let indexing = tweaks::index(index, source.clone(), *network, from, *every);
            Some(tokio::spawn(indexing))
        }
        _ => None,
    };
    let watching = chain
        .map(|(source, network, every)| tokio::spawn(watch(hub.clone(), source, network, every)));
    if watching.is_none() {
        tracing::warn!("following no chain (--chain): locks are refused");
    }
    let paying = gateway.clone().map(|gateway| {
        tracing::info!("running a Lightning gateway");
        tokio::spawn(gateway.run())
    });
    let announcing = announcing.map(|a| {
        let (relays, urls) = (a.relays.join(", "), a.urls.join(", "));
        tracing::info!(%relays, %urls, "announcing on Nostr");
        tokio::spawn(announce(hub.clone(), a.relays, a.route, a.urls, a.name))
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    for task in [watching, indexing, paying, announcing]
        .into_iter()
        .flatten()
    {
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
