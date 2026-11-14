//! The regtest ecosystem of `docker-compose.yml`, for the end-to-end tests:
//! Bitcoin Core, two hubs with Lightning gateways over their LND nodes, an
//! outside LND node, and wallets in temporary files. `run.sh` brings the
//! stack up; [`Eco::up`] funds its nodes and opens their channels once.

use std::{
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context};
use licash::spend::lock_hash;
use licash_api::{Info, ReceiveStatus};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use tokio::process::Command;

pub use licash_transport::Http;

/// Satoshis in each channel, half pushed to its other end.
const CHANNEL: u64 = 2_000_000;

/// The Lightning nodes: the outside one and each hub's.
pub const OUTSIDE: &str = "outside-ln";
pub const NODES: [&str; 3] = [OUTSIDE, "hub1-ln", "hub2-ln"];

/// A wallet of the tests, over HTTP.
pub type Wallet = licash_wallet::Wallet<Http>;

pub struct Eco {
    project: String,
    file: PathBuf,
    core: String,
    http: reqwest::Client,
    /// Each hub's URL, as published on this host.
    hubs: Vec<String>,
    /// The Nostr relay's URL, as published on this host.
    relay: String,
    dir: tempfile::TempDir,
}

/// Wait up to `secs` for `check` to give something, every half second.
pub async fn until<T, F: Future<Output = anyhow::Result<Option<T>>>>(
    what: &str,
    secs: u64,
    mut check: impl FnMut() -> F,
) -> anyhow::Result<T> {
    let start = Instant::now();
    loop {
        let last = match check().await {
            Ok(Some(found)) => return Ok(found),
            Ok(None) => String::new(),
            Err(e) => format!(": {e:#}"),
        };
        if start.elapsed() > Duration::from_secs(secs) {
            bail!("{what}: not after {secs} s{last}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

impl Eco {
    /// The stack of Compose project `$LICASH_E2E_PROJECT` (by default
    /// `licash-e2e`), with its nodes funded, a channel between each two of
    /// them, and its hubs' gateways up.
    pub async fn up() -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let project = std::env::var("LICASH_E2E_PROJECT").unwrap_or_else(|_| "licash-e2e".into());
        let mut eco = Self {
            project,
            file: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docker-compose.yml"),
            core: String::new(),
            http: reqwest::Client::new(),
            hubs: Vec::new(),
            relay: String::new(),
            dir: tempfile::tempdir()?,
        };
        eco.core = format!("http://{}", eco.port("bitcoind", 18443).await?);
        eco.relay = format!("ws://{}", eco.port("relay", 8080).await?);
        for hub in ["hub1", "hub2"] {
            let at = eco.port(hub, 8338).await?;
            eco.hubs.push(format!("http://{at}"));
        }
        eco.fund().await?;
        Ok(eco)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("docker");
        command
            .args(["compose", "-p", &self.project, "-f"])
            .arg(&self.file)
            .args(args)
            .kill_on_drop(true);
        command
    }

    async fn compose(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let output = self.command(args).output().await?;
        ensure!(
            output.status.success(),
            "docker compose {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    async fn port(&self, service: &str, port: u16) -> anyhow::Result<String> {
        let published = self.compose(&["port", service, &port.to_string()]).await;
        let published = String::from_utf8(published.unwrap_or_default())?;
        ensure!(
            !published.trim().is_empty(),
            "{service} of {} is not up: run apps/e2e/run.sh",
            self.project
        );
        Ok(published.trim().to_owned())
    }

    /// Restart `service`, and wait for it to be healthy.
    pub async fn restart(&self, service: &str) -> anyhow::Result<()> {
        self.compose(&["restart", service]).await?;
        self.compose(&["up", "--wait", service]).await?;
        Ok(())
    }

    /// The URL of hub `n`, 1 or 2.
    pub fn hub(&self, n: usize) -> &str {
        &self.hubs[n - 1]
    }

    /// The URL of the Nostr relay the hubs announce themselves on.
    pub fn relay(&self) -> &str {
        &self.relay
    }

    /// A new wallet, in a file of its own.
    pub fn wallet(&self, name: &str) -> anyhow::Result<Wallet> {
        let mut seed = [0; 32];
        OsRng.fill_bytes(&mut seed);
        let path = self.dir.path().join(format!("{name}.sqlite"));
        Ok(Wallet::create(&path, &seed, Http::new(None)?)?)
    }

    pub async fn info(&self, hub: &str) -> anyhow::Result<Info> {
        let url = format!("{hub}{}", licash_api::INFO);
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }

    /// Where the gateway of `hub` stands on the payment to a wallet of
    /// `hash`, in hex.
    pub async fn receipt(&self, hub: &str, hash: &str) -> anyhow::Result<ReceiveStatus> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(hash, &mut bytes)?;
        let url = format!("{hub}{}", licash_api::ln_receive_state(&bytes));
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }

    /// Call Bitcoin Core, at `path`.
    async fn rpc(&self, path: &str, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = json!({ "jsonrpc": "1.0", "id": "e2e", "method": method, "params": params });
        let answer: Value = self
            .http
            .post(format!("{}{path}", self.core))
            .basic_auth("licash", Some("licash"))
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if !answer["error"].is_null() {
            bail!("{method}: {}", answer["error"]);
        }
        Ok(answer["result"].clone())
    }

    /// Call the miner's wallet in Bitcoin Core.
    async fn miner(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.rpc("/wallet/miner", method, params).await
    }

    pub async fn height(&self) -> anyhow::Result<u32> {
        let count = self.rpc("", "getblockcount", json!([])).await?;
        Ok(count.as_u64().context("a block count")? as u32)
    }

    /// Mine `blocks`, then wait for every node and hub to follow. The new
    /// height.
    pub async fn mine(&self, blocks: u32) -> anyhow::Result<u32> {
        let address = self.miner("getnewaddress", json!([])).await?;
        self.miner("generatetoaddress", json!([blocks, address]))
            .await?;
        let height = self.height().await?;
        self.synced(height).await?;
        Ok(height)
    }

    /// Wait for every Lightning node and hub to reach `height`.
    pub async fn synced(&self, height: u32) -> anyhow::Result<()> {
        for node in NODES {
            until(&format!("{node} at {height}"), 120, || async move {
                let info = self.lncli(node, &["getinfo"]).await?;
                let at = info["block_height"].as_u64().unwrap_or(0);
                Ok((info["synced_to_chain"] == true && at >= u64::from(height)).then_some(()))
            })
            .await?;
        }
        for hub in &self.hubs {
            until(&format!("{hub} at {height}"), 120, || async move {
                let locks = self.info(hub).await?.locks;
                Ok(locks.filter(|l| l.height >= height).map(|_| ()))
            })
            .await?;
        }
        Ok(())
    }
}

/// Lightning nodes.
impl Eco {
    /// `lncli` on `node`, and its answer.
    pub async fn lncli(&self, node: &str, args: &[&str]) -> anyhow::Result<Value> {
        let mut all = vec!["exec", "-T", node, "lncli", "--network=regtest"];
        all.extend(args);
        Ok(serde_json::from_slice(&self.compose(&all).await?)?)
    }

    /// `node` pays `request`, in the background: whether it was paid.
    pub fn pay(&self, node: &str, request: &str) -> tokio::task::JoinHandle<anyhow::Result<bool>> {
        let mut command = self.command(&[
            "exec",
            "-T",
            node,
            "lncli",
            "--network=regtest",
            "payinvoice",
            "--force",
            "--json",
            "--timeout=3m",
            request,
        ]);
        tokio::spawn(async move {
            let output = command.output().await?;
            Ok(String::from_utf8_lossy(&output.stdout).contains("\"SUCCEEDED\""))
        })
    }

    /// An invoice of `node` for `sat`: its request and hash, in hex.
    pub async fn invoice(&self, node: &str, sat: u64) -> anyhow::Result<(String, String)> {
        let added = self
            .lncli(node, &["addinvoice", "--amt", &sat.to_string()])
            .await?;
        let request = added["payment_request"].as_str().context("a request")?;
        let hash = added["r_hash"].as_str().context("a hash")?;
        Ok((request.to_owned(), hash.to_owned()))
    }

    /// A held invoice of `node` for `sat`, under the hash of a preimage
    /// drawn here: its request, hash and preimage, in hex. It is settled
    /// or canceled at will.
    pub async fn hold_invoice(
        &self,
        node: &str,
        sat: u64,
    ) -> anyhow::Result<(String, String, String)> {
        let mut preimage = [0; 32];
        OsRng.fill_bytes(&mut preimage);
        let hash = hex::encode(lock_hash(&preimage));
        let added = self
            .lncli(node, &["addholdinvoice", &hash, "--amt", &sat.to_string()])
            .await?;
        let request = added["payment_request"].as_str().context("a request")?;
        Ok((request.to_owned(), hash, hex::encode(preimage)))
    }

    /// Where `node`'s invoice of `hash` stands, such as `SETTLED`, and its
    /// preimage.
    pub async fn invoice_state(&self, node: &str, hash: &str) -> anyhow::Result<(String, String)> {
        let invoice = self.lncli(node, &["lookupinvoice", hash]).await?;
        let state = invoice["state"].as_str().context("a state")?;
        let preimage = invoice["r_preimage"].as_str().unwrap_or_default();
        Ok((state.to_owned(), preimage.to_owned()))
    }

    /// The timelock of the route of each part of `node`'s payment of
    /// `hash`.
    pub async fn timelocks(&self, node: &str, hash: &str) -> anyhow::Result<Vec<u64>> {
        let payments = self.lncli(node, &["listpayments"]).await?;
        let payments = payments["payments"].as_array().context("payments")?;
        let payment = payments
            .iter()
            .find(|p| p["payment_hash"] == hash)
            .context("no payment of that hash")?;
        let htlcs = payment["htlcs"].as_array().into_iter().flatten();
        Ok(htlcs
            .filter_map(|h| h["route"]["total_time_lock"].as_u64())
            .collect())
    }

    async fn pubkey(&self, node: &str) -> anyhow::Result<String> {
        let info = self.lncli(node, &["getinfo"]).await?;
        Ok(info["identity_pubkey"]
            .as_str()
            .context("a node key")?
            .to_owned())
    }

    /// Whether `node` has a channel with `peer`, open or opening.
    async fn channel(&self, node: &str, peer: &str) -> anyhow::Result<bool> {
        let open = self.lncli(node, &["listchannels"]).await?;
        let pending = self.lncli(node, &["pendingchannels"]).await?;
        let open = open["channels"].as_array().into_iter().flatten();
        let opening = pending["pending_open_channels"]
            .as_array()
            .into_iter()
            .flatten();
        Ok(open.map(|c| &c["remote_pubkey"]).any(|k| k == peer)
            || opening
                .map(|c| &c["channel"]["remote_node_pub"])
                .any(|k| k == peer))
    }

    /// Whether `node`'s channel with `peer` is active.
    async fn active(&self, node: &str, peer: &str) -> anyhow::Result<bool> {
        let open = self.lncli(node, &["listchannels", "--active_only"]).await?;
        let mut open = open["channels"].as_array().into_iter().flatten();
        Ok(open.any(|c| c["remote_pubkey"] == peer))
    }

    /// Coins for the miner, a channel between each two nodes, half its
    /// funds on each side, and every hub's gateway up: whatever is missing.
    async fn fund(&self) -> anyhow::Result<()> {
        if let Err(e) = self.rpc("", "createwallet", json!(["miner"])).await {
            ensure!(format!("{e}").contains("already exists"), e);
            let _ = self.rpc("", "loadwallet", json!(["miner"])).await;
        }
        let height = self.height().await?;
        if height < 101 {
            self.mine(101 - height).await?;
        }
        let mut keys = Vec::new();
        for node in NODES {
            keys.push(self.pubkey(node).await?);
        }
        let mut missing = Vec::new();
        for (a, b) in [(0, 1), (0, 2), (1, 2)] {
            if !self.channel(NODES[a], &keys[b]).await? {
                missing.push((a, b));
            }
        }
        for &(a, _) in &missing {
            let address = self.lncli(NODES[a], &["newaddress", "p2wkh"]).await?;
            let address = address["address"].as_str().context("an address")?;
            let btc = (CHANNEL + 100_000) as f64 / 1e8;
            self.miner("sendtoaddress", json!([address, btc])).await?;
        }
        if !missing.is_empty() {
            self.mine(6).await?;
        }
        for &(a, b) in &missing {
            let (node, peer) = (NODES[a], &keys[b]);
            until(&format!("{node} funded"), 60, || async move {
                let balance = self.lncli(node, &["walletbalance"]).await?;
                let confirmed = balance["confirmed_balance"].as_str().unwrap_or("0");
                Ok((confirmed.parse::<u64>()? > CHANNEL).then_some(()))
            })
            .await?;
            let at = format!("{peer}@{}:9735", NODES[b]);
            if let Err(e) = self.lncli(node, &["connect", &at]).await {
                ensure!(format!("{e}").contains("already connected"), e);
            }
            self.lncli(
                node,
                &[
                    "openchannel",
                    &format!("--node_key={peer}"),
                    &format!("--local_amt={CHANNEL}"),
                    &format!("--push_amt={}", CHANNEL / 2),
                    "--sat_per_vbyte=1",
                ],
            )
            .await?;
        }
        let mut inactive = false;
        for (a, b) in [(0, 1), (0, 2), (1, 2)] {
            inactive |= !self.active(NODES[a], &keys[b]).await?;
        }
        if inactive {
            self.mine(6).await?;
        }
        for (a, b) in [(0, 1), (0, 2), (1, 2)] {
            let (node, peer) = (NODES[a], &keys[b]);
            until(
                &format!("{node}'s channel to {}", NODES[b]),
                120,
                || async move { Ok(self.active(node, peer).await?.then_some(())) },
            )
            .await?;
        }
        for hub in &self.hubs {
            until(&format!("the gateway of {hub}"), 120, || async move {
                Ok(self.info(hub).await?.lightning.map(|_| ()))
            })
            .await?;
        }
        Ok(())
    }
}
