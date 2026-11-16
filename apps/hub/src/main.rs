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
use licash_contracts::hub::Openings;
use licash_hub::{Fee, Hub, Ledger, Policy, Timing};
use licash_hub_service::{
    announce::announce,
    dashboard, deposit,
    lightning::{lnd::Lnd, Config, Gateway},
    notify::{self, Notifier},
    peer,
    tweaks::{self, Tweaks},
    watch::watch,
    Services,
};
use licash_nostr::{Announcement, Route};
use licash_notify::{Message, Priority, Target};
use licash_swaps::{Config as SwapsConfig, Issuer, Provider};
use licash_swaps_service::notify::Notifier as SwapsNotifier;
use licash_tor::{Onion, Tor};
use licash_transport::Http;
use licash_wallet::Wallet;
use rand_core::{OsRng, RngCore};
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
    /// Seconds between rounds of the scan and of deposits, below `--max-lag`.
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
    /// Open recovery contracts with wallets, kept in
    /// `<data>/openings.sqlite`, F funded and signed by the wallet of the
    /// node `--contracts-wallet` names, else `--chain`. On regtest under
    /// the mock authority, a contract there protecting nothing; on signet
    /// under the genuine programs; none elsewhere yet.
    #[arg(long, env = "LICASH_HUB_CONTRACTS",
          value_parser = clap::builder::BoolishValueParser::new())]
    contracts: bool,
    /// Automatically answer confirmed holder requests. Pins one request
    /// witness per opening in responses.sqlite; never runs hub timeouts.
    /// Before enabling on an older hub, reconcile any manual responses.
    #[arg(long, env = "LICASH_HUB_CONTRACTS_RESPOND", requires = "contracts",
          value_parser = clap::builder::BoolishValueParser::new())]
    contracts_respond: bool,
    /// Fund and sign contracts with the wallet of the node at this URL, its
    /// JSON-RPC as `--chain` takes it, such as
    /// `http://host:port/wallet/NAME?cookie=PATH`: one apart from the chain
    /// followed, which may be an Esplora API. By default, `--chain`'s.
    #[arg(
        long = "contracts-wallet",
        value_name = "URL",
        env = "LICASH_HUB_CONTRACTS_WALLET",
        hide_env_values = true,
        requires = "contracts"
    )]
    contracts_wallet: Option<String>,
    /// Let holders choose a contract's cap up to this many sat, and how
    /// long it lasts up to `--contracts-max-blocks`, besides the fixed
    /// terms (4 096 sat over 1 000 blocks) apps without that choice open
    /// on. By default, the fixed terms only.
    #[arg(
        long = "contracts-max-cap",
        value_name = "SAT",
        env = "LICASH_HUB_CONTRACTS_MAX_CAP",
        requires = "contracts"
    )]
    contracts_max_cap: Option<u32>,
    /// The most blocks a contract a holder chooses lasts, with
    /// `--contracts-max-cap`.
    #[arg(
        long = "contracts-max-blocks",
        value_name = "BLOCKS",
        env = "LICASH_HUB_CONTRACTS_MAX_BLOCKS",
        default_value_t = 1_000,
        requires = "contracts_max_cap"
    )]
    contracts_max_blocks: u32,
    /// A contract's cap at most this many times the coin its holder
    /// deposits, with `--contracts-max-cap`: no small coin locks much of
    /// the hub's.
    #[arg(
        long = "contracts-per-coin",
        value_name = "TIMES",
        env = "LICASH_HUB_CONTRACTS_PER_COIN",
        default_value_t = 2,
        requires = "contracts_max_cap"
    )]
    contracts_per_coin: u32,
    /// What a contract a holder chooses costs over the fixed price, in
    /// millionths of its cap for each year (52 560 blocks) it lasts, with
    /// `--contracts-max-cap`: 10 000 is 1% a year.
    #[arg(
        long = "contracts-rate",
        value_name = "PPM",
        env = "LICASH_HUB_CONTRACTS_RATE",
        default_value_t = 10_000,
        requires = "contracts_max_cap"
    )]
    contracts_rate: u32,
}

impl Chain {
    /// The contract terms holders may choose, if the operator lets them.
    fn range(&self) -> Option<licash_contracts::hub::terms::Range> {
        self.contracts_max_cap
            .map(|max_cap| licash_contracts::hub::terms::Range {
                max_cap,
                max_blocks: self.contracts_max_blocks,
                per_coin: self.contracts_per_coin,
                ppm_year: self.contracts_rate,
            })
    }

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
    /// A Nostr relay of its wallets' mailboxes, named in the hub's info:
    /// where payers send its notes to their payees. Repeat it, or separate
    /// relays with commas, for more; the `--nostr-relay` relays by default.
    #[arg(
        long = "mailbox-relay",
        value_name = "URL",
        env = "LICASH_HUB_MAILBOX_RELAYS",
        value_delimiter = ',',
        value_parser = relay
    )]
    mailbox_relays: Vec<String>,
}

