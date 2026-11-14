//! Reads Bitcoin for Licash services, block by block, from Bitcoin Core's
//! JSON-RPC or from an Esplora REST server, as a [`Source`] URL names it.

use std::{path::PathBuf, str::FromStr, time::Duration};

use anyhow::{bail, ensure, Context};
use bitcoin::{
    block::Header,
    consensus::{deserialize, serialize},
    Block, BlockHash, Network,
};
use licash::{chain::Checkpoint, spend::ChainProfile};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use url::Url;

/// A chain to read, by URL:
///
/// - `http[s]://[user:pass@]host:port[?cookie=PATH]`: Bitcoin Core's
///   JSON-RPC, with a password or the node's cookie file;
/// - `esplora+http[s]://host[:port][/path]`: an Esplora REST API, such as
///   `esplora+https://esplora.example/api`.
///
/// It shows as its scheme, host and path, never its credentials.
#[derive(Clone)]
pub struct Source {
    kind: Kind,
    shown: String,
    client: reqwest::Client,
}

#[derive(Clone)]
enum Kind {
    Rpc { url: Url, auth: Auth },
    Esplora { base: String },
}

#[derive(Clone)]
enum Auth {
    None,
    Password { user: String, pass: String },
    Cookie(PathBuf),
}

/// The best block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tip {
    pub height: u32,
    pub hash: BlockHash,
}

impl FromStr for Source {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let mut url = Url::parse(text.trim()).map_err(|e| anyhow::anyhow!("chain URL: {e}"))?;
        let host = url.host_str().context("chain URL: no host")?.to_owned();
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let path = url.path().trim_end_matches('/').to_owned();
        let shown = format!("{}://{host}{port}{path}", url.scheme());
        let kind = match url.scheme() {
            "http" | "https" => {
                let mut cookie = None;
                for (key, value) in url.query_pairs() {
                    ensure!(
                        key == "cookie" && cookie.is_none(),
                        "chain URL: unknown or repeated parameter {key}"
                    );
                    cookie = Some(PathBuf::from(value.into_owned()));
                }
                let auth = match (cookie, url.username(), url.password()) {
                    (Some(path), "", None) => Auth::Cookie(path),
                    (Some(_), ..) => bail!("chain URL: a cookie or a password, not both"),
                    (None, "", None) => Auth::None,
                    (None, user, pass) => Auth::Password {
                        user: decode(user),
                        pass: decode(pass.unwrap_or_default()),
                    },
                };
                url.set_query(None);
                let _ = url.set_username("");
                let _ = url.set_password(None);
                Kind::Rpc { url, auth }
            }
            "esplora+http" | "esplora+https" => {
                ensure!(
                    url.username().is_empty() && url.password().is_none() && url.query().is_none(),
                    "chain URL: Esplora takes no credentials or parameters"
                );
                let scheme = url.scheme().trim_start_matches("esplora+");
                Kind::Esplora {
                    base: format!("{scheme}://{host}{port}{path}"),
                }
            }
            other => bail!("chain URL: unknown scheme {other}: http(s) or esplora+http(s)"),
        };
        Ok(Self {
            kind,
            shown,
            client: client()?,
        })
    }
}

fn decode(part: &str) -> String {
    percent_encoding::percent_decode_str(part)
        .decode_utf8_lossy()
        .into_owned()
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown)
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source({})", self.shown)
    }
}

/// The checkpoint profile of `network`'s chain, if Licash has one.
pub fn profile(network: Network) -> anyhow::Result<ChainProfile> {
    match network {
        Network::Bitcoin => Ok(ChainProfile::Core30Mainnet),
        Network::Regtest => Ok(ChainProfile::Core30DefaultRegtest),
        other => bail!("no checkpoint profile for {other}"),
    }
}

/// The network of a checkpoint `profile`.
pub fn network(profile: ChainProfile) -> Network {
    match profile {
        ChainProfile::Core30Mainnet => Network::Bitcoin,
        ChainProfile::Core30DefaultRegtest => Network::Regtest,
    }
}

