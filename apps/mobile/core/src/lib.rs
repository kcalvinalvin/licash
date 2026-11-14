//! The Licash wallet of the mobile apps: [`Wallet`], over the SDK's wallet,
//! with blocking methods for the apps to call off their main thread, and
//! Bitcoin read through the app's light client ([`ChainSource`]).
//!
//! What a user sees is notes at hubs, and coins paid on Bitcoin to the
//! wallet's silent payment address, which [`Wallet::move_to_licash`] turns
//! into notes. One payment code ([`Wallet::payment_code`]) takes both.

mod chain;
mod log;
mod wallet;

pub use chain::{ChainError, ChainSource};
pub use log::{set_logger, Logger};
pub use wallet::{wallet_exists, Wallet};

uniffi::setup_scaffolding!();

/// The networks a wallet can be of. Every one but Bitcoin is for testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Network {
    Bitcoin,
    Signet,
    Mutinynet,
    Regtest,
}

impl Network {
    fn profile(self) -> licash::spend::ChainProfile {
        use licash::spend::ChainProfile::*;
        match self {
            Self::Bitcoin => Core30Mainnet,
            Self::Signet => Core30Signet,
            Self::Mutinynet => Mutinynet,
            Self::Regtest => Core30DefaultRegtest,
        }
    }

    fn of(profile: licash::spend::ChainProfile) -> Self {
        use licash::spend::ChainProfile::*;
        match profile {
            Core30Mainnet => Self::Bitcoin,
            Core30Signet => Self::Signet,
            Mutinynet => Self::Mutinynet,
            Core30DefaultRegtest => Self::Regtest,
        }
    }
}

/// The name the light client knows `network` by.
#[uniffi::export]
pub fn chain_name(network: Network) -> String {
    match network {
        Network::Bitcoin => "bitcoin",
        Network::Signet => "signet",
        Network::Mutinynet => "mutinynet",
        Network::Regtest => "regtest",
    }
    .into()
}

/// What went wrong, as the app tells its user.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum LicashError {
    /// Not something the wallet can do: as the user asked it, never.
    #[error("{reason}")]
    Invalid { reason: String },
    /// A hub, relay or the chain could not be reached: later, perhaps.
    #[error("{reason}")]
    Unavailable { reason: String },
    /// Under way, and finished by [`Wallet::resume`].
    #[error("{reason}")]
    Pending { reason: String },
    #[error("{needed} sat needed, {available} sat available")]
    Insufficient { needed: u64, available: u64 },
    /// A hub at an onion address, and the wallet opened without Tor.
    #[error("an onion hub is reached through Tor: turn it on")]
    NeedsTor,
    /// A payment of notes at the hub at `url`, which the wallet does not
    /// trust: [`Wallet::receive`] it trusting the hub to take it.
    #[error("a payment at hub {url}, not trusted yet")]
    UnknownHub { url: String },
    /// The wallet's own storage failed.
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<licash_wallet::Error> for LicashError {
    fn from(e: licash_wallet::Error) -> Self {
        use licash_wallet::Error as E;
        let reason = e.to_string();
        match e {
            E::Unavailable { .. } => Self::Unavailable { reason },
            E::Pending { .. } | E::NoteTaken => Self::Pending { reason },
            E::Insufficient { needed, available } => Self::Insufficient { needed, available },
            E::UnknownHub(url) => Self::UnknownHub { url },
            E::Setup(_) | E::Storage(_) => Self::Failed { reason },
            _ => Self::Invalid { reason },
        }
    }
}

impl LicashError {
    /// `e`, a wallet's error if it is one, else as `other`.
    fn from_any(e: anyhow::Error, other: fn(String) -> Self) -> Self {
        match e.downcast::<licash_wallet::Error>() {
            Ok(e) => e.into(),
            Err(e) => other(format!("{e:#}")),
        }
    }

    fn invalid(reason: impl ToString) -> Self {
        Self::Invalid {
            reason: reason.to_string(),
        }
    }

    fn failed(reason: impl ToString) -> Self {
        Self::Failed {
            reason: reason.to_string(),
        }
    }
}

/// A hub the wallet trusts.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Hub {
    pub url: String,
    /// Its fee on a payment: `base` plus `ppm` millionths of it.
    pub fee_base: u64,
    pub fee_ppm: u64,
    /// The least and most it turns into notes from Bitcoin, if it does.
    pub deposits: Option<Range>,
    /// Whether it serves what finds silent payments.
    pub silent: bool,
    /// Whether it names relays for mailboxes.
    pub mailbox: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Range {
    pub min: u64,
    pub max: u64,
}

/// What the wallet holds, in sat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct Balance {
    /// In notes, to pay with.
    pub notes: u64,
    /// In notes under way: change of a payment not finished.
    pub notes_pending: u64,
    /// Paid on Bitcoin, confirmed, to move to notes.
    pub onchain: u64,
    /// Paid on Bitcoin and being moved to notes.
    pub moving: u64,
}

/// A payment code to show: `text` to share, `qr` to draw.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Code {
    pub text: String,
    pub qr: String,
}

/// What a text scanned or pasted is.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Scanned {
    /// A payment code: to pay, `amount` if it says.
    Code {
        amount: Option<u64>,
        /// Whether it names a hub the wallet trusts, to pay in notes.
        notes: bool,
    },
    /// A payment handed over, to [`Wallet::receive`].
    Payment,
}

/// A payment made.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Paid {
    pub id: String,
    pub amount: u64,
    pub hub: String,
    /// Sent to the payee's mailbox.
    pub delivered: bool,
    /// Else the payment, for the payee to receive by hand.
    pub handover: Option<String>,
    /// Why it was not delivered.
    pub undelivered: Option<String>,
}

/// One line of the wallet's history.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Activity {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    /// In sat, fees aside.
    pub amount: u64,
    pub fee: u64,
    /// The hub, or why it failed.
    pub detail: String,
    /// When, in seconds since 1970, if known.
    pub at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Kind {
    Sent,
    Received,
    /// Bitcoin moved to notes.
    Moved,
    /// A coin paid on Bitcoin, not moved yet.
    Onchain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum State {
    Done,
    Pending,
    Failed,
}

/// How far looking for payments on Bitcoin got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Scan {
    /// The next block to look in.
    pub next: u32,
    /// The chain's height, as far as it could look.
    pub height: u32,
    /// The coins found, and their worth.
    pub found: u32,
    pub found_value: u64,
}
