//! Lifecycle tests after selection, on an explicit mock statement and a
//! synthetic chain. The real graph's signatures and amounts are retained.
use super::*;
use bitcoin::{block, hashes::Hash as _, Block, BlockHash, Network, ScriptBuf};
use licash::spend::ChainProfile;
use licash_contract::opening::{Hub, Trust};
use licash_wallet::{Blocks, Response as HttpResponse};
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

async fn lifecycle(amount: u32, altered: bool, answer: bool) {
    use super::super::store::New;
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[7; 32],
        ChainProfile::Core30DefaultRegtest,
        Offline,
    )
    .unwrap();
    let saved = crate::testing::opened();
    assert_eq!(wallet.contract_secrets().seed, saved.seed);
    let ctx = Context {
        tip: 0,
        trust: Trust::mock(),
    };
    let holder = Holder::load(&saved.holder, &saved.seed, &ctx).unwrap();
    let o = holder.signed().unwrap();
    let hub = Hub::load(&saved.hub, &ctx).unwrap();
    let ho = hub.signed().unwrap();
    let id = holder.id();
    let keys = HolderKeys::derive(&saved.seed, &id);
    let response = ho.respond(hub.keys(), amount).unwrap();
    let proof = lichen_babe::statement::mock_prove(&licash_contract::graph::journal(&o.base));
    let assertion = ho.assert(hub.keys(), &proof).unwrap();
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
            value: 60000,
            price: 23000,
            cap: o.base.parts().cap,
            assert_height: o.base.parts().graph.request_height,
            close_height: o.base.parts().graph.close_height,
            since: 0,
            state: &saved.holder,
        })
        .unwrap();
    let chain = Ledger::new();
    chain.mine(Some(response.clone()));
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
                value: Amount::from_sat(20000),
                script_pubkey: dispute::fee_script(&keys.party.payment).unwrap(),
            },
        ],
    };
    let state = State {
        amount: 4096,
        reduced: Some(amount),
        r: Some(o.request(&keys, 4096, &[22; 32]).unwrap()),
        r_height: Some(0),
        response: Some(response),
        response_height: Some(1),
        p: Some(p),
        from: Some(1),
        ..State::default()
    };
    store
        .start_recovery(&id, FORMAT, Stage::Answered.as_str(), &state.encode())
        .unwrap();
    assert!(store
        .save_recovery(
            &id,
            (Stage::Answered.as_str(), &state.encode()),
            Stage::Answered.as_str(),
            true,
            &state.encode()
        )
        .unwrap());
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let mut mined = assertion.clone();
    if altered {
        let mut witness: Vec<_> = mined.input[0].witness.iter().map(|v| v.to_vec()).collect();
        witness[0] = vec![];
        witness[1] = vec![];
        mined.input[0].witness = Witness::from_slice(&witness);
    }
    let asserted_at = chain.mine(Some(mined));
    let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    let observed = State::decode(&store.recovery(&id).unwrap().unwrap()).unwrap();
    assert_eq!(observed.verified, Some(!altered));
    let initial = if altered {
        Stage::Challenging
    } else {
        Stage::Asserted
    };
    assert_eq!(progressed[0].stage, initial, "{progressed:?}");
    let pinned = observed.assertion.clone();
    advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(
        State::decode(&store.recovery(&id).unwrap().unwrap())
            .unwrap()
            .assertion,
        pinned,
        "retry must not select a different assertion or amount"
    );
    let paid_at = if altered {
        let challenge = chain.last();
        let challenged_at = chain.mine(Some(challenge.clone()));
        let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(progressed[0].stage, Stage::Challenged, "{progressed:?}");
        if answer {
            // Pi2/Pi3 are not signed. Even if a miner alters them, the hub
            // retains its original proof and can answer the challenge.
            let i = ho.kept[0];
            let bytes = holder.instance(i).unwrap();
            let tx = ho
                .answer(&assertion, &challenge, i, &bytes)
                .unwrap()
                .unwrap();
            chain.mine(Some(tx))
        } else {
            let delay = blocks(presign(&o.set, "failure").unwrap().tx.input[0].sequence).unwrap();
            chain.to(challenged_at + delay - 1);
            let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
            assert_eq!(progressed[0].stage, Stage::TimingOut, "{progressed:?}");
            chain.mine(Some(chain.last()))
        }
    } else {
        let delay = blocks(presign(&o.set, "success").unwrap().tx.input[0].sequence).unwrap();
        chain.to(asserted_at + delay - 1);
        let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(progressed[0].stage, Stage::Succeeding, "{progressed:?}");
        chain.mine(Some(chain.last()))
    };
    let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    if amount == 0 && !altered {
        // A corrected zero leaves her nothing of the payout to collect.
        let done = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(done[0].stage, Stage::Done, "{progressed:?} {done:?}");
        assert_eq!(done[0].received, Some(0));
        return;
    }
    if progressed[0].stage != Stage::Sweeping {
        assert!(
            matches!(progressed[0].stage, Stage::Paid | Stage::TimedOut),
            "{progressed:?}"
        );
        chain.to(progressed[0]
            .next_at
            .unwrap_or(paid_at + 6)
            .saturating_sub(1));
        let progressed = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
        assert_eq!(progressed[0].stage, Stage::Sweeping, "{progressed:?}");
    }
    let sweep = chain.last();
    assert!(sweep.output[0].value >= sweep.output[0].script_pubkey.minimal_non_dust());
    let received = sweep.output[0].value.to_sat();
    chain.mine(Some(sweep));
    let done = advance(&wallet, &chain, dir.path(), rate).await.unwrap();
    assert_eq!(done[0].stage, Stage::Done, "{done:?}");
    assert_eq!(done[0].corrected, Some(amount as u64));
    assert_eq!(done[0].received, Some(received));
    assert_eq!(done[0].claimed, 4096);
    assert_eq!(
        State::decode(&store.recovery(&id).unwrap().unwrap())
            .unwrap()
            .reduced,
        Some(amount)
    );
}

