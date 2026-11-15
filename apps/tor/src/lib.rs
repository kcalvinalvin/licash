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
    time::{Duration, Instant},
};

use arti_client::{config::TorClientConfigBuilder, IsolationToken, StreamPrefs, TorClient};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Bytes,
    client::conn::http1,
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

/// How long a connection to a host stays open, idle, for the next request
/// to it: a payment's few requests, one after the other, share one.
const KEPT_FOR: Duration = Duration::from_secs(20);

/// The most idle connections kept open at once.
const MOST_KEPT: usize = 16;

/// How long a connection kept may take to be ready for the next request:
/// its own task taking in the last answer's end, not the network.
const READY: Duration = Duration::from_secs(1);

/// A Tor client, shared by its clones.
#[derive(Clone)]
pub struct Tor {
    client: Arc<TorClient<PreferredRuntime>>,
    tls: TlsConnector,
    isolation: Arc<Isolation>,
    kept: Arc<Kept>,
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
            kept: Arc::new(Kept::new(KEPT_FOR)),
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
        let get = request.method() == Method::GET;
        let reused = tokio::time::timeout_at(deadline, self.kept.reuse(&target, request, max));
        let request = match reused.await {
            Err(_) => {
                let why = format!("no answer over Tor in {} s", timeout.as_secs());
                return Err(Failed::Sent(why));
            }
            Ok(Ok(Ok(answer))) => return Ok(answer),
            // The connection kept may have closed as it was asked: a GET is
            // asked again.
            Ok(Ok(Err(_))) if get => prepare(url, None).map_err(Failed::Unsent)?.1,
            Ok(Ok(Err(why))) => return Err(Failed::Sent(why)),
            Ok(Err(request)) => *request,
        };
        // A cold start is not the host's slowness: `open` counts after it.
        tokio::time::timeout_at(deadline, self.client.bootstrap())
            .await
            .map_err(|_| Failed::Unsent(format!("no Tor in {} s", timeout.as_secs())))?
            .map_err(|e| Failed::Unsent(format!("bootstrapping Tor: {e}")))?;
        let open_by = deadline.min(tokio::time::Instant::now() + open);
        let opening = tokio::time::timeout_at(open_by, self.stream(target.clone()));
        let stream = opening
            .await
            .map_err(|_| Failed::Unsent(format!("no stream over Tor in {} s", open.as_secs())))?
            .map_err(Failed::Unsent)?;
        tokio::time::timeout_at(deadline, self.kept.first(target, stream, request, max))
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
        let get = request.method() == Method::GET;
        let request = match self.kept.reuse(&target, request, max).await {
            Ok(Ok(answer)) => return Ok(answer),
            // The connection kept may have closed as it was asked: a GET is
            // asked again.
            Ok(Err(_)) if get => prepare(url, None)?.1,
            Ok(Err(why)) => return Err(why),
            Err(request) => *request,
        };
        let stream = self.stream(target.clone()).await?;
        self.kept.first(target, stream, request, max).await
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// An HTTP/1.1 request as sent over Tor.
type Asked = Request<Full<Bytes>>;

/// An answer: its status and body.
type Answer = (u16, Vec<u8>);

/// HTTP/1.1 connections kept open once answered, idle, each for the next
/// request to the same host and port: no stream to open nor TLS handshake
/// to make for it, a second or two over Tor. Streams to a host share their
/// circuits already, and a host can tell they are one client's (TLS
/// resumes its sessions; an onion service sees one circuit), so keeping a
/// connection open links no more requests than before.
struct Kept {
    open: Mutex<Vec<(Target, Instant, Open)>>,
    /// How long a connection is kept, idle.
    keep: Duration,
}

/// An HTTP/1.1 connection, its task ended as it is dropped.
struct Open {
    sender: http1::SendRequest<Full<Bytes>>,
    task: tokio::task::AbortHandle,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How a request on a connection failed.
enum Lost {
    /// Not sent: the connection had closed. The request back.
    Unsent(Box<Asked>),
    /// Sent, in part or whole: its host may have had it.
    Sent(String),
}

impl Open {
    /// An HTTP/1.1 connection over `stream`, served by a task of its own.
    async fn over(stream: Box<dyn Stream>) -> Result<Self, String> {
        let (sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        let task = tokio::spawn(connection).abort_handle();
        Ok(Self { sender, task })
    }

    /// `request` on this connection: its answer, with a body of at most
    /// `max` bytes.
    async fn ask(&mut self, request: Asked, max: usize) -> Result<Answer, Lost> {
        let response = match self.sender.try_send_request(request).await {
            Ok(response) => response,
            Err(mut e) => {
                return Err(match e.take_message() {
                    Some(request) => Lost::Unsent(Box::new(request)),
                    None => Lost::Sent(e.into_error().to_string()),
                })
            }
        };
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), max)
            .collect()
            .await
            .map_err(|e| Lost::Sent(format!("the answer: {e}")))?;
        Ok((status, body.to_bytes().to_vec()))
    }
}

impl Kept {
    fn new(keep: Duration) -> Self {
        Self {
            open: Mutex::default(),
            keep,
        }
    }

    /// The connections kept, those expired or closed let go.
    fn kept(&self) -> std::sync::MutexGuard<'_, Vec<(Target, Instant, Open)>> {
        let mut kept = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        kept.retain(|(_, at, open)| {
            now.duration_since(*at) < self.keep && !open.sender.is_closed()
        });
        kept
    }

