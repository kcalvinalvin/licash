//! Deposits: coins a payer locks on Bitcoin for a hub, in a taproot output
//! of two scripts. The hub claims it with the preimage of a hash and its
//! signature; the payer takes it back with its own signature once a block
//! height passes. Its internal key is the two keys' aggregate (BIP327's
//! `KeyAgg` of the claim's and the refund's, in that order): spent by its
//! scripts alone, as neither party signs for the other, but a spend of it
//! counts as an input of silent payments (BIP352), which a payout's claim
//! pays.
//!
//! The same terms pay a wallet out: a swap provider locks coins the
//! wallet claims with the preimage, paying its payee exactly what it is
//! owed ([`Htlc::claim_exact`]), and the provider reads the preimage from
//! the claim ([`Htlc::preimage_in`]).

use crate::key_agg::{key_agg, reduce, tagged, KeyAgg};
use bitcoin::{
    absolute::LockTime,
    hashes::Hash,
    key::Secp256k1,
    opcodes::all::{OP_CHECKSIG, OP_CLTV, OP_CSV, OP_DROP, OP_EQUALVERIFY, OP_SHA256, OP_SIZE},
    script::Builder,
    secp256k1::{Keypair, Message, Parity, Scalar, SecretKey, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache},
    taproot::{LeafVersion, Signature, TaprootBuilder, TaprootSpendInfo},
    transaction, Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, Sequence, TapLeafHash,
    TapSighashType, Transaction, TxIn, TxOut, Witness,
};

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
    /// confirms in block `timeout + 1` at the earliest. Ignored (and none
    /// of the script's) where `wait` is set.
    pub timeout: u32,
    /// Blocks after the deposit's payment confirms that the payer may take
    /// it back, in place of `timeout`: a refund leaf of `OP_CHECKSEQUENCEVERIFY`,
    /// so that the script depends on no height, and a wallet knowing only its
    /// keys and the hub's can rebuild it. None: the refund is by `timeout`.
    pub wait: Option<u16>,
    /// Whether the claim takes a preimage of 32 bytes only (`OP_SIZE 32
    /// OP_EQUALVERIFY` first), as every lock and Lightning payment does.
    /// Where the claimer chose the hash, as a payout's client does, a claim
    /// with a longer preimage of it would take the coin and leave its payer
    /// none to settle the lock it pays for: such coins are sized. The hub's
    /// deposits, whose hashes their payers choose, are not.
    pub sized: bool,
}

/// What a sized claim's preimage is, in bytes.
const PREIMAGE_BYTES: i64 = 32;

/// How many blocks after its payment a deposit that returns a contract's
/// reserve may be taken back by its payer ([`Htlc::wait`]): some two weeks.
pub const CLOSE_WAIT: u16 = 2_016;

/// The tweak of the claim key of the hub whose deposit root is `root`, for
/// the deposit under `hash` that its payer takes back with `refund`. Of the
/// refund key too, which no claim shows, so that none who holds the root
/// can tell which deposits one hub's claims are.
fn close_tweak(root: &XOnlyPublicKey, hash: &[u8; 32], refund: &XOnlyPublicKey) -> Scalar {
    reduce(tagged(
        "licash/deposit/close-claim",
        &[&root.serialize(), hash, &refund.serialize()],
    ))
}

/// The key a hub claims the deposit under `hash` with, that returns a
/// contract's reserve to its payer, who takes it back with `refund`: the hub's
/// deposit root `root` and the tweak of that deposit, as the hub
/// publishes the root and anyone derives the key. So a wallet restored from
/// its seed, and the root, knows every address it may have paid.
pub fn close_claim_key(
    root: &XOnlyPublicKey,
    hash: &[u8; 32],
    refund: &XOnlyPublicKey,
) -> XOnlyPublicKey {
    let secp = Secp256k1::verification_only();
    let tweaked = root
        .public_key(Parity::Even)
        .add_exp_tweak(&secp, &close_tweak(root, hash, refund))
        .expect("a root tweaked to infinity: not found but by breaking SHA256");
    tweaked.x_only_public_key().0
}

/// The secret key of [`close_claim_key`], of the hub whose deposit root's
/// secret is `root_secret` (whichever the parity of its point: the root is
/// the point of even y of its x coordinate).
pub fn close_claim_secret(
    root_secret: &SecretKey,
    hash: &[u8; 32],
    refund: &XOnlyPublicKey,
) -> SecretKey {
    let secp = Secp256k1::new();
    let (root, parity) = root_secret.x_only_public_key(&secp);
    let even = match parity {
        Parity::Even => *root_secret,
        Parity::Odd => root_secret.negate(),
    };
    even.add_tweak(&close_tweak(&root, hash, refund))
        .expect("a secret tweaked to zero: not found but by breaking SHA256")
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
    /// All of the deposit but the fee at a rate, or all of it but dust if
    /// that fee takes more.
    Capped(FeeRate),
}

