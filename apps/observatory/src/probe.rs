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
    /// Directly: for regtest and tests, or a VPN of one's own. Onion
    /// services are skipped, and unless `local`, so is any host resolving
    /// to no public address: a URL anyone may announce reaches nothing on
    /// the observatory's own machine or network.
    Direct { tls: TlsConnector, local: bool },
}

impl Dialer {
    /// [`Dialer::Direct`] to public addresses only.
    pub fn direct() -> Self {
        Self::direct_reaching(false)
    }

    /// [`Dialer::Direct`], to local and private addresses too if `local`:
    /// for regtest, whose services run there.
    pub fn direct_reaching(local: bool) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = TlsConnector::from(Arc::new(tls));
        Self::Direct { tls, local }
    }

    /// This dialer, reaching local and private addresses too if direct:
    /// for a URL its operator gave, not one anyone may announce.
    pub fn trusting(&self) -> Self {
        match self {
            Self::Tor(tor) => Self::Tor(tor.clone()),
            Self::Direct { tls, .. } => Self::Direct {
                tls: tls.clone(),
                local: true,
            },
        }
    }

    /// Whether it reaches `url`: onion services only over Tor.
    pub fn reaches(&self, url: &str) -> bool {
        matches!(self, Self::Tor(_)) || !licash_nostr::onion(url)
    }

    async fn open(&self, url: &str) -> Result<Box<dyn Stream>, String> {
        match self {
            // Apart: on circuits of its own, none kept from another probe.
            Self::Tor(tor) => tor.apart().connect(url).await,
            Self::Direct { tls, local } => {
                let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
                let host = uri.host().ok_or("no host")?;
                let host = host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned();
                let https = uri.scheme_str() == Some("https");
                let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
                // Resolved once, and connected to only as vetted.
                let resolved = tokio::net::lookup_host((host.as_str(), port))
                    .await
                    .map_err(|e| format!("{host}: {e}"))?;
                let vetted: Vec<_> = resolved.filter(|a| *local || public(a.ip())).collect();
                if vetted.is_empty() {
                    return Err(format!("{host} has no public address"));
                }
                let tcp = tokio::net::TcpStream::connect(&vetted[..])
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

/// Whether `ip` is reachable on the internet at large: none of loopback,
/// private, shared (CGNAT), link-local, unspecified, documentation,
/// benchmarking, multicast or reserved addresses, nor an IPv6 one mapping
/// such an IPv4 one.
pub fn public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || (a == 192 && b == 0 && v4.octets()[2] == 0))
        }
        IpAddr::V6(v6) => {
            // Mapped, or translated by NAT64 (64:ff9b::/96): as its IPv4.
            let [a, b, c, d, e, f, ..] = v6.segments();
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            if (a, b, c, d, e, f) == (0x64, 0xff9b, 0, 0, 0, 0) {
                let [.., w, x, y, z] = v6.octets();
                return public(IpAddr::V4([w, x, y, z].into()));
            }
            let first = a;
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first, b) == (0x2001, 0x0db8))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_of_the_internet_at_large_are_public() {
        let public = |ip: &str| public(ip.parse().unwrap());
        for ip in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700::1111",
            "64:ff9b::101:101",
        ] {
            assert!(public(ip), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!public(ip), "{ip}");
        }
    }

    /// A URL anyone may announce reaches nothing local over a direct
    /// dialer, whatever answers there; a local dialer, for regtest, does.
    #[tokio::test]
    async fn a_direct_dialer_reaches_no_local_service_unless_local() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        for host in ["127.0.0.1", "localhost", "[::ffff:127.0.0.1]"] {
            let url = format!("http://{host}:{port}");
            let refused = Dialer::direct().open(&url).await.err();
            assert!(
                refused.is_some_and(|e| e.contains("no public address")),
                "{url}"
            );
        }
        let url = format!("http://127.0.0.1:{port}");
        assert!(Dialer::direct_reaching(true).open(&url).await.is_ok());
        assert!(Dialer::direct().trusting().open(&url).await.is_ok());
    }
}
