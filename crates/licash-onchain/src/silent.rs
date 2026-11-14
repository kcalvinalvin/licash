//! Silent payments (BIP352): a sender derives a fresh taproot output for a
//! receiver's address from the keys of the inputs it spends, which only the
//! receiver finds again, with its scan key and the transaction's tweak.
//!
//! The outputs are the keys themselves, untweaked: their owner spends them
//! on the key path with [`Keys::spend_secret`].

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    str::FromStr,
    sync::OnceLock,
};

use bitcoin::{
    bech32::{
        primitives::{
            decode::CheckedHrpstring,
            iter::{ByteIterExt, Fe32IterExt},
        },
        Bech32m, Fe32, Hrp,
    },
    bip32::{ChildNumber, Xpriv},
    consensus::serialize,
    hashes::{hash160, sha256, Hash, HashEngine},
    key::{Parity, Secp256k1},
    secp256k1::{All, PublicKey, Scalar, SecretKey, XOnlyPublicKey},
    NetworkKind, OutPoint, Script, Transaction, TxIn, WitnessVersion,
};

/// Most outputs to one receiver in a transaction: senders pay no more, and
/// receivers look for no more.
pub const K_MAX: u32 = 2323;

/// The label of change, which a wallet pays itself at and never hands out.
pub const CHANGE: u32 = 0;

/// BIP341's NUMS point H: inputs spent by a script under it count for nothing.
const NUMS: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not a silent payment address: {0}")]
    Address(&'static str),
    #[error("no silent payment: {0}")]
    Send(&'static str),
    #[error("invalid key")]
    Key,
}

fn secp() -> &'static Secp256k1<All> {
    static SECP: OnceLock<Secp256k1<All>> = OnceLock::new();
    SECP.get_or_init(Secp256k1::new)
}

fn tagged(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let tag = sha256::Hash::hash(tag.as_bytes());
    let mut engine = sha256::Hash::engine();
    engine.input(tag.as_ref());
    engine.input(tag.as_ref());
    for part in parts {
        engine.input(part);
    }
    sha256::Hash::from_engine(engine).to_byte_array()
}

/// A hash as a scalar, failing for 0 and beyond the group order.
fn scalar(hash: [u8; 32]) -> Result<SecretKey, Error> {
    SecretKey::from_slice(&hash).map_err(|_| Error::Send("hash out of range"))
}

/// `t_k`: the tweak of the `k`th output of a shared secret.
fn shared(ecdh: &PublicKey, k: u32) -> Result<SecretKey, Error> {
    scalar(tagged(
        "BIP0352/SharedSecret",
        &[&ecdh.serialize(), &k.to_be_bytes()],
    ))
}

fn input_hash(outpoints: &[OutPoint], sum: &PublicKey) -> Result<SecretKey, Error> {
    let smallest = outpoints
        .iter()
        .map(serialize)
        .min()
        .ok_or(Error::Send("no inputs"))?;
    scalar(tagged("BIP0352/Inputs", &[&smallest, &sum.serialize()]))
}

fn mul(point: &PublicKey, by: &SecretKey) -> PublicKey {
    point
        .mul_tweak(secp(), &Scalar::from(*by))
        .expect("a valid scalar times a point is a point")
}

/// A receiver's silent payment address, for mainnet (`sp1…`) or the test
/// networks (`tsp1…`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpAddress {
    pub scan: PublicKey,
    pub spend: PublicKey,
    pub network: NetworkKind,
}

impl SpAddress {
    fn hrp(network: NetworkKind) -> Hrp {
        let hrp = match network {
            NetworkKind::Main => "sp",
            NetworkKind::Test => "tsp",
        };
        Hrp::parse(hrp).expect("a valid hrp")
    }
}

impl fmt::Display for SpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys = self.scan.serialize().to_vec();
        keys.extend(self.spend.serialize());
        let hrp = Self::hrp(self.network);
        let chars = keys
            .into_iter()
            .bytes_to_fes()
            .with_checksum::<Bech32m>(&hrp)
            .with_witness_version(Fe32::Q)
            .chars();
        chars
            .into_iter()
            .try_for_each(|c| fmt::Write::write_char(f, c))
    }
}

impl FromStr for SpAddress {
    type Err = Error;

