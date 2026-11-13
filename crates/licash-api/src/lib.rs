//! The Licash hub API, version 1: JSON bodies that carry the protocol's
//! canonical bytes as hex, the paths they go to and the problems a hub
//! answers with. The transport is the application's: HTTP over TCP or Tor.
//!
//! | Path        | Body       | Answer      |
//! |-------------|------------|-------------|
//! | `GET /v1/info`     |     | [`Info`]     |
//! | `POST /v1/offer`   | [`Offer`]   | [`Offered`]  |
//! | `POST /v1/release` | [`Release`] | [`Released`] |
//! | `POST /v1/unlink`  | [`Unlink`]  | [`Unlinked`] |
//! | `POST /v1/lock`    | [`Lock`]    | [`Locked`]   |
//! | `GET /v1/lock/{id}` |            | [`LockState`] |
//! | `POST /v1/settle`  | [`Settle`]  | [`Issued`]   |
//! | `POST /v1/refund`  | [`Refund`]  | [`Issued`]   |
//!
//! Anything else is a [`Problem`], with the HTTP status of its [`Code`].
//!
//! A payer hands its payee a [`Payment`], by whatever channel they share.

use licash::{
    address::Address,
    authorization::{Authorization, Grant, SignedProposal, Unlock},
    chain::Checkpoint,
    note::{Keyset, MintResponse, Note},
    source::Certificate,
    spend::{lock_hash, Proposal, ProposalId},
    unlink::{Answer, Request},
};
use serde::{Deserialize, Serialize};

pub const INFO: &str = "/v1/info";
pub const OFFER: &str = "/v1/offer";
pub const RELEASE: &str = "/v1/release";
pub const UNLINK: &str = "/v1/unlink";
pub const LOCK: &str = "/v1/lock";
/// A lock's state, at [`lock_state`] for a lock's identifier.
pub const LOCK_STATE: &str = "/v1/lock/{id}";
pub const SETTLE: &str = "/v1/settle";
pub const REFUND: &str = "/v1/refund";

/// The path of lock `id`'s state.
pub fn lock_state(id: &ProposalId) -> String {
    format!("{LOCK}/{}", hex::encode(id.as_bytes()))
}

/// The identifier of a lock as [`lock_state`] writes it.
pub fn lock_id(text: &str) -> Result<ProposalId, Problem> {
    let bytes = hex::decode(text).map_err(|e| Problem::invalid(format!("lock: {e}")))?;
    Ok(ProposalId::from_bytes(Hex(bytes).array("lock")?))
}

/// Bytes, as hex in JSON.
#[derive(Clone, PartialEq, Eq)]
pub struct Hex(pub Vec<u8>);

impl std::fmt::Debug for Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hex({} bytes)", self.0.len())
    }
}

impl Serialize for Hex {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        hex::decode(text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl Hex {
    fn array<const N: usize>(&self, what: &str) -> Result<[u8; N], Problem> {
        self.0
            .as_slice()
            .try_into()
            .map_err(|_| Problem::invalid(format!("{what} must be {N} bytes")))
    }
}

/// A fee: `base` plus `ppm` parts per million of the amount moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fee {
    pub base: u64,
    pub ppm: u64,
}

impl Fee {
    /// The fee on moving `amount`, rounded up, saturating.
    pub fn due(&self, amount: u64) -> u64 {
        let rate = (u128::from(amount) * u128::from(self.ppm)).div_ceil(1_000_000);
        u64::try_from(rate).map_or(u64::MAX, |rate| rate.saturating_add(self.base))
    }
}

/// What a hub charges, in outputs to [`Info::address`]: a spend's fee in
/// its outputs, a lock's in each branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fees {
    pub spend: Fee,
    /// In a lock's pay branch.
    pub lock: Fee,
    /// In a lock's refund branch.
    pub refund: Fee,
}

/// The locks a hub admits, as of its scan of Bitcoin, in blocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locks {
    /// The checkpoint new locks name.
    pub checkpoint: Hex,
    /// The height the hub scanned to.
    pub height: u32,
    /// A lock's expiry, depth and `reaction` fit within `window` of the
    /// height the hub admits it at.
    pub window: u32,
    pub reaction: u32,
    /// Least activation depth.
    pub min_depth: u32,
    /// A lock is decided within this many blocks of its checkpoint.
    pub max_headers: u32,
}