/// A relay's URL, `ws://` or `wss://`.
fn relay(url: &str) -> Result<String, String> {
    let host = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"));
    match host {
        Some(host) if !host.is_empty() => Ok(url.to_owned()),
        _ => Err("a relay is at a ws:// or wss:// URL".into()),
    }
}

impl AnnounceArgs {
    /// The relays of its wallets' mailboxes.
    fn mailboxes(&self) -> Vec<String> {
        let relays = match self.mailbox_relays.is_empty() {
            true => &self.nostr_relays,
            false => &self.mailbox_relays,
        };
        relays.iter().filter(|r| !r.is_empty()).cloned().collect()
    }

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
#[derive(Clone)]
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

/// The hub's deposits on Bitcoin.
#[derive(Args)]
struct DepositArgs {
    /// Take deposits on Bitcoin, claiming their coins to this address:
    /// a payment confirmed locks the hub's own notes to its payer.
    #[arg(long, value_name = "ADDRESS", env = "LICASH_HUB_DEPOSIT_SWEEP_ADDRESS")]
    deposit_sweep_address: Option<String>,
    /// The fee on each deposit, in satoshis, paid on top of its notes.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_FEE_BASE", default_value_t = deposits().fee.base)]
    deposit_fee_base: u64,
    /// The fee on each deposit, in parts per million of its notes.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_FEE_PPM", default_value_t = deposits().fee.ppm,
          value_parser = ppm())]
    deposit_fee_ppm: u64,
    /// The least notes a deposit locks, in satoshis.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_MIN", default_value_t = deposits().min)]
    deposit_min: u64,
    /// The most notes a deposit locks, in satoshis.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_MAX", default_value_t = deposits().max)]
    deposit_max: u64,
    /// Confirmations a deposit's payment needs.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_DEPTH", default_value_t = deposits().depth)]
    deposit_depth: u32,
    /// Blocks from a request within which its payment must confirm.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_WINDOW", default_value_t = deposits().window)]
    deposit_window: u32,
    /// Blocks a deposit's notes stay locked to its payer.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_LOCK_BLOCKS", default_value_t = deposits().lock_blocks)]
    deposit_lock_blocks: u32,
    /// Blocks the hub may be down while a deposit is locked.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_DOWNTIME", default_value_t = deposits().downtime)]
    deposit_downtime: u32,
    /// Blocks the hub's claim of a deposit has to confirm in before its
    /// payer may take the coins back.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_SWEEP_BLOCKS", default_value_t = deposits().sweep_blocks)]
    deposit_sweep_blocks: u32,
    /// The least fee rate of claims, in sat/vB, also used while the chain
    /// estimates none.
    #[arg(long, env = "LICASH_HUB_DEPOSIT_MIN_FEE_RATE",
          default_value_t = deposits().min_fee_rate.to_sat_per_vb_ceil(),
          value_parser = clap::value_parser!(u64).range(1..=10_000))]
    deposit_min_fee_rate: u64,
}

/// The defaults of deposits.
fn deposits() -> deposit::Config {
    deposit::Config::new(bitcoin::ScriptBuf::new())
}

impl DepositArgs {
    /// The gateway's configuration, if the hub takes deposits on `profile`.
    fn config(&self, profile: ChainProfile) -> anyhow::Result<Option<deposit::Config>> {
        let Some(address) = &self.deposit_sweep_address else {
            return Ok(None);
        };
        let network = licash_chain_client::network(profile);
        let address = address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .context("--deposit-sweep-address")?
            .require_network(network)
            .with_context(|| format!("--deposit-sweep-address: not an address on {profile}"))?;
        let min_fee_rate = bitcoin::FeeRate::from_sat_per_vb(self.deposit_min_fee_rate)
            .context("--deposit-min-fee-rate: too high")?;
        Ok(Some(deposit::Config {
            fee: Fee {
                base: self.deposit_fee_base,
                ppm: self.deposit_fee_ppm,
            },
            min: self.deposit_min,
            max: self.deposit_max,
            depth: self.deposit_depth,
            window: self.deposit_window,
            lock_blocks: self.deposit_lock_blocks,
            downtime: self.deposit_downtime,
            sweep_blocks: self.deposit_sweep_blocks,
            min_fee_rate,
            ..deposit::Config::new(address.script_pubkey())
        }))
    }
}

