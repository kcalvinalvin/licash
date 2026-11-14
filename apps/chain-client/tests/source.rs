//! Both kinds of source against a mock node and a mock Esplora, serving
//! the same self-mined regtest chain.

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bitcoin::{
    absolute::LockTime,
    block::{Header, Version},
    consensus::{deserialize, serialize},
    hashes::Hash,
    pow::CompactTarget,
    transaction, Address, Amount, Block, BlockHash, FeeRate, Network, OutPoint, ScriptBuf,
    Sequence, Transaction, TxIn, TxMerkleNode, TxOut, Witness,
};
use licash::{chain::Checkpoint, spend::ChainProfile};
use licash_chain_client::Source;
use serde_json::{json, Value};

/// `user:p@ss w` and `__cookie__:c00k1e`.
const PASSWORD: &str = "Basic dXNlcjpwQHNzIHc=";
const COOKIE: &str = "Basic X19jb29raWVfXzpjMDBrMWU=";

struct Chain {
    blocks: Vec<Block>,
    by_hash: HashMap<String, Block>,
    /// Every output of the chain.
    outputs: HashMap<OutPoint, TxOut>,
    auth: &'static str,
    /// The mempool.
    sent: std::sync::Mutex<Vec<Transaction>>,
}

/// Outputs of each coinbase, which the next block spends.
const COINBASE_OUTPUTS: u32 = 30;

/// A script of output `i` of the coinbase at `height`, of a key hash.
fn script(height: u32, i: u32) -> ScriptBuf {
    let mut bytes = vec![0x00, 0x14, height as u8, i as u8];
    bytes.resize(22, 0xab);
    ScriptBuf::from_bytes(bytes)
}

/// Genesis and `n` blocks mined on it. From block 2, each spends outputs
/// of the coinbase before it, one or two at a time, and block 3 all of
/// them: a taproot output pays every other one.
fn mine(n: u32) -> Vec<Block> {
    let mut blocks = vec![bitcoin::constants::genesis_block(Network::Regtest)];
    for height in 1..=n {
        let previous = blocks.last().unwrap().header;
        let output = (0..COINBASE_OUTPUTS)
            .map(|i| TxOut {
                value: Amount::from_sat(50),
                script_pubkey: script(height, i),
            })
            .collect();
        let spent = match height {
            1 => 0,
            3 => COINBASE_OUTPUTS,
            _ => 2,
        };
        let spends = (0..spent).map(|i| Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: blocks.last().unwrap().txdata[0].compute_txid(),
                    vout: i,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(40),
                script_pubkey: match i % 2 {
                    0 => ScriptBuf::from_bytes([&[0x51, 0x20][..], &[i as u8; 32]].concat()),
                    _ => script(0, i),
                },
            }],
        });
        let coinbase = Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![0x01, height as u8, 0x00]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output,
        };
        let mut block = Block {
            header: Header {
                version: Version::from_consensus(0x2000_0000),
                prev_blockhash: previous.block_hash(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: previous.time + 600,
                bits: CompactTarget::from_consensus(0x207f_ffff),
                nonce: 0,
            },
            txdata: std::iter::once(coinbase).chain(spends).collect(),
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        while block.header.validate_pow(block.header.target()).is_err() {
            block.header.nonce += 1;
        }
        blocks.push(block);
    }
    blocks
}

fn rpc_reply(result: Value) -> Response {
    Json(json!({"result": result, "error": null, "id": "licash"})).into_response()
}

