//! Owner keys, addresses and receipts.
//!
//! A user has independent spend, scan and hold keys; `(B_s, B_v, B_h)` is
//! their address. A payer picks an ephemeral key `A = aG` for a proposal and
//! derives the recipient's output at each position as in silent payments
//! (BIP352): `shared = H_ecdh(xcoord(a B_v))`, then [`Derivation::derive`].
//! The recipient recomputes `shared` from `A` with the scan key and
//! recognizes its outputs before accepting them.
//!
//! An output's derivation index is its position in its branch of the
//! proposal; refund outputs of a timed lock count from [`REFUND_INDEX`], so
//! no payment and refund output of one proposal share an index.

use k256::{ProjectivePoint, Scalar};

use crate::{
    claim::Branch,
    crypto::{generator_mul, random_scalar, scalar_from_hash, tagged_hash, Point},
    note::{non_identity, Derivation, HoldRoot},
    spend::{Output, Proposal},
    Error,
};

pub const ADDRESS_BYTES: usize = 3 * 33;

/// The first derivation index of a timed lock's refund outputs.
pub const REFUND_INDEX: u32 = 1 << 31;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// The derivation index of the output at `position` of `branch`.
pub fn index(branch: Branch, position: usize) -> Result<u32, Error> {
    let position = u32::try_from(position)
        .ok()
        .filter(|p| *p < REFUND_INDEX)
        .ok_or_else(|| bad("output position out of range"))?;
    Ok(match branch {
        Branch::Pay => position,
        Branch::Refund => REFUND_INDEX | position,
    })
}

fn shared(secret: &Scalar, point: &Point) -> Result<[u8; 32], Error> {
    let product = non_identity(point)? * secret;
    if product == ProjectivePoint::IDENTITY {
        return Err(Error::InvalidPoint);
    }
    Ok(tagged_hash(
        "licash/ecdh",
        &[&Point::from_projective(&product).x_only()],
    ))
}

/// A recipient's public address: spending root, scan key and hold root.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address {
    spend: Point,
    scan: Point,
    hold: Point,
}

impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Address({})", hex::encode(&self.to_bytes()[..8]))
    }
}

impl Address {
    /// Valid, non-identity keys. Nothing here shows that their secrets are
    /// independent or held by one party.
    pub fn new(spend: Point, scan: Point, hold: Point) -> Result<Self, Error> {
        for key in [&spend, &scan, &hold] {
            non_identity(key)?;
        }
        Ok(Self { spend, scan, hold })
    }

    /// The spending root `B_s`.
    pub fn spend(&self) -> Point {
        self.spend
    }
    /// The scan key `B_v`.
    pub fn scan(&self) -> Point {
        self.scan
    }
    /// The hold root `B_h`.
    pub fn hold(&self) -> Point {
        self.hold
    }

    /// `B_s ‖ B_v ‖ B_h`, compressed.
    pub fn to_bytes(&self) -> [u8; ADDRESS_BYTES] {
        let mut bytes = [0; ADDRESS_BYTES];
        bytes[..33].copy_from_slice(&self.spend.0);
        bytes[33..66].copy_from_slice(&self.scan.0);
        bytes[66..].copy_from_slice(&self.hold.0);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != ADDRESS_BYTES {
            return Err(bad("address length"));
        }
        let key = |i: usize| Point(bytes[33 * i..33 * (i + 1)].try_into().expect("33 bytes"));
        Self::new(key(0), key(1), key(2))
    }
}

impl serde::Serialize for Address {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::crypto::serialize_fixed(&self.to_bytes(), s)
    }
}

impl<'de> serde::Deserialize<'de> for Address {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes = crate::crypto::deserialize_fixed::<D, ADDRESS_BYTES>(d, "address")?;
        Self::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

/// A payer's ephemeral secret for one proposal (`a`, with `A = aG` the
/// proposal's ephemeral key). Reusing it across proposals reuses outputs.
#[derive(Clone)]
pub struct Ephemeral(Scalar);

impl std::fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ephemeral(..)")
    }
}

impl Ephemeral {
    pub fn random() -> Self {
        Self(random_scalar())
    }

    pub fn from_scalar(scalar: Scalar) -> Result<Self, Error> {
        if scalar == Scalar::ZERO {
            return Err(Error::InvalidScalar);
        }
        Ok(Self(scalar))
    }

    /// `A`, the ephemeral key the proposal carries.
    pub fn public(&self) -> Point {
        Point::from_projective(&generator_mul(&self.0))
    }

    /// The receipt secret shared with `to`.
    pub fn shared(&self, to: &Address) -> Result<[u8; 32], Error> {
        shared(&self.0, &to.scan)
    }