fn ppm() -> clap::builder::RangedU64ValueParser<u64> {
    clap::value_parser!(u64).range(0..=1_000_000)
}

/// The swaps into the hub, which it provides, locking notes it issues.
#[derive(Args)]
struct SwapsArgs {
    /// Provide the swaps into the hub from the hubs of `--swaps-from`: the
    /// hub locks notes it issues to the client, against the client's lock
    /// there. The provider's wallet, which claims those locks, and its
    /// swaps are kept in `<data>/swaps`, made on the first start. Needs
    /// `--chain`.
    #[arg(long, env = "LICASH_HUB_SWAPS",
          value_parser = clap::builder::BoolishValueParser::new())]
    swaps: bool,
    /// Take swaps from the hub at URL while the provider holds, or is owed
    /// by the swaps under way, at most CAP of its notes: `URL=CAP`. Repeat
    /// it, or separate them with commas, for more hubs.
    #[arg(
        long = "swaps-from",
        value_name = "URL=CAP",
        env = "LICASH_HUB_SWAPS_FROM",
        value_delimiter = ','
    )]
    swaps_from: Vec<String>,
    /// Reach the hubs of `--swaps-from`, onion services among them, through
    /// a Tor client of the hub's own (that of `--onion`, its state in
    /// `<data>/tor`) rather than directly.
    #[arg(long, env = "LICASH_HUB_SWAPS_TOR",
          value_parser = clap::builder::BoolishValueParser::new())]
    swaps_tor: bool,
    /// The fee on each swap, in the smallest unit.
    #[arg(long, env = "LICASH_HUB_SWAPS_FEE_BASE",
          default_value_t = SwapsConfig::default().fee.base)]
    swaps_fee_base: u64,
    /// The fee on each swap, in parts per million of its amount.
    #[arg(long, env = "LICASH_HUB_SWAPS_FEE_PPM",
          default_value_t = SwapsConfig::default().fee.ppm, value_parser = ppm())]
    swaps_fee_ppm: u64,
    /// The least swapped at once.
    #[arg(long, env = "LICASH_HUB_SWAPS_MIN", default_value_t = SwapsConfig::default().min)]
    swaps_min: u64,
    /// The most swapped at once.
    #[arg(long, env = "LICASH_HUB_SWAPS_MAX", default_value_t = SwapsConfig::default().max)]
    swaps_max: u64,
    /// Blocks the provider needs to claim the client's lock once it holds
    /// the preimage.
    #[arg(long, env = "LICASH_HUB_SWAPS_MARGIN",
          default_value_t = SwapsConfig::default().margin)]
    swaps_margin: u32,
    /// Blocks within which a client's activation of the hub's lock on
    /// Bitcoin confirms: the hub's lock expires that, the margin and two
    /// blocks before the client's.
    #[arg(long, env = "LICASH_HUB_SWAPS_CONFIRM",
          default_value_t = SwapsConfig::default().confirm,
          value_parser = clap::value_parser!(u32).range(1..))]
    swaps_confirm: u32,
}

impl SwapsArgs {
    /// The provider's terms, and the hubs it takes swaps from with their
    /// caps, if the hub provides swaps.
    fn terms(&self) -> anyhow::Result<Option<(SwapsConfig, Vec<Capped>)>> {
        let from: Vec<Capped> = self
            .swaps_from
            .iter()
            .filter(|f| !f.trim().is_empty())
            .map(|f| capped(f).with_context(|| format!("--swaps-from {f}")))
            .collect::<anyhow::Result<_>>()?;
        if !self.swaps {
            anyhow::ensure!(from.is_empty(), "--swaps-from needs --swaps");
            anyhow::ensure!(!self.swaps_tor, "--swaps-tor needs --swaps");
            return Ok(None);
        }
        anyhow::ensure!(
            self.swaps_min <= self.swaps_max,
            "--swaps-min must not exceed --swaps-max"
        );
        anyhow::ensure!(
            self.swaps_margin > licash_wallet::CLAIM_MARGIN + 2,
            "--swaps-margin must exceed {}",
            licash_wallet::CLAIM_MARGIN + 2
        );
        let config = SwapsConfig {
            fee: Fee {
                base: self.swaps_fee_base,
                ppm: self.swaps_fee_ppm,
            },
            min: self.swaps_min,
            max: self.swaps_max,
            margin: self.swaps_margin,
            confirm: self.swaps_confirm,
        };
        Ok(Some((config, from)))
    }
}

