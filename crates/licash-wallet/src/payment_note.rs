//! Notes with payments: a few words for the payee and, if the payer
//! chooses, the payer's own code, so that the payee knows who paid and may
//! pay back. Sealed under the payment's receipt secret, which only payer
//! and payee know, the note travels beside the payment
//! ([`licash_api::inbox::Keep::note`], [`licash_api::Payment::note`]): the
//! hub keeping it and a relay mailing it read nothing of it, nor whether it
//! says anything, as every note of a payment to a code is sealed to the
//! same size, an empty one too.
//!
//! A code in a note is proved its payer's by a tag only the wallets of the
//! code and of the payee can make (of the Diffie-Hellman secret of their
//! scan keys): the payee knows who paid, and can show no one else, as it
//! could have made the tag itself.
//!
//! A note opened is the payee's to keep; one that does not open, or says
//! what no note says, is no note: the payment stands without it. Notes are
//! kept in their own tables of the wallet's file (`notes`, version
//! [`VERSION`]), made as the first is written: a file of another version of
//! them sends and takes payments without notes.

use std::collections::BTreeMap;

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use licash::{
    address::{Address, ScanKey},
    crypto::tagged_hash,
    spend::{Proposal, ProposalId},
};
use licash_api::{inbox::NOTE_SEALED, Hex};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::{Error, LicashCode, Transport, Wallet};

/// The most bytes of a note's text, in UTF-8.
pub const MAX_NOTE: usize = 200;

const COMPONENT: &str = "payment_notes";
const VERSION: i64 = 1;

/// No foreign keys, indexes or triggers: nothing an earlier build writing
/// the wallet's tables could ever trip over.
const SCHEMA: &str = "
-- The note sealed for a payment made, as sent with it on every try, and
-- what it says: its text, and the code shared, as text.
-- `keep` is whether it goes to the hub with the payment kept there too.
CREATE TABLE sent_notes (
    send BLOB PRIMARY KEY,
    sealed BLOB NOT NULL,
    keep INTEGER NOT NULL,
    text TEXT,
    code TEXT
) STRICT;
-- What the note of a payment taken says: its text, and its payer's code,
-- as text, if proved its payer's.
CREATE TABLE received_notes (
    receipt BLOB PRIMARY KEY,
    text TEXT,
    code TEXT
) STRICT;
";

/// The layout of a note before sealing: its version, its text's length and
/// text, its code's length (2 bytes, big-endian) and code
/// ([`LicashCode::to_bytes`]), the tag proving the code, and zeros.
const PLAIN: usize = NOTE_SEALED - 16;
const FORMAT: u8 = 1;
const TEXT_AT: usize = 2;
const CODE_LEN_AT: usize = TEXT_AT + MAX_NOTE;
const CODE_AT: usize = CODE_LEN_AT + 2;
const MAX_CODE: usize = 276;
const TAG_AT: usize = CODE_AT + MAX_CODE;
const _: () = assert!(TAG_AT + 32 == PLAIN);

/// What a note says: either may be absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaymentNote {
    /// A few words, as [`note_text`] makes them.
    pub text: Option<String>,
    /// The payer's code: one of its own to send, proved its payer's once
    /// taken.
    pub from: Option<LicashCode>,
}

impl PaymentNote {
    /// Whether it says nothing.
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.from.is_none()
    }
}

/// Characters that reorder the text around them, and so could show a note
/// as saying what it does not.
fn reorders(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `text` as a note says it: its words, one space apart, of 1 to
/// [`MAX_NOTE`] bytes, without control characters or those reordering
/// text. Refused, never cut.
pub fn note_text(text: &str) -> Result<String, Error> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.len() > MAX_NOTE {
        return Err(Error::Invalid(format!("a note of 1 to {MAX_NOTE} bytes")));
    }
    if text.chars().any(|c| c.is_control() || reorders(c)) {
        return Err(Error::Invalid(
            "a note of no control characters, nor any reordering text".into(),
        ));
    }
    Ok(text)
}

