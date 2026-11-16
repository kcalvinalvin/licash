//! Driver lifecycle tests after selection, using the explicit mock proof
//! authority and a synthetic chain. Counter cryptography is tested separately.

use super::*;
use bitcoin::{block, hashes::Hash as _, Block, BlockHash, Network, TxIn};
use licash::disclosure::{Disclosures, CHUNK_BYTES};
use licash::spend::ChainProfile;
use licash_contract::{graph::Babe, keys::HubKeys, opening::Hub};
use licash_wallet::{Blocks, Response as HttpResponse};
use lngap_lamport::winternitz::{WotsParams, WotsSig};
use lngap_sessions::skeleton::sig_name;
use std::{collections::BTreeMap, sync::Mutex};

struct Offline;
impl Transport for Offline {
    async fn request(&self, _: &str, _: Option<Vec<u8>>) -> Result<HttpResponse, String> {
        Err("no network in a driver test".into())
    }
}

struct Ledger {
    blocks: Mutex<BTreeMap<u32, Block>>,
    sent: Mutex<Vec<Transaction>>,
}
impl Ledger {
    fn new() -> Self {
        Self {
            blocks: Mutex::new(BTreeMap::from([(
                0,
                bitcoin::blockdata::constants::genesis_block(Network::Regtest),
            )])),
            sent: Mutex::new(vec![]),
        }
    }
    fn mine(&self, tx: Option<Transaction>) -> u32 {
        let mut blocks = self.blocks.lock().unwrap();
        let (&height, previous) = blocks.last_key_value().unwrap();
        let tx = tx.unwrap_or(Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new(),
            }],
        });
        let mut next = Block {
            header: block::Header {
                prev_blockhash: previous.block_hash(),
                time: previous.header.time + 600,
                ..previous.header
            },
            txdata: vec![tx],
        };
        next.header.merkle_root = next.compute_merkle_root().unwrap();
        blocks.insert(height + 1, next);
        height + 1
    }
    fn to(&self, height: u32) {
        while *self.blocks.lock().unwrap().last_key_value().unwrap().0 < height {
            self.mine(None);
        }
    }
    fn last(&self) -> Transaction {
        self.sent.lock().unwrap().last().unwrap().clone()
    }
}
impl Blocks for Ledger {
    async fn height(&self) -> Result<u32, String> {
        Ok(*self.blocks.lock().unwrap().last_key_value().unwrap().0)
    }
    async fn hash(&self, height: u32) -> Result<BlockHash, String> {
        self.blocks
            .lock()
            .unwrap()
            .get(&height)
            .map(Block::block_hash)
            .ok_or("no block".into())
    }
    async fn filter(&self, _: &BlockHash) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
    async fn block(&self, hash: &BlockHash) -> Result<Block, String> {
        self.blocks
            .lock()
            .unwrap()
            .values()
            .find(|b| b.block_hash() == *hash)
            .cloned()
            .ok_or("no block".into())
    }
}
impl Chain for Ledger {
    async fn checkpoint(&self, _: u32) -> Result<licash::chain::Checkpoint, String> {
        Err("selection already completed".into())
    }
    async fn broadcast(&self, tx: &Transaction) -> Result<(), String> {
        self.sent.lock().unwrap().push(tx.clone());
        Ok(())
    }
}

fn finish(o: &Opening, name: &str, wires: Vec<Vec<Vec<u8>>>) -> Transaction {
    let p = o.set.iter().find(|p| p.name == name).unwrap();
    let hub = (0..p.inputs())
        .map(|i| o.signed.hub[&sig_name(name, i)].clone())
        .collect::<Vec<_>>();
    let alice = (0..p.inputs())
        .map(|i| {
            let key = sig_name(name, i);
            o.signed
                .alice
                .get(&key)
                .or_else(|| o.kept.get(&key))
                .unwrap()
                .clone()
        })
        .collect::<Vec<_>>();
    p.finish_multi(wires, &hub, &alice)
}

