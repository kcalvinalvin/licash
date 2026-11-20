//! LND, over its REST API. The node's TLS certificate is pinned: LND's
//! own is a self-signed CA, which certificate validation refuses.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE},
    Engine,
};
use reqwest::{RequestBuilder, Response, StatusCode};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use serde_json::{json, Value};

use crate::{Attempt, Call, Currency, Held, HeldState, Liquidity, Node, NodeInfo, Status};

/// How long a call waits for the node.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The largest answer read.
const MAX_ANSWER: usize = 1 << 20;

/// An LND node's REST API.
pub struct Lnd {
    client: reqwest::Client,
    url: String,
    /// In hex.
    macaroon: String,
}

impl Lnd {
    /// The node at `url`, such as `https://127.0.0.1:8080`, called with
    /// `macaroon`, whose TLS certificate is `cert`, in PEM.
    pub fn new(url: &str, macaroon: &[u8], cert: Option<&[u8]>) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .user_agent(concat!("licash/", env!("CARGO_PKG_VERSION")));
        if let Some(pem) = cert {
            client = client.tls_backend_preconfigured(pinned(pem)?);
        }
        Ok(Self {
            client: client.build()?,
            url: url.trim_end_matches('/').to_owned(),
            macaroon: hex::encode(macaroon),
        })
    }

    async fn get(&self, path: &str) -> Result<Value, Fault> {
        let request = self.client.get(format!("{}{path}", self.url));
        answer(self.send(request).await?).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, Fault> {
        let request = self.client.post(format!("{}{path}", self.url)).json(&body);
        answer(self.send(request).await?).await
    }

    /// The first update a streaming call answers; the rest is dropped.
    async fn first(&self, request: RequestBuilder) -> Result<Value, Fault> {
        let mut response = self.send(request).await?;
        let status = response.status();
        let mut read = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(down)? {
            read.extend_from_slice(&chunk);
            if let Some(end) = read.iter().position(|b| *b == b'\n') {
                read.truncate(end);
                break;
            }
            if read.len() > MAX_ANSWER {
                return Err(Fault::Other("an answer too long".into()));
            }
        }
        let value: Value =
            serde_json::from_slice(&read).map_err(|e| Fault::Other(e.to_string()))?;
        if !status.is_success() {
            return Err(fault(&value, status));
        }
        if let Some(error) = value.get("error") {
            return Err(fault(error, status));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| Fault::Other("an update without a result".into()))
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, Fault> {
        request
            .header("Grpc-Metadata-macaroon", &self.macaroon)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(down)
    }
}

/// Why a call failed.
enum Fault {
    NotFound(String),
    Other(String),
}

impl Fault {
    fn says(&self, words: &[&str]) -> bool {
        let (Self::NotFound(message) | Self::Other(message)) = self;
        words.iter().any(|w| message.contains(w))
    }
}

impl From<Fault> for String {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::NotFound(message) => format!("not found: {message}"),
            Fault::Other(message) => message,
        }
    }
}

fn down(e: reqwest::Error) -> Fault {
    Fault::Other(format!("LND: {e}"))
}

async fn answer(mut response: Response) -> Result<Value, Fault> {
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(down)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ANSWER {
            return Err(Fault::Other("an answer too long".into()));
        }
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| Fault::Other(format!("LND answered {status} without JSON")))?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(fault(&value, status))
    }
}

/// The error LND answered, of gRPC code 5 if not found.
fn fault(error: &Value, status: StatusCode) -> Fault {
    let code = error.get("code").or_else(|| error.get("grpc_code"));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message")
        .to_owned();
    if status == StatusCode::NOT_FOUND || code.and_then(Value::as_u64) == Some(5) {
        Fault::NotFound(message)
    } else {
        Fault::Other(format!("LND: {message}"))
    }
}

/// A JSON integer, which LND writes as a string when 64 bits wide.
fn num(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("LND answered without {key}"))
}

fn bytes32(hex_text: &str) -> Result<[u8; 32], String> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(hex_text, &mut bytes).map_err(|e| format!("LND: {e}"))?;
    Ok(bytes)
}