    /// Version 0, or a later backwards compatible one, of which only the
    /// keys are read.
    fn from_str(s: &str) -> Result<Self, Error> {
        let checked = CheckedHrpstring::new::<Bech32m>(s).map_err(|_| Error::Address("bech32m"))?;
        let network = match checked.hrp().to_lowercase().as_str() {
            "sp" => NetworkKind::Main,
            "tsp" => NetworkKind::Test,
            _ => return Err(Error::Address("unknown prefix")),
        };
        let data = checked.data_part_ascii_no_checksum();
        let (version, data) = data.split_first().ok_or(Error::Address("empty"))?;
        let version = Fe32::from_char((*version).into()).map_err(|_| Error::Address("version"))?;
        let bytes = to_bytes(data)?;
        let keys = match version.to_u8() {
            0 if bytes.len() == 66 => &bytes[..],
            1..=30 if bytes.len() >= 66 => &bytes[..66],
            0..=30 => return Err(Error::Address("length")),
            _ => return Err(Error::Address("unsupported version")),
        };
        let key = |b: &[u8]| PublicKey::from_slice(b).map_err(|_| Error::Address("key"));
        Ok(Self {
            scan: key(&keys[..33])?,
            spend: key(&keys[33..])?,
            network,
        })
    }
}

/// Bech32 characters as bytes, refusing padding longer than four bits or
/// not zero, as BIP173 does.
fn to_bytes(chars: &[u8]) -> Result<Vec<u8>, Error> {
    let (mut acc, mut bits, mut bytes) = (0u32, 0u32, Vec::with_capacity(chars.len() * 5 / 8));
    for c in chars {
        let fe = Fe32::from_char((*c).into()).map_err(|_| Error::Address("character"))?;
        acc = (acc << 5) | u32::from(fe.to_u8());
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    if bits >= 5 || acc != 0 {
        return Err(Error::Address("padding"));
    }
    Ok(bytes)
}

/// The key of `input`, spending `prevout`, if BIP352 counts it: of a
/// P2TR, P2WPKH, P2SH-P2WPKH or P2PKH output, compressed or x-only.
pub fn input_key(input: &TxIn, prevout: &Script) -> Option<PublicKey> {
    let compressed = |b: &[u8]| match b.len() {
        33 => PublicKey::from_slice(b).ok(),
        _ => None,
    };
    let spk = prevout.as_bytes();
    if prevout.is_p2pkh() {
        // From the back of the script, even of a malleated one: the first
        // 33 bytes that hash to the key hash.
        let hash = &spk[3..23];
        let sig = input.script_sig.as_bytes();
        return (33..=sig.len()).rev().find_map(|end| {
            let key = &sig[end - 33..end];
            (hash160::Hash::hash(key).as_byte_array() == hash)
                .then(|| compressed(key))
                .flatten()
        });
    }
    if prevout.is_p2sh() {
        let sig = input.script_sig.as_bytes();
        let nested = sig.len() == 23 && Script::from_bytes(&sig[1..]).is_p2wpkh();
        return nested.then(|| compressed(input.witness.last()?)).flatten();
    }
    if prevout.is_p2wpkh() {
        return compressed(input.witness.last()?);
    }
    if prevout.is_p2tr() {
        let mut stack: Vec<&[u8]> = input.witness.iter().collect();
        if stack.len() > 1 && stack.last()?.first() == Some(&0x50) {
            stack.pop(); // The annex.
        }
        if stack.len() > 1 {
            let control = stack.last()?;
            let valid = control.len() >= 33
                && control.len() <= 33 + 32 * 128
                && (control.len() - 33) % 32 == 0;
            if !valid || control[1..33] == NUMS {
                return None;
            }
        } else if stack.is_empty() {
            return None;
        }
        let key = XOnlyPublicKey::from_slice(&spk[2..]).ok()?;
        return Some(PublicKey::from_x_only_public_key(key, Parity::Even));
    }
    None
}

/// The tweak of `tx`, spending `prevouts` in its inputs' order: the sum
/// of its inputs' keys that BIP352 counts, times their input hash, which
/// index servers serve receivers. None unless `tx` may pay a silent
/// payment: it has a taproot output and a counted input, spends no output
/// of a segwit version above 1, and its keys do not cancel out.
pub fn tweak(tx: &Transaction, prevouts: &[&Script]) -> Option<PublicKey> {
    if prevouts.len() != tx.input.len() || !tx.output.iter().any(|o| o.script_pubkey.is_p2tr()) {
        return None;
    }
    let later = |s: &&Script| s.witness_version().is_some_and(|v| v > WitnessVersion::V1);
    if prevouts.iter().any(later) {
        return None;
    }
    let keys: Vec<PublicKey> = tx
        .input
        .iter()
        .zip(prevouts)
        .filter_map(|(input, prevout)| input_key(input, prevout))
        .collect();
    if keys.is_empty() {
        return None;
    }
    let sum = PublicKey::combine_keys(&keys.iter().collect::<Vec<_>>()).ok()?;
    let outpoints: Vec<OutPoint> = tx.input.iter().map(|i| i.previous_output).collect();
    Some(mul(&sum, &input_hash(&outpoints, &sum).ok()?))
}

/// The keys of the taproot outputs of `tx` that are keys: what
/// [`Scanner::scan`] looks among.
pub fn taproot_outputs(tx: &Transaction) -> Vec<XOnlyPublicKey> {
    let taproot = tx.output.iter().filter(|o| o.script_pubkey.is_p2tr());
    taproot
        .filter_map(|o| XOnlyPublicKey::from_slice(&o.script_pubkey.as_bytes()[2..]).ok())
        .collect()
}

/// The outputs paying `recipients`, one each in their order, of a
/// transaction spending `outpoints`, all its inputs', with `keys`, the
/// secret keys of those BIP352 counts (see [`input_key`]), each with
/// whether it spends a taproot output. Every output must be in the
/// transaction for the later ones to be found.
pub fn outputs(
    keys: &[(SecretKey, bool)],
    outpoints: &[OutPoint],
    recipients: &[SpAddress],
) -> Result<Vec<XOnlyPublicKey>, Error> {
    // Summed without failing on a zero partial sum.
    let mut sum: Option<SecretKey> = None;
    for (key, taproot) in keys {
        let key = match *taproot && key.x_only_public_key(secp()).1 == Parity::Odd {
            true => key.negate(),
            false => *key,
        };
        sum = match sum {
            None => Some(key),
            Some(sum) => sum.add_tweak(&Scalar::from(key)).ok(),
        };
    }
    let a = sum.ok_or(Error::Send("the input keys sum to zero"))?;
    let hash = input_hash(outpoints, &a.public_key(secp()))?;
    let a = a.mul_tweak(&Scalar::from(hash)).map_err(|_| Error::Key)?;
    let mut groups: HashMap<PublicKey, (PublicKey, u32)> = HashMap::new();
    let mut paid = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let scan = recipient.scan;
        let (ecdh, k) = groups.entry(scan).or_insert_with(|| (mul(&scan, &a), 0));
        if *k == K_MAX {
            return Err(Error::Send("too many outputs to one receiver"));
        }
        let t = shared(ecdh, *k)?;
        *k += 1;
        let output = recipient
            .spend
            .add_exp_tweak(secp(), &Scalar::from(t))
            .map_err(|_| Error::Key)?;
        paid.push(output.x_only_public_key().0);
    }
    Ok(paid)
}

