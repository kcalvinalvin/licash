//! Strict Alice HASH160 request carriers.
//!
//! This is not the core SHA-256 WOTS variant. Decoding proves neither chain
//! inclusion nor publication, eligibility, opening authorization or settlement.
//! Callers must authenticate the actual witness with BIP141, bind its block and
//! opening, and compare the expected leaf/control block. Never ignore a decoder
//! error on a confirmed frozen R: it is an operational alarm.

use crate::{crypto::tagged_hash, Error};
use bitcoin::{
    hashes::{hash160, Hash},
    opcodes::all::*,
    script::Builder,
};

type Element = [u8; 20];
const B_DIGITS: usize = 10;
const E_DIGITS: usize = 67;
const TAG: &[u8] = b"LICASH-RKEY\x02";
pub const PUBLIC_KEYS_BYTES: usize = TAG.len() + (B_DIGITS + E_DIGITS) * 20;
pub const WITNESS_ITEMS: usize = 158;
const ARGUMENT_ITEMS: usize = WITNESS_ITEMS - 2;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}
fn hash(bytes: &[u8]) -> Element {
    hash160::Hash::hash(bytes).to_byte_array()
}
fn chain(mut value: Element, count: u8) -> Element {
    for _ in 0..count {
        value = hash(&value);
    }
    value
}
fn secret(entropy: &[u8; 32], mut index: usize) -> Element {
    let mut bytes = entropy.to_vec();
    while index > 0 {
        bytes.push((index & 255) as u8);
        index >>= 8;
    }
    hash(&bytes)
}
fn digits(message: &[u8], checksum_digits: usize) -> Vec<u8> {
    let mut result: Vec<_> = message.iter().flat_map(|b| [b >> 4, b & 15]).collect();
    let mut checksum: u32 = result.iter().map(|d| u32::from(15 - d)).sum();
    let mut tail = vec![0; checksum_digits];
    for digit in tail.iter_mut().rev() {
        *digit = (checksum & 15) as u8;
        checksum >>= 4;
    }
    result.extend(tail);
    result
}

/// Public keys are scoped to exactly one reserved funding context/outpoint.
/// Their encoding fixes HASH160, message lengths, checksum and chain equations.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKeys {
    b: [Element; B_DIGITS],
    e: [Element; E_DIGITS],
}
impl std::fmt::Debug for PublicKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestPublicKeys(..)")
    }
}
impl PublicKeys {
    /// Unbound request gadget. A full opening must use request_leaf_for_base.
    /// Block-based CSV is optional (zero means
    /// none). Both party keys must be valid, distinct x-only keys. The opening
    /// must bind these exact inputs and the resulting script to reserved F/R.
    /// This is not the timeout leaf: terminal timeout signatures may use ACP.
    pub fn request_leaf(
        &self,
        alice: &[u8; 32],
        hub: &[u8; 32],
        delay: u16,
    ) -> Result<Vec<u8>, Error> {
        if alice == hub {
            return Err(bad("request party keys must differ"));
        }
        for key in [alice, hub] {
            k256::schnorr::VerifyingKey::from_bytes(key).map_err(|_| Error::InvalidPoint)?;
        }
        let mut b = Builder::new();
        if delay > 0 {
            b = b
                .push_int(i64::from(delay))
                .push_opcode(OP_CSV)
                .push_opcode(OP_DROP);
        }
        for key in [alice, hub] {
            b = b
                .push_opcode(OP_SIZE)
                .push_int(64)
                .push_opcode(OP_EQUALVERIFY)
                .push_slice(*key)
                .push_opcode(OP_CHECKSIGVERIFY);
        }
        // Both signatures are gone. Digit slots are 0,2,...,152 from the top;
        // each PICK copy is consumed without moving any original argument.
        for depth in (0..154).step_by(2) {
            b = b
                .push_int(depth)
                .push_opcode(OP_PICK)
                .push_opcode(OP_SIZE)
                .push_int(2)
                .push_opcode(OP_LESSTHAN)
                .push_opcode(OP_VERIFY)
                .push_opcode(OP_DUP)
                .push_opcode(OP_SIZE)
                .push_opcode(OP_IF)
                .push_int(1)
                .push_int(16)
                .push_opcode(OP_WITHIN)
                .push_opcode(OP_VERIFY)
                .push_opcode(OP_ELSE)
                .push_int(0)
                .push_opcode(OP_EQUALVERIFY)
                .push_opcode(OP_ENDIF)
                .push_opcode(OP_DROP);
        }
        b = wots_script(b, &self.b, 8, 2);
        for _ in 0..4 {
            b = b.push_opcode(OP_2DROP);
        }
        b = wots_script(b, &self.e, 64, 3);
        for _ in 0..32 {
            b = b.push_opcode(OP_2DROP);
        }
        Ok(b.push_int(1).into_script().into_bytes())
    }