fn rpc_error(code: i64, message: &str) -> Response {
    let body = json!({"result": null, "error": {"code": code, "message": message}, "id": "licash"});
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

async fn rpc(
    State(chain): State<Arc<Chain>>,
    headers: HeaderMap,
    Json(call): Json<Value>,
) -> Response {
    let authorized = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(chain.auth);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let params = &call["params"];
    let by_hash = |i: usize| chain.by_hash.get(params[i].as_str().unwrap_or_default());
    match call["method"].as_str().unwrap_or_default() {
        "getblockchaininfo" => {
            let tip = chain.blocks.last().unwrap();
            rpc_reply(json!({
                "blocks": chain.blocks.len() - 1,
                "bestblockhash": tip.block_hash().to_string(),
            }))
        }
        "getblockhash" => match chain.blocks.get(params[0].as_u64().unwrap() as usize) {
            Some(block) => rpc_reply(json!(block.block_hash().to_string())),
            None => rpc_error(-8, "Block height out of range"),
        },
        "getblockheader" if params[1] == json!(false) => match by_hash(0) {
            Some(block) => rpc_reply(json!(hex::encode(serialize(&block.header)))),
            None => rpc_error(-5, "Block not found"),
        },
        "getblock" if params[1] == json!(0) => match by_hash(0) {
            Some(block) => rpc_reply(json!(hex::encode(serialize(block)))),
            None => rpc_error(-5, "Block not found"),
        },
        "getblock" if params[1] == json!(3) => match by_hash(0) {
            Some(block) => {
                let txs: Vec<Value> = block
                    .txdata
                    .iter()
                    .map(|tx| {
                        let vin: Vec<Value> = tx
                            .input
                            .iter()
                            .map(|input| match chain.outputs.get(&input.previous_output) {
                                Some(spent) => json!({"prevout": {
                                    "value": spent.value.to_btc(),
                                    "scriptPubKey": {"hex": spent.script_pubkey.to_hex_string()},
                                }}),
                                None => json!({"coinbase": "00"}),
                            })
                            .collect();
                        json!({"txid": tx.compute_txid().to_string(), "vin": vin})
                    })
                    .collect();
                rpc_reply(json!({"hash": block.block_hash().to_string(), "tx": txs}))
            }
            None => rpc_error(-5, "Block not found"),
        },
        "sendrawtransaction" => match send(&chain, params[0].as_str().unwrap_or_default()) {
            Ok(txid) => rpc_reply(json!(txid)),
            Err((code, message)) => rpc_error(code, message),
        },
        "estimatesmartfee" => match params[0].as_u64() {
            Some(target @ 1..=144) => rpc_reply(json!({"feerate": 0.00012, "blocks": target})),
            _ => {
                rpc_reply(json!({"errors": ["Insufficient data or no feerate found"], "blocks": 0}))
            }
        },
        "scantxoutset" => {
            let descriptor = params[1][0]["desc"].as_str().unwrap_or_default();
            let address = descriptor
                .strip_prefix("addr(")
                .and_then(|d| d.strip_suffix(')'))
                .and_then(|a| a.parse::<Address<_>>().ok());
            let Some(address) = address else {
                return rpc_error(-5, "Invalid descriptor");
            };
            let script = address.assume_checked().script_pubkey();
            let found: Vec<Value> = unspent(&chain, &script)
                .into_iter()
                .map(|(o, out, height)| {
                    json!({"txid": o.txid.to_string(), "vout": o.vout, "amount": out.value.to_btc(),
                           "scriptPubKey": script.to_hex_string(), "height": height})
                })
                .collect();
            rpc_reply(json!({"success": true, "unspents": found}))
        }
        _ => rpc_error(-32601, "Method not found"),
    }
}

/// The outputs paying `script` that no block spends, with their heights.
fn unspent<'a>(chain: &'a Chain, script: &'a ScriptBuf) -> Vec<(OutPoint, &'a TxOut, u32)> {
    let spent: std::collections::HashSet<OutPoint> = chain
        .blocks
        .iter()
        .flat_map(|b| &b.txdata)
        .flat_map(|tx| tx.input.iter().map(|i| i.previous_output))
        .collect();
    let mut found = Vec::new();
    for (height, block) in (0u32..).zip(&chain.blocks) {
        for tx in &block.txdata {
            let txid = tx.compute_txid();
            for (vout, output) in (0u32..).zip(&tx.output) {
                let outpoint = OutPoint { txid, vout };
                if output.script_pubkey == *script && !spent.contains(&outpoint) {
                    found.push((outpoint, output, height));
                }
            }
        }
    }
    found
}