/// An output found paying the receiver, with what its spend key needs
/// adding to spend it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub output: XOnlyPublicKey,
    pub tweak: SecretKey,
    /// The label of the address it paid, if any.
    pub label: Option<u32>,
}

/// A receiver's keys, which spend what they find.
#[derive(Clone)]
pub struct Keys {
    scan: SecretKey,
    spend: SecretKey,
    network: NetworkKind,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys(..)")
    }
}

impl Keys {
    pub fn new(scan: SecretKey, spend: SecretKey, network: NetworkKind) -> Self {
        Self {
            scan,
            spend,
            network,
        }
    }

    /// The keys BIP352 derives from a BIP32 `seed`: m/352'/c'/0'/1'/0 to
    /// scan and m/352'/c'/0'/0'/0 to spend, `c` 0 on mainnet and 1 on the
    /// test networks.
    pub fn from_seed(seed: &[u8], network: NetworkKind) -> Result<Self, Error> {
        let master = Xpriv::new_master(network, seed).map_err(|_| Error::Key)?;
        let coin = match network {
            NetworkKind::Main => 0,
            NetworkKind::Test => 1,
        };
        let key = |branch: u32| -> Result<SecretKey, Error> {
            let path = [
                ChildNumber::from_hardened_idx(352),
                ChildNumber::from_hardened_idx(coin),
                ChildNumber::from_hardened_idx(0),
                ChildNumber::from_hardened_idx(branch),
                ChildNumber::from_normal_idx(0),
            ];
            let path: Vec<ChildNumber> = path
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(|_| Error::Key)?;
            let derived = master.derive_priv(secp(), &path).map_err(|_| Error::Key)?;
            Ok(derived.private_key)
        };
        Ok(Self::new(key(1)?, key(0)?, network))
    }

