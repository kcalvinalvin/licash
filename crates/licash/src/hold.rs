//! Q-authorized public hold envelopes, not claim eligibility or hold adoption.
//!
//! E is computed from these unsigned records and all other claim metadata. Verifiers must supply the expected context and
//! exact hold root from that claim, never trust an envelope to select either.

use crate::{
    crypto::{self, Point, ScalarBytes, Signature},
    note::{Message, PUBLIC_FIELDS_BYTES},
    Error,
};

const TAG: &[u8] = b"LICASH-HOLD\x02";
const CONTEXT_BYTES: usize = 32 + 36 + 4 + 32;
pub const RECORD_BYTES: usize = PUBLIC_FIELDS_BYTES + 64;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// Opaque expected request binding. This type does not verify the opening,
/// deposit, amount or composition of E. The Base digest is not a
/// key-derivation input.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// The opening's Base digest ([`crate::base::Base::digest`]).
    pub base: [u8; 32],
    pub deposit: [u8; 36],
    pub amount: u32,
    pub claim_digest: [u8; 32],
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HoldContext(..)")
    }
}

impl Context {
    fn bytes(&self) -> [u8; CONTEXT_BYTES] {
        let mut bytes = [0; CONTEXT_BYTES];
        bytes[..32].copy_from_slice(&self.base);
        bytes[32..68].copy_from_slice(&self.deposit);
        bytes[68..72].copy_from_slice(&self.amount.to_be_bytes());
        bytes[72..].copy_from_slice(&self.claim_digest);
        bytes
    }

    /// Bind the exact note as well as its claim. A Q key can occur in more
    /// than one message, so Q's signature over the context alone is not
    /// evidence that any particular note authorized a second claim.
    pub fn signing_digest(&self, message: &Message) -> [u8; 32] {
        crypto::tagged_hash(
            "licash/hold-note-authorization/v2",
            &[&self.bytes(), &message.public_fields()],
        )
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            base: bytes[..32].try_into().expect("checked context width"),
            deposit: bytes[32..68].try_into().expect("checked context width"),
            amount: u32::from_be_bytes(bytes[68..72].try_into().expect("checked context width")),
            claim_digest: bytes[72..].try_into().expect("checked context width"),
        }
    }
}

/// Untrusted public authorization record. A signature proves only Q authority,
/// not spending authority, issuer evidence, ownership or recovery allocation.
#[derive(Clone, PartialEq, Eq)]
pub struct Record {
    pub message: Message,
    pub sig: Signature,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HoldRecord(..)")
    }
}

/// Exact counted unsigned opening root, sorted strictly by resulting Y.
/// Limit is negotiated/validated caller policy, not an inferred 32-source cap.
pub fn authorization_root(messages: &[Message], limit: usize) -> Result<[u8; 32], Error> {
    if messages.len() > limit {
        return Err(bad("hold authorization budget"));
    }
    let n = u32::try_from(messages.len()).map_err(|_| bad("hold count overflow"))?;
    let mut previous: Option<Point> = None;
    let mut bytes = n.to_be_bytes().to_vec();
    for message in messages {
        let y = message.identifier()?;
        if previous.is_some_and(|p| p >= y) {
            return Err(bad("noncanonical hold order"));
        }
        previous = Some(y);
        bytes.extend(message.public_fields());
    }
    Ok(crypto::tagged_hash("licash/hold-set/v2", &[&bytes]))
}

#[derive(Clone, PartialEq, Eq)]
pub struct Envelope {
    context: Context,
    records: Vec<Record>,
}

impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HoldEnvelope(..)")
    }
}

impl Envelope {
    pub fn sign(
        context: Context,
        entries: &[(Message, ScalarBytes)],
        limit: usize,
    ) -> Result<Self, Error> {
        let messages: Vec<_> = entries.iter().map(|(message, _)| message.clone()).collect();
        let root = authorization_root(&messages, limit)?;
        let mut records = Vec::new();
        for (message, key) in entries {
            let key = key.to_nonzero()?;
            if &crypto::x_only_pub(key.as_ref()) != message.hold() {
                return Err(bad("wrong hold authority"));
            }
            records.push(Record {
                message: message.clone(),
                sig: crypto::schnorr_sign(key.as_ref(), &context.signing_digest(message))?,
            });
        }
        Self::from_records(context, &root, records, limit)
    }

    pub fn from_records(
        context: Context,
        expected_root: &[u8; 32],
        records: Vec<Record>,
        limit: usize,
    ) -> Result<Self, Error> {
        let envelope = Self { context, records };
        envelope.verify(&context, expected_root, limit)?;
        Ok(envelope)
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }
    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn verify(
        &self,
        expected_context: &Context,
        expected_root: &[u8; 32],
        limit: usize,
    ) -> Result<(), Error> {
        if &self.context != expected_context {
            return Err(bad("hold request context mismatch"));
        }
        let messages: Vec<_> = self
            .records
            .iter()
            .map(|record| record.message.clone())
            .collect();
        if &authorization_root(&messages, limit)? != expected_root {
            return Err(bad("hold set differs from claim"));
        }
        for record in &self.records {
            crypto::schnorr_verify(
                record.message.hold(),
                &expected_context.signing_digest(&record.message),
                &record.sig,
            )?;
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = TAG.to_vec();
        bytes.extend(self.context.bytes());
        bytes.extend(
            u32::try_from(self.records.len())
                .expect("validated hold count")
                .to_be_bytes(),
        );
        for record in &self.records {
            bytes.extend(record.message.public_fields());
            bytes.extend(record.sig.0);
        }
        bytes
    }

    /// Expected root comes from the claim, not from the untrusted payload.
    /// No missing authority defaults are accepted.
    pub fn from_bytes(
        expected_context: &Context,
        expected_root: &[u8; 32],
        limit: usize,
        bytes: &[u8],
    ) -> Result<Self, Error> {
        let header = TAG.len() + CONTEXT_BYTES + 4;
        if bytes.len() < header || &bytes[..TAG.len()] != TAG {
            return Err(bad("hold envelope framing"));
        }
        let context = Context::from_bytes(&bytes[TAG.len()..TAG.len() + CONTEXT_BYTES]);
        if &context != expected_context {
            return Err(bad("hold request context mismatch"));
        }
        let n = usize::try_from(u32::from_be_bytes(
            bytes[header - 4..header]
                .try_into()
                .expect("checked header"),
        ))
        .map_err(|_| bad("hold count overflow"))?;
        let length = n
            .checked_mul(RECORD_BYTES)
            .and_then(|length| header.checked_add(length));
        if n > limit || length != Some(bytes.len()) {
            return Err(bad("hold envelope count or length"));
        }
        let mut records = Vec::new();
        for record in bytes[header..].as_chunks::<RECORD_BYTES>().0 {
            records.push(Record {
                message: Message::from_public_fields(&record[..PUBLIC_FIELDS_BYTES])?,
                sig: Signature(
                    record[PUBLIC_FIELDS_BYTES..]
                        .try_into()
                        .expect("fixed signature width"),
                ),
            });
        }
        Self::from_records(context, expected_root, records, limit)
    }
}