/// A hub swapped from, by URL, and the most of its notes the provider
/// holds.
type Capped = (String, u64);

/// A hub's URL and the cap on its notes, of `URL=CAP`.
fn capped(arg: &str) -> anyhow::Result<Capped> {
    let (url, cap) = arg.rsplit_once('=').context("not URL=CAP")?;
    let url = url.trim().trim_end_matches('/');
    anyhow::ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "not a hub URL: {url}"
    );
    let cap = cap.trim().parse().with_context(|| format!("cap {cap}"))?;
    Ok((url.to_owned(), cap))
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
        deposits: Box<DepositArgs>,
        #[command(flatten)]
        announce: Box<AnnounceArgs>,
        #[command(flatten)]
        swaps: Box<SwapsArgs>,
        /// Also serve the API as an onion service of a Tor client of the
        /// hub's own, at the same address on every start (its key in
        /// `<data>/tor`), and announce it, reaching relays through Tor too.
        #[arg(long, env = "LICASH_HUB_ONION",
              value_parser = clap::builder::BoolishValueParser::new())]
        onion: bool,
        /// Run even though own locks of the hub, which it may yet pay, are
        /// backed by no deposit, Lightning receipt or swap it keeps, as
        /// once such a file was restored older than `hub.sqlite`: alerting
        /// the operator, rather than refusing to start.
        #[arg(long, env = "LICASH_HUB_ALLOW_UNBACKED_LOCKS",
              value_parser = clap::builder::BoolishValueParser::new())]
        allow_unbacked_locks: bool,
        /// Notify only of these kinds of event [default: all but locked].
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
    /// An interactive dashboard of the hub's records, beside the running
    /// hub or without it: payments, fees, reserves, contracts, privacy and
    /// health, over a range (30 days at first) against the one before.
    Dashboard {
        #[command(flatten)]
        dashboard: licash_dashboard::Args,
        /// The running hub's URL, asked for its health [default: http://
        /// and `LICASH_HUB_LISTEN`, or 127.0.0.1:8338].
        #[arg(long)]
        url: Option<String>,
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
    /// Answer a holder's request on Bitcoin in one of the hub's contracts,
    /// beside the running hub: broadcast its plain answer and print its
    /// txid. Her proof decides then. Reads the openings only, so it needs
    /// neither the seed nor the contracts' wallet.
    Respond {
        /// The contract's opening, in hex.
        id: String,
        /// Her request R, a transaction in hex (`-` for stdin).
        request: PathBuf,
        /// The chain to read the tip from and broadcast to, as `run`'s.
        #[arg(
            long = "chain",
            value_name = "URL",
            env = "LICASH_HUB_CHAIN",
            hide_env_values = true
        )]
        chain: String,
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
            deposits,
            announce,
            swaps,
            onion,
            allow_unbacked_locks,
            notify_kinds,
        } => {
            let policy = policy.policy();
            let (indexing, tweaks_from) = (chain.tweaks, chain.tweaks_from);
            let contracting = chain.contracts;
            let responding = chain.contracts_respond;
            let response_poll = Duration::from_secs(chain.chain_poll);
            let range = chain.range();
            let wallet = match (&chain.contracts_wallet, &chain.url) {
                (Some(url), _) => Some(url.parse::<Source>().context("--contracts-wallet")?),
                (None, Some(url)) if contracting && url.starts_with("esplora+") => anyhow::bail!(
                    "opening contracts (--contracts) on an Esplora API needs --contracts-wallet, a node's JSON-RPC"
                ),
                (None, _) => None,
            };
            let chain = chain.source(&policy)?;
            anyhow::ensure!(
                !indexing || chain.is_some(),
                "indexing tweaks (--tweaks) needs --chain"
            );
            anyhow::ensure!(
                !contracting || chain.is_some(),
                "opening contracts (--contracts) needs --chain"
            );
            let lightning = lightning.gateway()?;
            anyhow::ensure!(
                lightning.is_none() || chain.is_some(),
                "a Lightning gateway (--lnd) needs --chain"
            );
            let depositing = match &chain {
                // Polling the chain as often as the hub follows it.
                Some((source, profile, every)) => deposits.config(*profile)?.map(|config| {
                    let config = deposit::Config {
                        poll: *every,
                        ..config
                    };
                    (source.clone(), *profile, config)
                }),
                None => {
                    anyhow::ensure!(
                        deposits.deposit_sweep_address.is_none(),
                        "taking deposits (--deposit-sweep-address) needs --chain"
                    );
                    None
                }
            };
            let relays = announce.mailboxes();
            let mut announcing = (*announce).announcing(onion)?;
            anyhow::ensure!(
                announcing.is_none() || chain.is_some(),
                "announcing on Nostr (--nostr-relay) needs --chain"
            );
            let swapping = swaps.terms()?;
            let swaps_tor = swaps.swaps_tor;
            anyhow::ensure!(
                swapping.is_none() || chain.is_some(),
                "providing swaps (--swaps) needs --chain"
            );
            let swaps_dir = cli.data.join("swaps");
            if swapping.is_none() {
                check_idle(&swaps_dir)?;
            }
            let targets = notify.targets()?;
            let swaps_targets = targets.clone();
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
            let deposits = match depositing {
                Some((source, profile, config)) => {
                    let path = cli.data.join("deposits.sqlite");
                    let deposits =
                        deposit::Gateway::open(hub.clone(), source, profile, config, &path)
                            .context("open the deposits")?;
                    Some(Arc::new(deposits))
                }
                None => None,
            };
            if let (Some(receipts), Some(deposits)) = (&gateway, &deposits) {
                peer::pair(receipts, deposits);
            }
            let tweaks = match indexing {
                true => {
                    let path = cli.data.join("tweaks.sqlite");
                    let index = Tweaks::open(&path).context("open the tweak index")?;
                    Some(Arc::new(index))
                }
                false => None,
            };
            let runtime = tokio::runtime::Runtime::new()?;
            let _entered = runtime.enter();
            let contracts = match (contracting, &chain) {
                (true, Some((source, _, _))) => {
                    let keysets = hub.info().keysets;
                    let keyset = keysets.first().context("the hub has no keyset")?;
                    let wallet = wallet.unwrap_or_else(|| source.clone());
                    let opening = Openings::with_wallet(
                        &cli.data,
                        source.clone(),
                        wallet,
                        keyset.clone(),
                        range,
                    );
                    let openings = runtime.block_on(opening).context("open the contracts")?;
                    openings.spawn_sweeper();
                    if responding {
                        openings.spawn_responder(response_poll);
                        tracing::info!("automatic contract responder enabled; one-time answers pinned in responses.sqlite");
                    }
                    tracing::info!(
                        mock = openings.advertise().mock,
                        ?range,
                        "opening contracts"
                    );
                    Some(openings)
                }
                _ => {
                    match licash_contracts::hub::signed(&cli.data) {
                        Ok(0) => {}
                        Ok(signed) => tracing::warn!(
                            signed,
                            "contracts signed in openings.sqlite, and --contracts is off"
                        ),
                        Err(e) => tracing::warn!("the contracts in openings.sqlite: {e:#}"),
                    }
                    None
                }
            };
            // One Tor client for the onion service and the swaps' wallet
            // alike, since its state directory takes only one.
            let tor = match onion || swaps_tor {
                true => Some(Tor::new(&cli.data.join("tor")).context("a Tor client")?),
                false => None,
            };
            let swapping = match (swapping, &chain) {
                (Some((config, from)), Some((_, profile, every))) => {
                    if from.is_empty() {
                        tracing::warn!("providing swaps from no hub (--swaps-from)");
                    }
                    // As clients reach the hub, or at least as it listens.
                    let url = announcing.as_ref().and_then(|a| a.urls.first().cloned());
                    let url = url.unwrap_or_else(|| format!("http://{listen}"));
                    let (receipts, deposits) = (gateway.clone(), deposits.clone());
                    let issuer = issuer(&hub, url, &from, receipts, deposits);
                    let transport = match &tor {
                        Some(tor) if swaps_tor => {
                            tracing::info!("the swaps' wallet reaches hubs through Tor");
                            Http::tor(tor.clone())
                        }
                        _ => Http::new(None)?,
                    };
                    let provider = swap_provider(&swaps_dir, *profile, config, issuer, transport)?;
                    let provider = Arc::new(provider);
                    let notifier = match swaps_targets.is_empty() {
                        true => None,
                        false => {
                            let cursors = swaps_dir.join("notify.json");
                            let name = format!("{} swaps", notify.name);
                            let notifier = SwapsNotifier::new(
                                provider.clone(),
                                &name,
                                swaps_targets,
                                &cursors,
                                Vec::new(),
                            )
                            .context("set up the swaps' notifications")?;
                            Some(notifier)
                        }
                    };
                    Some(Swapping {
                        provider,
                        every: *every,
                        notifier,
                    })
                }
                _ => None,
            };
            // Once the files of what backs them are open, and up to date.
            check_backed(&hub, &cli.data, allow_unbacked_locks)?;
            let swaps = match &swapping {
                Some(Swapping { provider, .. }) => {
                    let nostr = match &announcing {
                        Some(_) => Some(licash_nostr::public_key(&provider.nostr_secret()?)?),
                        None => None,
                    };
                    Some(licash_swaps_service::api(provider.clone(), nostr))
                }
                None => None,
            };
            let announced = announcing.as_ref().map(|a| a.urls.clone());
            let urls = Arc::new(std::sync::RwLock::new(announced.unwrap_or_default()));
            let services = Services {
                gateway: gateway.clone(),
                deposits: deposits.clone(),
                tweaks: tweaks.clone(),
                relays,
                contracts,
                swaps,
                urls: urls.clone(),
            };
            let router = licash_hub_service::router(hub.clone(), services);
            // Served until the hub stops.
            let _onion = match &tor {
                Some(tor) if onion => {
                    let onion = tor.serve("licash-hub", router.clone())?;
                    tracing::info!(url = onion.url(), "serving as an onion service");
                    announce_over(&mut announcing, tor, Some(&onion))?;
                    let mut urls = urls
                        .write()
                        .map_err(|_| anyhow::anyhow!("the URLs' lock"))?;
                    if !urls.iter().any(|url| url == onion.url()) {
                        urls.push(onion.url().to_owned());
                    }
                    Some(onion)
                }
                _ => None,
            };
            let tasks = Tasks {
                chain,
                notifier,
                gateway,
                deposits,
                announcing,
                tweaks: tweaks.map(|index| (index, tweaks_from)),
                swapping,
            };
            let serving = serve(hub, router, listen, policy, tasks);
            runtime.block_on(serving)
        }
        Command::TestNotify { notify } => test_notify(&notify),
        Command::Status { json } => status(&db, json),
        Command::Dashboard { dashboard, url } => show(&cli.data, dashboard, url),
        Command::Events {
            after,
            limit,
            follow,
        } => events(&db, after, limit, follow),
        Command::Respond { id, request, chain } => respond(&cli.data, &id, &request, &chain),
    }
}

