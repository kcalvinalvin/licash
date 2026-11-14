//! The opening's messages as bytes: an envelope (`LCOP`, version, kind,
//! the opening's id, the body's length) around a body of fixed-width
//! fields, length-prefixed blobs and counts. Whatever arrives is refused
//! unless whole and of this version: a receiver checks what it says, not
//! only that it reads.

use anyhow::{bail, ensure, Context, Result};
use k256::elliptic_curve::sec1::FromEncodedPoint;
use k256::{AffinePoint, EncodedPoint, ProjectivePoint};
use licash::crypto::{Point, Signature};
use lichen_babe::reveal::LamportPub;
use lichen_babe::setup::{Hash, Secret, DIGITS, VALUES};
use lichen_babe::vsss::Dealing;
use lngap_btc::adaptor::AdaptorSig;
use lngap_btc::bitcoin::consensus::{deserialize, serialize};
use lngap_btc::bitcoin::secp256k1::{PublicKey, SecretKey, XOnlyPublicKey};
use lngap_btc::bitcoin::{OutPoint, ScriptBuf, Transaction, TxOut};
use lngap_channel::PartyPubKeys;

pub const MAGIC: [u8; 4] = *b"LCOP";
pub const VERSION: u16 = 1;

/// What a message is, in the order of the opening; `Instance` carries one
/// kept instance after `Opened`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Open = 1,
    Terms,
    Keys,
    Commit,
    Choose,
    Opened,
    Consent,
    Signed,
    Funded,
    Instance,
}

impl Kind {
    const ALL: [Kind; 10] = [
        Kind::Open,
        Kind::Terms,
        Kind::Keys,
        Kind::Commit,
        Kind::Choose,
        Kind::Opened,
        Kind::Consent,
        Kind::Signed,
        Kind::Funded,
        Kind::Instance,
    ];

    fn from_u8(kind: u8) -> Option<Kind> {
        Kind::ALL.iter().find(|k| **k as u8 == kind).copied()
    }

    /// The kind of a message's bytes, unchecked: to route or log it.
    pub fn peek(bytes: &[u8]) -> Option<Kind> {
        bytes.get(MAGIC.len() + 2).and_then(|k| Kind::from_u8(*k))
    }
}

/// One message: its kind, the opening it belongs to (the holder's nonce,
/// Base's) and its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub kind: Kind,
    pub id: [u8; 32],
    pub body: Vec<u8>,
}

impl Envelope {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.0.extend(MAGIC);
        w.0.extend(VERSION.to_be_bytes());
        w.0.push(self.kind as u8);
        w.0.extend(self.id);
        w.blob(&self.body);
        w.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        ensure!(r.array::<4>()? == MAGIC, "not an opening's message");
        let version = u16::from_be_bytes(r.array()?);
        ensure!(version == VERSION, "message of version {version}");
        let kind = r.u8()?;
        let kind = Kind::from_u8(kind).with_context(|| format!("message of kind {kind}"))?;
        let id = r.array()?;
        let body = r.blob()?.to_vec();
        r.end()?;
        Ok(Self { kind, id, body })
    }
}

/// A message body's fields, written in order.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

/// A message body's fields, read in the order written.
pub struct Reader<'a>(pub &'a [u8]);

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend(v.to_be_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend(v.to_be_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend(v.to_be_bytes());
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.0.extend(v);
    }
    pub fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("a count of a message fits 32 bits"));
    }
    pub fn blob(&mut self, v: &[u8]) {
        self.count(v.len());
        self.0.extend(v);
    }
    pub fn tx(&mut self, tx: &Transaction) {
        self.blob(&serialize(tx));
    }
    pub fn outpoint(&mut self, o: &OutPoint) {
        self.bytes(&serialize(o));
    }
    pub fn txout(&mut self, o: &TxOut) {
        self.blob(&serialize(o));
    }
    pub fn party(&mut self, p: &PartyPubKeys) {
        for key in [p.funding, p.payment, p.delayed] {
            self.bytes(&key.serialize());
        }
        self.blob(p.payout_spk.as_bytes());
    }
    /// Hashes of fixed width, counted.
    pub fn hashes<const N: usize>(&mut self, hashes: &[[u8; N]]) {
        self.count(hashes.len());
        hashes.iter().for_each(|h| self.bytes(h));
    }
    /// A digit's four values each, counted by digit.
    pub fn digits<T>(&mut self, digits: &[[T; VALUES]], mut each: impl FnMut(&mut Self, &T)) {
        self.count(digits.len());
        for d in digits {
            d.iter().for_each(|v| each(self, v));
        }
    }
    pub fn dealing(&mut self, d: &Dealing) {
        self.count(d.commitments.len());
        for value in d.commitments.iter().flatten() {
            self.count(value.len());
            value.iter().for_each(|c| self.bytes(&point(c)));
        }
    }
    pub fn adaptor_sig(&mut self, s: &AdaptorSig) {
        self.bytes(&s.r_final.serialize());
        self.bytes(&s.r.serialize());
        self.bytes(&s.s_prime.secret_bytes());
    }
    /// Signatures by name, in the order of their names.
    pub fn sigs(&mut self, sigs: &std::collections::HashMap<String, Vec<u8>>) {
        let mut names: Vec<&String> = sigs.keys().collect();
        names.sort();
        self.count(names.len());
        for name in names {
            self.blob(name.as_bytes());
            self.blob(&sigs[name]);
        }
    }
}

