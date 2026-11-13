//! Stateless frozen-prefix checks for an opening.
//!
//! This component does not certify input confirmations/availability, reserve
//! coins, sign funding, authorize final D, validate a complete BABE graph or
//! admit collateral.

use crate::{
    chain::{IncludedTransaction, OutPoint, Output, Transaction},
    crypto::tagged_hash,
    request::{PublicKeys, Request},
    Error,
};
use bitcoin::{
    secp256k1::{Secp256k1, XOnlyPublicKey},
    taproot::{ControlBlock, LeafVersion},
    Script,
};
use std::collections::BTreeSet;

// BIP341's standard NUMS internal key, also used by pristine LN-GAP TapTree.
const NUMS: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];
fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// An output's negotiated index/value/script, not an output picked by evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphOutput {
    pub index: u32,
    pub output: Output,
}

/// Exact request recipe and pinned script-path commitment. This alone does not
/// prove that the recipe belongs to an agreed funding output or safe full tree.
#[derive(Clone)]
pub struct RequestLeaf {
    base: Option<[u8; 32]>,
    keys: PublicKeys,
    alice: [u8; 32],
    hub: [u8; 32],
    delay: u16,
    script: Vec<u8>,
    control: Vec<u8>,
}
impl std::fmt::Debug for RequestLeaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestLeaf(..)")
    }
}
impl RequestLeaf {
    /// Unbound carrier/fixture leaf, not usable by a full opening constructor.
    pub fn new(
        keys: PublicKeys,
        alice: [u8; 32],
        hub: [u8; 32],
        delay: u16,
        control: Vec<u8>,
    ) -> Result<Self, Error> {
        Self::build(None, keys, alice, hub, delay, control)
    }

    pub fn for_base(
        base: [u8; 32],
        keys: PublicKeys,
        alice: [u8; 32],
        hub: [u8; 32],
        delay: u16,
        control: Vec<u8>,
    ) -> Result<Self, Error> {
        Self::build(Some(base), keys, alice, hub, delay, control)
    }

    fn build(
        base: Option<[u8; 32]>,
        keys: PublicKeys,
        alice: [u8; 32],
        hub: [u8; 32],
        delay: u16,
        control: Vec<u8>,
    ) -> Result<Self, Error> {
        let script = match base {
            Some(base) => keys.request_leaf_for_base(&base, &alice, &hub, delay)?,
            None => keys.request_leaf(&alice, &hub, delay)?,
        };
        let block = ControlBlock::decode(&control).map_err(|_| bad("request control block"))?;
        if block.leaf_version != LeafVersion::TapScript || block.internal_key.serialize() != NUMS {
            return Err(bad("request requires tapscript and NUMS internal key"));
        }
        Ok(Self {
            base,
            keys,
            alice,
            hub,
            delay,
            script,
            control,
        })
    }
    pub fn keys(&self) -> &PublicKeys {
        &self.keys
    }
    pub fn alice(&self) -> &[u8; 32] {
        &self.alice
    }
    pub fn hub(&self) -> &[u8; 32] {
        &self.hub
    }
    pub fn delay(&self) -> u16 {
        self.delay
    }
    pub fn script(&self) -> &[u8] {
        &self.script
    }
    pub fn control(&self) -> &[u8] {
        &self.control
    }

    fn verify_output(&self, output: &Output) -> Result<(), Error> {
        let key = taproot_key(output)?;
        let block =
            ControlBlock::decode(&self.control).map_err(|_| bad("request control block"))?;
        if !block.verify_taproot_commitment(
            &Secp256k1::verification_only(),
            key,
            Script::from_bytes(&self.script),
        ) {
            return Err(bad("request leaf is not committed by funding graph output"));
        }
        Ok(())
    }
}

fn taproot_key(output: &Output) -> Result<XOnlyPublicKey, Error> {
    if output.value == 0
        || output.value > 2_100_000_000_000_000
        || output.script.len() != 34
        || output.script[..2] != [0x51, 0x20]
    {
        return Err(bad("graph output must be positive bounded native taproot"));
    }
    XOnlyPublicKey::from_slice(&output.script[2..]).map_err(|_| Error::InvalidPoint)
}
fn match_output(tx: &Transaction, expected: &GraphOutput) -> Result<(), Error> {
    let index = usize::try_from(expected.index).map_err(|_| bad("graph output index"))?;
    if tx.outputs().get(index) != Some(&expected.output) {
        return Err(bad("graph output index, value or script mismatch"));
    }
    taproot_key(&expected.output)?;
    Ok(())
}