/// Take `tx` into the mempool, as a node does, or why not.
fn send(chain: &Chain, raw: &str) -> Result<String, (i64, &'static str)> {
    let tx: Transaction = hex::decode(raw)
        .ok()
        .and_then(|raw| deserialize(&raw).ok())
        .ok_or((-22, "TX decode failed"))?;
    let txid = tx.compute_txid();
    let confirmed = chain.blocks.iter().flat_map(|b| &b.txdata);
    if confirmed.clone().any(|t| t.compute_txid() == txid) {
        return Err((-27, "Transaction outputs already in utxo set"));
    }
    let mut sent = chain.sent.lock().unwrap();
    if !sent.iter().any(|t| t.compute_txid() == txid) {
        let spends = confirmed.flat_map(|t| &t.input).map(|i| i.previous_output);
        let spent: Vec<OutPoint> = spends.collect();
        let spendable = |o: &OutPoint| chain.outputs.contains_key(o) && !spent.contains(o);
        if !tx.input.iter().all(|i| spendable(&i.previous_output)) {
            return Err((-25, "bad-txns-inputs-missingorspent"));
        }
        sent.push(tx);
    }
    Ok(txid.to_string())
}

async fn esplora_send(State(chain): State<Arc<Chain>>, raw: String) -> Response {
    match send(&chain, &raw) {
        Ok(txid) => txid.into_response(),
        Err((code, message)) => {
            let text = format!(
                "sendrawtransaction RPC error: {{\"code\":{code},\"message\":\"{message}\"}}"
            );
            (StatusCode::BAD_REQUEST, text).into_response()
        }
    }
}

async fn esplora_fees() -> Json<Value> {
    Json(json!({"1": 20.5, "6": 12.0, "144": 1.0}))
}

async fn esplora_utxo(State(chain): State<Arc<Chain>>, Path(address): Path<String>) -> Response {
    let Ok(address) = address.parse::<Address<_>>() else {
        return (StatusCode::BAD_REQUEST, "Invalid Bitcoin address").into_response();
    };
    let script = address.assume_checked().script_pubkey();
    let mut found: Vec<Value> = unspent(&chain, &script)
        .into_iter()
        .map(|(o, out, height)| {
            let status = json!({"confirmed": true, "block_height": height});
            json!({"txid": o.txid.to_string(), "vout": o.vout, "value": out.value.to_sat(), "status": status})
        })
        .collect();
    // And those of the mempool, unconfirmed.
    for tx in chain.sent.lock().unwrap().iter() {
        for (vout, output) in (0u32..).zip(&tx.output) {
            if output.script_pubkey == script {
                let txid = tx.compute_txid().to_string();
                let status = json!({"confirmed": false});
                found.push(json!({"txid": txid, "vout": vout, "value": output.value.to_sat(), "status": status}));
            }
        }
    }
    Json(found).into_response()
}

async fn esplora_tip(State(chain): State<Arc<Chain>>) -> String {
    (chain.blocks.len() - 1).to_string()
}

async fn esplora_hash(State(chain): State<Arc<Chain>>, Path(height): Path<usize>) -> Response {
    match chain.blocks.get(height) {
        Some(block) => block.block_hash().to_string().into_response(),
        None => (StatusCode::NOT_FOUND, "Block not found").into_response(),
    }
}