    /// A connection to `target` kept and ready for a request, the latest
    /// kept first, if any.
    async fn take(&self, target: &Target) -> Option<Open> {
        loop {
            let mut open = {
                let mut kept = self.kept();
                let at = kept.iter().rposition(|(to, ..)| to == target)?;
                kept.remove(at).2
            };
            if let Ok(Ok(())) = tokio::time::timeout(READY, open.sender.ready()).await {
                return Some(open);
            }
        }
    }

    /// `open`, its last answer read whole, kept for the next request to
    /// `target`: the oldest kept let go past [`MOST_KEPT`].
    fn put(&self, target: Target, open: Open) {
        if open.sender.is_closed() {
            return;
        }
        let mut kept = self.kept();
        if kept.len() >= MOST_KEPT {
            kept.remove(0);
        }
        kept.push((target, Instant::now(), open));
    }

    /// `request` on a connection kept to `target`: its answer, or how it
    /// failed once sent; or, none kept taking it, the request back.
    async fn reuse(
        &self,
        target: &Target,
        mut request: Asked,
        max: usize,
    ) -> Result<Result<Answer, String>, Box<Asked>> {
        while let Some(mut open) = self.take(target).await {
            match open.ask(request, max).await {
                Ok(answer) => {
                    self.put(target.clone(), open);
                    return Ok(Ok(answer));
                }
                // Closed: the next kept, or a new one.
                Err(Lost::Unsent(back)) => request = *back,
                Err(Lost::Sent(why)) => return Ok(Err(why)),
            }
        }
        Err(Box::new(request))
    }

    /// `request` on a new connection over `stream` to `target`: its
    /// answer, the connection kept once it is read whole.
    async fn first(
        &self,
        target: Target,
        stream: Box<dyn Stream>,
        request: Asked,
        max: usize,
    ) -> Result<Answer, String> {
        let mut open = Open::over(stream).await?;
        let answer = open.ask(request, max).await.map_err(|lost| match lost {
            Lost::Unsent(_) => "the connection closed".to_owned(),
            Lost::Sent(why) => why,
        })?;
        self.put(target, open);
        Ok(answer)
    }
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

    /// An HTTP/1.1 server on a loopback port answering `ok`, closing the
    /// connection if the path asks it (`/close`): its address, and how many
    /// connections it took.
    async fn server() -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let taken = Arc::new(AtomicUsize::new(0));
        let counted = taken.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let service = hyper::service::service_fn(
                    |request: Request<hyper::body::Incoming>| async move {
                        let mut answer = hyper::Response::builder();
                        if request.uri().path() == "/close" {
                            answer = answer.header(hyper::header::CONNECTION, "close");
                        }
                        answer.body(Full::new(Bytes::from_static(b"ok")))
                    },
                );
                tokio::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (at, taken)
    }

    /// GET `path` from the server at `at` as host `to`: on a connection
    /// kept, or a new one.
    async fn get(kept: &Kept, at: std::net::SocketAddr, to: &Target, path: &str) -> Answer {
        let url = format!("http://{}:{}{path}", to.host, to.port);
        let (_, request) = prepare(&url, None).unwrap();
        match kept.reuse(to, request, 1 << 10).await {
            Ok(answer) => answer.unwrap(),
            Err(request) => {
                let stream = tokio::net::TcpStream::connect(at).await.unwrap();
                kept.first(to.clone(), Box::new(stream), *request, 1 << 10)
                    .await
                    .unwrap()
            }
        }
    }

    fn taken(count: &std::sync::atomic::AtomicUsize) -> usize {
        count.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_hosts_requests_share_a_connection_kept_open() {
        let (at, count) = server().await;
        let kept = Kept::new(KEPT_FOR);
        let hub = target("hub.example", at.port(), false);
        for _ in 0..3 {
            assert_eq!(
                get(&kept, at, &hub, "/v1/info").await,
                (200, b"ok".to_vec())
            );
        }
        assert_eq!(taken(&count), 1);
        // Another host: a connection of its own, as on its own circuits.
        let relay = target("relay.example", at.port(), false);
        assert_eq!(get(&kept, at, &relay, "/").await.0, 200);
        assert_eq!(taken(&count), 2);
        assert_eq!(get(&kept, at, &hub, "/v1/info").await.0, 200);
        assert_eq!(taken(&count), 2);
    }

    #[tokio::test]
    async fn a_connection_idle_too_long_or_closed_is_not_used_again() {
        let (at, count) = server().await;
        let hub = target("hub.example", at.port(), false);
        let expired = Kept::new(Duration::ZERO);
        get(&expired, at, &hub, "/").await;
        get(&expired, at, &hub, "/").await;
        assert_eq!(taken(&count), 2);

        let kept = Kept::new(KEPT_FOR);
        get(&kept, at, &hub, "/close").await;
        get(&kept, at, &hub, "/").await;
        assert_eq!(taken(&count), 4);
        get(&kept, at, &hub, "/").await;
        assert_eq!(taken(&count), 4);
    }

    #[tokio::test]
    async fn at_most_so_many_connections_are_kept() {
        let (at, count) = server().await;
        let kept = Kept::new(KEPT_FOR);
        for i in 0..=MOST_KEPT {
            get(
                &kept,
                at,
                &target(&format!("host{i}.example"), at.port(), false),
                "/",
            )
            .await;
        }
        assert_eq!(kept.kept().len(), MOST_KEPT);
        // The oldest let go: asked again, it connects anew.
        get(&kept, at, &target("host0.example", at.port(), false), "/").await;
        assert_eq!(taken(&count), MOST_KEPT + 2);
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
