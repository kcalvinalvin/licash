//! licash: owner-bound ecash with local Bitcoin recovery.
//!
//! The protocol is specified in the paper (`paper/licash.pdf`). Modules by
//! section: the model's notes, owner keys, unlinking and contracts (`note`,
//! `address`, `crypto`, `ec`, `unlink`, `base`); payments and grants
//! (`spend`, `authorization`); recovery and reading Bitcoin (`request`,
//! `chain`, `claim`, `hold`, `recovery`, `disclosure`); interoperability
//! (`source`, `route`). The Hub's statement over [`recovery::owed`] is
//! `licash-hp`'s.
//! `codec` holds the canonical bytes of what parties exchange.

pub mod address;
pub mod authorization;
pub mod base;
pub mod chain;
pub mod claim;
mod codec;
pub mod crypto;
pub mod disclosure;
pub mod ec;
pub mod hold;
pub mod note;
pub mod recovery;
pub mod request;
pub mod route;
pub mod source;
pub mod spend;
pub mod unlink;

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