impl Htlc {
    /// `OP_SHA256 <hash> OP_EQUALVERIFY <claim> OP_CHECKSIG`, after
    /// `OP_SIZE 32 OP_EQUALVERIFY` where [`Htlc::sized`].
    pub fn claim_script(&self) -> ScriptBuf {
        let builder = match self.sized {
            true => Builder::new()
                .push_opcode(OP_SIZE)
                .push_int(PREIMAGE_BYTES)
                .push_opcode(OP_EQUALVERIFY),
            false => Builder::new(),
        };
        builder
            .push_opcode(OP_SHA256)
            .push_slice(self.hash)
            .push_opcode(OP_EQUALVERIFY)
            .push_x_only_key(&self.claim)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// `<timeout> OP_CHECKLOCKTIMEVERIFY OP_DROP <refund> OP_CHECKSIG`, or
    /// `<wait> OP_CHECKSEQUENCEVERIFY OP_DROP <refund> OP_CHECKSIG` where
    /// `wait` is set.
    pub fn refund_script(&self) -> ScriptBuf {
        let (delay, check) = match self.wait {
            Some(wait) => (i64::from(wait), OP_CSV),
            None => (i64::from(self.timeout), OP_CLTV),
        };
        Builder::new()
            .push_int(delay)
            .push_opcode(check)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.refund)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// The height after which the payer may take it back, its payment
    /// having confirmed in block `funded`: `timeout`, or `wait` blocks after
    /// it, the first block the refund confirms in being `wait` after it.
    pub fn refund_height(&self, funded: u32) -> u32 {
        match self.wait {
            Some(wait) => funded.saturating_add(u32::from(wait)).saturating_sub(1),
            None => self.timeout,
        }
    }

    /// Its internal key's aggregate: of the claim key, then the refund
    /// key, each the point of even y of its x coordinate.
    pub fn key_agg(&self) -> KeyAgg {
        let keys = [self.claim, self.refund].map(|k| k.public_key(Parity::Even));
        // Infinity only if the claim key's coefficient, a hash, weighs it
        // into the refund key's negation: not found but by breaking SHA256.
        key_agg(&keys).expect("two keys of even y aggregate")
    }

    /// Its taproot tree: two leaves, the claim's and the refund's.
    pub fn spend_info(&self) -> TaprootSpendInfo {
        let secp = Secp256k1::verification_only();
        let internal = self.key_agg().point.x_only_public_key().0;
        TaprootBuilder::new()
            .add_leaf(1, self.claim_script())
            .and_then(|tree| tree.add_leaf(1, self.refund_script()))
            .expect("two leaves of depth one")
            .finalize(&secp, internal)
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

    /// [`Htlc::claim_tx`] at `rate`, or paying `to` only dust if the fee at
    /// `rate` takes more: a claim racing a deadline, its claimer losing more
    /// than the fee if it is late.
    pub fn claim_capped(
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
            Pay::Capped(rate),
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

    /// Whether `tx` spends `outpoint` by the claim's script: a claim of
    /// the claimer's, whichever it made, not the payer's refund.
    pub fn claimed_by(&self, tx: &Transaction, outpoint: OutPoint) -> bool {
        tx.input
            .iter()
            .find(|i| i.previous_output == outpoint)
            .and_then(|i| i.witness.taproot_leaf_script())
            .is_some_and(|leaf| {
                leaf.version == LeafVersion::TapScript
                    && leaf.script == self.claim_script().as_script()
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

    /// [`Htlc::refund_tx`] at `rate`, or paying `to` only dust if the fee at
    /// `rate` takes more ([`Htlc::claim_capped`]).
    pub fn refund_capped(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        rate: FeeRate,
        secret: &SecretKey,
    ) -> Result<Transaction, Error> {
        self.spend(funding, value, to, Pay::Capped(rate), secret, Path::Refund)
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
        // A claim's input is replaceable and enforces no time; a refund's
        // enforces its lock time, or its relative wait.
        let (key, script, lock_time, sequence) = match (path, self.wait) {
            (Path::Claim(_), _) => (
                self.claim,
                self.claim_script(),
                LockTime::ZERO,
                Sequence::ENABLE_RBF_NO_LOCKTIME,
            ),
            (Path::Refund, None) => (
                self.refund,
                self.refund_script(),
                LockTime::from_consensus(self.timeout),
                Sequence::ENABLE_RBF_NO_LOCKTIME,
            ),
            (Path::Refund, Some(wait)) => (
                self.refund,
                self.refund_script(),
                LockTime::ZERO,
                Sequence::from_height(wait),
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
                sequence,
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
            Pay::Capped(rate) if value >= dust => {
                let fee = rate.fee_wu(tx.weight()).unwrap_or(Amount::MAX);
                value.checked_sub(fee).unwrap_or(dust).max(dust)
            }
            Pay::Capped(_) => return Err(Error::TooSmall { value, fee: value }),
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
