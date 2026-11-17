//! How the applications reach hubs and swap providers: HTTP(S), directly,
//! through a proxy such as Tor's SOCKS port, or, with the `tor` feature,
//! through a Tor client of their own.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Context;
use licash_wallet::{Response, Transport};

/// How long an onion service standing in for a hub has to open a stream
/// before the hub's own URL is asked instead, still over Tor.
#[cfg(feature = "tor")]
const ONION_OPEN: Duration = Duration::from_secs(30);

/// How long an onion service whose stream did not open is passed over.
const ONION_RESTS: Duration = Duration::from_secs(10 * 60);

/// The most an answer may hold: far above any grant.
const MAX_ANSWER: usize = 4 << 20;

/// How long a [`Transport::download`] may take: its answer is large.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(900);

/// How long an answer may take, but for sending a large body.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// A body larger than this is given time to be sent ([`sending`]).
const LARGE_BODY: usize = 1 << 20;

/// The slowest a large body is sent, in bytes a second: Tor's slowest
/// circuits, from a phone.
const SLOWEST_SEND: usize = 32 << 10;

/// How long POSTing `bytes` and its answer may take, if that is longer
/// than a client's own: an instance of a contract's opening, 25 MB, has
/// 15 minutes.
fn sending(bytes: usize) -> Option<Duration> {
    let secs = (bytes / SLOWEST_SEND) as u64;
    (bytes > LARGE_BODY).then(|| ANSWER_TIMEOUT + Duration::from_secs(secs))
}

/// A wallet's [`Transport`] over HTTP(S).
pub struct Http {
    route: Route,
    onions: Onions,
}

/// Onion services standing in for hubs' own URLs, over Tor only: by the
/// hub's URL, the onion service's and when a stream to it last did not
/// open.
#[derive(Default)]
struct Onions(Mutex<HashMap<String, (String, Option<Instant>)>>);

impl Onions {
    /// `onion` for the hub at `base`, if both are as [`Http::set_onion`]
    /// takes them, the one known kept otherwise; none if None: whether
    /// taken. The same again keeps resting if it rests.
    fn set(&self, base: &str, onion: Option<&str>) -> bool {
        let base = base.trim_end_matches('/');
        let mut onions = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(onion) = onion else {
            onions.remove(base);
            return false;
        };
        // Not for a hub over plain HTTP: what it says of itself crosses a
        // Tor exit unauthenticated, and its URLs are not in its proof.
        let usable = !base.contains(char::is_whitespace)
            && base.starts_with("https://")
            && !onion_host(base);
        let Some(onion) = onion_url(onion).filter(|_| usable) else {
            return false;
        };
        if onions.get(base).is_none_or(|(known, _)| *known != onion) {
            onions.insert(base.to_owned(), (onion, None));
        }
        true
    }

    /// For `url`, the hub's URL it is under, the longest, and `url` at the
    /// onion service standing in for it, unless that one rests since it
    /// did not open.
    #[cfg_attr(not(feature = "tor"), allow(dead_code))]
    fn route(&self, url: &str, now: Instant) -> Option<(String, String)> {
        let onions = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (base, (onion, unopened), rest) = onions
            .iter()
            .filter_map(|(base, known)| {
                let rest = url.strip_prefix(base.as_str())?;
                rest.starts_with('/').then_some((base, known, rest))
            })
            .max_by_key(|(base, _, _)| base.len())?;
        let resting = unopened.is_some_and(|at| now.saturating_duration_since(at) < ONION_RESTS);
        (!resting).then(|| (base.clone(), format!("{onion}{rest}")))
    }

    /// That a stream to the onion service for `base` did not open at `now`.
    #[cfg_attr(not(feature = "tor"), allow(dead_code))]
    fn unopened(&self, base: &str, now: Instant) {
        let mut onions = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, unopened)) = onions.get_mut(base) {
            *unopened = Some(now);
        }
    }
}

/// `url` if that of an onion service as a hub names its own: `http://`, a
/// v3 address and a port at most, no path; in lower case, without a
/// trailing `/`.
fn onion_url(url: &str) -> Option<String> {
    let url = url.trim().to_ascii_lowercase();
    let rest = url.strip_prefix("http://")?.trim_end_matches('/');
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    let address = host.strip_suffix(".onion")?;
    let v3 = address.len() == 56
        && address
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b));
    let port = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
    (v3 && port).then(|| format!("http://{rest}"))
}

/// Whether the host of `url` is an onion service.
fn onion_host(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split(['/', ':', '?', '#']).next().unwrap_or(host);
    host.to_ascii_lowercase().ends_with(".onion")
}

enum Route {
    Direct(reqwest::Client),
    #[cfg(feature = "tor")]
    Tor(licash_tor::Tor),
}

