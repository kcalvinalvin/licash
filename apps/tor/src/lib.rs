//! Tor for the applications, through a Tor client of their own (Arti):
//! HTTP(S) requests over it, to onion services and to the clear web alike,
//! and an axum router served as an onion service of their own.
//!
//! Arti may end the process if the Tor network's consensus declares its
//! version obsolete.

mod serve;

use std::{path::Path, sync::Arc, time::Duration};

use arti_client::{config::TorClientConfigBuilder, TorClient};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Bytes,
    header::{CONTENT_TYPE, HOST, USER_AGENT},
    Method, Request, Uri,
};
use hyper_util::rt::TokioIo;
use tokio_rustls::{rustls, TlsConnector};
use tor_rtcompat::PreferredRuntime;

pub use serve::Onion;

/// How long a request may take, its circuit included.
const TIMEOUT: Duration = Duration::from_secs(120);

/// A Tor client, shared by its clones.
#[derive(Clone)]
pub struct Tor {
    client: Arc<TorClient<PreferredRuntime>>,
    tls: TlsConnector,
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
        tokio::time::timeout(TIMEOUT, self.exchange(url, body, max))
            .await
            .unwrap_or_else(|_| Err(format!("no answer over Tor in {} s", TIMEOUT.as_secs())))
    }

    async fn exchange(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        let (target, request) = prepare(url, body)?;
        let stream = self
            .client
            .connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| format!("over Tor: {e}"))?;
        if !target.https {
            return send(TokioIo::new(stream), request, max).await;
        }
        let name = rustls::pki_types::ServerName::try_from(target.host)
            .map_err(|e| format!("not a TLS name: {e}"))?;
        let stream = self
            .tls
            .connect(name, stream)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        send(TokioIo::new(stream), request, max).await
    }
}

/// Where a request goes.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    https: bool,
}

/// Where the request for `url` goes, and the request: a POST of `body` as
/// JSON if given, else a GET.
fn prepare(url: &str, body: Option<Vec<u8>>) -> Result<(Target, Request<Full<Bytes>>), String> {
    let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        _ => return Err(format!("not an http:// or https:// URL: {url}")),
    };
    let authority = uri.authority().ok_or("a URL without a host")?;
    let host = authority.host().to_owned();
    let port = authority.port_u16().unwrap_or(if https { 443 } else { 80 });
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
    Ok((Target { host, port, https }, request))
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

    fn target(host: &str, port: u16, https: bool) -> Target {
        Target {
            host: host.into(),
            port,
            https,
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
            "socks5h://127.0.0.1:9050",
            "/v1/info",
            "hub",
        ] {
            assert!(prepare(url, None).is_err(), "{url}");
        }
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
