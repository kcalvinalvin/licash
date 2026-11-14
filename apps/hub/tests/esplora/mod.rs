//! A mock Esplora over a self-mined chain, with a mempool: it serves the
//! blocks, takes transactions, estimates fees and finds what pays an
//! address.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bitcoin::{consensus::serialize, Address, OutPoint, Transaction};
use serde_json::json;

use super::chain::Chain;

/// A chain and the transactions sent to it.
pub struct Backend {
    pub chain: Chain,
    pub mempool: Vec<Transaction>,
}

pub type Shared = Arc<Mutex<Backend>>;

pub fn backend(chain: Chain) -> Shared {
    Arc::new(Mutex::new(Backend {
        chain,
        mempool: Vec::new(),
    }))
}

async fn tip(State(backend): State<Shared>) -> String {
    backend.lock().unwrap().chain.height().to_string()
}

async fn hash(State(backend): State<Shared>, Path(height): Path<usize>) -> Response {
    match backend.lock().unwrap().chain.blocks.get(height) {
        Some(block) => block.block_hash().to_string().into_response(),
        None => (StatusCode::NOT_FOUND, "Block not found").into_response(),
    }
}

async fn block(
    State(backend): State<Shared>,
    Path((hash, part)): Path<(String, String)>,
) -> Response {
    let backend = backend.lock().unwrap();
    let blocks = &backend.chain.blocks;
    let block = blocks.iter().find(|b| b.block_hash().to_string() == hash);
    match (block, part.as_str()) {
        (Some(block), "header") => hex::encode(serialize(&block.header)).into_response(),
        (Some(block), "raw") => serialize(block).into_response(),
        _ => (StatusCode::NOT_FOUND, "Block not found").into_response(),
    }
}

async fn send(State(backend): State<Shared>, raw: String) -> Response {
    let tx: Transaction = match hex::decode(raw.trim())
        .ok()
        .and_then(|raw| bitcoin::consensus::deserialize(&raw).ok())
    {
        Some(tx) => tx,
        None => return (StatusCode::BAD_REQUEST, "TX decode failed").into_response(),
    };
    let txid = tx.compute_txid();
    let mut backend = backend.lock().unwrap();
    if !backend.mempool.iter().any(|t| t.compute_txid() == txid) {
        backend.mempool.push(tx);
    }
    txid.to_string().into_response()
}

async fn fees() -> Json<serde_json::Value> {
    Json(json!({"2": 3.0, "6": 2.0, "144": 1.0}))
}

/// The confirmed outputs paying `address` that no block spends.
async fn utxo(State(backend): State<Shared>, Path(address): Path<String>) -> Response {
    let Ok(address) = address.parse::<Address<_>>() else {
        return (StatusCode::BAD_REQUEST, "Invalid Bitcoin address").into_response();
    };
    let script = address.assume_checked().script_pubkey();
    let backend = backend.lock().unwrap();
    let blocks = &backend.chain.blocks;
    let spent: Vec<OutPoint> = blocks
        .iter()
        .flat_map(|b| &b.txdata)
        .flat_map(|tx| tx.input.iter().map(|i| i.previous_output))
        .collect();
    let mut found = Vec::new();
    for (height, block) in (0u32..).zip(blocks) {
        for tx in &block.txdata {
            let txid = tx.compute_txid();
            for (vout, output) in (0u32..).zip(&tx.output) {
                if output.script_pubkey == script && !spent.contains(&OutPoint { txid, vout }) {
                    let status = json!({"confirmed": true, "block_height": height});
                    let value = output.value.to_sat();
                    found.push(json!({"txid": txid.to_string(), "vout": vout, "value": value, "status": status}));
                }
            }
        }
    }
    Json(found).into_response()
}

/// Serve `backend` on a local port, on a thread of its own: its URL, as
/// a chain source names it.
pub fn serve(backend: Shared) -> String {
    let app = Router::new()
        .route("/api/blocks/tip/height", get(tip))
        .route("/api/block-height/{height}", get(hash))
        .route("/api/block/{hash}/{part}", get(block))
        .route("/api/tx", post(send))
        .route("/api/fee-estimates", get(fees))
        .route("/api/address/{address}/utxo", get(utxo))
        .with_state(backend);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("esplora+http://{address}/api")
}