    /// Anchor the independently agreed Base digest in F's request leaf, never
    /// final D. This adds 34 script bytes and consumes no extra witness items.
    /// Base must exclude keys/scripts/roots/F/R/instance hashlocks derived from
    /// it; the future opening verifies its complete preimage and named deposit.
    pub fn request_leaf_for_base(
        &self,
        base: &[u8; 32],
        alice: &[u8; 32],
        hub: &[u8; 32],
        delay: u16,
    ) -> Result<Vec<u8>, Error> {
        let mut bytes = Builder::new()
            .push_slice(*base)
            .push_opcode(OP_DROP)
            .into_script()
            .into_bytes();
        bytes.extend(self.request_leaf(alice, hub, delay)?);
        Ok(bytes)
    }
    pub fn amount_elements(&self) -> &[Element; B_DIGITS] {
        &self.b
    }
    pub fn claim_elements(&self) -> &[Element; E_DIGITS] {
        &self.e
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = TAG.to_vec();
        for element in self.b.iter().chain(self.e.iter()) {
            bytes.extend(element);
        }
        bytes
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != PUBLIC_KEYS_BYTES || !bytes.starts_with(TAG) {
            return Err(bad("request public-key framing"));
        }
        let mut elements = bytes[TAG.len()..].as_chunks::<20>().0.iter();
        let b = std::array::from_fn(|_| *elements.next().expect("checked key count"));
        let e = std::array::from_fn(|_| *elements.next().expect("checked key count"));
        Ok(Self { b, e })
    }

    /// Parse the exact no-payload witness, returning only script-verified b/E.
    /// Party signatures remain opaque, size-64 values: consensus validates them,
    /// and their particular bytes do not enter the decoded request identity.
    /// This method itself does not authenticate the supplied witness or script.
    pub fn decode_witness(
        &self,
        witness: &[Vec<u8>],
        expected_leaf: &[u8],
        expected_control: &[u8],
    ) -> Result<Request, Error> {
        if witness.len() != WITNESS_ITEMS
            || expected_leaf.is_empty()
            || expected_control.len() < 33
            || expected_control.len() > 33 + 32 * 128
            || !(expected_control.len() - 33).is_multiple_of(32)
            || expected_control[0] & 0xfe != 0xc0
            || witness[ARGUMENT_ITEMS] != expected_leaf
            || witness[ARGUMENT_ITEMS + 1] != expected_control
            || witness[ARGUMENT_ITEMS - 1].len() != 64
            || witness[ARGUMENT_ITEMS - 2].len() != 64
        {
            return Err(bad("request witness profile"));
        }
        let e = verify_pairs(&self.e, &witness[..2 * E_DIGITS], 32, 3)?;
        let b = verify_pairs(
            &self.b,
            &witness[2 * E_DIGITS..2 * (E_DIGITS + B_DIGITS)],
            4,
            2,
        )?;
        Ok(Request {
            amount: u32::from_be_bytes(b.try_into().expect("four-byte amount")),
            claim_digest: e.try_into().expect("32-byte digest"),
        })
    }
}

