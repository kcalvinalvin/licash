//! A probe of one service URL, timed in parts so that Tor's share of the
//! time can be told apart from the service's.
//!
//! A probe opens a connection of its own, on a circuit of its own when over
//! Tor, and asks two things on it:
//!
//! 1. `open`: the connection opened. Over Tor this is mostly Tor's: the
//!    circuit, the exit's (or the onion service's) connection, and TLS.
//! 2. `rtt`: `GET /health` on the open connection. A round trip through
//!    the circuit to the service, which answers it at once.
//! 3. `info`: the service's info, on the same connection. What it takes
//!    beyond `rtt` is the service's own work.
//!
//! Each probe takes a fresh circuit, so that a service's numbers are the
//! median over many random circuits, as fair to one service as to another.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Empty, Limited};
use hyper::{
    body::Bytes,
    header::{HOST, USER_AGENT},
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use licash_tor::{Stream, Tor};
use tokio_rustls::{rustls, TlsConnector};

/// The largest info read.
const MAX_BODY: usize = 1 << 20;

/// How the observatory reaches services.
#[derive(Clone)]
pub enum Dialer {
    /// Through Tor, a new circuit for each probe.
    Tor(Tor),
    /// Directly: for regtest and tests. Onion services are skipped.
    Direct(TlsConnector),
}

impl Dialer {
    pub fn direct() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self::Direct(TlsConnector::from(Arc::new(tls)))
    }

    /// Whether it reaches `url`: onion services only over Tor.
    pub fn reaches(&self, url: &str) -> bool {
        matches!(self, Self::Tor(_)) || !licash_nostr::onion(url)
    }

    async fn open(&self, url: &str) -> Result<Box<dyn Stream>, String> {
        match self {
            // Apart: on circuits of its own, none kept from another probe.
            Self::Tor(tor) => tor.apart().connect(url).await,
            Self::Direct(tls) => {
                let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
                let host = uri.host().ok_or("no host")?.to_owned();
                let https = uri.scheme_str() == Some("https");
                let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
                let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|e| e.to_string())?;
                if !https {
                    return Ok(Box::new(tcp));
                }
                let name = rustls::pki_types::ServerName::try_from(host)
                    .map_err(|e| format!("not a TLS name: {e}"))?;
                let tls = tls
                    .connect(name, tcp)
                    .await
                    .map_err(|e| format!("TLS: {e}"))?;
                Ok(Box::new(tls))
            }
        }
    }
}

/// A probe's times, in milliseconds, as far as it went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Times {
    pub open: Option<u32>,
    pub rtt: Option<u32>,
    pub info: Option<u32>,
}

/// What a probe found: its times, and the info's body, or why not.
#[derive(Debug)]
pub struct Probed {
    pub times: Times,
    pub info: Result<Vec<u8>, String>,
}

/// Probe the service at `base` (a URL, a path prefix allowed): its health,
/// then its info at `info_path`, within `timeout` in all.
pub async fn probe(dialer: &Dialer, base: &str, info_path: &str, timeout: Duration) -> Probed {
    let mut times = Times::default();
    let info = tokio::time::timeout(timeout, steps(dialer, base, info_path, &mut times)).await;
    let info = info.unwrap_or_else(|_| Err(format!("no answer in {} s", timeout.as_secs())));
    Probed { times, info }
}

async fn steps(
    dialer: &Dialer,
    base: &str,
    info_path: &str,
    times: &mut Times,
) -> Result<Vec<u8>, String> {
    let base = base.trim_end_matches('/');
    let uri: Uri = base.parse().map_err(|e| format!("not a URL: {e}"))?;
    let authority = uri.authority().ok_or("no host")?.to_string();
    let prefix = uri.path().trim_end_matches('/').to_owned();

    let started = Instant::now();
    let stream = dialer.open(base).await?;
    times.open = Some(ms(started));
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("HTTP: {e}"))?;
    let driving = tokio::spawn(connection);

    let started = Instant::now();
    let health = get(&mut sender, &authority, &format!("{prefix}/health")).await;
    let health = health.map(|(status, _)| status);
    times.rtt = Some(ms(started));
    let answer = match health {
        Ok(200) => {
            let started = Instant::now();
            let info = get(&mut sender, &authority, &format!("{prefix}{info_path}")).await;
            times.info = Some(ms(started));
            match info {
                Ok((200, body)) => Ok(body),
                Ok((status, _)) => Err(format!("info: status {status}")),
                Err(e) => Err(format!("info: {e}")),
            }
        }
        Ok(status) => Err(format!("health: status {status}")),
        Err(e) => Err(format!("health: {e}")),
    };
    driving.abort();
    answer
}

async fn get(
    sender: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>>,
    authority: &str,
    path: &str,
) -> Result<(u16, Vec<u8>), String> {
    sender.ready().await.map_err(|e| e.to_string())?;
    let request = Request::get(path)
        .header(HOST, authority)
        .header(USER_AGENT, "licash-observatory")
        .body(Empty::new())
        .map_err(|e| e.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = Limited::new(response.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((status, body.to_vec()))
}

fn ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}