/// The key a note of payment `id` is sealed with, of its receipt secret
/// `shared`: one note only is ever sealed with it.
fn key(shared: &[u8; 32], id: &ProposalId) -> [u8; 32] {
    tagged_hash("licash/note/key", &[shared, id.as_bytes()])
}

/// The tag proving the code in `plain`, a note of payment `id` from the
/// wallet of `from` to that of `to`, of the secret `dh` of their scan
/// keys.
fn proof(dh: &[u8; 32], from: &Address, to: &Address, id: &ProposalId, plain: &[u8]) -> [u8; 32] {
    let (from, to) = (from.to_bytes(), to.to_bytes());
    let secret = tagged_hash("licash/note/dh", &[dh, &from, &to]);
    tagged_hash(
        "licash/note/from",
        &[&secret, id.as_bytes(), &plain[..TAG_AT]],
    )
}

/// `note`, of payment `id` to `to` with receipt secret `shared`, sealed:
/// its code proved with `scan`, the scan key of its code's wallet.
pub(crate) fn seal(
    shared: &[u8; 32],
    id: &ProposalId,
    to: &Address,
    note: &PaymentNote,
    scan: &ScanKey,
) -> Result<Vec<u8>, Error> {
    let mut plain = vec![0u8; PLAIN];
    plain[0] = FORMAT;
    if let Some(text) = &note.text {
        let text = note_text(text)?;
        plain[1] = text.len() as u8;
        plain[TEXT_AT..TEXT_AT + text.len()].copy_from_slice(text.as_bytes());
    }
    if let Some(from) = &note.from {
        let code = from.to_bytes();
        if code.len() > MAX_CODE {
            return Err(Error::Invalid("a code too long for a note".into()));
        }
        plain[CODE_LEN_AT..CODE_AT].copy_from_slice(&(code.len() as u16).to_be_bytes());
        plain[CODE_AT..CODE_AT + code.len()].copy_from_slice(&code);
        let dh = scan.shared(&to.scan())?;
        let tag = proof(&dh, &from.address, to, id, &plain);
        plain[TAG_AT..].copy_from_slice(&tag);
    }
    let cipher = ChaCha20Poly1305::new(&Key::from(key(shared, id)));
    let payload = Payload {
        msg: &plain,
        aad: id.as_bytes(),
    };
    let sealed = (cipher.encrypt(&Nonce::default(), payload))
        .map_err(|_| Error::Invalid("a note not sealed".into()))?;
    debug_assert_eq!(sealed.len(), NOTE_SEALED);
    Ok(sealed)
}

/// What `sealed`, the note of payment `id` to `to` with receipt secret
/// `shared`, says: its text, and its code if `scan`, the scan key of `to`,
/// proves it its payer's and it is of `network`. None if it does not open,
/// or says nothing.
pub(crate) fn open(
    sealed: &[u8],
    shared: &[u8; 32],
    id: &ProposalId,
    to: &Address,
    scan: &ScanKey,
    network: licash::spend::ChainProfile,
) -> Option<PaymentNote> {
    if sealed.len() != NOTE_SEALED {
        return None;
    }
    let cipher = ChaCha20Poly1305::new(&Key::from(key(shared, id)));
    let payload = Payload {
        msg: sealed,
        aad: id.as_bytes(),
    };
    let plain = cipher.decrypt(&Nonce::default(), payload).ok()?;
    if plain.len() != PLAIN || plain[0] != FORMAT {
        return None;
    }
    let text_len = usize::from(plain[1]);
    let text = (text_len <= MAX_NOTE)
        .then(|| std::str::from_utf8(&plain[TEXT_AT..TEXT_AT + text_len]).ok())
        .flatten()
        .filter(|t| !t.is_empty())
        // Said as a note says it, whatever its payer's wallet let through.
        .and_then(|t| note_text(t).ok());
    let code_len = usize::from(u16::from_be_bytes([
        plain[CODE_LEN_AT],
        plain[CODE_LEN_AT + 1],
    ]));
    let from = (code_len > 0 && code_len <= MAX_CODE)
        .then(|| LicashCode::from_bytes(&plain[CODE_AT..CODE_AT + code_len]).ok())
        .flatten()
        .filter(|code| code.network == network)
        .filter(|code| {
            let Ok(dh) = scan.shared(&code.address.scan()) else {
                return false;
            };
            let tag = proof(&dh, &code.address, to, id, &plain);
            tag.iter()
                .zip(&plain[TAG_AT..])
                .fold(0, |d, (a, b)| d | (a ^ b))
                == 0
        });
    let note = PaymentNote { text, from };
    (!note.is_empty()).then_some(note)
}