fn respond(data: &Path, id: &str, request: &Path, chain: &str) -> anyhow::Result<()> {
    let id = licash_contracts::parse_id(id.trim()).context("the opening's id")?;
    let hex = match request.as_os_str() == "-" {
        true => std::io::read_to_string(std::io::stdin()).context("read R from stdin")?,
        false => std::fs::read_to_string(request)
            .with_context(|| format!("read {}", request.display()))?,
    };
    let r: bitcoin::Transaction =
        bitcoin::consensus::encode::deserialize_hex(hex.trim()).context("R, in hex")?;
    let source: Source = chain.parse().context("--chain")?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let answer = licash_contracts::hub::respond(data, &source, &id, &r).await?;
        let txid = source
            .broadcast(&answer)
            .await
            .context("broadcast the answer")?;
        tracing::info!(%txid, request = %r.compute_txid(), "answered her request");
        println!("{txid}");
        Ok(())
    })
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

/// `hub`, reached at `url`, issuing the locks of the swaps into it from
/// the hubs of `from` under their caps: under no hash its Lightning
/// receipts or its deposits hold.
fn issuer(
    hub: &Arc<Hub>,
    url: String,
    from: &[Capped],
    receipts: Option<Arc<Gateway>>,
    deposits: Option<Arc<deposit::Gateway>>,
) -> Issuer {
    let issuer = from
        .iter()
        .fold(Issuer::new(hub.clone(), url), |issuer, (from, cap)| {
            issuer.cap(from, *cap)
        });
    issuer.guard(move |hash| {
        if let Some(gateway) = &receipts {
            if gateway.holds(hash)? {
                return Ok(true);
            }
        }
        match &deposits {
            Some(deposits) => deposits.holds(hash),
            None => Ok(false),
        }
    })
}

