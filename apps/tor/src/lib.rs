//! Tor for the applications, through a Tor client of their own (Arti):
//! HTTP(S) requests and streams over it, to onion services and to the
//! clear web alike, and an axum router served as an onion service of their
//! own.
//!
//! Arti may end the process if the Tor network's consensus declares its
//! version obsolete.

mod serve;
mod socks;

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use arti_client::{config::TorClientConfigBuilder, IsolationToken, StreamPrefs, TorClient};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Bytes,
    header::{CONTENT_TYPE, HOST, USER_AGENT},
    Method, Request, Uri,
};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{rustls, TlsConnector};
use tor_rtcompat::PreferredRuntime;

pub use serve::Onion;
pub use socks::Socks;

/// How long a request may take, its circuit included.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// The most hosts kept apart at once: past it, they start over apart from
/// those before, as from a fresh client.
const MAX_ISOLATED: usize = 1024;

/// A Tor client, shared by its clones.
#[derive(Clone)]
pub struct Tor {
    client: Arc<TorClient<PreferredRuntime>>,
    tls: TlsConnector,
    isolation: Arc<Isolation>,
}

/// Which circuits streams may share: those to the same host only, so that
/// no exit or onion service sees two of them on one circuit, and no host
/// learns which others the application reaches.
#[derive(Default)]
struct Isolation(Mutex<HashMap<String, IsolationToken>>);

impl Isolation {
    /// What streams to `host` share their circuits with.
    fn of(&self, host: &str) -> IsolationToken {
        let mut hosts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if hosts.len() >= MAX_ISOLATED && !hosts.contains_key(&host.to_ascii_lowercase()) {
            hosts.clear();
        }
        *hosts
            .entry(host.to_ascii_lowercase())
            .or_insert_with(IsolationToken::new)
    }

    /// Stream preferences keeping streams to `host` to their own circuits.
    fn prefs(&self, host: &str) -> StreamPrefs {
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(self.of(host));
        prefs
    }
}

impl Tor {
    /// A Tor client keeping its state, onion service keys included, in
    /// `dir/state` and its directory cache in `dir/cache`. It connects to
    /// the Tor network once first used. Within a Tokio runtime.
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        // Arti's TLS needs a process-wide provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = TorClientConfigBuilder::from_directories(dir.join("state"), dir.join("cache"))
            .build()?;
        let client = TorClient::builder()
            .config(config)
            .create_unbootstrapped()?;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            client,
            tls: TlsConnector::from(Arc::new(tls)),
            isolation: Arc::default(),
        })
    }

    /// GET `url`, or POST `body` to it as JSON, over Tor: its status and
    /// body, of at most `max` bytes.
    pub async fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        self.request_for(url, body, max, TIMEOUT).await
    }

    /// [`Tor::request`], given `timeout` rather than its own: for a large
    /// answer.
    pub async fn request_for(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), String> {
        tokio::time::timeout(timeout, self.exchange(url, body, max))
            .await
            .unwrap_or_else(|_| Err(format!("no answer over Tor in {} s", timeout.as_secs())))
    }

    /// [`Tor::request_for`], its stream to open within `open` once the
    /// client is bootstrapped: told whether it failed before any of it was
    /// sent, to try another way safely.
    pub async fn request_opened(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
        open: Duration,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), Failed> {
        let deadline = tokio::time::Instant::now() + timeout;
        let (target, request) = prepare(url, body).map_err(Failed::Unsent)?;
        // A cold start is not the host's slowness: `open` counts after it.
        tokio::time::timeout_at(deadline, self.client.bootstrap())
            .await
            .map_err(|_| Failed::Unsent(format!("no Tor in {} s", timeout.as_secs())))?
            .map_err(|e| Failed::Unsent(format!("bootstrapping Tor: {e}")))?;
        let open_by = deadline.min(tokio::time::Instant::now() + open);
        let opening = tokio::time::timeout_at(open_by, self.stream(target));
        let stream = opening
            .await
            .map_err(|_| Failed::Unsent(format!("no stream over Tor in {} s", open.as_secs())))?
            .map_err(Failed::Unsent)?;
        tokio::time::timeout_at(deadline, send(TokioIo::new(stream), request, max))
            .await
            .map_err(|_| Failed::Sent(format!("no answer over Tor in {} s", timeout.as_secs())))?
            .map_err(Failed::Sent)
    }

    async fn exchange(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        let (target, request) = prepare(url, body)?;
        let stream = self.stream(target).await?;
        send(TokioIo::new(stream), request, max).await
    }

    /// A SOCKS5 proxy at a loopback address through this client, served on
    /// the current Tokio runtime until dropped: for code bringing its own
    /// sockets, such as a light client. It takes CONNECT, and Tor's
    /// RESOLVE, only with its random username and password, each
    /// destination host on circuits apart from the others' as this
    /// client's own streams.
    pub async fn socks(&self) -> std::io::Result<Socks> {
        socks::serve(self.clone()).await
    }

    /// A stream over Tor to the host and port of `url`, an `http[s]://` or
    /// `ws[s]://` URL, within TLS for `https://` and `wss://`.
    pub async fn connect(&self, url: &str) -> Result<Box<dyn Stream>, String> {
        let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
        self.stream(Target::of(&uri)?).await
    }

    async fn stream(&self, target: Target) -> Result<Box<dyn Stream>, String> {
        let prefs = self.isolation.prefs(&target.host);
        let stream = self
            .client
            .connect_with_prefs((target.host.as_str(), target.port), &prefs)
            .await
            .map_err(|e| format!("over Tor: {e}"))?;
        if !target.tls {
            return Ok(Box::new(stream));
        }
        let name = rustls::pki_types::ServerName::try_from(target.host)
            .map_err(|e| format!("not a TLS name: {e}"))?;
        let stream = self
            .tls
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        Ok(Box::new(stream))
    }
}

