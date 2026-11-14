//! Nostr relays reached through a Tor client of the application's own.

use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use async_wsocket::Message;
use futures_util::{future, Sink, SinkExt, StreamExt};
use nostr_sdk::prelude::{
    transport::websocket::{WebSocketSink, WebSocketStream, WebSocketTransport},
    Error, Url,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::{self, Message as Native};

/// Connects to relays over Tor, `ws://` and `wss://` ones alike, onion
/// services included.
pub(crate) struct OverTor(pub(crate) licash_tor::Tor);

impl fmt::Debug for OverTor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OverTor")
    }
}

/// A connection under way: the socket's halves, as nostr-sdk takes them.
type Connecting<'a> =
    Pin<Box<dyn Future<Output = Result<(WebSocketSink, WebSocketStream), Error>> + Send + 'a>>;

impl WebSocketTransport for OverTor {
    fn support_ping(&self) -> bool {
        true
    }

    fn connect<'a>(&'a self, url: &'a Url, _proxy: Option<SocketAddr>) -> Connecting<'a> {
        Box::pin(async move {
            let stream = self
                .0
                .connect(url.as_str())
                .await
                .map_err(Error::transport)?;
            handshake(url, stream).await
        })
    }
}

/// The WebSocket handshake with the relay at `url` over `stream`: the
/// socket's halves, as nostr-sdk takes them.
pub(crate) async fn handshake<S>(
    url: &Url,
    stream: S,
) -> Result<(WebSocketSink, WebSocketStream), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (socket, _) = tokio_tungstenite::client_async(url.as_str(), stream)
        .await
        .map_err(Error::transport)?;
    let (sending, receiving) = socket.split();
    let receiving = receiving.filter_map(|message| {
        future::ready(match message {
            Ok(message) => from_native(message).map(Ok),
            Err(e) => Some(Err(Error::transport(e))),
        })
    });
    Ok((Box::pin(Sending(sending)), Box::pin(receiving)))
}

/// `message` as nostr-sdk takes it. Raw frames never come in reading.
fn from_native(message: Native) -> Option<Message> {
    Some(match message {
        Native::Text(text) => Message::Text(text.to_string()),
        Native::Binary(data) => Message::Binary(data.to_vec()),
        Native::Ping(data) => Message::Ping(data.to_vec()),
        Native::Pong(data) => Message::Pong(data.to_vec()),
        Native::Close(frame) => Message::Close(frame.map(Into::into)),
        Native::Frame(_) => return None,
    })
}

/// The socket's sending half, with nostr-sdk's errors. Not through
/// `sink_map_err`, which panics on a second error.
struct Sending<S>(S);

impl<S> Sink<Message> for Sending<S>
where
    S: Sink<Native, Error = tungstenite::Error> + Unpin,
{
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.0.poll_ready_unpin(cx).map_err(Error::transport)
    }

    fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Error> {
        self.0
            .start_send_unpin(message.into())
            .map_err(Error::transport)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.0.poll_flush_unpin(cx).map_err(Error::transport)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.0.poll_close_unpin(cx).map_err(Error::transport)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nostr_sdk::prelude::{Client, EventBuilder, Filter, FinalizeEvent, Keys, Kind, MockRelay};

    use super::*;
    use crate::{discover, Role, Route};

    /// Plain TCP streams, as a Tor client's would carry the same bytes.
    #[derive(Debug)]
    struct OverTcp;

    impl WebSocketTransport for OverTcp {
        fn support_ping(&self) -> bool {
            true
        }

        fn connect<'a>(&'a self, url: &'a Url, _proxy: Option<SocketAddr>) -> Connecting<'a> {
            Box::pin(async move {
                let at = (url.host_str().unwrap(), url.port().unwrap());
                let stream = tokio::net::TcpStream::connect(at)
                    .await
                    .map_err(Error::transport)?;
                handshake(url, stream).await
            })
        }
    }

    #[tokio::test]
    async fn a_relay_takes_and_answers_events_over_streams_of_our_own() {
        let relay = MockRelay::run().await.unwrap();
        let client = Client::builder().websocket_transport(OverTcp).build();
        client.add_relay(relay.url().await).await.unwrap();
        client.connect().and_wait(Duration::from_secs(5)).await;
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "over our own stream")
            .finalize(&keys)
            .unwrap();
        let sent = client.send_event(&event).await.unwrap();
        assert_eq!(sent.success.len(), 1, "{:?}", sent.failed);
        let filter = Filter::new().author(keys.public_key());
        let events = client
            .fetch_events(filter)
            .timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(events.first().map(|e| e.id), Some(event.id));
        client.shutdown().await;
    }

    /// Discovery on the relay `LICASH_TEST_RELAY` names, over Tor.
    #[tokio::test]
    #[ignore = "reaches the Tor network and the relay LICASH_TEST_RELAY names"]
    async fn relays_answer_over_tor() {
        let relay = std::env::var("LICASH_TEST_RELAY").expect("LICASH_TEST_RELAY: a relay URL");
        let dir = tempfile::tempdir().unwrap();
        let tor = licash_tor::Tor::new(dir.path()).unwrap();
        let route = Route::Tor(tor);
        discover(&[relay], &route, Role::Hub, "bitcoin")
            .await
            .unwrap();
    }

    /// A payment to a mailbox on the relay `LICASH_TEST_RELAY` names,
    /// sent and fetched directly and over Tor: whether it takes and serves
    /// gift wraps.
    #[tokio::test]
    #[ignore = "reaches the Tor network and the relay LICASH_TEST_RELAY names"]
    async fn mailboxes_work_on_the_test_relay() {
        let relay = std::env::var("LICASH_TEST_RELAY").expect("LICASH_TEST_RELAY: a relay URL");
        let dir = tempfile::tempdir().unwrap();
        let routes = [
            Route::Direct,
            Route::Tor(licash_tor::Tor::new(dir.path()).unwrap()),
        ];
        for route in routes {
            let secret = Keys::generate().secret_key().to_secret_bytes();
            let to = crate::keys(&secret).unwrap().public_key().to_bytes();
            let relays = [relay.clone()];
            crate::mailbox::send(&to, "{}", &relays, &route)
                .await
                .unwrap();
            let mail = crate::mailbox::fetch(&secret, &relays, &route)
                .await
                .unwrap();
            assert_eq!(mail.len(), 1);
        }
    }
}
