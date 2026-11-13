//! Canonical Base: the contract's terms, keys and programs, fixed before
//! funding and committed by the final digest D.
//!
//! Response keys are chosen independently before Base and use SHA-256 WOTS,
//! not Alice's HASH160 request variant. K signs 160 bytes.

use crate::{
    chain::{Checkpoint, OutPoint},
    crypto::{self, Point, Signature},
    disclosure::CHUNK_DIGITS,
    note::{non_identity, Keyset},
    opening::FrozenPrefix,
    payout::RecoveryBudget,
    statement::HubKeys,
    statement::K_DIGITS,
    wots::{Hashes, B_DIGITS, MSG_DIGITS},
    Error,
};

const BASE_TAG: &[u8] = b"LICASH-BASE\x02";
pub const MAX_BASE_BYTES: usize = 128_000;
const MAX_MONEY: u64 = 2_100_000_000_000_000;

const RESPONSE_TAG: &[u8] = b"LICASH-RSPK\x02";
const ROLE_DIGITS: [usize; 10] = [
    B_DIGITS,
    MSG_DIGITS,
    MSG_DIGITS,
    K_DIGITS,
    B_DIGITS,
    CHUNK_DIGITS[0],
    CHUNK_DIGITS[1],
    CHUNK_DIGITS[2],
    CHUNK_DIGITS[3],
    CHUNK_DIGITS[4],
];
const ENDPOINTS: usize = 2 * B_DIGITS
    + 2 * MSG_DIGITS
    + K_DIGITS
    + CHUNK_DIGITS[0]
    + CHUNK_DIGITS[1]
    + CHUNK_DIGITS[2]
    + CHUNK_DIGITS[3]
    + CHUNK_DIGITS[4];
pub const RESPONSE_KEYS_BYTES: usize = RESPONSE_TAG.len() + 1 + 10 * 5 + ENDPOINTS * 32;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

/// Exact ten-role public response-key carrier, with no signing/admission
/// authority. Shape checks prove neither independent secret generation nor
/// one-time state. The funding gate must match retained Hub key material.
#[derive(Clone, PartialEq, Eq)]
pub struct ResponseKeys(HubKeys);

impl std::fmt::Debug for ResponseKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponseKeys(..)")
    }
}

impl ResponseKeys {
    pub fn new(keys: HubKeys) -> Result<Self, Error> {
        let keys = Self(keys);
        if keys
            .roles()
            .iter()
            .zip(ROLE_DIGITS)
            .any(|(key, count)| key.len() != count)
        {
            return Err(bad("native sealed response-key shape"));
        }
        Ok(keys)
    }

    pub fn keys(&self) -> &HubKeys {
        &self.0
    }

    fn roles(&self) -> [&Hashes; 10] {
        let keys = &self.0;
        [
            &keys.b,
            &keys.e_plain,
            &keys.e_counter,
            &keys.k,
            &keys.b_corr,
            &keys.disclosure[0],
            &keys.disclosure[1],
            &keys.disclosure[2],
            &keys.disclosure[3],
            &keys.disclosure[4],
        ]
    }

    /// Canonical role count, ordered role tags, counted 32-byte endpoints.
    /// This is not a serde/bincode encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(RESPONSE_KEYS_BYTES);
        bytes.extend(RESPONSE_TAG);
        bytes.push(10);
        for (role, key) in self.roles().into_iter().enumerate() {
            bytes.push(role as u8);
            bytes.extend((key.len() as u32).to_be_bytes());
            for endpoint in &key.0 {
                bytes.extend(endpoint);
            }
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != RESPONSE_KEYS_BYTES
            || !bytes.starts_with(RESPONSE_TAG)
            || bytes[RESPONSE_TAG.len()] != 10
        {
            return Err(bad("native response-key framing"));
        }
        let mut at = RESPONSE_TAG.len() + 1;
        let mut keys = Vec::with_capacity(10);
        for (role, count) in ROLE_DIGITS.into_iter().enumerate() {
            if bytes[at] != role as u8
                || u32::from_be_bytes(bytes[at + 1..at + 5].try_into().expect("fixed role header"))
                    != count as u32
            {
                return Err(bad("native response-key role or count"));
            }
            at += 5;
            let end = at + count * 32;
            keys.push(Hashes(bytes[at..end].as_chunks::<32>().0.to_vec()));
            at = end;
        }
        let [b, e_plain, e_counter, k, b_corr, d0, d1, d2, d3, d4] =
            <[Hashes; 10]>::try_from(keys).expect("ten fixed roles");
        Self::new(HubKeys {
            b,
            e_plain,
            e_counter,
            k,
            b_corr,
            disclosure: [d0, d1, d2, d3, d4],
        })
    }
}