async fn esplora_block(
    State(chain): State<Arc<Chain>>,
    Path((hash, part)): Path<(String, String)>,
) -> Response {
    let Some(block) = chain.by_hash.get(&hash) else {
        return (StatusCode::NOT_FOUND, "Block not found").into_response();
    };
    match part.as_str() {
        "header" => hex::encode(serialize(&block.header)).into_response(),
        "raw" => serialize(block).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn esplora_txs(
    State(chain): State<Arc<Chain>>,
    Path((hash, start)): Path<(String, usize)>,
) -> Response {
    let Some(block) = chain.by_hash.get(&hash) else {
        return (StatusCode::NOT_FOUND, "Block not found").into_response();
    };
    if start % 25 != 0 || start >= block.txdata.len() {
        return (StatusCode::BAD_REQUEST, "start index out of range").into_response();
    }
    let txs: Vec<Value> = block.txdata[start..]
        .iter()
        .take(25)
        .map(|tx| {
            let vin: Vec<Value> = tx
                .input
                .iter()
                .map(|input| {
                    let spent = chain.outputs.get(&input.previous_output);
                    json!({
                        "is_coinbase": spent.is_none(),
                        "prevout": spent.map(|o| json!({
                            "scriptpubkey": o.script_pubkey.to_hex_string(),
                            "value": o.value.to_sat(),
                        })),
                    })
                })
                .collect();
            json!({"txid": tx.compute_txid().to_string(), "vin": vin})
        })
        .collect();
    Json(txs).into_response()
}

/// Serves `blocks` as a node at `/` and as Esplora under `/api`, with
/// `tampered` answering for a hash that is not its block's.
async fn serve(
    blocks: Vec<Block>,
    auth: &'static str,
    tampered: Option<(BlockHash, Block)>,
) -> String {
    let mut by_hash: HashMap<String, Block> = blocks
        .iter()
        .map(|b| (b.block_hash().to_string(), b.clone()))
        .collect();
    if let Some((hash, block)) = tampered {
        by_hash.insert(hash.to_string(), block);
    }
    let outputs = blocks
        .iter()
        .flat_map(|b| &b.txdata)
        .flat_map(|tx| {
            let txid = tx.compute_txid();
            (0..)
                .zip(tx.output.clone())
                .map(move |(vout, o)| (OutPoint { txid, vout }, o))
        })
        .collect();
    let chain = Arc::new(Chain {
        blocks,
        by_hash,
        outputs,
        auth,
        sent: Default::default(),
    });
    let app = Router::new()
        .route("/", post(rpc))
        .route("/api/blocks/tip/height", get(esplora_tip))
        .route("/api/block-height/{height}", get(esplora_hash))
        .route("/api/block/{hash}/{part}", get(esplora_block))
        .route("/api/block/{hash}/txs/{start}", get(esplora_txs))
        .route("/api/tx", post(esplora_send))
        .route("/api/fee-estimates", get(esplora_fees))
        .route("/api/address/{address}/utxo", get(esplora_utxo))
        .with_state(chain);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address.to_string()
}

fn sources(address: &str) -> [Source; 2] {
    [
        format!("http://user:p%40ss%20w@{address}").parse().unwrap(),
        format!("esplora+http://{address}/api/").parse().unwrap(),
    ]
}

#[tokio::test]
async fn a_node_and_esplora_read_the_same_chain() {
    let blocks = mine(12);
    let address = serve(blocks.clone(), PASSWORD, None).await;
    let mut commitments = Vec::new();
    for source in sources(&address) {
        let tip = source.tip().await.unwrap();
        assert_eq!(tip.height, 12, "{source}");
        assert_eq!(tip.hash, blocks[12].block_hash());
        assert_eq!(source.network().await.unwrap(), Network::Regtest);
        for (height, block) in blocks.iter().enumerate() {
            let hash = source.hash(height as u32).await.unwrap();
            assert_eq!(hash, block.block_hash());
            assert_eq!(source.header(&hash).await.unwrap(), block.header);
            assert_eq!(&source.block(&hash).await.unwrap(), block);
        }
        // Anchored at genesis, with fewer than eleven times, and at the tip.
        let genesis = source.checkpoint(0).await.unwrap();
        let expected = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
        assert_eq!(genesis.commitment(), expected.commitment());
        let early = source.checkpoint(4).await.unwrap();
        assert_eq!(
            (early.height(), early.header()),
            (4, &serialize(&blocks[4].header)[..].try_into().unwrap())
        );
        let checkpoint = source.checkpoint(12).await.unwrap();
        assert_eq!(checkpoint.profile(), ChainProfile::Core30DefaultRegtest);
        commitments.push(checkpoint.commitment());
    }
    assert_eq!(commitments[0], commitments[1]);
}

#[tokio::test]
async fn both_read_what_the_transactions_paying_taproot_outputs_spend() {
    let blocks = mine(4);
    let address = serve(blocks.clone(), PASSWORD, None).await;
    for source in sources(&address) {
        for (height, block) in (0u32..).zip(&blocks) {
            let spends = source.taproot_spends(block).await.unwrap();
            let expected: Vec<Option<Vec<ScriptBuf>>> = (0u32..)
                .zip(&block.txdata)
                .map(|(i, _)| match i {
                    // The coinbase, and those paying a key hash.
                    0 => None,
                    i if (i - 1) % 2 == 1 => None,
                    i => Some(vec![script(height - 1, i - 1)]),
                })
                .collect();
            assert_eq!(spends, expected, "{source}: block {height}");
        }
        // Spread over two of Esplora's pages.
        assert_eq!(blocks[3].txdata.len(), 31);
        // The source's transactions are not those of the block asked of.
        let mut forged = blocks[2].clone();
        forged.txdata.swap(1, 2);
        let error = source.taproot_spends(&forged).await.unwrap_err();
        assert!(format!("{error:#}").contains("are not its"), "{error:#}");
    }
}

#[tokio::test]
async fn both_broadcast_estimate_fees_and_find_what_pays_an_address() {
    let blocks = mine(4);
    let address = serve(blocks.clone(), PASSWORD, None).await;
    let to = |script: ScriptBuf| Address::from_script(&script, Network::Regtest).unwrap();
    let [node, esplora] = sources(&address);
    for source in [&node, &esplora] {
        // Paid by a transaction of each block from 2 on, never spent.
        let found = source.unspent(&to(script(0, 1))).await.unwrap();
        let heights: Vec<u32> = found.iter().map(|u| u.height).collect();
        assert_eq!(heights, [2, 3, 4], "{source}");
        assert!(found.iter().all(|u| u.value == Amount::from_sat(40)));
        // Spent by the next block.
        assert!(source.unspent(&to(script(1, 0))).await.unwrap().is_empty());
        let tip = source.unspent(&to(script(4, 5))).await.unwrap();
        let coinbase = blocks[4].txdata[0].compute_txid();
        assert_eq!(tip.len(), 1);
        assert_eq!(
            (tip[0].outpoint.txid, tip[0].outpoint.vout, tip[0].height),
            (coinbase, 5, 4)
        );
        assert_eq!(tip[0].value, Amount::from_sat(50));
    }

    // 12 sat/vB, or 3000 per thousand weight units.
    let twelve = FeeRate::from_sat_per_kwu(3000);
    assert_eq!(node.fee_rate(6).await.unwrap(), Some(twelve));
    assert_eq!(node.fee_rate(1008).await.unwrap(), None);
    assert_eq!(esplora.fee_rate(6).await.unwrap(), Some(twelve));
    // The latest estimate within the blocks asked, else the soonest.
    let soonest = FeeRate::from_sat_per_kwu(5125);
    assert_eq!(esplora.fee_rate(3).await.unwrap(), Some(soonest));
    assert_eq!(esplora.fee_rate(0).await.unwrap(), Some(soonest));
    assert_eq!(
        esplora.fee_rate(1000).await.unwrap(),
        Some(FeeRate::from_sat_per_kwu(250))
    );

    // A spend of the tip's coinbase, paying the address of many outputs.
    let spend = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: coinbase_of(&blocks, 4),
                vout: 7,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(45),
            script_pubkey: script(0, 1),
        }],
    };
    let txid = spend.compute_txid();
    for source in [&node, &esplora] {
        // Again, or once it is in a block: no error.
        assert_eq!(source.broadcast(&spend).await.unwrap(), txid, "{source}");
        let mined = &blocks[3].txdata[1];
        assert_eq!(source.broadcast(mined).await.unwrap(), mined.compute_txid());
        let mut missing = spend.clone();
        missing.input[0].previous_output.vout = 99;
        let error = format!("{:#}", source.broadcast(&missing).await.unwrap_err());
        assert!(error.contains("bad-txns-inputs-missingorspent"), "{error}");
    }
    // Unconfirmed, it pays the address nothing yet.
    assert_eq!(esplora.unspent(&to(script(0, 1))).await.unwrap().len(), 3);
}

