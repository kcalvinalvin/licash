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
//!
//! Anything else is a [`Problem`], with the HTTP status of its [`Code`].

use licash::{
    address::Address,
    authorization::{Grant, SignedProposal, Unlock},
    note::{Keyset, Note},
    spend::Proposal,
    unlink::{Answer, Request},
};
use serde::{Deserialize, Serialize};

pub const INFO: &str = "/v1/info";
pub const OFFER: &str = "/v1/offer";
pub const RELEASE: &str = "/v1/release";
pub const UNLINK: &str = "/v1/unlink";

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

/// What a hub charges. A spend pays its fee in outputs to [`Info::address`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fees {
    pub spend: Fee,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Info {
    pub keysets: Vec<Hex>,
    pub address: Address,
    pub fees: Fees,
}

impl Info {
    pub fn new(keysets: &[Keyset], address: Address, fees: Fees) -> Self {
        Self {
            keysets: keysets.iter().map(|k| Hex(k.to_bytes())).collect(),
            address,
            fees,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unlink {
    pub request: Hex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unlinked {
    pub answer: Hex,
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
    /// Timed spends are not served.
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
    /// The hub cannot serve now; retry later.
    Unavailable,
}

impl Code {
    /// The HTTP status a hub answers with.
    pub fn status(self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::UnknownKeyset | Self::UnknownOffer | Self::NotIssued => 404,
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
        let keyset = keysets(&self.keyset.array("keyset")?)
            .ok_or_else(|| Problem::new(Code::UnknownKeyset, "unknown keyset"))?;
        let notes = notes(keyset, &self.notes)?;
        let signed = SignedProposal::from_bytes(keyset, &notes, &self.signed.0)?;
        Ok((signed, notes))
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