impl Http {
    /// Through `proxy` if given, e.g. `socks5h://127.0.0.1:9050`, else
    /// directly: never through a proxy the environment names unasked.
    pub fn new(proxy: Option<&str>) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(90))
            .user_agent(concat!("licash/", env!("CARGO_PKG_VERSION")));
        client = match proxy {
            Some(proxy) => client.proxy(reqwest::Proxy::all(proxy).context("the proxy URL")?),
            None => client.no_proxy(),
        };
        // Android's verifier needs the application's Java side: Mozilla's
        // roots instead, as for Tor.
        #[cfg(target_os = "android")]
        {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let tls = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            client = client.tls_backend_preconfigured(tls);
        }
        Ok(Self {
            route: Route::Direct(client.build()?),
            onions: Onions::default(),
        })
    }

    /// Through `tor`, a Tor client of the application's own, to onion
    /// services and to the clear web alike.
    #[cfg(feature = "tor")]
    pub fn tor(tor: licash_tor::Tor) -> Self {
        Self {
            route: Route::Tor(tor),
            onions: Onions::default(),
        }
    }

    /// Reach the hub at `base` at `onion` instead, an onion service it
    /// names as its own, while over Tor: back at `base`, still over Tor,
    /// when a stream to `onion` does not open, and for a while after. None
    /// forgets the one known. Whether taken, the one known kept if not:
    /// never directly, nor for a `base` that is an onion service itself,
    /// nor an `onion` but `http://`, a v3 address and a port at most, nor
    /// a `base` but `https://`. Only for a hub the caller trusts, of what
    /// it said of itself: an onion service named by anyone else could
    /// stand in for the hub. One that opens but is not the hub's stays
    /// until set otherwise, its answers going to the caller.
    pub fn set_onion(&self, base: &str, onion: Option<&str>) -> bool {
        !matches!(self.route, Route::Direct(_)) && self.onions.set(base, onion)
    }
}

impl Transport for Http {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        // A large body on circuits of its own, as a download: sent beside
        // others to the same host.
        let timeout = body.as_ref().and_then(|b| sending(b.len()));
        self.exchange(url, body, MAX_ANSWER, timeout, timeout.is_some())
    }

    fn download(
        &self,
        url: &str,
        max: usize,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        // Over Tor on circuits of its own: one carries only so much, and
        // downloads run beside each other.
        self.exchange(url, None, max, Some(DOWNLOAD_TIMEOUT), true)
    }
}

