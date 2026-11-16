//! An axum router served as an onion service.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use futures_util::StreamExt;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use safelog::DisplayRedacted;
use tor_cell::relaycell::msg::{Connected, End, EndReason};
use tor_hsservice::{
    config::OnionServiceConfigBuilder, HsNickname, RunningOnionService, StreamRequest,
};
use tor_proto::stream::IncomingStreamRequest;

use crate::Tor;

/// The port the service answers on.
const PORT: u16 = 80;

/// How soon to try connecting to the Tor network again.
const RETRY: Duration = Duration::from_secs(60);

/// Bound accepted streams, including clients that never send HTTP headers.
const MAX_STREAMS: usize = 128;
const HEADERS: Duration = Duration::from_secs(120);

/// An onion service, served until dropped.
pub struct Onion {
    url: String,
    _service: Arc<RunningOnionService>,
    serving: tokio::task::JoinHandle<()>,
}

impl Onion {
    /// Where it answers: `http://<address>.onion`.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for Onion {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

impl Tor {
    /// Serve `router` on port 80 of the onion service `nickname`, at the
    /// same address on every start: the client keeps its key in its state.
    /// Within a Tokio runtime.
    pub fn serve(&self, nickname: &str, router: axum::Router) -> anyhow::Result<Onion> {
        let nickname: HsNickname = nickname.parse().context("an onion service's nickname")?;
        let config = OnionServiceConfigBuilder::default()
            .nickname(nickname)
            .build()?;
        let (service, requests) = self
            .client
            .launch_onion_service(config)?
            .context("the onion service is disabled")?;
        let address = service
            .onion_address()
            .context("the onion service has no address")?;
        let url = format!("http://{}", address.display_unredacted());
        let mut streams = Box::pin(tor_hsservice::handle_rend_requests(requests));
        let client = self.client.clone();
        let serving = tokio::spawn(async move {
            // The service publishes itself once the client holds the
            // network's directory, which only a bootstrap fetches.
            while let Err(e) = client.bootstrap().await {
                tracing::warn!("Tor, for the onion service: {e}");
                tokio::time::sleep(RETRY).await;
            }
            let mut tasks = tokio::task::JoinSet::new();
            while let Some(request) = streams.next().await {
                while tasks.try_join_next().is_some() {}
                if tasks.len() >= MAX_STREAMS {
                    // No task is spawned for an excess stream. Bound even
                    // rejection so the accept loop cannot hang on a peer.
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        request.reject(End::new_with_reason(EndReason::RESOURCELIMIT)),
                    )
                    .await;
                    continue;
                }
                tasks.spawn(answer(request, router.clone()));
            }
        });
        Ok(Onion {
            url,
            _service: service,
            serving,
        })
    }
}

/// Serve `router` on the stream `request` asks for, if it asks for port 80.
async fn answer(request: StreamRequest, router: axum::Router) {
    let port = match request.request() {
        IncomingStreamRequest::Begin(begin) => Some(begin.port()),
        _ => None,
    };
    if port != Some(PORT) {
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            request.reject(End::new_with_reason(EndReason::DONE)),
        )
        .await;
        return;
    }
    let stream = match tokio::time::timeout(HEADERS, request.accept(Connected::new_empty())).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            tracing::debug!("an onion stream not accepted: {e}");
            return;
        }
        Err(_) => return,
    };
    http(stream, router).await;
}

/// A header deadline releases idle streams without limiting the duration
/// of a legitimate response (contract downloads can take minutes).
async fn http(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    router: axum::Router,
) {
    let service = TowerToHyperService::new(router);
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADERS)
        .serve_connection(TokioIo::new(stream), service)
        .await;
    if let Err(e) = connection {
        tracing::debug!("an onion connection: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_idle_http_stream_times_out_and_a_normal_request_still_works() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let (_client, server) = tokio::io::duplex(4096);
        let idle = tokio::spawn(http(server, router.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(HEADERS + Duration::from_secs(1)).await;
        idle.await.unwrap();

        let (mut client, server) = tokio::io::duplex(4096);
        let served = tokio::spawn(http(server, router));
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
        served.await.unwrap();
    }

    /// One Tor client serves a router, another reaches it once its service
    /// is published.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "reaches the Tor network"]
    async fn a_router_answers_as_an_onion_service() {
        let (served, asking) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let onion = Tor::new(served.path())
            .unwrap()
            .serve("licash-test", router)
            .unwrap();
        assert!(onion.url().ends_with(".onion"), "{}", onion.url());
        let client = Tor::new(asking.path()).unwrap();
        let url = format!("{}/health", onion.url());
        let mut last = String::new();
        for _ in 0..20 {
            match client.request(&url, None, 1024).await {
                Ok((200, body)) => return assert_eq!(body, b"ok"),
                answer => last = format!("{answer:?}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        panic!("{url}: {last}");
    }
}