impl Locks {
    pub fn checkpoint(&self) -> Result<Checkpoint, Problem> {
        Ok(Checkpoint::from_bytes(&self.checkpoint.0)?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Info {
    pub keysets: Vec<Hex>,
    pub address: Address,
    pub fees: Fees,
    /// None while the hub follows no chain.
    #[serde(default)]
    pub locks: Option<Locks>,
}

impl Info {
    pub fn new(keysets: &[Keyset], address: Address, fees: Fees, locks: Option<Locks>) -> Self {
        Self {
            keysets: keysets.iter().map(|k| Hex(k.to_bytes())).collect(),
            address,
            fees,
            locks,
        }
    }

    pub fn keysets(&self) -> Result<Vec<Keyset>, Problem> {
        self.keysets
            .iter()
            .map(|k| Keyset::from_bytes(&k.0).map_err(Problem::from))
            .collect()
    }
}

/// Ask for a grant on `proposal`, spending `notes`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub proposal: Hex,
    pub notes: Vec<Hex>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Offered {
    pub grant: Hex,
}

/// Ask for the unlock of the grant that `signed` authorizes. `keyset` is
/// the one its proposal names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub keyset: Hex,
    pub signed: Hex,
    pub notes: Vec<Hex>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Released {
    pub unlock: Hex,
}

/// A released spend and its unlock, as its payer hands it to a payee at
/// the hub `hub`: the payee opens the outputs to its address. Only an
/// output's owner can open it, so a payment is safe to send in the clear.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payment {
    pub hub: String,
    #[serde(flatten)]
    pub release: Release,
    pub unlock: Hex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unlink {
    pub request: Hex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unlinked {
    pub answer: Hex,
}

/// Lock `notes` as `signed`, a plainly authorized timed spend, makes them.
/// `keyset` is the one its proposal names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lock {
    pub keyset: Hex,
    pub signed: Hex,
    pub notes: Vec<Hex>,
}

/// The hub's certificate of a lock: its acknowledgment of the debt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Locked {
    pub certificate: Hex,
}

/// A lock as its hub holds it. The preimage is known once the lock was
/// paid or activated on Bitcoin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockState {
    pub proposal: Hex,
    pub certificate: Hex,
    pub settled: Option<Settled>,
    pub preimage: Option<Hex>,
}

/// The branch of a lock its hub issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settled {
    Pay,
    Refund,
}

/// A [`LockState`], decoded and checked.
#[derive(Clone, Debug)]
pub struct LockView {
    pub proposal: Proposal,
    pub certificate: Certificate,
    pub settled: Option<Settled>,
    pub preimage: Option<[u8; 32]>,
}

/// Issue the pay branch of `lock` for `preimage`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settle {
    pub lock: Hex,
    pub preimage: Hex,
}

/// Issue the refund branch of `lock`, once it expired unactivated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Refund {
    pub lock: Hex,
}

/// The issuances of a branch, in output order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issued {
    pub issuances: Vec<Hex>,
}

/// Why a hub did not serve a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The request does not decode or verify.
    Invalid,
    UnknownKeyset,
    UnknownOffer,
    NotIssued,
    UnknownLock,
    /// A timed spend, served at [`LOCK`] only.
    Timed,
    /// An input is already spent.
    Spent,
    /// An output is already issued.
    Reissued,
    /// The fee paid to the hub is below its fees ([`Info::fees`]).
    Fee,
    /// Already unlinked under another request.
    Unlinked,
    /// Unlinking closed by a hub restart: swap the note instead.
    Expired,
    /// Not a plainly authorized timed spend.
    NotALock,
    /// The lock names a checkpoint the hub does not follow.
    Checkpoint,
    /// The lock's expiry or depth is outside [`Info::locks`].
    Timing,
    /// Not the preimage of the lock's hash.
    Preimage,
    /// The lock was settled the other way.
    Settled,
    /// The lock expired before it was paid: it is refunded.
    LockExpired,
    /// The lock has not expired at depth: retry later.
    Unexpired,
    /// The lock was activated on Bitcoin in time: it pays.
    Activated,
    /// The hub cannot serve now, or is behind the chain; retry later.
    Unavailable,
}

impl Code {
    /// The HTTP status a hub answers with.
    pub fn status(self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::UnknownKeyset | Self::UnknownOffer | Self::NotIssued | Self::UnknownLock => 404,
            Self::Unavailable => 503,
            _ => 409,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Problem {
    pub code: Code,
    pub message: String,
}

impl Problem {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Code::Invalid, message)
    }
}