/// The state of a payment as LND tracks it: in flight only while LND
/// says so, as initiated or in flight. A status it does not know is an
/// error, which the caller asks again on, rather than a payment awaited
/// for good.
fn status_of(payment: &Value) -> Result<Status, String> {
    Ok(match payment["status"].as_str().unwrap_or_default() {
        "SUCCEEDED" => Status::Succeeded(bytes32(&text(payment, "payment_preimage")?)?),
        "FAILED" => Status::Failed(
            payment["failure_reason"]
                .as_str()
                .unwrap_or("FAILURE_REASON_UNKNOWN")
                .to_owned(),
        ),
        "IN_FLIGHT" | "INITIATED" => Status::InFlight,
        other => return Err(format!("LND answered a payment of status {other:?}")),
    })
}

impl Node for Lnd {
    fn info(&self) -> Call<'_, NodeInfo> {
        Box::pin(async move {
            let info = self.get("/v1/getinfo").await?;
            let network = info["chains"][0]["network"].as_str().unwrap_or_default();
            let currency = match network {
                "mainnet" => Currency::Bitcoin,
                "testnet" | "testnet3" | "testnet4" => Currency::BitcoinTestnet,
                "signet" => Currency::Signet,
                "regtest" => Currency::Regtest,
                "simnet" => Currency::Simnet,
                other => return Err(format!("LND on an unknown network {other:?}")),
            };
            // LND writes the block hash in display order.
            let mut block_hash = bytes32(&text(&info, "block_hash")?)?;
            block_hash.reverse();
            Ok(NodeInfo {
                pubkey: text(&info, "identity_pubkey")?,
                height: num(&info["block_height"])
                    .and_then(|h| u32::try_from(h).ok())
                    .ok_or("LND answered without its height")?,
                block_hash,
                currency,
            })
        })
    }

    fn liquidity(&self) -> Call<'_, Liquidity> {
        Box::pin(async move {
            let balance = self.get("/v1/balance/channels").await?;
            let sat = |key: &str| {
                num(&balance[key]["sat"]).ok_or_else(|| format!("LND answered without {key}"))
            };
            Ok(Liquidity {
                local: sat("local_balance")?,
                remote: sat("remote_balance")?,
            })
        })
    }

    fn pay(&self, request: String, fee_limit: u64, cltv_limit: u32, timeout: u32) -> Call<'_, ()> {
        Box::pin(async move {
            let body = json!({
                "payment_request": request,
                "fee_limit_sat": fee_limit.to_string(),
                "cltv_limit": cltv_limit,
                "timeout_seconds": timeout,
                "no_inflight_updates": false,
                "allow_self_payment": false,
            });
            let send = self
                .client
                .post(format!("{}/v2/router/send", self.url))
                .json(&body);
            match self.first(send).await {
                Ok(_) => Ok(()),
                Err(fault) if fault.says(&["already", "in transition"]) => Ok(()),
                Err(fault) => Err(fault.into()),
            }
        })
    }

    fn payment(&self, hash: [u8; 32]) -> Call<'_, Option<Attempt>> {
        Box::pin(async move {
            let path = format!(
                "{}/v2/router/track/{}?no_inflight_updates=false",
                self.url,
                URL_SAFE.encode(hash)
            );
            let payment = match self.first(self.client.get(path)).await {
                Ok(payment) => payment,
                Err(Fault::NotFound(_)) => return Ok(None),
                Err(fault) if fault.says(&["isn't initiated", "not found"]) => return Ok(None),
                Err(fault) => return Err(fault.into()),
            };
            let status = status_of(&payment)?;
            let created_ns = num(&payment["creation_time_ns"]).ok_or("LND answered no time")?;
            Ok(Some(Attempt { status, created_ns }))
        })
    }

    fn hold(
        &self,
        hash: [u8; 32],
        msat: u64,
        cltv_expiry: u32,
        expiry: u64,
        description: Option<[u8; 32]>,
    ) -> Call<'_, String> {
        Box::pin(async move {
            let mut body = json!({
                "hash": STANDARD.encode(hash),
                "value_msat": msat.to_string(),
                "cltv_expiry": cltv_expiry.to_string(),
                "expiry": expiry.to_string(),
            });
            if let Some(description) = description {
                body["description_hash"] = STANDARD.encode(description).into();
            }
            let invoice = self.post("/v2/invoices/hodl", body).await?;
            text(&invoice, "payment_request")
        })
    }

    fn invoice(&self, hash: [u8; 32]) -> Call<'_, Option<Held>> {
        Box::pin(async move {
            let invoice = match self
                .get(&format!("/v1/invoice/{}", hex::encode(hash)))
                .await
            {
                Ok(invoice) => invoice,
                Err(Fault::NotFound(_)) => return Ok(None),
                // A node that never made an invoice says so.
                Err(fault) if fault.says(&["no existing invoices", "unable to locate"]) => {
                    return Ok(None)
                }
                Err(fault) => return Err(fault.into()),
            };
            let state = match invoice["state"].as_str().unwrap_or_default() {
                "OPEN" => HeldState::Open,
                "ACCEPTED" => HeldState::Accepted,
                "SETTLED" => HeldState::Settled,
                "CANCELED" => HeldState::Canceled,
                other => return Err(format!("LND: an invoice {other:?}")),
            };
            let htlcs = invoice["htlcs"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let min_expiry = htlcs
                .iter()
                .filter(|h| h["state"] == "ACCEPTED")
                .filter_map(|h| num(&h["expiry_height"]).and_then(|e| u32::try_from(e).ok()))
                .min();
            Ok(Some(Held {
                state,
                request: text(&invoice, "payment_request")?,
                min_expiry,
            }))
        })
    }

    fn settle(&self, preimage: [u8; 32]) -> Call<'_, ()> {
        Box::pin(async move {
            let body = json!({ "preimage": STANDARD.encode(preimage) });
            self.post("/v2/invoices/settle", body).await?;
            Ok(())
        })
    }

    fn cancel(&self, hash: [u8; 32]) -> Call<'_, ()> {
        Box::pin(async move {
            let body = json!({ "payment_hash": STANDARD.encode(hash) });
            self.post("/v2/invoices/cancel", body).await?;
            Ok(())
        })
    }
}

