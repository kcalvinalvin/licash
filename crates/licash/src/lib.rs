//! licash: owner-bound ecash with local Bitcoin recovery.
//!
//! The protocol is specified in the paper (`paper/licash.pdf`). Modules by
//! section: notes and owner keys (`note`, `crypto`, `ec`); contracts and
//! reading Bitcoin (`base`, `opening`, `request`, `chain`, `payout`);
//! payments and grants (`spend`, `authorization`); recovery (`claim`,
//! `hold`, `recovery`, `statement`, `disclosure`, `wots`, `guest`);
//! interoperability (`source`, `route`).

pub mod address;
pub mod authorization;
pub mod base;
pub mod chain;
pub mod claim;
mod codec;
pub mod crypto;
pub mod disclosure;
pub mod ec;
pub mod guest;
pub mod hold;
pub mod note;
pub mod opening;
pub mod payout;
pub mod recovery;
pub mod request;
pub mod route;
pub mod source;
pub mod spend;
pub mod statement;
pub mod wots;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid point encoding")]
    InvalidPoint,
    #[error("invalid scalar")]
    InvalidScalar,
    #[error("DLEQ proof does not verify")]
    BadDleq,
    #[error("signature does not verify")]
    BadSignature,
    #[error("unknown denomination {0}")]
    UnknownDenomination(u64),
    #[error("{0}")]
    Protocol(String),
}