/// Independent public party identities, holder first and Hub second. Payout
/// destinations are selected scripts, never Base-derived compiled graph roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Party {
    pub funding: [u8; 32],
    pub payment: [u8; 32],
    pub delayed: [u8; 32],
    pub payout_script: Vec<u8>,
}

/// Public parameters of the initial native Corrected graph recipe. Absolute
/// deadlines in this recipe are block heights; old timestamp contracts stay
/// unchanged. No duplicated request CSV or serialized graph funding value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphTerms {
    pub id: u32,
    pub unit_sats: u64,
    pub bits: u8,
    pub low_bits: u8,
    pub deposit_units: u32,
    pub close_height: u32,
    pub reserve_alice: u64,
    pub reserve_hub: u64,
    pub to_self_delay: u16,
    pub commit_fee: u64,
    pub presign_fee: u64,
    pub dust: u64,
    pub delta: u16,
    pub delta_prime: u16,
    pub deadline_margin: u32,
    pub response_window: u16,
    pub assert_height: u32,
    pub hub_challenge: u16,
    pub holder_answer: u16,
}

impl GraphTerms {
    pub fn maximum_amount(&self) -> Result<u32, Error> {
        if !(1..=32).contains(&self.bits) || self.low_bits >= self.bits {
            return Err(bad("native graph amount dimensions"));
        }
        Ok(((1u64 << self.bits) - 1) as u32)
    }

    pub fn graph_value(&self) -> Result<u64, Error> {
        u64::from(self.maximum_amount()?)
            .checked_add(self.reserve_alice)
            .and_then(|sum| sum.checked_add(self.reserve_hub))
            .filter(|sum| *sum <= MAX_MONEY)
            .ok_or_else(|| bad("native graph monetary range"))
    }
}

/// Static recipe parameters and program identities, not generated instance
/// hashes or verifying keys produced under D. The funding gate must recognize
/// the actual recipe/programs; this unsigned carrier does not claim they exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub graph_version: u32,
    pub holder_program: [u8; 32],
    pub counter_program: [u8; 32],
    pub full_instances: u16,
    pub full_kept: u16,
    pub corrected_instances: u16,
    pub corrected_kept: u16,
}

/// The proof programs a party trusts: the image ids it built or audited.
/// Base only names programs; each funding gate compares them with these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Programs {
    pub holder: [u8; 32],
    pub counter: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct ChainTerms {
    pub checkpoint: Checkpoint,
    pub funding_depth: u32,
    pub request_depth: u32,
    pub publication_depth: u32,
    pub lookback: u32,
    pub observation_margin: u32,
    pub max_headers: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_transaction_bytes: u32,
    pub max_publication_bytes: u32,
    pub max_authorizations: u32,
    pub max_source_inputs: u32,
}

/// Fee ceilings are explicit Base terms. The actual quote and all terminal
/// reserve/fee-resource checks remain mandatory before funding signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fees {
    pub sweep_fee_max: u64,
    pub external_fee_max: u64,
    pub max_rate_sat_per_kvb: u64,
    pub opening_fee_quote: u64,
}

#[derive(Clone)]
pub struct Parts {
    pub nonce: [u8; 32],
    pub spend_root: Point,
    pub claim_key: [u8; 32],
    pub keyset: Keyset,
    /// Bitcoin outpoint bytes: raw txid followed by little-endian vout.
    pub deposit: [u8; 36],
    pub cap: u32,
    pub response_keys: ResponseKeys,
    pub parties: [Party; 2],
    pub graph: GraphTerms,
    pub recipe: Recipe,
    pub chain: ChainTerms,
    pub limits: Limits,
    pub fees: Fees,
}

/// An unsigned canonical preimage, NOT an accepted/prepared opening, a trusted
/// checkpoint, input reservation, or permission to release funding signatures.
#[derive(Clone)]
pub struct Base(Parts);