/// Canonical non-witness F/R identities and their actual graph vouts. Changes
/// require a fresh opening; this object supplies no RBF/fee-mutating API.
#[derive(Clone)]
pub struct FrozenPrefix {
    funding: Transaction,
    request: Transaction,
    funding_graph: GraphOutput,
    request_graph: GraphOutput,
    leaf: RequestLeaf,
}
impl std::fmt::Debug for FrozenPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrozenPrefix(..)")
    }
}
impl FrozenPrefix {
    pub fn new(
        funding: Transaction,
        request: Transaction,
        funding_graph: GraphOutput,
        request_graph: GraphOutput,
        leaf: RequestLeaf,
    ) -> Result<Self, Error> {
        if funding.inputs().is_empty()
            || funding.is_coinbase()
            || funding
                .inputs()
                .iter()
                .any(|input| !input.script_sig.is_empty())
        {
            return Err(bad(
                "funding inputs require non-coinbase empty-scriptSig preparation",
            ));
        }
        let mut inputs = BTreeSet::new();
        if funding
            .inputs()
            .iter()
            .any(|input| !inputs.insert((input.previous.txid, input.previous.index)))
        {
            return Err(bad("duplicate funding inputs"));
        }
        match_output(&funding, &funding_graph)?;
        match_output(&request, &request_graph)?;
        leaf.verify_output(&funding_graph.output)?;
        if request.is_coinbase()
            || request.inputs().len() != 1
            || !request.inputs()[0].script_sig.is_empty()
            || request.inputs()[0].previous
                != (OutPoint {
                    txid: funding.txid(),
                    index: funding_graph.index,
                })
        {
            return Err(bad(
                "request must spend exactly the frozen funding graph outpoint",
            ));
        }
        let sequence = request.inputs()[0].sequence;
        // Core 30 interprets version as uint32_t; the raw-byte view exposes
        // rust-bitcoin's i32. Preserve its bits for BIP68/112 activation.
        let relative_lock_active = request.version() as u32 >= 2 && sequence & (1 << 31) == 0;
        if (relative_lock_active
            && (sequence & (1 << 22) != 0 || sequence & 0xffff != u32::from(leaf.delay)))
            || (!relative_lock_active && leaf.delay != 0)
        {
            return Err(bad(
                "request effective relative lock differs from block CSV",
            ));
        }
        if request
            .outputs()
            .iter()
            .try_fold(0u64, |sum, output| sum.checked_add(output.value))
            .is_none_or(|sum| sum > funding_graph.output.value)
        {
            return Err(bad("request outputs exceed frozen input value"));
        }
        Ok(Self {
            funding,
            request,
            funding_graph,
            request_graph,
            leaf,
        })
    }
    pub fn funding(&self) -> &Transaction {
        &self.funding
    }
    pub fn request(&self) -> &Transaction {
        &self.request
    }
    pub fn funding_graph(&self) -> &GraphOutput {
        &self.funding_graph
    }
    pub fn request_graph(&self) -> &GraphOutput {
        &self.request_graph
    }
    pub fn leaf(&self) -> &RequestLeaf {
        &self.leaf
    }

    /// A full opening must compare this with its recomputed Base digest before
    /// deriving final D. Unbound carrier metadata proves no Base agreement.
    pub fn bound_base(&self) -> Result<&[u8; 32], Error> {
        self.leaf
            .base
            .as_ref()
            .ok_or_else(|| bad("unbound request leaf cannot authenticate Base"))
    }
    pub fn funding_outpoint(&self) -> OutPoint {
        OutPoint {
            txid: self.funding.txid(),
            index: self.funding_graph.index,
        }
    }
    pub fn request_outpoint(&self) -> OutPoint {
        OutPoint {
            txid: self.request.txid(),
            index: self.request_graph.index,
        }
    }

    /// A component of the final-D formula, never key-derivation input
    /// or standalone owner/admission authorization. Lengths use fixed u64 BE.
    pub fn commitment(&self) -> [u8; 32] {
        tagged_hash(
            "licash/frozen-prefix/v2",
            &[
                &(self.funding.bytes().len() as u64).to_be_bytes(),
                self.funding.bytes(),
                &self.funding_graph.index.to_be_bytes(),
                &(self.request.bytes().len() as u64).to_be_bytes(),
                self.request.bytes(),
                &self.request_graph.index.to_be_bytes(),
                &self.leaf.keys.to_bytes(),
                &self.leaf.alice,
                &self.leaf.hub,
                &self.leaf.delay.to_be_bytes(),
                &(self.leaf.control.len() as u64).to_be_bytes(),
                &self.leaf.control,
            ],
        )
    }

    /// Compose body identity, authenticated witness access and exact leaf/key
    /// parsing. Caller must additionally bind the chain/profile/checkpoint,
    /// block reference, post-checkpoint/deployment height, depth, P and final D.
    pub fn request_from_included(&self, included: &IncludedTransaction) -> Result<Request, Error> {
        if included.transaction().bytes() != self.request.bytes() {
            return Err(bad("included request differs from frozen body"));
        }
        self.leaf.keys.decode_witness(
            included.input_witness(0)?,
            &self.leaf.script,
            &self.leaf.control,
        )
    }
}
