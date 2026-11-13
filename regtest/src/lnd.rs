//! Pinned LND nodes on the isolated Core of `regtest/lightning.sh`: two
//! disposable wallets, `payer` and `receiver`, and one channel between them.

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use bitcoincore_rpc::RpcApi;
use lngap_btc::{
    bitcoin::{Address, Amount, Network},
    regtest::Regtest,
};
use serde_json::Value;

/// The pinned image `regtest/lightning.sh` must have pulled.
pub const IMAGE: &str = "lightninglabs/lnd:v0.21.6-beta@sha256:54b3c87ad0bc3fea3fab8af23ace6421147ea97e6b56bf3911592d7853c185b8";

pub struct Lnd {
    name: String,
    role: &'static str,
    artifacts: PathBuf,
}

fn docker(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("docker").args(args).output()?;
    ensure!(
        output.status.success(),
        "isolated Docker command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

impl Lnd {
    pub fn start(rt: &Regtest, role: &'static str) -> Result<Self> {
        let network = std::env::var("LICASH_LN_NETWORK").context("run regtest/lightning.sh")?;
        let name = format!("{}-{role}", std::env::var("LICASH_LN_CORE")?);
        let artifacts = PathBuf::from(std::env::var("LICASH_LN_DIR")?);
        let data = artifacts.join(role);
        std::fs::create_dir(&data).context("refusing to reuse an LND wallet directory")?;
        let security = docker(&["info", "--format", "{{json .SecurityOptions}}"])?;
        let user = if String::from_utf8_lossy(&security).contains("rootless") {
            "0:0".to_owned()
        } else {
            format!(
                "{}:{}",
                String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?.trim(),
                String::from_utf8(Command::new("id").arg("-g").output()?.stdout)?.trim()
            )
        };
        let cookie = std::fs::read_to_string(rt.cookie())?;
        let (rpcuser, rpcpass) = cookie.trim().split_once(':').context("Core cookie shape")?;
        let port = rt
            .rpc_url()
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .context("Core RPC port")?;
        // Credentials are ephemeral and remain on the isolated test network.
        // Never print command arguments containing the cookie.
        docker(&[
            "run",
            "--detach",
            "--init",
            "--name",
            &name,
            "--network",
            &network,
            "--network-alias",
            role,
            "--user",
            &user,
            "--volume",
            &format!("{}:/data:z", data.display()),
            "--entrypoint",
            "lnd",
            IMAGE,
            "--lnddir=/data",
            "--bitcoin.regtest",
            "--bitcoin.node=bitcoind",
            &format!("--bitcoind.rpchost=core:{port}"),
            &format!("--bitcoind.rpcuser={rpcuser}"),
            &format!("--bitcoind.rpcpass={rpcpass}"),
            "--bitcoind.zmqpubrawblock=tcp://core:28332",
            "--bitcoind.zmqpubrawtx=tcp://core:28333",
            "--listen=0.0.0.0:9735",
            "--noseedbackup",
            "--nobootstrap",
        ])?;
        Ok(Self {
            name,
            role,
            artifacts,
        })
    }

    pub fn cli(&self, args: &[&str]) -> Result<Value> {
        let mut command = vec![
            "exec",
            &self.name,
            "lncli",
            "--lnddir=/data",
            "--network=regtest",
        ];
        command.extend(args);
        Ok(serde_json::from_slice(&docker(&command)?)?)
    }

    pub fn wait(&self, args: &[&str], predicate: impl Fn(&Value) -> bool) -> Result<Value> {
        let start = Instant::now();
        loop {
            let result = self.cli(args);
            if let Ok(value) = &result {
                if predicate(value) {
                    return result;
                }
            }
            ensure!(
                start.elapsed() < Duration::from_secs(90),
                "{} did not reach expected {} state: {result:?}",
                self.role,
                args[0]
            );
            thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn pay(&self, invoice: &str, label: &str) -> Result<Payment> {
        let output = std::fs::File::create(self.artifacts.join(format!("{label}-payment.json")))?;
        let errors = output.try_clone()?;
        Ok(Payment(
            Command::new("docker")
                .args([
                    "exec",
                    &self.name,
                    "lncli",
                    "--lnddir=/data",
                    "--network=regtest",
                    "payinvoice",
                    "--force",
                    "--json",
                    "--timeout=1m",
                    invoice,
                ])
                .stdout(Stdio::from(output))
                .stderr(Stdio::from(errors))
                .spawn()?,
        ))
    }
}

impl Drop for Lnd {
    fn drop(&mut self) {
        if let Ok(log) = Command::new("docker").args(["logs", &self.name]).output() {
            let mut bytes = log.stdout;
            bytes.extend(log.stderr);
            let _ = std::fs::write(self.artifacts.join(format!("{}.log", self.role)), bytes);
        }
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

pub struct Payment(pub Child);
impl Drop for Payment {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn connect_channel(rt: &Regtest, payer: &Lnd, receiver: &Lnd) -> Result<()> {
    payer.wait(&["getinfo"], |v| v["synced_to_chain"] == true)?;
    let remote = receiver.wait(&["getinfo"], |v| v["synced_to_chain"] == true)?;
    let address = payer.cli(&["newaddress", "p2wkh"])?;
    let address = Address::from_str(address["address"].as_str().context("LND funding address")?)?
        .require_network(Network::Regtest)?;
    rt.rpc.send_to_address(
        &address,
        Amount::from_sat(2_000_000),
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    rt.mine(6)?;
    payer.wait(&["walletbalance"], |v| {
        v["confirmed_balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|b| b >= 2_000_000)
    })?;
    let key = remote["identity_pubkey"]
        .as_str()
        .context("receiver node key")?;
    payer.cli(&["connect", &format!("{key}@receiver:9735")])?;
    payer.cli(&[
        "openchannel",
        &format!("--node_key={key}"),
        "--local_amt=1000000",
        "--sat_per_vbyte=1",
    ])?;
    rt.mine(6)?;
    payer.wait(&["listchannels"], |v| {
        v["channels"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["active"] == true))
    })?;
    receiver.wait(&["listchannels"], |v| {
        v["channels"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["active"] == true))
    })?;
    Ok(())
}
