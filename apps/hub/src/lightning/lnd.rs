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

use super::{Attempt, Call, Currency, Held, HeldState, Node, NodeInfo, Status};

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
            .user_agent(concat!("licash-hub/", env!("CARGO_PKG_VERSION")));
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

async fn answer(response: Response) -> Result<Value, Fault> {
    let status = response.status();
    let body = response.bytes().await.map_err(down)?;
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
            let status = match payment["status"].as_str().unwrap_or_default() {
                "SUCCEEDED" => Status::Succeeded(bytes32(&text(&payment, "payment_preimage")?)?),
                "FAILED" => Status::Failed(
                    payment["failure_reason"]
                        .as_str()
                        .unwrap_or("FAILURE_REASON_UNKNOWN")
                        .to_owned(),
                ),
                _ => Status::InFlight,
            };
            let created_ns = num(&payment["creation_time_ns"]).ok_or("LND answered no time")?;
            Ok(Some(Attempt { status, created_ns }))
        })
    }

    fn hold(&self, hash: [u8; 32], msat: u64, cltv_expiry: u32, expiry: u64) -> Call<'_, String> {
        Box::pin(async move {
            let body = json!({
                "hash": STANDARD.encode(hash),
                "value_msat": msat.to_string(),
                "cltv_expiry": cltv_expiry.to_string(),
                "expiry": expiry.to_string(),
            });
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