/// How a request over Tor failed ([`Tor::request_opened`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failed {
    /// Before any of it was sent: no stream to its host opened in time, or
    /// at all, or it was not a URL to ask.
    Unsent(String),
    /// Once its stream opened: its host may have had it.
    Sent(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsent(why) | Self::Sent(why) => f.write_str(why),
        }
    }
}

impl socks::Exit for Tor {
    async fn connect(&self, host: String, port: u16) -> Result<Box<dyn Stream>, String> {
        let tls = false;
        self.stream(Target { host, port, tls }).await
    }

    async fn resolve(&self, host: String) -> Result<Vec<std::net::IpAddr>, String> {
        let prefs = self.isolation.prefs(&host);
        (self.client.resolve_with_prefs(&host, &prefs).await).map_err(|e| format!("over Tor: {e}"))
    }
}

/// A byte stream over Tor.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Where a stream goes.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    tls: bool,
}

impl Target {
    /// The host of `uri`, and its port or its scheme's, within TLS for
    /// `https://` and `wss://`.
    fn of(uri: &Uri) -> Result<Self, String> {
        let tls = match uri.scheme_str() {
            Some("https" | "wss") => true,
            Some("http" | "ws") => false,
            _ => return Err(format!("not an http[s]:// or ws[s]:// URL: {uri}")),
        };
        let authority = uri.authority().ok_or("a URL without a host")?;
        Ok(Self {
            host: authority.host().to_owned(),
            port: authority.port_u16().unwrap_or(if tls { 443 } else { 80 }),
            tls,
        })
    }
}

/// Where the request for `url` goes, and the request: a POST of `body` as
/// JSON if given, else a GET.
fn prepare(url: &str, body: Option<Vec<u8>>) -> Result<(Target, Request<Full<Bytes>>), String> {
    let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(format!("not an http:// or https:// URL: {url}"));
    }
    let target = Target::of(&uri)?;
    let authority = uri.authority().ok_or("a URL without a host")?;
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let request = Request::builder()
        .uri(path)
        .header(HOST, authority.as_str())
        .header(USER_AGENT, concat!("licash/", env!("CARGO_PKG_VERSION")));
    let request = match body {
        Some(body) => request
            .method(Method::POST)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body))),
        None => request.method(Method::GET).body(Full::default()),
    };
    let request = request.map_err(|e| e.to_string())?;
    Ok((target, request))
}