fn client() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("licash-chain-client/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

#[derive(Deserialize)]
struct Reply<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl Source {
    /// The best block.
    pub async fn tip(&self) -> anyhow::Result<Tip> {
        match &self.kind {
            Kind::Rpc { .. } => {
                #[derive(Deserialize)]
                struct Info {
                    blocks: u32,
                    bestblockhash: String,
                }
                let info: Info = self.call("getblockchaininfo", json!([])).await?;
                Ok(Tip {
                    height: info.blocks,
                    hash: info.bestblockhash.parse().context("best block hash")?,
                })
            }
            Kind::Esplora { .. } => {
                let height = self.text("blocks/tip/height").await?;
                let height = height.parse().context("tip height")?;
                Ok(Tip {
                    height,
                    hash: self.hash(height).await?,
                })
            }
        }
    }

    /// The hash of the best chain's block at `height`.
    pub async fn hash(&self, height: u32) -> anyhow::Result<BlockHash> {
        match &self.kind {
            Kind::Rpc { .. } => self.call::<String>("getblockhash", json!([height])).await,
            Kind::Esplora { .. } => self.text(&format!("block-height/{height}")).await,
        }?
        .trim()
        .parse()
        .context("block hash")
    }

    pub async fn header(&self, hash: &BlockHash) -> anyhow::Result<Header> {
        let hex: String = match &self.kind {
            Kind::Rpc { .. } => {
                self.call("getblockheader", json!([hash.to_string(), false]))
                    .await?
            }
            Kind::Esplora { .. } => self.text(&format!("block/{hash}/header")).await?,
        };
        let header: Header = deserialize(&hex::decode(hex.trim())?).context("header encoding")?;
        ensure!(
            header.block_hash() == *hash,
            "header {hash} does not hash to itself"
        );
        Ok(header)
    }

    pub async fn block(&self, hash: &BlockHash) -> anyhow::Result<Block> {
        let bytes = match &self.kind {
            Kind::Rpc { .. } => {
                let hex: String = self.call("getblock", json!([hash.to_string(), 0])).await?;
                hex::decode(hex.trim())?
            }
            Kind::Esplora { base } => self
                .get(&format!("{base}/block/{hash}/raw"))
                .await?
                .bytes()
                .await?
                .to_vec(),
        };
        let block: Block = deserialize(&bytes).context("block encoding")?;
        ensure!(
            block.block_hash() == *hash,
            "block {hash} does not hash to itself"
        );
        ensure!(block.check_merkle_root(), "block {hash}: merkle root");
        Ok(block)
    }

    /// The network, from the node or from its genesis block.
    pub async fn network(&self) -> anyhow::Result<Network> {
        let genesis = self.hash(0).await?;
        [
            Network::Bitcoin,
            Network::Testnet,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ]
        .into_iter()
        .find(|n| bitcoin::constants::genesis_block(*n).block_hash() == genesis)
        .with_context(|| format!("unknown genesis block {genesis}"))
    }

    /// The checkpoint at `height` of the best chain: its header, the times
    /// of the eleven blocks to it and, on mainnet, the time of the first
    /// block of its retarget period.
    pub async fn checkpoint(&self, height: u32) -> anyhow::Result<Checkpoint> {
        let profile = profile(self.network().await?)?;
        let mut headers: Vec<Header> = Vec::new();
        for h in height.saturating_sub(10)..=height {
            let header = self.header(&self.hash(h).await?).await?;
            if let Some(previous) = headers.last() {
                // The chain moved while it was read.
                ensure!(
                    header.prev_blockhash == previous.block_hash(),
                    "the chain changed while the checkpoint was read: retry"
                );
            }
            headers.push(header);
        }
        let anchor = *headers.last().expect("at least the anchor");
        let period_start = match profile {
            ChainProfile::Core30Mainnet => {
                let first = height - height % 2016;
                Some(self.header(&self.hash(first).await?).await?.time)
            }
            ChainProfile::Core30DefaultRegtest => None,
        };
        let bytes = serialize(&anchor).try_into().expect("a header is 80 bytes");
        let times = headers.iter().map(|h| h.time).collect();
        Checkpoint::new(profile, height, bytes, times, period_start)
            .map_err(|e| anyhow::anyhow!("checkpoint at {height}: {e}"))
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let Kind::Rpc { url, auth } = &self.kind else {
            unreachable!("JSON-RPC on a node");
        };
        let body = json!({"jsonrpc": "1.0", "id": "licash", "method": method, "params": params});
        let mut request = self.client.post(url.clone()).json(&body);
        request = match auth {
            Auth::None => request,
            Auth::Password { user, pass } => request.basic_auth(user, Some(pass)),
            Auth::Cookie(path) => {
                // Read each time: the node writes a new one when it starts.
                let cookie = std::fs::read_to_string(path)
                    .with_context(|| format!("cookie {}", path.display()))?;
                let (user, pass) = cookie
                    .trim()
                    .split_once(':')
                    .with_context(|| format!("cookie {}: not user:password", path.display()))?;
                request.basic_auth(user, Some(pass))
            }
        };
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{self}: {method}: {}", e.without_url()))?;
        let status = response.status();
        ensure!(
            status != reqwest::StatusCode::UNAUTHORIZED,
            "{self}: {method}: unauthorized, check the credentials"
        );
        let reply: Reply<T> = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("{self}: {method}: HTTP {status}: {}", e.without_url()))?;
        if let Some(e) = reply.error {
            bail!("{self}: {method}: {} ({})", e.message, e.code);
        }
        reply
            .result
            .with_context(|| format!("{self}: {method}: no result"))
    }

    async fn text(&self, path: &str) -> anyhow::Result<String> {
        let Kind::Esplora { base } = &self.kind else {
            unreachable!("REST on Esplora");
        };
        Ok(self.get(&format!("{base}/{path}")).await?.text().await?)
    }

    async fn get(&self, url: &str) -> anyhow::Result<reqwest::Response> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{self}: {}", e.without_url()))?;
        let status = response.status();
        ensure!(status.is_success(), "{self}: {url}: HTTP {status}");
        Ok(response)
    }
}