fn point(p: &ProjectivePoint) -> [u8; 33] {
    Point::from_projective(p).0
}

impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "message cut short");
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    /// A count of items of at least `min` bytes each: no more than the
    /// rest could hold.
    pub fn count(&mut self, min: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        ensure!(
            n.saturating_mul(min.max(1)) <= self.0.len(),
            "count past the message"
        );
        Ok(n)
    }
    pub fn blob(&mut self) -> Result<&'a [u8]> {
        let n = self.count(1)?;
        self.take(n)
    }
    pub fn end(&self) -> Result<()> {
        ensure!(self.0.is_empty(), "bytes past the message");
        Ok(())
    }
    pub fn tx(&mut self) -> Result<Transaction> {
        Ok(deserialize(self.blob()?)?)
    }
    pub fn outpoint(&mut self) -> Result<OutPoint> {
        Ok(deserialize(self.take(36)?)?)
    }
    pub fn txout(&mut self) -> Result<TxOut> {
        Ok(deserialize(self.blob()?)?)
    }
    pub fn xonly(&mut self) -> Result<XOnlyPublicKey> {
        Ok(XOnlyPublicKey::from_slice(&self.array::<32>()?)?)
    }
    pub fn party(&mut self) -> Result<PartyPubKeys> {
        Ok(PartyPubKeys {
            funding: self.xonly()?,
            payment: self.xonly()?,
            delayed: self.xonly()?,
            payout_spk: ScriptBuf::from_bytes(self.blob()?.to_vec()),
        })
    }
    pub fn hashes<const N: usize>(&mut self) -> Result<Vec<[u8; N]>> {
        let n = self.count(N)?;
        (0..n).map(|_| self.array()).collect()
    }
    pub fn digits<T>(
        &mut self,
        size: usize,
        mut each: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<[T; VALUES]>> {
        let n = self.count(size * VALUES)?;
        ensure!(n == DIGITS, "{n} digits, not {DIGITS}");
        (0..n)
            .map(|_| {
                let values: Vec<T> = (0..VALUES).map(|_| each(self)).collect::<Result<_>>()?;
                Ok(values
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("four values")))
            })
            .collect()
    }
    pub fn dealing(&mut self) -> Result<Dealing> {
        let n = self.count(VALUES * 4)?;
        let commitments = (0..n)
            .map(|_| {
                let values: Vec<Vec<ProjectivePoint>> = (0..VALUES)
                    .map(|_| {
                        let n = self.count(33)?;
                        (0..n).map(|_| decode_point(&self.array()?)).collect()
                    })
                    .collect::<Result<_>>()?;
                Ok(values
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("four values")))
            })
            .collect::<Result<_>>()?;
        Ok(Dealing { commitments })
    }
    pub fn adaptor_sig(&mut self) -> Result<AdaptorSig> {
        Ok(AdaptorSig {
            r_final: self.xonly()?,
            r: PublicKey::from_slice(self.take(33)?)?,
            s_prime: SecretKey::from_slice(self.take(32)?)?,
        })
    }
    pub fn sigs(&mut self) -> Result<std::collections::HashMap<String, Vec<u8>>> {
        let n = self.count(8)?;
        let mut sigs = std::collections::HashMap::new();
        let mut last: Option<String> = None;
        for _ in 0..n {
            let name = String::from_utf8(self.blob()?.to_vec())?;
            ensure!(last.as_ref() < Some(&name), "signatures out of order");
            sigs.insert(name.clone(), self.blob()?.to_vec());
            last = Some(name);
        }
        Ok(sigs)
    }
}