/// Refuse to run without `--swaps` while the provider in `dir`, if any,
/// has swaps into the hub under way: the notes the hub locked for them
/// are backed once the provider claims the clients' locks, which it then
/// would not.
fn check_idle(dir: &Path) -> anyhow::Result<()> {
    let (wallet, swaps) = (dir.join("wallet.sqlite"), dir.join("swaps.sqlite"));
    if !swaps.exists() {
        return Ok(());
    }
    let off = "--swaps is off";
    let wallet = Wallet::open(&wallet, Http::new(None)?).context(off)?;
    Provider::open(&swaps, wallet, SwapsConfig::default()).context(off)?;
    tracing::warn!(dir = %dir.display(), "{off}: the swap provider takes no swaps");
    Ok(())
}

/// Refuse to run, unless `allow`ed, while own locks of `hub` it may yet pay
/// are backed by no deposit, Lightning receipt or swap under way in `data`:
/// each was recorded before its lock, so such a file was restored older
/// than `hub.sqlite`, or lost, and the hub would pay the notes it locked
/// without collecting what was paid for them. Allowed, the operator is
/// alerted. Opens no file while no own lock is live.
fn check_backed(hub: &Hub, data: &Path, allow: bool) -> anyhow::Result<()> {
    let live = hub.live_own_locks().context("read the hub's own locks")?;
    if live.is_empty() {
        return Ok(());
    }
    let hashes: Vec<[u8; 32]> = live
        .iter()
        .filter_map(|lock| Some(*lock.proposal.lock()?.terms().hash()))
        .collect();
    let (deposits, receipts) = (data.join("deposits.sqlite"), data.join("lightning.sqlite"));
    let swaps = data.join("swaps").join("swaps.sqlite");
    let mut backed = std::collections::HashSet::new();
    let mut unreadable = Vec::new();
    let mut read = |path: &Path, found: Result<_, String>| match found {
        Ok(found) => backed.extend(found),
        Err(e) => unreadable.push(format!("{} ({e})", path.display())),
    };
    let held = deposit::Gateway::holding(&deposits, &hashes).map_err(|e| e.to_string());
    read(&deposits, held);
    let held = Gateway::holding(&receipts, &hashes).map_err(|e| e.to_string());
    read(&receipts, held);
    let held = licash_swaps::backing_own(&swaps, &hashes).map_err(|e| e.to_string());
    read(&swaps, held);
    let unbacked: Vec<String> = live
        .iter()
        .filter_map(|lock| {
            let terms = lock.proposal.lock()?.terms();
            if backed.contains(terms.hash()) {
                return None;
            }
            let amount: u64 = lock
                .proposal
                .outputs()
                .iter()
                .map(|o| o.denomination())
                .sum();
            Some(format!(
                "lock {} under hash {}, {amount} sat, expiring at {}",
                hex::encode(lock.proposal.id().as_bytes()),
                hex::encode(terms.hash()),
                terms.expiry()
            ))
        })
        .collect();
    if unbacked.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "{} own locks of the hub, which it may yet pay, are backed by no deposit, Lightning \
         receipt or swap under way in {}: {}. Restore deposits.sqlite, lightning.sqlite and \
         swaps/swaps.sqlite as recent as hub.sqlite.",
        unbacked.len(),
        data.display(),
        unbacked.join("; ")
    );
    if !unreadable.is_empty() {
        message.push_str(&format!(" Unreadable: {}.", unreadable.join(", ")));
    }
    anyhow::ensure!(
        allow,
        "{message} Or run with --allow-unbacked-locks, to pay them unbacked."
    );
    hub.alert(&message).context("alert the operator")?;
    Ok(())
}