impl Http {
    /// `body` POSTed to `url`, or a GET: an answer of at most `max` bytes,
    /// in `timeout` if given rather than the client's own. Over Tor on
    /// circuits of its own if `apart` ([`licash_tor::Tor::apart`]).
    #[cfg_attr(not(feature = "tor"), allow(clippy::infallible_destructuring_match))]
    #[cfg_attr(not(feature = "tor"), allow(unused_variables))]
    fn exchange(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
        max: usize,
        timeout: Option<Duration>,
        apart: bool,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send + '_ {
        let client = match &self.route {
            Route::Direct(client) => client,
            #[cfg(feature = "tor")]
            Route::Tor(tor) => {
                let tor = if apart { tor.apart() } else { tor.clone() };
                let url = url.to_owned();
                let timeout = timeout.unwrap_or(licash_tor::TIMEOUT);
                let onion = self.onions.route(&url, Instant::now());
                // Boxed: Arti's futures are too deep a type to nest in the
                // callers'.
                let tor: futures_util::future::BoxFuture<'_, _> = Box::pin(async move {
                    if let Some((base, at)) = onion {
                        let opened =
                            tor.request_opened(&at, body.clone(), max, ONION_OPEN, timeout);
                        match opened.await {
                            Ok((status, body)) => return Ok(Response { status, body }),
                            // Its onion service may have had it: not again.
                            Err(licash_tor::Failed::Sent(why)) => return Err(why),
                            Err(licash_tor::Failed::Unsent(why)) => {
                                tracing::info!(%base, %why, "its onion service did not open, asking its own URL");
                                self.onions.unopened(&base, Instant::now());
                            }
                        }
                    }
                    let (status, body) = tor.request_for(&url, body, max, timeout).await?;
                    Ok(Response { status, body })
                });
                return futures_util::future::Either::Left(tor);
            }
        };
        let mut request = match body {
            Some(body) => client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            None => client.get(url),
        };
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let direct = async move {
            let mut response = request
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| e.without_url().to_string())?
            {
                if body.len() + chunk.len() > max {
                    return Err(format!("an answer over {max} bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Response { status, body })
        };
        #[cfg(feature = "tor")]
        let direct = futures_util::future::Either::Right(direct);
        direct
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3() -> String {
        format!("{}.onion", "a2".repeat(28))
    }

    #[test]
    fn only_a_v3_onion_services_http_url_with_a_port_at_most_stands_in() {
        let host = v3();
        assert_eq!(
            onion_url(&format!("http://{host}")),
            Some(format!("http://{host}"))
        );
        assert_eq!(
            onion_url(&format!(" HTTP://{}:8338/ ", host.to_uppercase())),
            Some(format!("http://{host}:8338"))
        );
        for refused in [
            format!("https://{host}"),
            format!("http://{host}/v1"),
            format!("http://{host}:0"),
            format!("http://{host}:99999"),
            format!("http://user@{host}"),
            format!("http://{}.onion", "a".repeat(16)),
            format!("http://{}.onion", "1".repeat(56)),
            "http://hub.example".into(),
            "https://hub.example/x.onion".into(),
        ] {
            assert_eq!(onion_url(&refused), None, "{refused}");
        }
        assert!(onion_host(&format!("http://{host}:80/v1")));
        assert!(!onion_host("https://onion.example/a.onion"));
    }

    #[test]
    fn a_hubs_requests_go_to_its_onion_service_but_while_it_rests() {
        let onions = Onions::default();
        let (base, onion) = ("https://hub.example", format!("http://{}", v3()));
        // An onion service stands in for none of an onion service.
        assert!(!onions.set(
            &onion,
            Some(&format!("http://{}", "b".repeat(56) + ".onion"))
        ));
        assert!(!onions.set(base, Some("https://hub.example")));
        let now = Instant::now();
        assert_eq!(onions.route(&format!("{base}/v1/info"), now), None);

        assert!(onions.set(&format!("{base}/"), Some(&onion)));
        let routed = onions.route(&format!("{base}/v1/info"), now);
        assert_eq!(routed, Some((base.to_owned(), format!("{onion}/v1/info"))));
        // A hub over plain HTTP: none.
        assert!(!onions.set("http://plain.example", Some(&onion)));
        assert_eq!(onions.route("http://plain.example/v1/info", now), None);
        // Only under its URL exactly.
        assert_eq!(onions.route("https://hub.example.evil/v1/info", now), None);
        assert_eq!(onions.route("https://hub.example:8443/v1/info", now), None);
        assert_eq!(onions.route(base, now), None);

        // Not opened: its own URL for a while, though set again.
        onions.unopened(base, now);
        assert_eq!(onions.route(&format!("{base}/v1/info"), now), None);
        assert!(onions.set(base, Some(&onion)));
        let later = now + ONION_RESTS - Duration::from_secs(1);
        assert_eq!(onions.route(&format!("{base}/v1/info"), later), None);
        let rested = now + ONION_RESTS;
        assert!(onions.route(&format!("{base}/v1/info"), rested).is_some());

        // One not taken keeps the one known; none forgets it.
        assert!(!onions.set(base, Some("http://hub.example")));
        assert!(onions.route(&format!("{base}/v1"), rested).is_some());
        assert!(!onions.set(base, None));
        assert_eq!(onions.route(&format!("{base}/v1"), rested), None);
    }

    #[test]
    fn a_hub_under_another_hubs_url_goes_to_its_own_onion_service() {
        let onions = Onions::default();
        let (outer, inner) = (
            format!("http://{}", v3()),
            format!("http://{}", "b".repeat(56)),
        );
        let inner = format!("{inner}.onion");
        assert!(onions.set("https://hub.example", Some(&outer)));
        assert!(onions.set("https://hub.example/b", Some(&inner)));
        let now = Instant::now();
        for _ in 0..8 {
            let routed = onions.route("https://hub.example/b/v1/info", now);
            assert_eq!(
                routed,
                Some(("https://hub.example/b".into(), format!("{inner}/v1/info")))
            );
        }
        // Its own resting, the other's is not taken instead.
        onions.unopened("https://hub.example/b", now);
        assert_eq!(onions.route("https://hub.example/b/v1/info", now), None);
        let routed = onions.route("https://hub.example/v1/info", now);
        assert_eq!(
            routed,
            Some(("https://hub.example".into(), format!("{outer}/v1/info")))
        );
    }

    #[test]
    fn a_large_body_has_the_time_to_be_sent_over_tor() {
        assert_eq!(sending(0), None);
        assert_eq!(sending(LARGE_BODY), None);
        let instance = sending(25_024_328).unwrap();
        assert_eq!(instance, ANSWER_TIMEOUT + Duration::from_secs(763));
        assert!(sending(LARGE_BODY + 1).unwrap() > ANSWER_TIMEOUT);
    }

    #[test]
    fn a_direct_transport_takes_no_onion_service() {
        let http = Http::new(None).unwrap();
        let onion = format!("http://{}", v3());
        assert!(!http.set_onion("https://hub.example", Some(&onion)));
    }
}