/// An intentionally non-Clone, consumed signer. Durable context reservation and
/// preventing reconstruction from the same seed remain the caller's job. An
/// abandoned opening burns its key context. Final D must never feed this API.
pub struct Signer {
    b: [u8; 32],
    e: [u8; 32],
}
impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestSigner(..)")
    }
}
impl Signer {
    pub fn derive(seed: &[u8; 32], independent_base_context: &[u8; 32]) -> Self {
        Self {
            b: tagged_hash("licash/request-b-key/v2", &[seed, independent_base_context]),
            e: tagged_hash("licash/request-e-key/v2", &[seed, independent_base_context]),
        }
    }
    pub fn public(&self) -> PublicKeys {
        PublicKeys {
            b: std::array::from_fn(|i| chain(secret(&self.b, i), 15)),
            e: std::array::from_fn(|i| chain(secret(&self.e, i), 15)),
        }
    }
    pub fn sign(self, request: Request) -> SignedRequest {
        let mut pairs = sign_pairs(&self.e, &request.claim_digest, 3);
        pairs.extend(sign_pairs(&self.b, &request.amount.to_be_bytes(), 2));
        SignedRequest { request, pairs }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub amount: u32,
    pub claim_digest: [u8; 32],
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Request(..)")
    }
}
pub struct SignedRequest {
    request: Request,
    pairs: Vec<Vec<u8>>,
}
impl std::fmt::Debug for SignedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedRequest(..)")
    }
}
impl SignedRequest {
    pub fn request(&self) -> Request {
        self.request
    }
    /// Bottom-first E and b pairs. Append hub then Alice signature, followed by
    /// the expected leaf/control block. There are no dropped publication slots.
    pub fn pairs(&self) -> &[Vec<u8>] {
        &self.pairs
    }
    pub fn witness(
        &self,
        alice_signature: [u8; 64],
        hub_signature: [u8; 64],
        leaf: Vec<u8>,
        control: Vec<u8>,
    ) -> Vec<Vec<u8>> {
        let mut items = self.pairs.clone();
        items.extend([
            hub_signature.to_vec(),
            alice_signature.to_vec(),
            leaf,
            control,
        ]);
        items
    }
}
fn sign_pairs(entropy: &[u8; 32], message: &[u8], checksum_digits: usize) -> Vec<Vec<u8>> {
    let mut pairs = Vec::new();
    for (i, digit) in digits(message, checksum_digits).into_iter().enumerate() {
        pairs.push(chain(secret(entropy, i), digit).to_vec());
        pairs.push(if digit == 0 { vec![] } else { vec![digit] });
    }
    pairs
}
fn verify_pairs(
    public: &[Element],
    pairs: &[Vec<u8>],
    message_bytes: usize,
    checksum_digits: usize,
) -> Result<Vec<u8>, Error> {
    if public.len() != message_bytes * 2 + checksum_digits || pairs.len() != public.len() * 2 {
        return Err(bad("request WOTS shape"));
    }
    let mut supplied = Vec::with_capacity(public.len());
    for (key, pair) in public.iter().zip(pairs.as_chunks::<2>().0) {
        let digit = match pair[1].as_slice() {
            [] => 0,
            [d @ 1..=15] => *d,
            _ => return Err(bad("noncanonical request digit")),
        };
        let reveal: Element = pair[0]
            .as_slice()
            .try_into()
            .map_err(|_| bad("request reveal width"))?;
        if chain(reveal, 15 - digit) != *key {
            return Err(bad("request WOTS chain"));
        }
        supplied.push(digit);
    }
    let message: Vec<_> = supplied[..message_bytes * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| pair[0] << 4 | pair[1])
        .collect();
    if digits(&message, checksum_digits) != supplied {
        return Err(bad("request WOTS checksum"));
    }
    Ok(message)
}

// Native bytecode port of LN-GAP's HASH160 wots_step/checksum_finale. Keep the
// existing chain/checksum gadget unchanged; canonicality is a separate guard
// above. Regtest compares these bytes with the pristine dependency's builder,
// which cannot be a dependency of the core/guest-facing statement layer.
fn wots_script(
    mut b: Builder,
    public: &[Element],
    message_digits: u32,
    checksum_digits: u32,
) -> Builder {
    for key in public.iter().rev() {
        b = b
            .push_opcode(OP_SWAP)
            .push_opcode(OP_SIZE)
            .push_int(20)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_SWAP)
            .push_int(15)
            .push_opcode(OP_MIN)
            .push_opcode(OP_DUP)
            .push_opcode(OP_TOALTSTACK)
            .push_int(8)
            .push_opcode(OP_2DUP)
            .push_opcode(OP_LESSTHAN)
            .push_opcode(OP_IF)
            .push_opcode(OP_DROP)
            .push_opcode(OP_TOALTSTACK);
        for _ in 0..8 {
            b = b.push_opcode(OP_HASH160);
        }
        b = b
            .push_opcode(OP_ELSE)
            .push_opcode(OP_SUB)
            .push_opcode(OP_TOALTSTACK)
            .push_opcode(OP_ENDIF);
        for _ in 0..7 {
            b = b.push_opcode(OP_DUP).push_opcode(OP_HASH160);
        }
        b = b
            .push_opcode(OP_FROMALTSTACK)
            .push_opcode(OP_PICK)
            .push_slice(*key)
            .push_opcode(OP_EQUALVERIFY);
        for _ in 0..4 {
            b = b.push_opcode(OP_2DROP);
        }
    }
    b = b
        .push_opcode(OP_FROMALTSTACK)
        .push_opcode(OP_DUP)
        .push_opcode(OP_NEGATE);
    for _ in 1..message_digits {
        b = b
            .push_opcode(OP_FROMALTSTACK)
            .push_opcode(OP_TUCK)
            .push_opcode(OP_SUB);
    }
    b = b
        .push_int(i64::from(15 * message_digits))
        .push_opcode(OP_ADD)
        .push_opcode(OP_FROMALTSTACK);
    for _ in 0..checksum_digits - 1 {
        for _ in 0..4 {
            b = b.push_opcode(OP_DUP).push_opcode(OP_ADD);
        }
        b = b.push_opcode(OP_FROMALTSTACK).push_opcode(OP_ADD);
    }
    b.push_opcode(OP_EQUALVERIFY)
}