impl std::fmt::Debug for Base {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Base(..)")
    }
}

/// Deliberately not interchangeable with the raw independent Base key context.
/// No constructor from peer-supplied bytes or funding/admission authority.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FinalDigest([u8; 32]);

impl std::fmt::Debug for FinalDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FinalDigest(..)")
    }
}

impl FinalDigest {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Base {
    /// Generate before key derivation. Validation cannot establish randomness
    /// or freshness of caller-supplied bytes; neither is inferred from absence.
    pub fn fresh_nonce() -> [u8; 32] {
        use rand_core::RngCore;
        let mut nonce = [0; 32];
        rand_core::OsRng.fill_bytes(&mut nonce);
        nonce
    }

    pub fn new(parts: Parts) -> Result<Self, Error> {
        let base = Self(parts);
        base.validate()?;
        Ok(base)
    }

    pub fn parts(&self) -> &Parts {
        &self.0
    }

    pub fn deposit_outpoint(&self) -> OutPoint {
        OutPoint {
            txid: self.0.deposit[..32].try_into().expect("fixed txid"),
            index: u32::from_le_bytes(self.0.deposit[32..].try_into().expect("fixed vout")),
        }
    }

    pub fn recovery_budget(&self) -> Result<RecoveryBudget, Error> {
        RecoveryBudget::new(
            self.0.graph.unit_sats,
            self.0.graph.low_bits,
            self.0.fees.sweep_fee_max,
            self.0.fees.external_fee_max,
        )
    }

