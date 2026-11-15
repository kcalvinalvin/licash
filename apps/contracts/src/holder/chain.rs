//! What a holder reads of Bitcoin and hands it, and the fate of her F.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{anyhow, Result};
use bitcoin::{BlockHash, OutPoint, Transaction, Txid};
use licash::chain::Checkpoint;
use licash_chain_client::Source;
use licash_contract::opening::{Fate, Holder};
use licash_wallet::Blocks;

/// The best chain as the holder's wallet sees it, and the network.
pub trait Chain: Blocks {
    /// The checkpoint at block `height` of the best chain: its header,
    /// and the times of the blocks before it.
    fn checkpoint(&self, height: u32) -> impl Future<Output = Result<Checkpoint, String>> + Send;

    /// Hand `tx` to the network; one it has already is no error.
    fn broadcast(&self, tx: &Transaction) -> impl Future<Output = Result<(), String>> + Send;
}

/// A node or an Esplora API, as a holder's chain.
pub struct Node(pub Source);

fn text(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl Blocks for Node {
    async fn height(&self) -> Result<u32, String> {
        Ok(self.0.tip().await.map_err(text)?.height)
    }

    async fn hash(&self, height: u32) -> Result<BlockHash, String> {
        self.0.hash(height).await.map_err(text)
    }

    async fn filter(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>, String> {
        self.0.filter(hash).await.map_err(text)
    }

    async fn block(&self, hash: &BlockHash) -> Result<bitcoin::Block, String> {
        self.0.block(hash).await.map_err(text)
    }
}

impl Chain for Node {
    async fn checkpoint(&self, height: u32) -> Result<Checkpoint, String> {
        self.0.checkpoint(height).await.map_err(text)
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<(), String> {
        self.0.broadcast(tx).await.map(|_| ()).map_err(text)
    }
}

/// Where `holder`'s F stands on `chain`, its inputs looked up in the
/// blocks from `since` to the tip: none before she knows F.
pub async fn fate(holder: &Holder, chain: &impl Chain, since: u32) -> Result<Option<Fate>> {
    let Some(funding) = holder.funding() else {
        return Ok(None);
    };
    let inputs: Vec<OutPoint> = funding.tx.input.iter().map(|i| i.previous_output).collect();
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let mut spent: HashMap<OutPoint, (Txid, u32)> = HashMap::new();
    for height in since..=tip {
        let hash = chain.hash(height).await.map_err(|e| anyhow!(e))?;
        let block = chain.block(&hash).await.map_err(|e| anyhow!(e))?;
        for tx in &block.txdata {
            for input in &tx.input {
                if inputs.contains(&input.previous_output) {
                    let depth = tip - height + 1;
                    spent.insert(input.previous_output, (tx.compute_txid(), depth));
                }
            }
        }
    }
    holder
        .fate(|o| Ok::<_, std::convert::Infallible>(spent.get(&o).copied()))
        .map_err(|e| match e {})
}