fn decode_point(bytes: &[u8; 33]) -> Result<ProjectivePoint> {
    ensure!(matches!(bytes[0], 2 | 3), "a point not compressed");
    let encoded = EncodedPoint::from_bytes(bytes)?;
    let affine: Option<AffinePoint> = AffinePoint::from_encoded_point(&encoded).into();
    Ok(affine.context("not a point")?.into())
}

/// The assertion a dispute step or a dealing belongs to: the exit's own
/// (journal `c = 0`) or a Corrected contract's (`c = 1`).
pub const STAGES: usize = 2;

/// M1, the holder's: her opening's terms and her side of Base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub spend_root: Point,
    pub party: PartyPubKeys,
    pub deposit: OutPoint,
    pub wanted: Wanted,
    /// Whether she trusts mock proofs: an opening of either kind only.
    pub mock: bool,
}

/// The deadlines a holder asks of a hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub assert_height: u32,
    pub close_height: u32,
    pub response_window: u16,
}

impl Open {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(&self.spend_root.0);
        w.party(&self.party);
        w.outpoint(&self.deposit);
        w.u32(self.wanted.assert_height);
        w.u32(self.wanted.close_height);
        w.u16(self.wanted.response_window);
        w.u8(u8::from(self.mock));
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let open = Self {
            spend_root: Point(r.array()?),
            party: r.party()?,
            deposit: r.outpoint()?,
            wanted: Wanted {
                assert_height: r.u32()?,
                close_height: r.u32()?,
                response_window: r.u16()?,
            },
            mock: match r.u8()? {
                0 => false,
                1 => true,
                other => bail!("trust {other}"),
            },
        };
        r.end()?;
        Ok(open)
    }
}

fn stages<T>(
    r: &mut Reader<'_>,
    mut each: impl FnMut(&mut Reader<'_>) -> Result<T>,
) -> Result<[T; STAGES]> {
    Ok([each(r)?, each(r)?])
}

/// M3, the holder's: her public keys under Base: her request keys (her
/// exit's `b` and `E`), and for each assertion π1's Lamport key and the
/// challenge's adaptor keys; her corrected `b`'s chains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    pub request: licash::request::PublicKeys,
    pub lamport: [LamportPub; STAGES],
    pub adaptor: [Vec<[XOnlyPublicKey; VALUES]>; STAGES],
    pub corrected_b: Vec<[u8; 20]>,
}

impl Keys {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.blob(&self.request.to_bytes());
        for stage in 0..STAGES {
            w.digits(&self.lamport[stage].0, |w, h| w.bytes(h));
            w.digits(&self.adaptor[stage], |w, k| w.bytes(&k.serialize()));
        }
        w.hashes(&self.corrected_b);
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let request = licash::request::PublicKeys::from_bytes(r.blob()?)?;
        let [full, corrected] = stages(&mut r, |r| {
            Ok((
                LamportPub(r.digits(20, |r| r.array())?),
                r.digits(32, |r| r.xonly())?,
            ))
        })?;
        let keys = Self {
            request,
            lamport: [full.0, corrected.0],
            adaptor: [full.1, corrected.1],
            corrected_b: r.hashes()?,
        };
        r.end()?;
        Ok(keys)
    }
}

/// M4, the hub's: F as it funds it, unsigned, the publication coins it
/// funded, final D, and for each assertion its dealing and its instances'
/// commitments, all before the holder chooses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub funding: Transaction,
    pub publication: [(OutPoint, TxOut); 5],
    pub d: [u8; 32],
    pub dealings: [Dealing; STAGES],
    pub commitments: [Vec<Hash>; STAGES],
}

impl Commit {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.tx(&self.funding);
        for (outpoint, txout) in &self.publication {
            w.outpoint(outpoint);
            w.txout(txout);
        }
        w.bytes(&self.d);
        self.dealings.iter().for_each(|d| w.dealing(d));
        self.commitments.iter().for_each(|c| w.hashes(c));
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let funding = r.tx()?;
        let publication: Vec<(OutPoint, TxOut)> = (0..5)
            .map(|_| Ok((r.outpoint()?, r.txout()?)))
            .collect::<Result<_>>()?;
        let commit = Self {
            funding,
            publication: publication.try_into().expect("five coins"),
            d: r.array()?,
            dealings: stages(&mut r, |r| r.dealing())?,
            commitments: stages(&mut r, |r| r.hashes())?,
        };
        r.end()?;
        Ok(commit)
    }
}

/// M5, the holder's: the instances she keeps of each assertion (0-based,
/// ascending); the hub opens the others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choose {
    pub kept: [Vec<usize>; STAGES],
}