    /// What scans for the receiver's outputs, without spending them, for
    /// the labels `labels` besides [`CHANGE`].
    pub fn scanner(&self, labels: impl IntoIterator<Item = u32>) -> Scanner {
        Scanner::new(
            self.scan,
            self.spend.public_key(secp()),
            self.network,
            labels,
        )
    }

    /// The receiver's address, unlabeled.
    pub fn address(&self) -> SpAddress {
        self.scanner([]).address()
    }

    /// The secret key of `found`, which signs for its output on the key
    /// path, as it is: the output key carries no taproot tweak.
    pub fn spend_secret(&self, found: &Found) -> Result<SecretKey, Error> {
        self.spend
            .add_tweak(&Scalar::from(found.tweak))
            .map_err(|_| Error::Key)
    }
}

/// The half of a receiver's keys that finds its outputs: its scan secret
/// and spend public key, and the labels it looks for.
#[derive(Clone)]
pub struct Scanner {
    scan: SecretKey,
    spend: PublicKey,
    network: NetworkKind,
    /// Each label's tweak and point.
    labels: BTreeMap<u32, (SecretKey, PublicKey)>,
}

impl fmt::Debug for Scanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Scanner(..)")
    }
}

impl Scanner {
    /// For `labels`, and [`CHANGE`] always.
    pub fn new(
        scan: SecretKey,
        spend: PublicKey,
        network: NetworkKind,
        labels: impl IntoIterator<Item = u32>,
    ) -> Self {
        let mut scanner = Self::exactly(scan, spend, network, labels);
        scanner.add_label(CHANGE);
        scanner
    }

    /// For `labels` only, not even [`CHANGE`] unless named.
    pub fn exactly(
        scan: SecretKey,
        spend: PublicKey,
        network: NetworkKind,
        labels: impl IntoIterator<Item = u32>,
    ) -> Self {
        let mut scanner = Self {
            scan,
            spend,
            network,
            labels: BTreeMap::new(),
        };
        for m in labels {
            scanner.add_label(m);
        }
        scanner
    }

    fn label(&self, m: u32) -> SecretKey {
        // A hash out of range is beyond any practical chance.
        scalar(tagged(
            "BIP0352/Label",
            &[&self.scan.secret_bytes(), &m.to_be_bytes()],
        ))
        .expect("a label in range")
    }

    fn add_label(&mut self, m: u32) {
        let tweak = self.label(m);
        self.labels.insert(m, (tweak, tweak.public_key(secp())));
    }

    /// The receiver's address, unlabeled.
    pub fn address(&self) -> SpAddress {
        SpAddress {
            scan: self.scan.public_key(secp()),
            spend: self.spend,
            network: self.network,
        }
    }

    /// The receiver's address with label `m`, never [`CHANGE`] to others.
    pub fn labeled(&self, m: u32) -> SpAddress {
        let spend = self
            .spend
            .add_exp_tweak(secp(), &Scalar::from(self.label(m)))
            .expect("a labeled key");
        SpAddress {
            spend,
            ..self.address()
        }
    }

    /// The outputs among `outputs`, a transaction's taproot output keys,
    /// that pay the receiver, given the transaction's [`tweak`].
    ///
    /// Each `k`th candidate key, unlabeled then plus each label's point, is
    /// looked up among the outputs by its x coordinate, which matches an
    /// output of either parity, as subtracting it from each output and
    /// its negation would: the work grows with the labels, not the outputs.
    pub fn scan(&self, tweak: &PublicKey, outputs: &[XOnlyPublicKey]) -> Vec<Found> {
        let ecdh = mul(tweak, &self.scan);
        let mut left: HashSet<XOnlyPublicKey> = outputs.iter().copied().collect();
        let mut found = Vec::new();
        for k in 0..K_MAX {
            let Ok(t) = shared(&ecdh, k) else { break };
            let Ok(p) = self.spend.add_exp_tweak(secp(), &Scalar::from(t)) else {
                break;
            };
            let labeled = self.labels.iter().filter_map(|(m, (tweak, point))| {
                Some((p.combine(point).ok()?, Some((*m, tweak))))
            });
            let hit = std::iter::once((p, None))
                .chain(labeled)
                .map(|(point, label)| (point.x_only_public_key().0, label))
                .find(|(output, _)| left.contains(output));
            let Some((output, label)) = hit else { break };
            let tweak = match label {
                None => t,
                Some((_, tweak)) => match t.add_tweak(&Scalar::from(*tweak)) {
                    Ok(tweak) => tweak,
                    Err(_) => break,
                },
            };
            left.remove(&output);
            found.push(Found {
                output,
                tweak,
                label: label.map(|(m, _)| m),
            });
        }
        found
    }
}
