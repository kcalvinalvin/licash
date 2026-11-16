//! The app's light client, as the wallet reads Bitcoin through it.

use std::{future::Future, sync::Arc};

use bitcoin::{
    block::Header,
    consensus::{deserialize, serialize},
    hashes::Hash,
    Block, BlockHash, Transaction,
};
use licash::{chain::Checkpoint, spend::ChainProfile};
use licash_wallet::Blocks as _;

/// Bitcoin as the app's light client sees it: on Android, the Go module
/// `org.licash.chain`. Hashes are 32 bytes in Bitcoin's internal order,
/// blocks and transactions consensus serialized. Called from threads of
/// the wallet's own, never the app's main one: it may block.
#[uniffi::export(with_foreign)]
pub trait ChainSource: Send + Sync {
    /// The best chain's height.
    fn height(&self) -> Result<u32, ChainError>;
    /// The hash of the best chain's block at `height`.
    fn hash(&self, height: u32) -> Result<Vec<u8>, ChainError>;
    /// The time the best chain's block at `height` claims, in seconds since
    /// the Unix epoch.
    fn time(&self, height: u32) -> Result<i64, ChainError>;
    /// The header of the best chain's block at `height`, its 80 bytes as
    /// serialized: from the light client's headers, no block fetched.
    fn header(&self, height: u32) -> Result<Vec<u8>, ChainError>;
    /// The basic compact filter (BIP158) of the block of `hash`, checked
    /// against the chain's filter headers.
    fn filter(&self, hash: Vec<u8>) -> Result<Vec<u8>, ChainError>;
    /// The block of `hash`, checked against its header.
    fn block(&self, hash: Vec<u8>) -> Result<Vec<u8>, ChainError>;
    /// Hand the transaction `tx` to the network.
    fn broadcast(&self, tx: Vec<u8>) -> Result<(), ChainError>;
    /// The transactions in no block yet that peers relayed paying the
    /// output script `script`, consensus serialized back to back: hints,
    /// which a peer can make up. Watched from the first call on.
    fn unconfirmed(&self, script: Vec<u8>) -> Result<Vec<u8>, ChainError>;
}

/// The light client failed.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ChainError {
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ChainError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { reason: e.reason }
    }
}

/// A [`ChainSource`] of the chain of a profile as the wallet's
/// [`licash_wallet::Blocks`], and as a contract holder's chain, each call
/// on a thread that may block.
pub(crate) struct Chain(pub Arc<dyn ChainSource>, pub ChainProfile);

impl Chain {
    async fn time(&self, height: u32) -> Result<u32, String> {
        let time = blocking(&self.0, move |c| c.time(height)).await?;
        u32::try_from(time).map_err(|_| format!("block {height} at time {time}"))
    }
}

async fn blocking<R: Send + 'static>(
    source: &Arc<dyn ChainSource>,
    call: impl FnOnce(&dyn ChainSource) -> Result<R, ChainError> + Send + 'static,
) -> Result<R, String> {
    let source = source.clone();
    match tokio::task::spawn_blocking(move || call(source.as_ref())).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(e) => Err(format!("the light client: {e}")),
    }
}

fn block_hash(hash: &[u8]) -> Result<BlockHash, String> {
    let bytes: [u8; 32] = hash
        .try_into()
        .map_err(|_| format!("a block hash of {} bytes", hash.len()))?;
    Ok(BlockHash::from_byte_array(bytes))
}

impl licash_wallet::Blocks for Chain {
    fn height(&self) -> impl Future<Output = Result<u32, String>> + Send {
        blocking(&self.0, |c| c.height())
    }

    fn hash(&self, height: u32) -> impl Future<Output = Result<BlockHash, String>> + Send {
        let hash = blocking(&self.0, move |c| c.hash(height));
        async move { block_hash(&hash.await?) }
    }