impl Choose {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        for kept in &self.kept {
            w.count(kept.len());
            kept.iter()
                .for_each(|k| w.u8(u8::try_from(*k).expect("an instance of 256")));
        }
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let choose = Self {
            kept: stages(&mut r, |r| {
                let n = r.count(1)?;
                (0..n).map(|_| Ok(usize::from(r.u8()?))).collect()
            })?,
        };
        r.end()?;
        Ok(choose)
    }
}

/// One opened instance: its index (0-based), its garbling seed and its
/// input secrets, the dealing's shares at `index + 1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub index: usize,
    pub seed: [u8; 32],
    pub shares: Vec<[Secret; VALUES]>,
}

/// M6, the hub's: each assertion's instances the holder did not keep,
/// opened. The kept follow, one `Instance` message each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub opened: [Vec<Shown>; STAGES],
}

impl Opened {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        for opened in &self.opened {
            w.count(opened.len());
            for s in opened {
                w.u8(u8::try_from(s.index).expect("an instance of 256"));
                w.bytes(&s.seed);
                w.digits(&s.shares, |w, v| w.bytes(v));
            }
        }
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let opened = Self {
            opened: stages(&mut r, |r| {
                let n = r.count(33)?;
                (0..n)
                    .map(|_| {
                        Ok(Shown {
                            index: usize::from(r.u8()?),
                            seed: r.array()?,
                            shares: r.digits(32, |r| r.array())?,
                        })
                    })
                    .collect()
            })?,
        };
        r.end()?;
        Ok(opened)
    }
}

/// M10, the hub's: one kept instance of one assertion, as
/// `lichen_babe::setup::Instance::to_bytes` writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub stage: usize,
    pub index: usize,
    pub instance: Vec<u8>,
}

impl Kept {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(u8::try_from(self.stage).expect("a stage"));
        w.u8(u8::try_from(self.index).expect("an instance of 256"));
        w.blob(&self.instance);
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let kept = Self {
            stage: usize::from(r.u8()?),
            index: usize::from(r.u8()?),
            instance: r.blob()?.to_vec(),
        };
        ensure!(kept.stage < STAGES, "stage {}", kept.stage);
        r.end()?;
        Ok(kept)
    }
}

/// M7, the holder's: her consent to D, her adaptor pre-signatures of each
/// assertion's challenge, and her signatures of the graph but those she
/// withholds (`Babe::withheld`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consent {
    pub consent: Signature,
    pub presigs: [Vec<[AdaptorSig; VALUES]>; STAGES],
    pub sigs: std::collections::HashMap<String, Vec<u8>>,
}

impl Consent {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(&self.consent.0);
        for presigs in &self.presigs {
            w.digits(presigs, |w, s| w.adaptor_sig(s));
        }
        w.sigs(&self.sigs);
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let consent = Self {
            consent: Signature(r.array()?),
            presigs: stages(&mut r, |r| r.digits(97, |r| r.adaptor_sig()))?,
            sigs: r.sigs()?,
        };
        r.end()?;
        Ok(consent)
    }
}

/// M8, the hub's: its signatures of every graph transaction, and F with
/// its own inputs' witnesses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub sigs: std::collections::HashMap<String, Vec<u8>>,
    pub funding: Transaction,
}

impl Signed {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.sigs(&self.sigs);
        w.tx(&self.funding);
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let signed = Self {
            sigs: r.sigs()?,
            funding: r.tx()?,
        };
        r.end()?;
        Ok(signed)
    }
}

/// M9, the holder's: F signed whole, for either party to broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funded {
    pub funding: Transaction,
}