fn coinbase_of(blocks: &[Block], height: usize) -> bitcoin::Txid {
    blocks[height].txdata[0].compute_txid()
}

#[tokio::test]
async fn a_block_or_header_that_is_not_the_one_asked_for_is_refused() {
    let blocks = mine(3);
    let claimed = blocks[2].block_hash();
    let address = serve(blocks.clone(), PASSWORD, Some((claimed, blocks[3].clone()))).await;
    // The tampered entry replaced block 2 under its own hash.
    for source in sources(&address) {
        let header = source.header(&claimed).await.unwrap_err().to_string();
        assert!(header.contains("does not hash to itself"), "{header}");
        let block = source.block(&claimed).await.unwrap_err().to_string();
        assert!(block.contains("does not hash to itself"), "{block}");
    }
}

#[tokio::test]
async fn node_errors_and_missing_blocks_are_reported() {
    let address = serve(mine(2), PASSWORD, None).await;
    let [node, esplora] = sources(&address);
    let error = node.hash(99).await.unwrap_err().to_string();
    assert!(
        error.contains("getblockhash: Block height out of range (-8)"),
        "{error}"
    );
    let error = esplora.hash(99).await.unwrap_err().to_string();
    assert!(error.contains("HTTP 404"), "{error}");
    let missing = BlockHash::all_zeros();
    assert!(node
        .block(&missing)
        .await
        .unwrap_err()
        .to_string()
        .contains("Block not found"));
    assert!(esplora.block(&missing).await.is_err());
}

