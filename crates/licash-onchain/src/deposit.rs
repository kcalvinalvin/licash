//! Deposits: coins a payer locks on Bitcoin for a hub, in a taproot output
//! of two scripts. The hub claims it with the preimage of a hash and its
//! signature; the payer takes it back with its own signature once a block
//! height passes. Its key path is unspendable: its internal key is the
//! point of BIP341 that no one knows the secret of.
//!
//! The same terms pay a wallet out: a swap provider locks coins the
//! wallet claims with the preimage, paying its payee exactly what it is
//! owed ([`Htlc::claim_exact`]), and the provider reads the preimage from
//! the claim ([`Htlc::preimage_in`]).

use bitcoin::{
    absolute::LockTime,
    hashes::Hash,
    key::Secp256k1,
    opcodes::all::{OP_CHECKSIG, OP_CLTV, OP_DROP, OP_EQUALVERIFY, OP_SHA256},
    script::Builder,
    secp256k1::{Keypair, Message, SecretKey, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache},
    taproot::{LeafVersion, Signature, TaprootBuilder, TaprootSpendInfo},
    transaction, Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, Sequence, TapLeafHash,
    TapSighashType, Transaction, TxIn, TxOut, Witness,
};

/// The x coordinate of BIP341's point of unknown discrete logarithm.
const NUMS: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

/// Why a deposit cannot be spent as asked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the deposit of {value} cannot pay a fee of {fee} and leave more than dust")]
    TooSmall { value: Amount, fee: Amount },
    #[error("the deposit of {value} cannot pay exactly {amount} and no dust")]
    Exact { value: Amount, amount: Amount },
    #[error("not the preimage of the deposit's hash")]
    Preimage,
    #[error("not the key the deposit names")]
    Key,
    #[error("signing: {0}")]
    Signing(String),
}

/// A deposit's terms: what it is paid under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Htlc {
    /// The SHA256 hash the hub claims it with the preimage of.
    pub hash: [u8; 32],
    /// The hub's key.
    pub claim: XOnlyPublicKey,
    /// The payer's key.
    pub refund: XOnlyPublicKey,
    /// The height after which the payer may take it back: its refund
    /// confirms in block `timeout + 1` at the earliest.
    pub timeout: u32,
}

/// How a deposit is spent.
#[derive(Clone, Copy)]
enum Path {
    Claim([u8; 32]),
    Refund,
}

/// What a spend pays its one output.
#[derive(Clone, Copy)]
enum Pay {
    /// All of the deposit but the fee at a rate.
    All(FeeRate),
    /// Exactly an amount, the rest of the deposit its fee.
    Exact(Amount),
}

impl Htlc {
    /// `OP_SHA256 <hash> OP_EQUALVERIFY <claim> OP_CHECKSIG`
    pub fn claim_script(&self) -> ScriptBuf {
        Builder::new()
            .push_opcode(OP_SHA256)
            .push_slice(self.hash)
            .push_opcode(OP_EQUALVERIFY)
            .push_x_only_key(&self.claim)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// `<timeout> OP_CHECKLOCKTIMEVERIFY OP_DROP <refund> OP_CHECKSIG`
    pub fn refund_script(&self) -> ScriptBuf {
        Builder::new()
            .push_int(i64::from(self.timeout))
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.refund)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    fn spend_info(&self) -> TaprootSpendInfo {
        let secp = Secp256k1::verification_only();
        let nums = XOnlyPublicKey::from_slice(&NUMS).expect("BIP341's point");
        TaprootBuilder::new()
            .add_leaf(1, self.claim_script())
            .and_then(|tree| tree.add_leaf(1, self.refund_script()))
            .expect("two leaves of depth one")
            .finalize(&secp, nums)
            .expect("a complete tree")
    }

    /// The output script it is paid to.
    pub fn script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_p2tr_tweaked(self.spend_info().output_key())
    }

    /// The address it is paid to, on `network`.
    pub fn address(&self, network: Network) -> Address {
        Address::p2tr_tweaked(self.spend_info().output_key(), network)
    }

    /// The hub's claim of `funding`, the deposit's output worth `value`,
    /// paying `to` all of it but the fee at `rate`, with the preimage of
    /// its hash and the secret key of `claim`.
    pub fn claim_tx(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        rate: FeeRate,
        secret: &SecretKey,
        preimage: &[u8; 32],
    ) -> Result<Transaction, Error> {
        self.check_preimage(preimage)?;
        self.spend(
            funding,
            value,
            to,
            Pay::All(rate),
            secret,
            Path::Claim(*preimage),
        )
    }