/// The provider of the swaps into the hub, on its files in `dir`: made on
/// the first start, its wallet's on `network`, and never replaced. Its
/// wallet reaches hubs through `transport`.
fn swap_provider(
    dir: &Path,
    network: ChainProfile,
    config: SwapsConfig,
    issuer: Issuer,
    transport: Http,
) -> anyhow::Result<Provider<Http>> {
    let (wallet, swaps) = (dir.join("wallet.sqlite"), dir.join("swaps.sqlite"));
    match (wallet.exists(), swaps.exists()) {
        (true, true) => {
            let wallet = Wallet::open(&wallet, transport).context("open the swaps' wallet")?;
            anyhow::ensure!(
                wallet.network() == network,
                "the swaps' wallet is of {}, the hub of {network}",
                wallet.network()
            );
            Provider::open_issuing(&swaps, wallet, config, issuer).context("open the swaps file")
        }
        (false, false) => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
            let mut seed = [0; 32];
            OsRng.fill_bytes(&mut seed);
            let wallet = Wallet::create(&wallet, &seed, network, transport)
                .context("create the swaps' wallet")?;
            let address = hex::encode(wallet.address().to_bytes());
            let provider = Provider::create_issuing(&swaps, wallet, config, issuer)
                .context("create the swaps file")?;
            tracing::info!(dir = %dir.display(), %address, "swap provider created: back up its wallet file");
            Ok(provider)
        }
        _ => anyhow::bail!(
            "{}: wallet.sqlite and swaps.sqlite go together, and one is missing: restore it",
            dir.display()
        ),
    }
}