/// Whether this build reads the notes of the file `c`: none written yet,
/// or of [`VERSION`].
fn readable(c: &Connection) -> Result<bool, Error> {
    Ok(matches!(component_version(c)?, None | Some(VERSION)))
}

fn component_version(c: &Connection) -> Result<Option<i64>, Error> {
    licash_store::component_version(c, COMPONENT).map_err(stored)
}

fn stored(e: licash_store::Error) -> Error {
    match e {
        licash_store::Error::Sqlite(e) => Error::Storage(e),
        e => Error::Setup(e.to_string()),
    }
}

/// Make the notes' tables in `tx`, if none: whether this build writes them.
fn writable(tx: &Transaction) -> Result<bool, Error> {
    match component_version(tx)? {
        Some(VERSION) => Ok(true),
        Some(_) => Ok(false),
        None => {
            licash_store::component_in(tx, COMPONENT, VERSION, SCHEMA).map_err(stored)?;
            Ok(true)
        }
    }
}

/// Record the note `sealed` of send `id`, saying `note`, before its hub
/// hears of it, to go with it on every try, to its hub too if `keep`.
pub(crate) fn record_sent(
    tx: &Transaction,
    id: &ProposalId,
    sealed: &[u8],
    keep: bool,
    note: &PaymentNote,
) -> Result<(), Error> {
    if !writable(tx)? {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO sent_notes (send, sealed, keep, text, code) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id.as_bytes(),
            sealed,
            keep,
            note.text,
            note.from.as_ref().map(LicashCode::to_string)
        ],
    )?;
    Ok(())
}