fn counter(o: &Opening, hub: &HubKeys, seed: &[u8; 32], amount: u32) -> Transaction {
    let request = Request {
        amount: 4_096,
        claim_digest: [22; 32],
    };
    let signed = Signer::derive(seed, &o.base.digest()).sign(request);
    let r = close::request(o, &signed).unwrap();
    let (_, b) = close::read_request(o, &r).unwrap();
    let items = r.input[0]
        .witness
        .iter()
        .take(134)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let e = WotsSig::from_consumption_order(
        WotsParams::for_bytes(32),
        &items.into_iter().rev().collect::<Vec<_>>(),
    )
    .unwrap();
    let main = Babe::respond_corrected_wire(
        &hub.b.sign(&request.amount.to_be_bytes()).unwrap(),
        &b,
        &e,
        &hub.e_counter.sign(&request.claim_digest).unwrap(),
        &hub.k.sign(&[0; licash::statement::K_LEN]).unwrap(),
        &hub.b_corr.sign(&amount.to_be_bytes()).unwrap(),
    );
    let mut wires = vec![main];
    let bytes = Disclosures::empty().to_bytes();
    let mut at = 0;
    for (i, size) in CHUNK_BYTES.into_iter().enumerate() {
        let sig = hub.disclosure[i].sign(&bytes[at..at + size]).unwrap();
        wires.push(sig.consumption_order().into_iter().rev().collect());
        at += size;
    }
    finish(o, dispute::RESPOND_COUNTER, wires)
}