impl From<licash::Error> for Problem {
    fn from(e: licash::Error) -> Self {
        Self::invalid(e.to_string())
    }
}

fn notes(keyset: &Keyset, notes: &[Hex]) -> Result<Vec<Note>, Problem> {
    notes
        .iter()
        .map(|n| Note::from_bytes(keyset, &n.0).map_err(Problem::from))
        .collect()
}

fn hexes(notes: &[Note]) -> Vec<Hex> {
    notes.iter().map(|n| Hex(n.to_bytes())).collect()
}

impl Offer {
    pub fn new(proposal: &Proposal, notes: &[Note]) -> Self {
        Self {
            proposal: Hex(proposal.commitment_bytes()),
            notes: hexes(notes),
        }
    }

    /// The proposal and its notes, under the keyset the proposal names
    /// among `keysets`.
    pub fn decode<'k>(
        &self,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<(Proposal, Vec<Note>), Problem> {
        let proposal = Proposal::from_commitment_bytes(&self.proposal.0)?;
        let keyset = keysets(proposal.keyset_id())
            .ok_or_else(|| Problem::new(Code::UnknownKeyset, "unknown keyset"))?;
        let notes = notes(keyset, &self.notes)?;
        Ok((proposal, notes))
    }
}

impl Offered {
    pub fn new(grant: &Grant) -> Result<Self, Problem> {
        Ok(Self {
            grant: Hex(grant.to_bytes()?),
        })
    }

    pub fn decode(&self) -> Result<Grant, Problem> {
        Ok(Grant::from_bytes(&self.grant.0)?)
    }
}

impl Release {
    pub fn new(signed: &SignedProposal, notes: &[Note]) -> Self {
        Self {
            keyset: Hex(signed.proposal().keyset_id().to_vec()),
            signed: Hex(signed.to_bytes()),
            notes: hexes(notes),
        }
    }

    /// The signed proposal and its notes, under `keyset` among `keysets`.
    pub fn decode<'k>(
        &self,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<(SignedProposal, Vec<Note>), Problem> {
        signed(&self.keyset, &self.signed, &self.notes, keysets)
    }
}

fn signed<'k>(
    keyset: &Hex,
    signed: &Hex,
    notes: &[Hex],
    keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
) -> Result<(SignedProposal, Vec<Note>), Problem> {
    let keyset = keysets(&keyset.array("keyset")?)
        .ok_or_else(|| Problem::new(Code::UnknownKeyset, "unknown keyset"))?;
    let notes = self::notes(keyset, notes)?;
    let signed = SignedProposal::from_bytes(keyset, &notes, &signed.0)?;
    Ok((signed, notes))
}

impl Payment {
    pub fn new(hub: &str, signed: &SignedProposal, notes: &[Note], unlock: &Unlock) -> Self {
        Self {
            hub: hub.to_owned(),
            release: Release::new(signed, notes),
            unlock: Hex(unlock.to_vec()),
        }
    }

    /// The granted spend, its notes and its unlock, under `keyset` among
    /// `keysets`: the unlock checked to open every output of the grant.
    pub fn decode<'k>(
        &self,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<(SignedProposal, Vec<Note>, Unlock), Problem> {
        let id = self.release.keyset.array("keyset")?;
        let keyset =
            keysets(&id).ok_or_else(|| Problem::new(Code::UnknownKeyset, "unknown keyset"))?;
        let (signed, notes) = self.release.decode(|_| Some(keyset))?;
        let Authorization::Granted(grant) = signed.authorization() else {
            return Err(Problem::invalid("not a granted spend"));
        };
        let unlock = self.unlock.array("unlock")?;
        grant.verify_unlock(keyset, &unlock)?;
        Ok((signed, notes, unlock))
    }
}

impl Released {
    pub fn new(unlock: &Unlock) -> Self {
        Self {
            unlock: Hex(unlock.to_vec()),
        }
    }

    pub fn decode(&self) -> Result<Unlock, Problem> {
        self.unlock.array("unlock")
    }
}

impl Unlink {
    pub fn new(request: &Request) -> Self {
        Self {
            request: Hex(request.to_bytes().to_vec()),
        }
    }

    pub fn decode(&self) -> Result<Request, Problem> {
        Ok(Request::from_bytes(&self.request.0)?)
    }
}