/// The note sealed for send `id`, if one was, and whether it goes to its
/// hub too.
pub(crate) fn sealed(c: &Connection, id: &ProposalId) -> Result<Option<(Vec<u8>, bool)>, Error> {
    if component_version(c)? != Some(VERSION) {
        return Ok(None);
    }
    Ok(c.query_row(
        "SELECT sealed, keep FROM sent_notes WHERE send = ?1",
        [id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?)
}

/// Whether the hub of `info` keeps a note with a payment it keeps.
pub(crate) fn kept_by(info: &licash_api::Info) -> bool {
    let named = |f: &String| f == licash_api::inbox::FEATURE_NOTE;
    info.features.iter().any(named)
}

/// Notes with payments.
impl<T: Transport> Wallet<T> {
    /// What the notes of the payments made and taken say, by payment id in
    /// hex: those saying something only.
    pub fn payment_notes(&self) -> Result<BTreeMap<String, PaymentNote>, Error> {
        self.store.read(|c| {
            if component_version(c)? != Some(VERSION) {
                return Ok(BTreeMap::new());
            }
            let mut notes = BTreeMap::new();
            for table in [
                "SELECT send, text, code FROM sent_notes",
                "SELECT receipt, text, code FROM received_notes",
            ] {
                let mut stmt = c.prepare(table)?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, text, code) = row?;
                    let from = code.and_then(|c| c.parse().ok());
                    let note = PaymentNote { text, from };
                    if !note.is_empty() {
                        notes.insert(hex::encode(id), note);
                    }
                }
            }
            Ok(notes)
        })
    }

    /// `note`, of payment `id` to `to` with receipt secret `shared`,
    /// sealed: its code one of this wallet's.
    pub(crate) fn seal_note(
        &self,
        shared: &[u8; 32],
        id: &ProposalId,
        to: &Address,
        note: &PaymentNote,
    ) -> Result<Vec<u8>, Error> {
        if let Some(from) = &note.from {
            if from.address != self.address() || from.network != self.network {
                return Err(Error::Invalid(
                    "a note sharing another wallet's code".into(),
                ));
            }
        }
        seal(shared, id, to, note, self.owner.scan_key())
    }

    /// Keep what the note `sealed` of the payment of `proposal` to this
    /// wallet says, if it says anything: never an error, a note being no
    /// reason not to take a payment.
    pub(crate) fn keep_note(&self, proposal: &Proposal, sealed: Option<&Hex>) {
        let Some(sealed) = sealed else { return };
        let scan = self.owner.scan_key();
        let Ok(shared) = scan.shared(&proposal.ephemeral()) else {
            return;
        };
        let id = proposal.id();
        let Some(note) = open(&sealed.0, &shared, &id, &self.address(), scan, self.network) else {
            return;
        };
        let kept = self.store.write(|tx| {
            if !writable(tx)? {
                return Ok(());
            }
            tx.execute(
                "INSERT INTO received_notes (receipt, text, code) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO NOTHING",
                params![
                    id.as_bytes(),
                    note.text,
                    note.from.as_ref().map(LicashCode::to_string)
                ],
            )?;
            Ok(())
        });
        if let Err(e) = kept {
            tracing::warn!(id = %hex::encode(id.as_bytes()), "a note not kept: {e}");
        }
    }

    /// What the note of the payment made or taken `id` (in hex) says, if
    /// it was sent or taken with one saying anything.
    pub fn payment_note(&self, id: &str) -> Result<Option<PaymentNote>, Error> {
        Ok(self.payment_notes()?.remove(id))
    }

    /// Whether payments go with notes: unless the wallet's file keeps
    /// another version of them, of a later build.
    pub(crate) fn notes_on(&self) -> Result<bool, Error> {
        self.store.read(readable)
    }

    /// `note` as [`Wallet::pay_code_saying`] sends it at the hub at `url`,
    /// to check before anything else is done to pay there: refused if it
    /// says anything and the hub keeps no note with a payment
    /// ([`Error::NoteUnkept`]), its info asked again first, or this
    /// wallet's file keeps its notes as a later version does.
    pub async fn note_for(&self, url: &str, note: &PaymentNote) -> Result<PaymentNote, Error> {
        let url = crate::hub_url(url)?;
        let note = self.own_note(note)?;
        if !note.is_empty() && !self.notes_on()? {
            return Err(Error::Invalid(
                "a note, the wallet's file keeping notes as a later version does".into(),
            ));
        }
        if !note.is_empty() && !self.keeps_notes(&url).await? {
            return Err(Error::NoteUnkept { hub: url });
        }
        Ok(note)
    }

    /// `note` as sent: its text as [`note_text`] says it, none if blank,
    /// and its code one of this wallet's.
    pub(crate) fn own_note(&self, note: &PaymentNote) -> Result<PaymentNote, Error> {
        let text = note.text.as_deref().filter(|t| !t.trim().is_empty());
        let text = text.map(note_text).transpose()?;
        if let Some(from) = &note.from {
            if from.address != self.address() || from.network != self.network {
                return Err(Error::Invalid(
                    "a note sharing another wallet's code".into(),
                ));
            }
        }
        Ok(PaymentNote {
            text,
            from: note.from.clone(),
        })
    }

    /// Whether the hub at `url` keeps a note with a payment it keeps: its
    /// info asked again if it did not say so.
    pub(crate) async fn keeps_notes(&self, url: &str) -> Result<bool, Error> {
        if kept_by(&self.hub(url)?.1) {
            return Ok(true);
        }
        Ok(kept_by(&self.refresh(url).await?.1))
    }
}

#[cfg(test)]
mod tests {
    use licash::{address::OwnerKeys, spend::ChainProfile};

    use super::*;

    const REGTEST: ChainProfile = ChainProfile::Core30DefaultRegtest;