    /// The output of `denom` to `to` at `position` of `branch`.
    pub fn derive(
        &self,
        to: &Address,
        denom: u64,
        branch: Branch,
        position: usize,
    ) -> Result<Derivation, Error> {
        Derivation::derive(
            denom,
            to.spend,
            to.hold,
            &self.shared(to)?,
            index(branch, position)?,
        )
    }
}

/// An output of a proposal that a scan key recognized as its own.
#[derive(Clone, Debug)]
pub struct Received {
    pub branch: Branch,
    pub position: u32,
    pub derivation: Derivation,
}

/// Watch-only authority: recognizes received outputs and derives their
/// openings, but cannot spend or claim them.
#[derive(Clone)]
pub struct ScanKey {
    scan: Scalar,
    spend: Point,
    hold: Point,
}

impl std::fmt::Debug for ScanKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScanKey(..)")
    }
}

impl ScanKey {
    pub fn new(scan: Scalar, spend: Point, hold: Point) -> Result<Self, Error> {
        if scan == Scalar::ZERO {
            return Err(Error::InvalidScalar);
        }
        non_identity(&spend)?;
        non_identity(&hold)?;
        Ok(Self { scan, spend, hold })
    }

    pub fn address(&self) -> Address {
        Address {
            spend: self.spend,
            scan: Point::from_projective(&generator_mul(&self.scan)),
            hold: self.hold,
        }
    }

    /// The receipt secret of a proposal with ephemeral key `ephemeral`.
    pub fn shared(&self, ephemeral: &Point) -> Result<[u8; 32], Error> {
        shared(&self.scan, ephemeral)
    }

    /// The expected output of `denom` at `position` of `branch` under the
    /// proposal's ephemeral key.
    pub fn derive(
        &self,
        ephemeral: &Point,
        denom: u64,
        branch: Branch,
        position: usize,
    ) -> Result<Derivation, Error> {
        Derivation::derive(
            denom,
            self.spend,
            self.hold,
            &self.shared(ephemeral)?,
            index(branch, position)?,
        )
    }

    /// The outputs of `proposal`, in both branches, derived to this address:
    /// those whose denomination, blinded point and unlink commitment are
    /// exactly what this key derives at their position.
    pub fn receive(&self, proposal: &Proposal) -> Result<Vec<Received>, Error> {
        let shared = self.shared(&proposal.ephemeral())?;
        let refund = proposal.lock().map(|lock| lock.refund()).unwrap_or(&[]);
        let mut received = Vec::new();
        for (branch, outputs) in [(Branch::Pay, proposal.outputs()), (Branch::Refund, refund)] {
            for (position, output) in outputs.iter().enumerate() {
                let derivation = Derivation::derive(
                    output.denomination(),
                    self.spend,
                    self.hold,
                    &shared,
                    index(branch, position)?,
                )?;
                if Output::from_derivation(&derivation)? == *output {
                    received.push(Received {
                        branch,
                        position: position as u32,
                        derivation,
                    });
                }
            }
        }
        Ok(received)
    }
}

/// A user's spend, scan and hold secrets. The spend secret authorizes
/// spending, the hold root claiming on Bitcoin, the scan secret only
/// recognizing receipts.
#[derive(Clone)]
pub struct OwnerKeys {
    spend: Scalar,
    scan: ScanKey,
    hold: HoldRoot,
}

impl std::fmt::Debug for OwnerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnerKeys(..)")
    }
}

impl OwnerKeys {
    /// The three keys of a wallet seed, each under its own domain.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, Error> {
        let spend = scalar_from_hash(&tagged_hash("licash/root/spend/v2", &[seed]));
        let scan = scalar_from_hash(&tagged_hash("licash/root/scan/v2", &[seed]));
        Self::from_parts(spend, scan, HoldRoot::from_seed(seed)?)
    }

    pub fn from_parts(spend: Scalar, scan: Scalar, hold: HoldRoot) -> Result<Self, Error> {
        if spend == Scalar::ZERO {
            return Err(Error::InvalidScalar);
        }
        let spend_root = Point::from_projective(&generator_mul(&spend));
        Ok(Self {
            spend,
            scan: ScanKey::new(scan, spend_root, hold.public())?,
            hold,
        })
    }

    pub fn address(&self) -> Address {
        self.scan.address()
    }

    /// The spending root's secret `b_s`, which signs for every owner key
    /// derived to this address (`Ownership::owner_secret`).
    pub fn spend_secret(&self) -> &Scalar {
        &self.spend
    }

    pub fn scan_key(&self) -> &ScanKey {
        &self.scan
    }

    pub fn hold_root(&self) -> &HoldRoot {
        &self.hold
    }
}