    fn filter(
        &self,
        hash: &BlockHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send {
        let hash = hash.to_byte_array().to_vec();
        let filter = blocking(&self.0, move |c| c.filter(hash));
        async move { filter.await.map(Some) }
    }

    fn block(&self, hash: &BlockHash) -> impl Future<Output = Result<Block, String>> + Send {
        let (want, hash) = (*hash, hash.to_byte_array().to_vec());
        let raw = blocking(&self.0, move |c| c.block(hash));
        async move {
            let block: Block = bitcoin::consensus::deserialize(&raw.await?)
                .map_err(|e| format!("not a block: {e}"))?;
            if block.block_hash() != want {
                return Err(format!("block {} for {want}", block.block_hash()));
            }
            Ok(block)
        }
    }
}

impl licash_contracts::holder::Chain for Chain {
    /// From the light client's headers: the block at `height`'s, and the
    /// times of the eleven to it and of the first of its retarget period.
    async fn checkpoint(&self, height: u32) -> Result<Checkpoint, String> {
        let hash = self.hash(height).await?;
        let header = licash_contracts::holder::Chain::header(self, height).await?;
        if header.block_hash() != hash {
            return Err("the chain changed while its checkpoint was read: try again".into());
        }
        let mut times = Vec::new();
        for at in height.saturating_sub(10)..=height {
            times.push(self.time(at).await?);
        }
        let period_start = match self.1.retarget_interval() {
            Some(interval) => Some(self.time(height - height % interval).await?),
            None => None,
        };
        if self.hash(height).await? != hash {
            return Err("the chain changed while its checkpoint was read: try again".into());
        }
        let bytes = serialize(&header).try_into().expect("a header is 80 bytes");
        Checkpoint::new(self.1, height, bytes, times, period_start)
            .map_err(|e| format!("the checkpoint at block {height}: {e}"))
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<(), String> {
        let tx = serialize(tx);
        blocking(&self.0, move |c| c.broadcast(tx)).await
    }

    /// From the light client's headers, no block fetched: a forced
    /// close's statement reads thousands of them.
    async fn header(&self, height: u32) -> Result<Header, String> {
        let bytes = blocking(&self.0, move |c| c.header(height)).await?;
        deserialize(&bytes).map_err(|e| format!("the header of block {height}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        block::Version, blockdata::constants::genesis_block, CompactTarget, Network, TxMerkleNode,
    };
    use licash_contracts::holder::Chain as _;

    /// Regtest headers on genesis, mined, and no block to fetch.
    struct Headers(Vec<Header>);

    impl Headers {
        fn new(n: usize) -> Self {
            let mut headers = vec![genesis_block(Network::Regtest).header];
            while headers.len() < n {
                let prev = headers.last().unwrap();
                let mut header = Header {
                    version: Version::from_consensus(4),
                    prev_blockhash: prev.block_hash(),
                    merkle_root: TxMerkleNode::all_zeros(),
                    time: prev.time + 600,
                    bits: CompactTarget::from_consensus(0x207f_ffff),
                    nonce: 0,
                };
                while header.validate_pow(header.target()).is_err() {
                    header.nonce += 1;
                }
                headers.push(header);
            }
            Self(headers)
        }

        fn at(&self, height: u32) -> Result<&Header, ChainError> {
            self.0.get(height as usize).ok_or(ChainError::Failed {
                reason: format!("no block {height}"),
            })
        }
    }

    impl ChainSource for Headers {
        fn height(&self) -> Result<u32, ChainError> {
            Ok(self.0.len() as u32 - 1)
        }
        fn hash(&self, height: u32) -> Result<Vec<u8>, ChainError> {
            Ok(self.at(height)?.block_hash().to_byte_array().to_vec())
        }
        fn time(&self, height: u32) -> Result<i64, ChainError> {
            Ok(self.at(height)?.time.into())
        }
        fn header(&self, height: u32) -> Result<Vec<u8>, ChainError> {
            Ok(serialize(self.at(height)?))
        }
        fn filter(&self, _: Vec<u8>) -> Result<Vec<u8>, ChainError> {
            unreachable!("no filter is read")
        }
        fn block(&self, _: Vec<u8>) -> Result<Vec<u8>, ChainError> {
            unreachable!("no block is fetched")
        }
        fn broadcast(&self, _: Vec<u8>) -> Result<(), ChainError> {
            unreachable!()
        }
        fn unconfirmed(&self, _: Vec<u8>) -> Result<Vec<u8>, ChainError> {
            unreachable!()
        }
    }

    /// A checkpoint and the headers a forced close's statement reads come
    /// from the light client's headers, no block fetched.
    #[test]
    fn headers_and_checkpoints_are_read_without_their_blocks() {
        let source = Arc::new(Headers::new(14));
        let chain = Chain(source.clone(), ChainProfile::Core30DefaultRegtest);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert_eq!(chain.header(13).await.unwrap(), source.0[13]);
            assert!(chain.header(14).await.is_err());
            let checkpoint = chain.checkpoint(13).await.unwrap();
            assert_eq!(checkpoint.height(), 13);
            assert_eq!(checkpoint.header()[..], serialize(&source.0[13])[..]);
        });
    }
}