    fn code(keys: &OwnerKeys, network: ChainProfile) -> LicashCode {
        LicashCode {
            network,
            address: keys.address(),
            mailbox: Some([7; 32]),
            hubs: Vec::new(),
            locations: Vec::new(),
        }
    }

    struct Case {
        alice: OwnerKeys,
        bob: OwnerKeys,
        shared: [u8; 32],
        id: ProposalId,
    }

    fn case() -> Case {
        Case {
            alice: OwnerKeys::from_seed(&[1; 32]).unwrap(),
            bob: OwnerKeys::from_seed(&[2; 32]).unwrap(),
            shared: [9; 32],
            id: ProposalId::from_bytes([5; 32]),
        }
    }

    fn opened(c: &Case, sealed: &[u8]) -> Option<PaymentNote> {
        let bob = &c.bob;
        open(
            sealed,
            &c.shared,
            &c.id,
            &bob.address(),
            bob.scan_key(),
            REGTEST,
        )
    }

    #[test]
    fn a_note_opens_to_what_it_said_and_its_payers_code() {
        let c = case();
        let note = PaymentNote {
            text: Some("thanks".into()),
            from: Some(code(&c.alice, REGTEST)),
        };
        let to = c.bob.address();
        let sealed = seal(&c.shared, &c.id, &to, &note, c.alice.scan_key()).unwrap();
        assert_eq!(sealed.len(), NOTE_SEALED);
        assert_eq!(opened(&c, &sealed), Some(note));
        let empty = seal(
            &c.shared,
            &c.id,
            &to,
            &PaymentNote::default(),
            c.alice.scan_key(),
        );
        assert_eq!(empty.as_ref().map(Vec::len).ok(), Some(NOTE_SEALED));
        assert_eq!(opened(&c, &empty.unwrap()), None, "says nothing");
    }

    #[test]
    fn a_code_not_proved_its_payers_is_dropped_and_the_text_kept() {
        let c = case();
        let carol = OwnerKeys::from_seed(&[3; 32]).unwrap();
        // Alice claims Carol's code: she cannot prove it, Carol's scan key
        // not hers.
        let note = PaymentNote {
            text: Some("from carol".into()),
            from: Some(code(&carol, REGTEST)),
        };
        let to = c.bob.address();
        let sealed = seal(&c.shared, &c.id, &to, &note, c.alice.scan_key()).unwrap();
        let said = opened(&c, &sealed).unwrap();
        assert_eq!(said.text.as_deref(), Some("from carol"));
        assert_eq!(said.from, None);
        // Nor is a code of another network kept.
        let main = PaymentNote {
            text: None,
            from: Some(code(&c.alice, ChainProfile::Core30Mainnet)),
        };
        let sealed = seal(&c.shared, &c.id, &to, &main, c.alice.scan_key()).unwrap();
        assert_eq!(opened(&c, &sealed), None);
    }

    #[test]
    fn a_note_opens_only_for_its_payment() {
        let c = case();
        let note = PaymentNote {
            text: Some("hi".into()),
            from: None,
        };
        let to = c.bob.address();
        let sealed = seal(&c.shared, &c.id, &to, &note, c.alice.scan_key()).unwrap();
        let other = Case {
            id: ProposalId::from_bytes([6; 32]),
            ..case()
        };
        assert_eq!(opened(&other, &sealed), None);
        let other = Case {
            shared: [8; 32],
            ..case()
        };
        assert_eq!(opened(&other, &sealed), None);
        assert_eq!(opened(&c, &sealed[1..]), None);
    }

    #[test]
    fn note_text_is_refused_rather_than_cut() {
        assert_eq!(note_text(" a \n b\t").unwrap(), "a b");
        assert!(note_text("   ").is_err());
        assert!(note_text(&"é".repeat(MAX_NOTE / 2)).is_ok());
        assert!(note_text(&"é".repeat(MAX_NOTE / 2 + 1)).is_err());
        for reordering in ['\u{202e}', '\u{2067}', '\u{200f}', '\u{061c}'] {
            assert!(note_text(&format!("a{reordering}b")).is_err());
        }
        assert!(note_text("a\u{0}b").is_err());
    }
}
