//! An owner's offer of her notes in a spend ([`crate::spend::Proposal`]).
//!
//! Whoever knows a note may offer it in a spend: its payer, who made it,
//! does. A hub grants a note in a bounded number of spends, so a payer
//! could have her payee's notes refused. An offer signed by each note's
//! hold key, which only its owner has, is the owner's: a hub counts those
//! apart. The signature is made under a tag of its own: no
//! [`crate::hold`] authorization, inquiry ([`crate::inquiry`]) or spend
//! authorization is this signature, so offering can never void a note.

use crate::{
    crypto::{self, ScalarBytes, Signature},
    note::Message,
    spend::ProposalId,
    Error,
};

const TAG: &str = "licash/offer/v1";

/// What an offer of `note` in the spend `proposal` signs.
pub fn digest(proposal: &ProposalId, note: &Message) -> [u8; 32] {
    crypto::tagged_hash(TAG, &[proposal.as_bytes(), &note.public_fields()])
}

/// The signature of `note`'s hold key `key` on an offer of it in the spend
/// `proposal`.
pub fn sign(proposal: &ProposalId, note: &Message, key: &ScalarBytes) -> Result<Signature, Error> {
    let key = key.to_nonzero()?;
    if &crypto::x_only_pub(key.as_ref()) != note.hold() {
        return Err(Error::Protocol("wrong hold authority".into()));
    }
    crypto::schnorr_sign(key.as_ref(), &digest(proposal, note))
}

/// Whether `sig` is `note`'s hold key's signature on an offer of it in the
/// spend `proposal`.
pub fn verify(proposal: &ProposalId, note: &Message, sig: &Signature) -> Result<(), Error> {
    crypto::schnorr_verify(note.hold(), &digest(proposal, note), sig)
}