impl Funded {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.tx(&self.funding);
        w.0
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let funded = Self { funding: r.tx()? };
        r.end()?;
        Ok(funded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{HolderKeys, HubKeys};
    use lichen_babe::reveal::points;
    use lngap_btc::bitcoin::hashes::Hash as _;
    use lngap_btc::bitcoin::secp256k1::Message;
    use lngap_btc::bitcoin::{absolute, transaction, Amount, TxIn, Txid};

    fn tx() -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint(1),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        }
    }

    fn outpoint(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout: u32::from(n),
        }
    }

    fn envelope(kind: Kind) -> Vec<u8> {
        Envelope {
            kind,
            id: [5; 32],
            body: vec![1, 2, 3],
        }
        .to_bytes()
    }

    #[test]
    fn envelopes_read_whole_and_of_this_version_only() {
        let bytes = envelope(Kind::Consent);
        assert_eq!(Kind::peek(&bytes), Some(Kind::Consent));
        let read = Envelope::from_bytes(&bytes).unwrap();
        assert_eq!(
            (read.kind, read.id, read.body),
            (Kind::Consent, [5; 32], vec![1, 2, 3])
        );
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        let mut version = bytes.clone();
        version[5] = 2;
        let mut kind = bytes.clone();
        kind[6] = 11;
        let mut long = bytes.clone();
        long.push(0);
        let mut length = bytes.clone();
        length[42] = 4;
        let short = &bytes[..bytes.len() - 1];
        for bad in [&magic[..], &version, &kind, &long, &length, short, &[]] {
            assert!(Envelope::from_bytes(bad).is_err());
        }
        assert_eq!(Kind::peek(&kind), None);
    }

    #[test]
    fn counts_past_the_message_are_refused_before_allocating() {
        let mut w = Writer::default();
        w.u32(u32::MAX);
        assert!(Reader(&w.0).hashes::<32>().is_err());
        assert!(Reader(&w.0).blob().is_err());
        assert!(Reader(&w.0).sigs().is_err());
    }

    #[test]
    fn the_holders_messages_read_back() {
        let keys = HolderKeys::from_label("wire");
        let open = Open {
            spend_root: Point([2; 33]),
            party: keys.party.public(),
            deposit: outpoint(3),
            wanted: Wanted {
                assert_height: 160,
                close_height: 300,
                response_window: 6,
            },
            mock: true,
        };
        assert_eq!(Open::decode(&open.encode()).unwrap(), open);
        let mut trust = open.encode();
        *trust.last_mut().unwrap() = 2;
        assert!(Open::decode(&trust).is_err());
        let public = Keys {
            request: licash::request::Signer::derive(&[7; 32], &[1; 32]).public(),
            lamport: [keys.lamport.public(), keys.corrected.lamport.public()],
            adaptor: [keys.adaptor.public(), keys.corrected.adaptor.public()],
            corrected_b: keys.corrected.b_key.public().digits,
        };
        assert_eq!(Keys::decode(&public.encode()).unwrap(), public);
        let choose = Choose {
            kept: [vec![0, 3], vec![1, 2]],
        };
        assert_eq!(Choose::decode(&choose.encode()).unwrap(), choose);
        let hub = HubKeys::from_label("wire");
        let msg = Message::from_digest([9; 32]);
        let mut sigs = std::collections::HashMap::new();
        sigs.insert("request".to_owned(), vec![4; 64]);
        sigs.insert("assert#1".to_owned(), vec![6; 64]);
        let consent = Consent {
            consent: Signature([8; 64]),
            presigs: [
                keys.adaptor.presign(&msg, &points(&hub.full.dealing)),
                keys.corrected
                    .adaptor
                    .presign(&msg, &points(&hub.corrected.dealing)),
            ],
            sigs,
        };
        assert_eq!(Consent::decode(&consent.encode()).unwrap(), consent);
        let funded = Funded { funding: tx() };
        assert_eq!(Funded::decode(&funded.encode()).unwrap(), funded);
    }

    #[test]
    fn the_hubs_messages_read_back() {
        let hub = HubKeys::from_label("wire");
        let commit = Commit {
            funding: tx(),
            publication: std::array::from_fn(|i| (outpoint(i as u8), tx().output[0].clone())),
            d: [4; 32],
            dealings: [hub.full.dealing.clone(), hub.corrected.dealing.clone()],
            commitments: [vec![[1; 32]; 4], vec![[2; 32]; 4]],
        };
        assert_eq!(Commit::decode(&commit.encode()).unwrap(), commit);
        let opened = Opened {
            opened: std::array::from_fn(|s| {
                (0..2)
                    .map(|i| Shown {
                        index: i,
                        seed: [s as u8; 32],
                        shares: hub.full.dealer.shares(i as u64 + 1),
                    })
                    .collect()
            }),
        };
        assert_eq!(Opened::decode(&opened.encode()).unwrap(), opened);
        let kept = Kept {
            stage: 1,
            index: 3,
            instance: vec![7; 100],
        };
        assert_eq!(Kept::decode(&kept.encode()).unwrap(), kept);
        let mut stage = kept.encode();
        stage[0] = 2;
        assert!(Kept::decode(&stage).is_err());
        let mut sigs = std::collections::HashMap::new();
        sigs.insert("respond".to_owned(), vec![5; 64]);
        let signed = Signed {
            sigs,
            funding: tx(),
        };
        assert_eq!(Signed::decode(&signed.encode()).unwrap(), signed);
    }
}
