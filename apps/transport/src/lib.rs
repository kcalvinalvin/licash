//! How the applications reach hubs and swap providers: HTTP(S), directly,
//! through a proxy such as Tor's SOCKS port, or, with the `tor` feature,
//! through a Tor client of their own.

use std::time::Duration;

use anyhow::Context;
use licash_wallet::{Response, Transport};

/// The most an answer may hold: far above any grant.
const MAX_ANSWER: usize = 4 << 20;

/// A wallet's [`Transport`] over HTTP(S).
pub struct Http(Route);

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
        Ok(Self(Route::Direct(client.build()?)))
    }

    /// Through `tor`, a Tor client of the application's own, to onion
    /// services and to the clear web alike.
    #[cfg(feature = "tor")]
    pub fn tor(tor: licash_tor::Tor) -> Self {
        Self(Route::Tor(tor))
    }
}

impl Transport for Http {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        let client = match &self.0 {
            Route::Direct(client) => client,
            #[cfg(feature = "tor")]
            Route::Tor(tor) => {
                let url = url.to_owned();
                // Boxed: Arti's futures are too deep a type to nest in the
                // callers'.
                let tor: futures_util::future::BoxFuture<'_, _> = Box::pin(async move {
                    let (status, body) = tor.request(&url, body, MAX_ANSWER).await?;
                    Ok(Response { status, body })
                });
                return futures_util::future::Either::Left(tor);
            }
        };
        let request = match body {
            Some(body) => client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            None => client.get(url),
        };
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
                if body.len() + chunk.len() > MAX_ANSWER {
                    return Err(format!("an answer over {MAX_ANSWER} bytes"));
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