/// `request` over `io`, one HTTP/1.1 exchange: the answer's status and
/// body, of at most `max` bytes.
async fn send<I>(io: I, request: Request<Full<Bytes>>, max: usize) -> Result<(u16, Vec<u8>), String>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    let connection = tokio::spawn(connection);
    let answer = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), max)
            .collect()
            .await
            .map_err(|e| format!("the answer: {e}"))?;
        Ok((status, body.to_bytes().to_vec()))
    }
    .await;
    connection.abort();
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, port: u16, tls: bool) -> Target {
        Target {
            host: host.into(),
            port,
            tls,
        }
    }

    #[test]
    fn requests_go_to_the_host_and_port_of_their_url() {
        let onion = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion";
        let (to, request) = prepare(&format!("http://{onion}/v1/info"), None).unwrap();
        assert_eq!(to, target(onion, 80, false));
        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.uri(), "/v1/info");
        assert_eq!(request.headers()[HOST], onion);

        let url = "https://hub.example:8443/v1/swaps?x=1";
        let (to, request) = prepare(url, Some(b"{}".to_vec())).unwrap();
        assert_eq!(to, target("hub.example", 8443, true));
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri(), "/v1/swaps?x=1");
        assert_eq!(request.headers()[HOST], "hub.example:8443");
        assert_eq!(request.headers()[CONTENT_TYPE], "application/json");

        let (to, request) = prepare("https://hub.example", None).unwrap();
        assert_eq!(to, target("hub.example", 443, true));
        assert_eq!(request.uri(), "/");
    }

    #[test]
    fn only_http_and_https_urls_are_requested() {
        for url in [
            "ftp://hub.example/",
            "ws://hub.example/",
            "socks5h://127.0.0.1:9050",
            "/v1/info",
            "hub",
        ] {
            assert!(prepare(url, None).is_err(), "{url}");
        }
    }

    #[test]
    fn streams_go_to_the_host_and_port_of_their_url() {
        let at = |url: &str| Target::of(&url.parse().unwrap());
        let onion = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion";
        assert_eq!(at(&format!("ws://{onion}")), Ok(target(onion, 80, false)));
        let relay = "relay.example";
        assert_eq!(at("wss://relay.example/"), Ok(target(relay, 443, true)));
        assert_eq!(
            at("wss://relay.example:7777"),
            Ok(target(relay, 7777, true))
        );
        assert_eq!(at("https://relay.example"), Ok(target(relay, 443, true)));
        assert!(at("ftp://relay.example").is_err());
        assert!(at("/v1/info").is_err());
    }

    #[test]
    fn streams_share_circuits_only_with_those_to_the_same_host() {
        let isolation = Isolation::default();
        let hub = isolation.of("hub.example");
        assert_eq!(isolation.of("HUB.example"), hub);
        assert_ne!(isolation.of("relay.example"), hub);
        assert_ne!(isolation.of("relay.example"), isolation.of("other.example"));
        assert_eq!(isolation.of("hub.example"), hub);
    }

    #[test]
    fn hosts_past_the_most_kept_start_over_apart() {
        let isolation = Isolation::default();
        let first = isolation.of("host0.example");
        for i in 1..MAX_ISOLATED {
            isolation.of(&format!("host{i}.example"));
        }
        assert_eq!(isolation.of("host0.example"), first);
        let next = isolation.of("one-more.example");
        assert_eq!(isolation.0.lock().unwrap().len(), 1);
        assert_eq!(isolation.of("one-more.example"), next);
        assert_ne!(isolation.of("host0.example"), first);
    }

    /// The Tor Project's check that a request came over Tor.
    #[tokio::test]
    #[ignore = "reaches the Tor network"]
    async fn requests_reach_the_clear_web_over_tor() {
        let dir = tempfile::tempdir().unwrap();
        let tor = Tor::new(dir.path()).unwrap();
        let url = "https://check.torproject.org/api/ip";
        let (status, body) = tor.request(url, None, 1 << 16).await.unwrap();
        assert_eq!(status, 200);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("\"IsTor\":true"), "{body}");
    }
}
