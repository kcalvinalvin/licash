//! The app's light client, as the wallet reads Bitcoin through it.

use std::{future::Future, sync::Arc};

use bitcoin::{hashes::Hash, Block, BlockHash};

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

/// A [`ChainSource`] as the wallet's [`licash_wallet::Blocks`], each call
/// on a thread that may block.
pub(crate) struct Chain(pub Arc<dyn ChainSource>);

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
