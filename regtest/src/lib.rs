//! licash's recovery on Lichen's BABE graph, on a local regtest. `graph`
//! builds the transaction graph, `contract` plays it, `native` opens a
//! contract from licash's Base and statement, `native_fixture` sets up the
//! parties for tests, `htlc` and `lnd` are the interoperability ends. Proofs
//! are mocked here (`mock_proof`); `r0/` runs and proves the real guests.

pub mod contract;
pub mod graph;
pub mod htlc;
pub mod lnd;
pub mod native;
pub mod native_fixture;

use crate::graph::ExitKeys;
use anyhow::{Context, Result};
use ark_bn254::Bn254;
use ark_groth16::Proof;
use licash::statement::{HubKeys, Journal};
use licash::wots::Hashes;
use lichen_babe::statement::{mock_prove, Kind};
use lngap_lamport::winternitz::{Wots256Public, Wots256Sig};

use contract::Statement;

/// Decode proof points in the holder graph's encoding.
/// This is Lichen d01bb5b's removed `risc0::proof`, unchanged.
pub fn proof_from_points(bytes: &[u8]) -> Result<Proof<Bn254>> {
    let p = lngap_zk::groth16::decode(bytes)?;
    Ok(Proof {
        a: p.a,
        b: p.b,
        c: p.c,
    })
}

/// The hub's `respond` keys as the statement reads them: one hash per
/// digit, checksum digits included (the same chains, `licash::wots`
/// cross-checked against Lichen's encoding).
pub fn hub_keys(
    x: &ExitKeys,
    hub_b: &Wots256Public,
    hub_b_corr: &Wots256Public,
    disclosure: [Hashes; 5],
) -> HubKeys {
    let h = |k: &Wots256Public| Hashes(k.digits.clone());
    HubKeys {
        b: h(hub_b),
        e_plain: h(&x.hub_e_plain),
        e_counter: h(&x.hub_e_counter),
        k: h(&x.hub_k),
        b_corr: h(hub_b_corr),
        disclosure,
    }
}

/// A one-time signature as the statement reads it.
pub fn sig(s: &Wots256Sig) -> Hashes {
    Hashes(s.hashes.clone())
}

/// The statement BABE encrypts under, with the mock proof system: Lichen's
/// `Kind::Mock` key and public inputs of the journal.
pub fn mock_statement(journal: &Journal) -> Statement {
    let bytes = journal.to_bytes();
    Statement {
        vk: Kind::Mock.verifying_key(),
        public: Kind::Mock.public_inputs(&bytes),
    }
}

/// A mock proof of `journal`: the user has one when her statement runs
/// (`StatementInput::run` is `Ok`); the regtest hub, like Lichen's with the
/// mock, judges the claim natively instead of by the proof.
pub fn mock_proof(journal: &Journal) -> Proof<Bn254> {
    mock_prove(&journal.to_bytes())
}

/// Whether `proof` is of `journal` under the mock statement.
pub fn mock_verify(journal: &Journal, proof: &Proof<Bn254>) -> bool {
    Kind::Mock.verify(&journal.to_bytes(), proof)
}

fn mock_counter_seal(public: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(public).repeat(4)
}

/// Native counter `K` under the same insecure mock seal (fast tests only).
pub fn native_mock_counter(journal: &licash::recovery::CounterJournal) -> Result<Vec<u8>> {
    Ok(licash::statement::counter_k(
        journal,
        &mock_counter_seal(&journal.to_bytes()),
    )?)
}

pub fn native_mock_counter_verify(seal: &[u8], public: &[u8]) -> bool {
    public.len() == licash::recovery::CounterJournal::LEN && seal == mock_counter_seal(public)
}

/// Import a real receipt export only against the caller's independently
/// known image and journal. An artifact never chooses its own authority.
pub fn verified_points(data: &[u8], image: &[u8; 32], public: &[u8]) -> Result<Vec<u8>> {
    let data: serde_json::Value = serde_json::from_slice(data)?;
    let field = |name: &str| {
        data[name]
            .as_str()
            .with_context(|| format!("missing {name}"))
    };
    anyhow::ensure!(hex::decode(field("image_id")?)? == image, "wrong image");
    anyhow::ensure!(hex::decode(field("journal")?)? == public, "wrong journal");
    let points = hex::decode(field("proof")?)?;
    anyhow::ensure!(
        licash_groth16::verify(image, &points, public),
        "invalid proof"
    );
    Ok(points)
}