    fn validate(&self) -> Result<(), Error> {
        let p = &self.0;
        non_identity(&p.spend_root)?;
        for key in [&p.claim_key].into_iter().chain(
            p.parties
                .iter()
                .flat_map(|party| [&party.funding, &party.payment, &party.delayed]),
        ) {
            k256::schnorr::VerifyingKey::from_bytes(key).map_err(|_| Error::InvalidPoint)?;
        }
        if p.nonce == [0; 32]
            || p.deposit[..32] == [0; 32]
            || p.parties[0].payment == p.parties[1].payment
            || p.cap == 0
            || p.cap > p.graph.maximum_amount()?
        {
            return Err(bad("native Base identity or capacity"));
        }
        for party in &p.parties {
            if party.payout_script.len() < 2 || party.payout_script.len() > 10_000 {
                return Err(bad("native payout destination length"));
            }
        }
        let g = &p.graph;
        let budget = self.recovery_budget()?;
        g.graph_value()?;
        if g.dust != 330
            || (1u64 << g.low_bits) < g.dust
            || g.reserve_alice < g.dust
            || g.reserve_hub < budget.minimum_surplus()
            || g.deposit_units > g.maximum_amount()?
            || g.to_self_delay == 0
            || g.delta == 0
            || g.delta_prime == 0
            || g.deadline_margin == 0
            || g.response_window <= 1
            || g.hub_challenge <= 1
            || g.holder_answer <= 1
            || g.assert_height <= p.chain.checkpoint.height()
            || g.assert_height >= g.close_height
            || g.close_height >= 500_000_000
        {
            return Err(bad("native graph profile terms"));
        }
        if [
            g.commit_fee,
            g.presign_fee,
            p.fees.sweep_fee_max,
            p.fees.external_fee_max,
            p.fees.opening_fee_quote,
            p.fees.max_rate_sat_per_kvb,
        ]
        .into_iter()
        .any(|value| value > MAX_MONEY)
            || p.fees.max_rate_sat_per_kvb == 0
        {
            return Err(bad("native fee quote range"));
        }
        // Both programs read Bitcoin as a header chain from the checkpoint,
        // and a statement can be needed until the assertion deadline. A
        // budget that ends earlier would leave a late recovery unprovable.
        let c = &p.chain;
        if c.max_headers == 0
            || u64::from(c.checkpoint.height()) + u64::from(c.max_headers)
                < u64::from(g.assert_height)
            || [c.funding_depth, c.request_depth, c.publication_depth]
                .into_iter()
                .any(|depth| depth == 0 || depth > c.max_headers)
            || c.observation_margin
                .checked_add(c.request_depth)
                .is_none_or(|need| need > u32::from(g.response_window))
        {
            return Err(bad("native chain confirmation profile"));
        }
        let limits = &p.limits;
        if !(60..=4_000_000).contains(&limits.max_transaction_bytes)
            || limits.max_publication_bytes == 0
            || limits.max_publication_bytes > limits.max_transaction_bytes
            || limits.max_authorizations < 32
            || limits.max_source_inputs == 0
        {
            return Err(bad("native resource limits"));
        }
        let r = &p.recipe;
        if r.graph_version != 1
            || r.holder_program == [0; 32]
            || r.counter_program == [0; 32]
            || r.holder_program == r.counter_program
            || r.full_kept == 0
            || r.full_kept >= r.full_instances
            || r.corrected_kept == 0
            || r.corrected_kept >= r.corrected_instances
        {
            return Err(bad("native static recipe identity"));
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let p = &self.0;
        let mut bytes = BASE_TAG.to_vec();
        bytes.extend(p.nonce);
        bytes.extend(p.spend_root.0);
        bytes.extend(p.claim_key);
        put_blob(&mut bytes, &p.keyset.to_bytes());
        bytes.extend(p.deposit);
        bytes.extend(p.cap.to_be_bytes());
        bytes.extend(p.response_keys.to_bytes());
        for party in &p.parties {
            bytes.extend(party.funding);
            bytes.extend(party.payment);
            bytes.extend(party.delayed);
            put_blob(&mut bytes, &party.payout_script);
        }
        let g = &p.graph;
        bytes.extend(g.id.to_be_bytes());
        bytes.extend(g.unit_sats.to_be_bytes());
        bytes.extend([g.bits, g.low_bits]);
        bytes.extend(g.deposit_units.to_be_bytes());
        bytes.extend(g.close_height.to_be_bytes());
        for value in [g.reserve_alice, g.reserve_hub] {
            bytes.extend(value.to_be_bytes());
        }
        bytes.extend(g.to_self_delay.to_be_bytes());
        for value in [g.commit_fee, g.presign_fee, g.dust] {
            bytes.extend(value.to_be_bytes());
        }
        bytes.extend(g.delta.to_be_bytes());
        bytes.extend(g.delta_prime.to_be_bytes());
        bytes.extend(g.deadline_margin.to_be_bytes());
        bytes.extend(g.response_window.to_be_bytes());
        bytes.extend(g.assert_height.to_be_bytes());
        bytes.extend(g.hub_challenge.to_be_bytes());
        bytes.extend(g.holder_answer.to_be_bytes());
        let r = &p.recipe;
        bytes.extend(r.graph_version.to_be_bytes());
        bytes.push(2); // Corrected only
        bytes.extend(32u32.to_be_bytes()); // Actual item/unlock capacity, not a source-input cap.
        bytes.extend(r.holder_program);
        bytes.extend(r.counter_program);
        for count in [
            r.full_instances,
            r.full_kept,
            r.corrected_instances,
            r.corrected_kept,
        ] {
            bytes.extend(count.to_be_bytes());
        }
        let c = &p.chain;
        put_blob(&mut bytes, &c.checkpoint.to_bytes());
        for value in [
            c.funding_depth,
            c.request_depth,
            c.publication_depth,
            c.lookback,
            c.observation_margin,
            c.max_headers,
            p.limits.max_transaction_bytes,
            p.limits.max_publication_bytes,
            p.limits.max_authorizations,
            p.limits.max_source_inputs,
        ] {
            bytes.extend(value.to_be_bytes());
        }
        for value in [
            p.fees.sweep_fee_max,
            p.fees.external_fee_max,
            p.fees.max_rate_sat_per_kvb,
            p.fees.opening_fee_quote,
        ] {
            bytes.extend(value.to_be_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BASE_BYTES {
            return Err(bad("native Base byte limit"));
        }
        let mut reader = Reader(bytes);
        if reader.take(BASE_TAG.len())? != BASE_TAG {
            return Err(bad("native Base tag"));
        }
        let nonce = reader.array()?;
        let spend_root = Point(reader.array()?);
        let claim_key = reader.array()?;
        let keyset = Keyset::from_bytes(reader.blob(4_096)?)?;
        let deposit = reader.array()?;
        let cap = reader.u32()?;
        let response_keys = ResponseKeys::from_bytes(reader.take(RESPONSE_KEYS_BYTES)?)?;
        let mut parties = Vec::with_capacity(2);
        for _ in 0..2 {
            parties.push(Party {
                funding: reader.array()?,
                payment: reader.array()?,
                delayed: reader.array()?,
                payout_script: reader.blob(10_000)?.to_vec(),
            });
        }
        let graph = GraphTerms {
            id: reader.u32()?,
            unit_sats: reader.u64()?,
            bits: reader.array::<1>()?[0],
            low_bits: reader.array::<1>()?[0],
            deposit_units: reader.u32()?,
            close_height: reader.u32()?,
            reserve_alice: reader.u64()?,
            reserve_hub: reader.u64()?,
            to_self_delay: reader.u16()?,
            commit_fee: reader.u64()?,
            presign_fee: reader.u64()?,
            dust: reader.u64()?,
            delta: reader.u16()?,
            delta_prime: reader.u16()?,
            deadline_margin: reader.u32()?,
            response_window: reader.u16()?,
            assert_height: reader.u32()?,
            hub_challenge: reader.u16()?,
            holder_answer: reader.u16()?,
        };
        let graph_version = reader.u32()?;
        if reader.array::<1>()? != [2] || reader.u32()? != 32 {
            return Err(bad("native Corrected capacity recipe"));
        }
        let recipe = Recipe {
            graph_version,
            holder_program: reader.array()?,
            counter_program: reader.array()?,
            full_instances: reader.u16()?,
            full_kept: reader.u16()?,
            corrected_instances: reader.u16()?,
            corrected_kept: reader.u16()?,
        };
        let chain = ChainTerms {
            checkpoint: Checkpoint::from_bytes(reader.blob(256)?)?,
            funding_depth: reader.u32()?,
            request_depth: reader.u32()?,
            publication_depth: reader.u32()?,
            lookback: reader.u32()?,
            observation_margin: reader.u32()?,
            max_headers: reader.u32()?,
        };
        let limits = Limits {
            max_transaction_bytes: reader.u32()?,
            max_publication_bytes: reader.u32()?,
            max_authorizations: reader.u32()?,
            max_source_inputs: reader.u32()?,
        };
        let fees = Fees {
            sweep_fee_max: reader.u64()?,
            external_fee_max: reader.u64()?,
            max_rate_sat_per_kvb: reader.u64()?,
            opening_fee_quote: reader.u64()?,
        };
        if !reader.0.is_empty() {
            return Err(bad("trailing native Base bytes"));
        }
        Self::new(Parts {
            nonce,
            spend_root,
            claim_key,
            keyset,
            deposit,
            cap,
            response_keys,
            parties: parties.try_into().expect("two fixed parties"),
            graph,
            recipe,
            chain,
            limits,
            fees,
        })
    }

    pub fn digest(&self) -> [u8; 32] {
        crypto::tagged_hash("licash/base/v2", &[&self.to_bytes()])
    }

    /// Bind known Base terms to the actual prefix, without certifying its full
    /// tree or authorizing funding. Publication coin outpoints are deliberately
    /// excluded: their plain scripts depend on the Base-derived Alice E key.
    pub fn final_digest(&self, prefix: &FrozenPrefix) -> Result<FinalDigest, Error> {
        let digest = self.digest();
        if prefix.bound_base()? != &digest {
            return Err(bad("frozen prefix Base mismatch"));
        }
        let p = &self.0;
        if prefix.leaf().alice() != &p.parties[0].payment
            || prefix.leaf().hub() != &p.parties[1].payment
            || prefix.leaf().delay() != p.graph.to_self_delay
            || prefix.funding_graph().output.value != p.graph.graph_value()?
            || prefix
                .funding()
                .inputs()
                .iter()
                .filter(|input| input.previous == self.deposit_outpoint())
                .count()
                != 1
        {
            return Err(bad("native Base named-deposit or prefix terms mismatch"));
        }
        Ok(FinalDigest(crypto::tagged_hash(
            "licash/final-statement/v2",
            &[&digest, &prefix.commitment()],
        )))
    }

    pub fn owner_consent_message(&self, prefix: &FrozenPrefix) -> Result<[u8; 32], Error> {
        Ok(crypto::tagged_hash(
            "licash/authorize-opening/v2",
            &[self.final_digest(prefix)?.as_bytes()],
        ))
    }

    /// Owner agreement only. The funding gate must independently compare its
    /// own agreed Base, native/confirmed reserved coins, complete graph recipe,
    /// every real instance/presignature, fee resource and actual deadline.
    pub fn verify_owner_consent(
        &self,
        prefix: &FrozenPrefix,
        signature: &Signature,
    ) -> Result<(), Error> {
        crypto::schnorr_verify(
            &self.0.spend_root.x_only(),
            &self.owner_consent_message(prefix)?,
            signature,
        )
    }

    /// Checks shared by both funding gates. `expected_graph` is the F graph
    /// output script the caller compiled itself from this Base, the derived
    /// request keys and the actual instances, never the peer's assertion.
    fn check_opening(
        &self,
        prefix: &FrozenPrefix,
        expected_graph: &[u8],
        trusted: &Programs,
        tip_height: u32,
    ) -> Result<FinalDigest, Error> {
        let d = self.final_digest(prefix)?;
        let g = &self.0.graph;
        // A counter program that accepts anything would let the Hub void any
        // claim; a holder program that does would let the holder take the cap.
        if self.0.recipe.holder_program != trusted.holder
            || self.0.recipe.counter_program != trusted.counter
        {
            return Err(bad(
                "opening names proof programs this party does not trust",
            ));
        }
        if prefix.funding_graph().output.script != expected_graph {
            return Err(bad("funded graph differs from the locally compiled graph"));
        }
        let latest_request = [
            self.0.chain.funding_depth,
            u32::from(g.to_self_delay),
            self.0.chain.request_depth,
            u32::from(g.response_window),
            g.deadline_margin,
        ]
        .into_iter()
        .try_fold(tip_height, |sum, step| sum.checked_add(step));
        if latest_request.is_none_or(|height| height > g.assert_height) {
            return Err(bad(
                "opening cannot complete a recovery before its deadline",
            ));
        }
        Ok(d)
    }

    /// Holder funding gate: trusted proof programs, own request keys, own spending root, compiled
    /// graph and deadline, then consent. Release funding signatures only after
    /// this returns, and record the consent durably before releasing them.
    pub fn holder_gate(
        &self,
        prefix: &FrozenPrefix,
        expected_graph: &[u8],
        trusted: &Programs,
        seed: &[u8; 32],
        spend_secret: &k256::Scalar,
        tip_height: u32,
    ) -> Result<Signature, Error> {
        self.check_opening(prefix, expected_graph, trusted, tip_height)?;
        if prefix.leaf().keys() != &crate::request::Signer::derive(seed, &self.digest()).public() {
            return Err(bad("request keys are not the holder's own"));
        }
        if crypto::x_only_pub(spend_secret) != self.0.spend_root.x_only()
            || crypto::Point::from_projective(&crypto::generator_mul(spend_secret))
                != self.0.spend_root
        {
            return Err(bad("spending root is not the holder's"));
        }
        crypto::schnorr_sign(spend_secret, &self.owner_consent_message(prefix)?)
    }

    /// Hub funding gate: retained response keys, compiled graph, deadline and
    /// the holder's consent. The Hub signs F's inputs only after this.
    pub fn hub_gate(
        &self,
        prefix: &FrozenPrefix,
        expected_graph: &[u8],
        trusted: &Programs,
        retained: &ResponseKeys,
        consent: &Signature,
        tip_height: u32,
    ) -> Result<FinalDigest, Error> {
        let d = self.check_opening(prefix, expected_graph, trusted, tip_height)?;
        if retained != &self.0.response_keys {
            return Err(bad("response keys are not the Hub's retained keys"));
        }
        self.verify_owner_consent(prefix, consent)?;
        Ok(d)
    }
}

fn put_blob(bytes: &mut Vec<u8>, blob: &[u8]) {
    bytes.extend((blob.len() as u32).to_be_bytes());
    bytes.extend(blob);
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        if count > self.0.len() {
            return Err(bad("truncated native Base"));
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("fixed Base field"))
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn blob(&mut self, limit: usize) -> Result<&'a [u8], Error> {
        let count = self.u32()? as usize;
        if count > limit {
            return Err(bad("native Base component limit"));
        }
        self.take(count)
    }
}