/// A TLS client trusting exactly the certificate `pem`.
fn pinned(pem: &[u8]) -> anyhow::Result<rustls::ClientConfig> {
    let cert = CertificateDer::from_pem_slice(pem).context("the node's certificate in PEM")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { cert, provider }))
        .with_no_client_auth();
    Ok(config)
}

/// Trusts one certificate, whatever names it carries.
#[derive(Debug)]
struct Pinned {
    cert: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.cert.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the node's certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer past [`MAX_ANSWER`] is refused as it is read, streamed or
    /// not.
    #[tokio::test]
    async fn an_answer_too_long_is_refused() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let serve = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = format!("{{\"alias\": \"{}\"}}", "a".repeat(MAX_ANSWER));
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        });
        let lnd = Lnd::new(&url, b"macaroon", None).unwrap();
        let read = lnd.get("/v1/getinfo").await;
        assert!(
            matches!(&read, Err(Fault::Other(m)) if m == "an answer too long"),
            "{:?}",
            read.map_err(String::from)
        );
        serve.abort();
    }

    #[test]
    fn a_payment_is_in_flight_only_while_lnd_says_so() {
        let of = |v: Value| status_of(&v);
        assert_eq!(of(json!({"status": "IN_FLIGHT"})), Ok(Status::InFlight));
        assert_eq!(of(json!({"status": "INITIATED"})), Ok(Status::InFlight));
        assert_eq!(
            of(json!({"status": "SUCCEEDED", "payment_preimage": "11".repeat(32)})),
            Ok(Status::Succeeded([0x11; 32]))
        );
        assert_eq!(
            of(json!({"status": "FAILED", "failure_reason": "FAILURE_REASON_NO_ROUTE"})),
            Ok(Status::Failed("FAILURE_REASON_NO_ROUTE".into()))
        );
        for unknown in [
            json!({"status": "UNKNOWN"}),
            json!({"status": "LOST"}),
            json!({}),
        ] {
            assert!(of(unknown.clone()).is_err(), "{unknown}");
        }
    }
}