impl Unlinked {
    pub fn new(answer: &Answer) -> Self {
        Self {
            answer: Hex(answer.to_bytes().to_vec()),
        }
    }

    pub fn decode(&self) -> Result<Answer, Problem> {
        Ok(Answer::from_bytes(self.answer.array("answer")?)?)
    }
}

impl Lock {
    pub fn new(signed: &SignedProposal, notes: &[Note]) -> Self {
        Self {
            keyset: Hex(signed.proposal().keyset_id().to_vec()),
            signed: Hex(signed.to_bytes()),
            notes: hexes(notes),
        }
    }

    /// The signed lock and its notes, under `keyset` among `keysets`.
    pub fn decode<'k>(
        &self,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<(SignedProposal, Vec<Note>), Problem> {
        signed(&self.keyset, &self.signed, &self.notes, keysets)
    }
}

impl Locked {
    pub fn new(certificate: &Certificate) -> Self {
        Self {
            certificate: Hex(certificate.to_bytes()),
        }
    }

    /// The certificate, checked to be `keyset`'s of the lock `signed`
    /// makes of `notes`.
    pub fn decode(
        &self,
        keyset: &Keyset,
        signed: &SignedProposal,
        notes: &[Note],
    ) -> Result<Certificate, Problem> {
        let certificate = Certificate::from_bytes(keyset, &self.certificate.0)?;
        certificate.verify_source(keyset, signed, notes)?;
        Ok(certificate)
    }
}

impl LockState {
    pub fn new(
        proposal: &Proposal,
        certificate: &Certificate,
        settled: Option<Settled>,
        preimage: Option<[u8; 32]>,
    ) -> Self {
        Self {
            proposal: Hex(proposal.commitment_bytes()),
            certificate: Hex(certificate.to_bytes()),
            settled,
            preimage: preimage.map(|p| Hex(p.to_vec())),
        }
    }

    /// The lock under the keyset its proposal names among `keysets`, its
    /// certificate checked to be that keyset's of it and its preimage to
    /// be of its hash.
    pub fn decode<'k>(
        &self,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<LockView, Problem> {
        let proposal = Proposal::from_commitment_bytes(&self.proposal.0)?;
        let hash = *proposal
            .lock()
            .ok_or_else(|| Problem::invalid("not a lock"))?
            .terms()
            .hash();
        let keyset = keysets(proposal.keyset_id())
            .ok_or_else(|| Problem::new(Code::UnknownKeyset, "unknown keyset"))?;
        let certificate = Certificate::from_bytes(keyset, &self.certificate.0)?;
        certificate.verify(keyset)?;
        if certificate.metadata().source_id() != proposal.id() {
            return Err(Problem::invalid("the certificate is of another proposal"));
        }
        let preimage = match &self.preimage {
            Some(p) => Some(p.array("preimage")?),
            None => None,
        };
        if preimage.is_some_and(|p| lock_hash(&p) != hash) {
            return Err(Problem::invalid("not the preimage of the lock's hash"));
        }
        Ok(LockView {
            proposal,
            certificate,
            settled: self.settled,
            preimage,
        })
    }
}

impl Settle {
    pub fn new(lock: &ProposalId, preimage: &[u8; 32]) -> Self {
        Self {
            lock: Hex(lock.as_bytes().to_vec()),
            preimage: Hex(preimage.to_vec()),
        }
    }

    pub fn decode(&self) -> Result<(ProposalId, [u8; 32]), Problem> {
        let lock = ProposalId::from_bytes(self.lock.array("lock")?);
        Ok((lock, self.preimage.array("preimage")?))
    }
}

impl Refund {
    pub fn new(lock: &ProposalId) -> Self {
        Self {
            lock: Hex(lock.as_bytes().to_vec()),
        }
    }

    pub fn decode(&self) -> Result<ProposalId, Problem> {
        Ok(ProposalId::from_bytes(self.lock.array("lock")?))
    }
}

impl Issued {
    pub fn new(issued: &[MintResponse]) -> Self {
        Self {
            issuances: issued.iter().map(|i| Hex(i.to_bytes())).collect(),
        }
    }

    /// The issuances under `keyset`, the lock's.
    pub fn decode(&self, keyset: &Keyset) -> Result<Vec<MintResponse>, Problem> {
        self.issuances
            .iter()
            .map(|i| MintResponse::from_bytes(keyset, &i.0).map_err(Problem::from))
            .collect()
    }
}