    /// The claim of `funding`, worth `value`, paying `to` exactly `amount`
    /// and the rest of it as its fee: a payout's, whose payee is owed
    /// `amount` whatever the fee comes to, which its claimer judges.
    /// Refused unless `amount` is at most `value` and no dust.
    pub fn claim_exact(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        amount: Amount,
        secret: &SecretKey,
        preimage: &[u8; 32],
    ) -> Result<Transaction, Error> {
        self.check_preimage(preimage)?;
        self.spend(
            funding,
            value,
            to,
            Pay::Exact(amount),
            secret,
            Path::Claim(*preimage),
        )
    }

    /// The preimage of its hash in `tx`'s spend of `outpoint`, if it spends
    /// it with one: how a party whose coin was claimed learns the preimage,
    /// to settle what else it unlocks. Read from any item of the input's
    /// witness, which only the preimage hashes to the hash.
    pub fn preimage_in(&self, tx: &Transaction, outpoint: OutPoint) -> Option<[u8; 32]> {
        let input = tx.input.iter().find(|i| i.previous_output == outpoint)?;
        input.witness.iter().find_map(|item| {
            let item: [u8; 32] = item.try_into().ok()?;
            self.check_preimage(&item).is_ok().then_some(item)
        })
    }

    fn check_preimage(&self, preimage: &[u8; 32]) -> Result<(), Error> {
        if bitcoin::hashes::sha256::Hash::hash(preimage).to_byte_array() != self.hash {
            return Err(Error::Preimage);
        }
        Ok(())
    }

    /// The payer's refund of `funding`, as [`Htlc::claim_tx`] but with the
    /// secret key of `refund`: it confirms after block `timeout`.
    pub fn refund_tx(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        rate: FeeRate,
        secret: &SecretKey,
    ) -> Result<Transaction, Error> {
        self.spend(funding, value, to, Pay::All(rate), secret, Path::Refund)
    }
}

impl Htlc {
    fn spend(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        pay: Pay,
        secret: &SecretKey,
        path: Path,
    ) -> Result<Transaction, Error> {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, secret);
        let (key, script, lock_time) = match path {
            Path::Claim(_) => (self.claim, self.claim_script(), LockTime::ZERO),
            Path::Refund => (
                self.refund,
                self.refund_script(),
                LockTime::from_consensus(self.timeout),
            ),
        };
        if keypair.x_only_public_key().0 != key {
            return Err(Error::Key);
        }
        let control = self
            .spend_info()
            .control_block(&(script.clone(), LeafVersion::TapScript))
            .expect("a leaf of the tree")
            .serialize();
        let witness = |signature: &[u8]| {
            let mut witness = Witness::new();
            witness.push(signature);
            if let Path::Claim(preimage) = path {
                witness.push(preimage);
            }
            witness.push(script.as_bytes());
            witness.push(&control);
            witness
        };
        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time,
            input: vec![TxIn {
                previous_output: funding,
                script_sig: ScriptBuf::new(),
                // Replaceable, and its lock time enforced.
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                // Sized with a signature's 64 bytes.
                witness: witness(&[0; 64]),
            }],
            output: vec![TxOut {
                value,
                script_pubkey: to,
            }],
        };
        let dust = tx.output[0].script_pubkey.minimal_non_dust();
        tx.output[0].value = match pay {
            Pay::All(rate) => {
                let fee = rate.fee_wu(tx.weight()).unwrap_or(Amount::MAX);
                value
                    .checked_sub(fee)
                    .filter(|left| *left >= dust)
                    .ok_or(Error::TooSmall { value, fee })?
            }
            Pay::Exact(amount) if amount <= value && amount >= dust => amount,
            Pay::Exact(amount) => return Err(Error::Exact { value, amount }),
        };
        let prevout = TxOut {
            value,
            script_pubkey: self.script_pubkey(),
        };
        let leaf = TapLeafHash::from_script(&script, LeafVersion::TapScript);
        let sighash = SighashCache::new(&tx)
            .taproot_script_spend_signature_hash(
                0,
                &Prevouts::All(&[prevout]),
                leaf,
                TapSighashType::Default,
            )
            .map_err(|e| Error::Signing(e.to_string()))?;
        let message = Message::from_digest(sighash.to_byte_array());
        let signature = Signature {
            signature: secp.sign_schnorr_no_aux_rand(&message, &keypair),
            sighash_type: TapSighashType::Default,
        };
        tx.input[0].witness = witness(&signature.to_vec());
        Ok(tx)
    }
}