/// What runs beside the API, if configured.
struct Tasks {
    chain: Option<(Source, ChainProfile, Duration)>,
    notifier: Option<Notifier>,
    gateway: Option<Arc<Gateway>>,
    deposits: Option<Arc<deposit::Gateway>>,
    announcing: Option<Announcing>,
    /// With the first block to index if the index is new.
    tweaks: Option<(Arc<Tweaks>, Option<u32>)>,
    swapping: Option<Swapping>,
}

/// The provider of the swaps into the hub, which takes them on every
/// `every`, and tells the operator of them through `notifier`.
struct Swapping {
    provider: Arc<Provider<Http>>,
    every: Duration,
    notifier: Option<SwapsNotifier>,
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
        deposits,
        announcing,
        tweaks,
        swapping,
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
    let depositing = deposits.map(|deposits| {
        tracing::info!("taking deposits on Bitcoin");
        tokio::spawn(deposits.run())
    });
    let mut swaps = Vec::new();
    if let Some(Swapping {
        provider,
        every,
        notifier,
    }) = swapping
    {
        let address = hex::encode(provider.wallet().address().to_bytes());
        tracing::info!(%address, config = %serde_json::to_string(provider.config())?, "providing swaps");
        swaps.push(tokio::spawn(licash_swaps_service::step(
            provider.clone(),
            every,
        )));
        // Its events not yet sent go out after the next start.
        swaps.extend(notifier.map(|notifier| tokio::spawn(notifier.run())));
        if let Some(a) = &announcing {
            let (relays, route) = (a.relays.clone(), a.route.clone());
            let (urls, name) = (a.urls.clone(), a.name.clone());
            let announcing = licash_swaps_service::announce(provider, relays, route, urls, name);
            swaps.push(tokio::spawn(announcing));
        }
    }
    let announcing = announcing.map(|a| {
        let (relays, urls) = (a.relays.join(", "), a.urls.join(", "));
        tracing::info!(%relays, %urls, "announcing on Nostr");
        tokio::spawn(announce(hub.clone(), a.relays, a.route, a.urls, a.name))
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    for task in [watching, indexing, paying, depositing, announcing]
        .into_iter()
        .flatten()
        .chain(swaps)
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

fn show(data: &Path, args: licash_dashboard::Args, url: Option<String>) -> anyhow::Result<()> {
    let url = url.unwrap_or_else(|| {
        let listen = std::env::var("LICASH_HUB_LISTEN");
        format!("http://{}", listen.as_deref().unwrap_or("127.0.0.1:8338"))
    });
    let probe = licash_dashboard::Probe::new(&url, licash_hub_service::HEALTH)?;
    let data = data.to_owned();
    args.run(move || dashboard::report(&data, Some(&probe)))
}

fn status(db: &Path, json: bool) -> anyhow::Result<()> {
    let stats = Ledger::open(db).context("read the hub")?.stats()?;
    if json {
        println!("{}", serde_json::to_string(&stats)?);
        return Ok(());
    }
    // What it earned first, then what it did.
    let n = |v: u64| licash_dashboard::grouped(v as i64);
    for (name, value) in [
        ("income", format!("{} sat", n(stats.income))),
        (
            "offers",
            format!("{}, {} released", n(stats.offers), n(stats.released)),
        ),
        (
            "locks",
            format!("{}, {} open", n(stats.locks), n(stats.open_locks)),
        ),
        (
            "notes",
            format!(
                "{} issued, {} spent, {} unlinked",
                n(stats.issued),
                n(stats.spent),
                n(stats.unlinked)
            ),
        ),
        ("events", n(stats.events)),
    ] {
        println!("{name:<6}  {value}");
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    fn mailboxes(args: &[&str]) -> Vec<String> {
        let cli = Cli::try_parse_from([&["licash-hub", "run"], args].concat()).unwrap();
        match cli.command {
            Command::Run { announce, .. } => announce.mailboxes(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn mailboxes_are_on_the_announcement_relays_unless_named() {
        assert!(mailboxes(&[]).is_empty());
        let announced = ["--nostr-relay", "wss://a.example,wss://b.example"];
        assert_eq!(
            mailboxes(&announced),
            ["wss://a.example", "wss://b.example"]
        );
        let named = [&announced[..], &["--mailbox-relay", "ws://c.example"]].concat();
        assert_eq!(mailboxes(&named), ["ws://c.example"]);
    }
}
