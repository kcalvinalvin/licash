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
    consensus::serialize,
    hashes::Hash,
    pow::CompactTarget,
    transaction, Amount, Block, BlockHash, Network, OutPoint, ScriptBuf, Sequence, Transaction,
    TxIn, TxMerkleNode, TxOut, Witness,
};
use licash::{
    chain::{activation_script, Checkpoint},
    spend::ChainProfile,
};
use licash_chain_client::{activations, Published, Source};
use serde_json::{json, Value};

const SECRET: [u8; 32] = [7; 32];
/// The block that carries the activation.
const ACTIVATED: u32 = 9;
/// `user:p@ss w` and `__cookie__:c00k1e`.
const PASSWORD: &str = "Basic dXNlcjpwQHNzIHc=";
const COOKIE: &str = "Basic X19jb29raWVfXzpjMDBrMWU=";

struct Chain {
    blocks: Vec<Block>,
    by_hash: HashMap<String, Block>,
    auth: &'static str,
}

/// Genesis and `n` blocks mined on it, block `ACTIVATED` with an
/// activation output in its coinbase.
fn mine(n: u32) -> Vec<Block> {
    let mut blocks = vec![bitcoin::constants::genesis_block(Network::Regtest)];
    for height in 1..=n {
        let previous = blocks.last().unwrap().header;
        let mut output = vec![TxOut {
            value: Amount::from_sat(50),
            script_pubkey: ScriptBuf::new_op_return([1u8]),
        }];
        if height == ACTIVATED {
            output.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(activation_script(&SECRET)),
            });
        }
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
            txdata: vec![coinbase],
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
        _ => rpc_error(-32601, "Method not found"),
    }
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
    let chain = Arc::new(Chain {
        blocks,
        by_hash,
        auth,
    });
    let app = Router::new()
        .route("/", post(rpc))
        .route("/api/blocks/tip/height", get(esplora_tip))
        .route("/api/block-height/{height}", get(esplora_hash))
        .route("/api/block/{hash}/{part}", get(esplora_block))
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
async fn activations_are_read_from_their_block_only() {
    let blocks = mine(ACTIVATED + 1);
    for (height, block) in blocks.iter().enumerate() {
        let found = activations(block);
        if height as u32 == ACTIVATED {
            let coinbase = block.txdata[0].compute_txid().to_byte_array();
            assert_eq!(
                found,
                [Published {
                    secret: SECRET,
                    txid: coinbase,
                    output: 1
                }]
            );
        } else {
            assert!(found.is_empty(), "block {height}");
        }
    }
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
