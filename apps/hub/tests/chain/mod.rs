//! A self-mined regtest chain, for the tests that need Bitcoin behind a hub.

#![allow(dead_code)]

use bitcoin::{
    absolute::LockTime,
    block::{Header, Version},
    hashes::Hash,
    pow::CompactTarget,
    transaction, Amount, Block, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use licash::{
    chain::{activation_script, Checkpoint},
    spend::ChainProfile,
};

/// Blocks from genesis, the best chain.
pub struct Chain {
    pub blocks: Vec<Block>,
    /// Makes each coinbase unique, across forks too.
    mined: u32,
}

impl Chain {
    pub fn new() -> Self {
        Self {
            blocks: vec![bitcoin::constants::genesis_block(Network::Regtest)],
            mined: 0,
        }
    }

    pub fn height(&self) -> u32 {
        self.blocks.len() as u32 - 1
    }

    /// The tip's hash, in Bitcoin's byte order.
    pub fn tip(&self) -> [u8; 32] {
        self.blocks.last().unwrap().block_hash().to_byte_array()
    }

    /// Mine a block on the tip, with an activation of `secret` if given.
    pub fn mine(&mut self, secret: Option<[u8; 32]>) -> &Block {
        self.mine_with(secret, Vec::new())
    }

    /// [`Chain::mine`], with `txs` after the coinbase.
    pub fn mine_with(&mut self, secret: Option<[u8; 32]>, txs: Vec<Transaction>) -> &Block {
        self.mined += 1;
        let previous = self.blocks.last().unwrap().header;
        let mut output = vec![TxOut {
            value: Amount::from_sat(50),
            script_pubkey: ScriptBuf::new_op_return(self.mined.to_be_bytes()),
        }];
        if let Some(secret) = secret {
            output.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(activation_script(&secret)),
            });
        }
        let coinbase = Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![0x01, 0x01]),
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
            txdata: std::iter::once(coinbase).chain(txs).collect(),
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        while block.header.validate_pow(block.header.target()).is_err() {
            block.header.nonce += 1;
        }
        self.blocks.push(block);
        self.blocks.last().unwrap()
    }

    /// The checkpoint at `height`.
    pub fn checkpoint(&self, height: u32) -> Checkpoint {
        let headers = &self.blocks[..=height as usize];
        let times = headers
            .iter()
            .rev()
            .take(11)
            .rev()
            .map(|b| b.header.time)
            .collect();
        let bytes = bitcoin::consensus::serialize(&headers[height as usize].header);
        Checkpoint::new(
            ChainProfile::Core30DefaultRegtest,
            height,
            bytes.try_into().unwrap(),
            times,
            None,
        )
        .unwrap()
    }
}
