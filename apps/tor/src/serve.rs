//! An axum router served as an onion service.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use futures_util::StreamExt;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
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
            while let Some(request) = streams.next().await {
                tokio::spawn(answer(request, router.clone()));
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
        let _ = request.reject(End::new_with_reason(EndReason::DONE)).await;
        return;
    }
    let stream = match request.accept(Connected::new_empty()).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!("an onion stream not accepted: {e}");
            return;
        }
    };
    let service = TowerToHyperService::new(router);
    let connection = hyper::server::conn::http1::Builder::new()
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
