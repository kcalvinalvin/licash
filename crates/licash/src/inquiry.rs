//! A holder's inquiry of her Hub: what it holds against her notes.
//!
//! A note is asked about only by whoever can sign with its hold key, so
//! nobody learns the state of another's notes, and the signature is made
//! under a tag of its own: no [`crate::hold`] authorization, no spend
//! authorization and no inquiry for another Hub, keyset or time is this
//! signature, so asking can never void a note ([`crate::recovery::Evidence`]
//! cites only those).
//!
//! The Hub answers with its evidence ([`crate::recovery::Evidence`]), which
//! the asker verifies herself ([`crate::recovery::voids_note`]): she drops a
//! note only on a proof, never on the Hub's word.

use crate::{
    crypto::{self, ScalarBytes, Signature},
    note::Message,
    Error,
};

const TAG: &str = "licash/inquiry/v1";

/// How long an inquiry is valid, either side of its time, in seconds.
pub const WINDOW: u64 = 600;

/// The most notes one inquiry asks about.
pub const MAX_NOTES: usize = 256;

/// What an inquiry signs for `note`, of the keyset `keyset`, at time `at`
/// (unix seconds).
pub fn digest(keyset: &[u8; 32], at: u64, note: &Message) -> [u8; 32] {
    crypto::tagged_hash(TAG, &[keyset, &at.to_be_bytes(), &note.public_fields()])
}

/// The signature of `note`'s hold key `key` on an inquiry of `keyset` at `at`.
pub fn sign(
    keyset: &[u8; 32],
    at: u64,
    note: &Message,
    key: &ScalarBytes,
) -> Result<Signature, Error> {
    let key = key.to_nonzero()?;
    if &crypto::x_only_pub(key.as_ref()) != note.hold() {
        return Err(Error::Protocol("wrong hold authority".into()));
    }
    crypto::schnorr_sign(key.as_ref(), &digest(keyset, at, note))
}

/// Whether `sig` is `note`'s hold key's signature on an inquiry of `keyset`
/// at `at`, made within [`WINDOW`] seconds of `now`.
pub fn verify(
    keyset: &[u8; 32],
    at: u64,
    note: &Message,
    sig: &Signature,
    now: u64,
) -> Result<(), Error> {
    if at.abs_diff(now) > WINDOW {
        return Err(Error::Protocol("an inquiry out of its time".into()));
    }
    crypto::schnorr_verify(note.hold(), &digest(keyset, at, note), sig)
}