#[tokio::test]
async fn credentials_authenticate_and_never_show() {
    let address = serve(mine(1), PASSWORD, None).await;
    let [node, _] = sources(&address);
    assert_eq!(node.to_string(), format!("http://{address}"));
    assert_eq!(format!("{node:?}"), format!("Source(http://{address})"));
    node.tip().await.unwrap();
    let wrong: Source = format!("http://user:nope@{address}/").parse().unwrap();
    let error = wrong.tip().await.unwrap_err().to_string();
    assert!(
        error.contains("unauthorized") && !error.contains("nope"),
        "{error}"
    );
    let anonymous: Source = format!("http://{address}").parse().unwrap();
    assert!(anonymous.tip().await.is_err());
}

#[tokio::test]
async fn a_cookie_file_is_read_on_each_call() {
    let address = serve(mine(1), COOKIE, None).await;
    let dir = tempfile::tempdir().unwrap();
    let cookie = dir.path().join(".cookie");
    let node: Source = format!("http://{address}?cookie={}", cookie.display())
        .parse()
        .unwrap();
    assert_eq!(node.to_string(), format!("http://{address}"));
    let error = node.tip().await.unwrap_err().to_string();
    assert!(error.contains(".cookie"), "{error}");
    std::fs::write(&cookie, "__cookie__:stale").unwrap();
    assert!(node.tip().await.is_err());
    // The node restarted and wrote a new cookie.
    std::fs::write(&cookie, "__cookie__:c00k1e\n").unwrap();
    assert_eq!(node.tip().await.unwrap().height, 1);
}

#[test]
fn urls_are_checked() {
    for bad in [
        "ftp://host:1",
        "localhost:8332",
        "http://u:p@host:1?cookie=/c",
        "http://host:1?wallet=w",
        "http://host:1?cookie=/a&cookie=/b",
        "esplora+https://u:p@host/api",
        "esplora+https://host/api?x=1",
        "esplora+https:///api",
    ] {
        assert!(bad.parse::<Source>().is_err(), "{bad}");
    }
    let esplora: Source = "esplora+https://esplora.example/api/".parse().unwrap();
    assert_eq!(esplora.to_string(), "esplora+https://esplora.example/api");
}