#[tokio::test]
async fn hub_receipts_collect_the_selected_amount_including_zero() {
    lifecycle(0, false, false).await;
    lifecycle(4096, false, false).await;
}
#[tokio::test]
async fn an_invalid_assertion_pays_the_claim_when_no_answer_arrives() {
    lifecycle(15, true, false).await;
}
#[tokio::test]
#[ignore = "reconstructs garbled instances; run in release mode"]
async fn a_retained_hub_receipt_answers_a_challenge_and_collects() {
    lifecycle(15, true, true).await;
}

fn signed() -> (Opening, HolderKeys) {
    let saved = crate::testing::opened();
    let ctx = Context {
        tip: 0,
        trust: Trust::mock(),
    };
    let holder = Holder::load(&saved.holder, &saved.seed, &ctx).unwrap();
    let keys = HolderKeys::derive(&saved.seed, &holder.id());
    (holder.signed().unwrap(), keys)
}

/// `request` pays her all her reserve but the dust limit (at most half
/// the hub's), for her forced close's fees, at the vout after its slot; where the hub's proof stands
/// she gets that much less of her reserve, and the hub's is as before.
#[test]
fn a_request_pays_her_purse_of_her_reserve_and_each_ends_with_what_it_would() {
    let (o, keys) = signed();
    let g = &o.base.parts().graph;
    let (r_a, r_h, dust) = (g.reserve_alice, g.reserve_hub, g.dust);
    let purse = o.babe.purse().unwrap().to_sat();
    assert_eq!(purse, (r_a - dust).min(r_h / 2));
    let r = o.request(&keys, 4096, &[22; 32]).unwrap();
    assert_eq!(r.output.len(), 4);
    // Her claim's slot just before it, where the statement reads it.
    assert_eq!(r.output[PURSE_VOUT as usize - 1].value.to_sat(), 330);
    let mine = dispute::fee_script(&keys.party.payment).unwrap();
    assert_eq!(r.output[PURSE_VOUT as usize].value.to_sat(), purse);
    assert_eq!(r.output[PURSE_VOUT as usize].script_pubkey, mine);
    let value = o.babe.value().to_sat();
    let bits = (g.low_bits..g.bits).count();
    let paid = |name: &str| {
        let p = presign(&o.set, name).unwrap();
        // What the links before it took of the contract, her purse too.
        (p.tx.output.clone(), value - p.prevout.value.to_sat())
    };
    // Her claim stands: both reserves less what the links took, her purse
    // among it, which she has.
    for name in ["request_timeout", "respond_timeout", "failure"] {
        let (outs, taken) = paid(name);
        assert_eq!(outs[bits].value.to_sat(), r_a + r_h - taken, "{name}");
    }
    // The hub's proof stands: her reserve less her purse, to her payout
    // script; the hub's less the links' floors alone.
    for name in ["success", "wrongly_challenged_0"] {
        let (outs, taken) = paid(name);
        assert_eq!(outs[bits].value.to_sat(), r_a - purse, "{name}");
        assert_eq!(outs[bits].script_pubkey, o.babe.core().pubs[0].payout_spk);
        assert_eq!(
            outs[bits + 1].value.to_sat(),
            r_h - (taken - purse),
            "{name}"
        );
    }
}

/// A contract the hub signed under another graph (opened before `request`
/// paid her purse, say): no forced close, as its request would be invalid.
#[test]
fn a_request_the_hub_signed_under_another_graph_is_refused() {
    let (mut o, keys) = signed();
    let respond = o.offer.sigs["respond"].clone();
    o.offer.sigs.insert("request".into(), respond);
    let refused = o.request(&keys, 4096, &[22; 32]).unwrap_err();
    assert!(
        format!("{refused:#}").contains("opened under an older protocol"),
        "{refused:#}"
    );
}