async fn lifecycle(amount: u32, challenged: bool) {
    use super::super::store::New;
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[7; 32],
        ChainProfile::Core30DefaultRegtest,
        Offline,
    )
    .unwrap();
    let secrets = wallet.contract_secrets();
    let ctx = Context {
        tip: 0,
        trust: Trust::mock(),
    };
    let bytes =
        include_bytes!("../../../../../crates/licash-contract/tests/golden/holder-funded.bin");
    let o = Holder::load(bytes, secrets.seed, secrets.spend, &ctx)
        .unwrap()
        .opening()
        .unwrap();
    let id = o.base.parts().nonce;
    let raw_hub =
        include_bytes!("../../../../../crates/licash-contract/tests/golden/hub-funded.bin");
    let hub_keys = HubKeys::from_secret(raw_hub[9..41].try_into().unwrap());
    let response = counter(&o, &hub_keys, &secrets.seed, amount);
    let Response::Counter(HubResponse::Counter { corr_sig, .. }) =
        dispute::response(&o, &response).unwrap()
    else {
        panic!("not a counter")
    };
    let claim = dispute::Corrected::new(&o, amount, &corr_sig).unwrap();
    let proof = claim.mock_proof(&ctx.trust).unwrap();
    let export = serde_json::to_vec(&serde_json::json!({
        "image_id": hex::encode(dispute::holder_image(&o)),
        "journal": hex::encode(claim.expected().to_bytes()),
        "proof": hex::encode(dispute::proof_to_points(&proof)),
    }))
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    let deposit = o.base.deposit_outpoint();
    store
        .insert(&New {
            id,
            hub: "http://hub.example",
            mock: true,
            deposit: OutPoint {
                txid: Txid::from_byte_array(deposit.txid),
                vout: deposit.index,
            },
            value: 60_000,
            price: 23_000,
            cap: o.base.parts().cap,
            assert_height: o.babe.spec.windows.t_a,
            close_height: o.babe.spec.terms.t_close,
            since: 0,
            state: bytes,
        })
        .unwrap();
    if challenged {
        let hub = Hub::load(raw_hub, &ctx).unwrap();
        for (s, kept) in o.instances.iter().enumerate() {
            for k in kept {
                let message = licash_contract::wire::Envelope::from_bytes(
                    &hub.instance(s, k.index, &ctx).unwrap(),
                )
                .unwrap();
                let kept = licash_contract::wire::Kept::decode(&message.body).unwrap();
                store.keep(&id, s, kept.index, &kept.instance).unwrap();
            }
        }
    }
    let chain = Ledger::new();
    chain.mine(Some(response.clone()));
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let keys = HolderKeys::derive(&secrets.seed, &id);
    let p = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![
            TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new(),
            },
            TxOut {
                value: Amount::from_sat(20_000),
                script_pubkey: close::fee_script(&keys).unwrap(),
            },
        ],
    };
    let state = State {
        amount: 4_096,
        corrected: Some(amount),
        spend: Some(response),
        p: Some(p),
        spent_height: Some(1),
        from: Some(1),
        deadline: Some(dispute::deadline(&o)),
        ..State::default()
    };
    store
        .start_recovery(&id, FORMAT, Stage::Proving.as_str(), &state.encode())
        .unwrap();
    assert!(store
        .save_recovery(
            &id,
            (Stage::Proving.as_str(), &state.encode()),
            Stage::Proving.as_str(),
            true,
            &state.encode()
        )
        .unwrap());
    // Wrong-mode imports leave the selected row and signing keys untouched.
    let before = store.recovery(&id).unwrap().unwrap().state;
    let mut wrong: serde_json::Value = serde_json::from_slice(&export).unwrap();
    wrong["journal"] = hex::encode(dispute::expected(&o).to_bytes()).into();
    assert!(import_proof(
        &wallet,
        &chain,
        dir.path(),
        &id,
        &serde_json::to_vec(&wrong).unwrap(),
        rate
    )
    .await
    .is_err());
    assert_eq!(store.recovery(&id).unwrap().unwrap().state, before);
    assert!(chain.sent.lock().unwrap().is_empty());
    let imported = import_proof(&wallet, &chain, dir.path(), &id, &export, rate)
        .await
        .unwrap();
    assert_eq!(imported.stage, Stage::Asserting);
    let assertion = chain.last();
    import_proof(&wallet, &chain, dir.path(), &id, &export, rate)
        .await
        .unwrap();
    assert_eq!(
        chain.last(),
        assertion,
        "retry must not sign a different proof or amount"
    );

    // Only Pi1 is Lamport-signed. A miner can alter the unsigned Pi2/Pi3;
    // the original verified receipt must survive confirmation unchanged.
    let mut mined = assertion;
    let mut witness = mined.input[0]
        .witness
        .iter()
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let at = witness.len() - 4 - o.babe.spec.corrected.as_ref().unwrap().lamport.0.len() - 2;
    witness[at] = vec![];
    witness[at + 1] = vec![];
    mined.input[0].witness = Witness::from_slice(&witness);
    let asserted_at = chain.mine(Some(mined.clone()));
    let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(progressed[0].stage, Stage::Asserted, "{progressed:?}");
    assert_eq!(
        State::decode(&store.recovery(&id).unwrap().unwrap())
            .unwrap()
            .proof,
        Some(hex::encode(dispute::proof_bytes(&proof)))
    );

    if challenged {
        use lichen_babe::reveal::{complete, reveal_args};
        let preimages = mined.input[0]
            .witness
            .iter()
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .skip(4)
            .take(o.babe.spec.corrected.as_ref().unwrap().lamport.0.len())
            .map(|p| p.try_into().unwrap())
            .collect::<Vec<[u8; 20]>>();
        let lamport = &o.babe.spec.corrected.as_ref().unwrap().lamport;
        let digits = preimages
            .iter()
            .enumerate()
            .map(|(i, p)| lamport.read(i, p).unwrap())
            .collect::<Vec<_>>();
        let revealed = digits
            .iter()
            .enumerate()
            .map(|(i, d)| hub_keys.corrected.dealer.reveal(i, *d))
            .collect::<Vec<_>>();
        let wire = Babe::challenge_wire(reveal_args(
            &preimages,
            &complete(&o.presigs[1], &digits, &revealed),
        ));
        chain.mine(Some(finish(&o, dispute::CHALLENGE_CORRECTED, vec![wire])));
        let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(progressed[0].stage, Stage::Answering, "{progressed:?}");
    } else {
        chain.to(asserted_at + dispute::success_delay(&o) - 1);
        let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(progressed[0].stage, Stage::Succeeding, "{progressed:?}");
    }
    let paid_at = chain.mine(Some(chain.last()));
    let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(progressed[0].stage, Stage::Paid, "{progressed:?}");
    chain.to(paid_at + close::sweep_delay(&o) - 1);
    let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(progressed[0].stage, Stage::Sweeping, "{progressed:?}");
    let sweep = chain.last();
    assert!(sweep.output[0].value >= sweep.output[0].script_pubkey.minimal_non_dust());
    let received = sweep.output[0].value.to_sat();
    chain.mine(Some(sweep));
    let done = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(done[0].stage, Stage::Done, "{done:?}");
    assert_eq!(done[0].corrected, Some(u64::from(amount)));
    assert_eq!(done[0].received, Some(received));
    assert_eq!(done[0].claimed, 4_096);
    assert_eq!(
        State::decode(&store.recovery(&id).unwrap().unwrap())
            .unwrap()
            .corrected,
        Some(amount)
    );
}

#[tokio::test]
async fn selected_corrected_receipts_import_assert_and_collect_including_zero() {
    lifecycle(0, false).await;
    lifecycle(1_024, false).await;
}

#[tokio::test]
#[ignore = "reconstructs garbled instances; run in release mode"]
async fn a_selected_corrected_receipt_answers_a_challenge_and_collects() {
    lifecycle(15, true).await;
}
