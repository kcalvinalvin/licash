//! One-time answers shared by the daemon and the operator. The request is
//! committed before signing; a restart can only sign that same witness.
//! With it, the evidence the hub decided her claim on: its response, its
//! disclosure and its proof all cite that evidence, whatever the ledger
//! learns after.

use std::path::Path;

use anyhow::{ensure, Context, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::Transaction;
use licash_store::Kind;
use rusqlite::{params, Connection, OptionalExtension};

/// The component the pins are, and its version.
const COMPONENT: &str = "responses";
const VERSION: i64 = 1;

/// Per opening, or per assertion's tag: the request committed before
/// signing, the evidence its decision was made on, and once signed its
/// response. A pin is never rewritten: what is pinned first is what the
/// hub answers on.
const SCHEMA: &str = "
CREATE TABLE response_pins (
    id BLOB PRIMARY KEY,
    request BLOB NOT NULL,
    response BLOB,
    evidence BLOB
) STRICT;
";

pub(super) struct Responses(Connection);

impl Responses {
    /// The pins in the hub's data directory `data`, made if none were.
    pub fn open(data: &Path) -> Result<Self> {
        let mut c = licash_store::open(&crate::hub_db(data), Kind::Hub)?;
        licash_store::component(&mut c, COMPONENT, VERSION, SCHEMA)?;
        Ok(Self(c))
    }

    /// The evidence `evidence` (its bytes) pinned for `request` of opening
    /// `id`, unless some was pinned first: what is pinned. `request` must
    /// be validated before this call.
    pub fn pin(&self, id: &[u8; 32], request: &Transaction, evidence: &[u8]) -> Result<Vec<u8>> {
        self.0.execute(
            "INSERT INTO response_pins (id, request, evidence) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
            params![id, serialize(request), evidence],
        )?;
        let (pinned, evidence): (Vec<u8>, Option<Vec<u8>>) = self.0.query_row(
            "SELECT request, evidence FROM response_pins WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let pinned: Transaction = deserialize(&pinned).context("pinned request")?;
        ensure!(
            pinned.compute_wtxid() == request.compute_wtxid(),
            "request witness differs from the one-time signing pin; refusing to sign again"
        );
        Ok(evidence.unwrap_or_default())
    }

    /// The evidence pinned for opening `id`, if any was: empty bytes for
    /// none (a response of version 1).
    pub fn evidence(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let row: Option<Option<Vec<u8>>> = self
            .0
            .query_row(
                "SELECT evidence FROM response_pins WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(Option::unwrap_or_default))
    }

    /// The answer signed for `id`, if one was.
    pub fn response(&self, id: &[u8; 32]) -> Result<Option<Transaction>> {
        let row: Option<Option<Vec<u8>>> = self
            .0
            .query_row(
                "SELECT response FROM response_pins WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        row.flatten()
            .map(|b| deserialize(&b).context("pinned response"))
            .transpose()
    }

    /// The proof `proof` (its bytes) pinned for the assertion `id` of
    /// `request`, in its evidence, unless one was pinned first or its
    /// answer was signed before proofs were pinned: what is pinned, none
    /// then. `request` must be validated before this call.
    pub fn pin_proof(
        &self,
        id: &[u8; 32],
        request: &Transaction,
        proof: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.0.execute(
            "INSERT INTO response_pins (id, request) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![id, serialize(request)],
        )?;
        self.0.execute(
            "UPDATE response_pins SET evidence = ?2
             WHERE id = ?1 AND evidence IS NULL AND response IS NULL",
            params![id, proof],
        )?;
        let (pinned, proof): (Vec<u8>, Option<Vec<u8>>) = self.0.query_row(
            "SELECT request, evidence FROM response_pins WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let pinned: Transaction = deserialize(&pinned).context("pinned request")?;
        ensure!(
            pinned.compute_wtxid() == request.compute_wtxid(),
            "request witness differs from the one-time signing pin; refusing to sign again"
        );
        Ok(proof)
    }

    /// `request` must be validated before this call. Its witness, not its
    /// txid alone, binds the one-time signatures. No signing in a SQL
    /// transaction: the pin must survive even a stopped signer.
    pub fn answer(
        &self,
        id: &[u8; 32],
        request: &Transaction,
        sign: impl FnOnce() -> Result<Transaction>,
    ) -> Result<Transaction> {
        self.0.execute(
            "INSERT INTO response_pins (id, request) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![id, serialize(request)],
        )?;
        let (pinned, response): (Vec<u8>, Option<Vec<u8>>) = self.0.query_row(
            "SELECT request, response FROM response_pins WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let pinned: Transaction = deserialize(&pinned).context("pinned request")?;
        ensure!(
            pinned.compute_wtxid() == request.compute_wtxid(),
            "request witness differs from the one-time signing pin; refusing to sign again"
        );
        if let Some(response) = response {
            return deserialize(&response).context("pinned response");
        }
        let response = sign()?;
        self.0.execute(
            "UPDATE response_pins SET response = ?2 WHERE id = ?1 AND response IS NULL",
            params![id, serialize(&response)],
        )?;
        let kept: Vec<u8> = self.0.query_row(
            "SELECT response FROM response_pins WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            kept == serialize(&response),
            "a different response was pinned"
        );
        Ok(response)
    }

    #[cfg(test)]
    fn contains(&self, id: &[u8; 32]) -> Result<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM response_pins WHERE id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{absolute, transaction, TxIn, Witness};

    fn request(byte: u8) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                witness: Witness::from_slice(&[&[byte]]),
                ..Default::default()
            }],
            output: vec![],
        }
    }

    #[test]
    fn a_restart_returns_the_pinned_answer_without_signing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        let r = request(1);
        let answer = request(2);
        assert!(!pins.contains(&[1; 32])?);
        assert_eq!(pins.answer(&[1; 32], &r, || Ok(answer.clone()))?, answer);
        drop(pins);
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        assert_eq!(
            pins.answer(&[1; 32], &r, || panic!("signed twice"))?,
            answer
        );
        Ok(())
    }

    #[test]
    fn the_pin_survives_a_failed_signer_and_rejects_another_witness() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        let r = request(1);
        let other = request(2);
        assert_eq!(r.compute_txid(), other.compute_txid());
        assert!(pins
            .answer(&[1; 32], &r, || anyhow::bail!("stopped"))
            .is_err());
        drop(pins);
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        let err = pins
            .answer(&[1; 32], &other, || panic!("signed another claim"))
            .unwrap_err();
        assert!(err.to_string().contains("witness differs"));
        assert_eq!(pins.answer(&[1; 32], &r, || Ok(other.clone()))?, other);
        Ok(())
    }

    #[test]
    fn another_connection_obeys_the_same_pin_and_another_opening_is_independent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let a = Responses::open(&crate::testing::hub_data(dir.path()))?;
        let b = Responses::open(&crate::testing::hub_data(dir.path()))?;
        a.answer(&[1; 32], &request(1), || Ok(request(3)))?;
        assert!(b
            .answer(&[1; 32], &request(2), || panic!("signed twice"))
            .is_err());
        assert_eq!(
            b.answer(&[2; 32], &request(2), || Ok(request(4)))?,
            request(4)
        );
        Ok(())
    }

    #[test]
    fn a_newer_pin_format_is_refused() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let data = crate::testing::hub_data(dir.path());
        drop(Responses::open(&data)?);
        let c = Connection::open(crate::hub_db(&data))?;
        c.execute(
            "UPDATE schema_versions SET version = 3 WHERE component = 'responses'",
            [],
        )?;
        drop(c);
        assert!(Responses::open(&data)
            .err()
            .unwrap()
            .to_string()
            .contains("version 3"));
        Ok(())
    }

    /// Evidence is pinned once, with the request.
    #[test]
    fn evidence_is_pinned_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        assert_eq!(pins.evidence(&[1; 32])?, None);
        assert_eq!(pins.pin(&[1; 32], &request(1), b"first")?, b"first");
        assert_eq!(pins.pin(&[1; 32], &request(1), b"later")?, b"first");
        assert!(pins.pin(&[1; 32], &request(9), b"first").is_err());
        assert_eq!(pins.evidence(&[1; 32])?, Some(b"first".to_vec()));
        Ok(())
    }

    /// An assertion's proof is pinned once, before it is signed; one signed
    /// before proofs were pinned (a request alone pinned, then its answer)
    /// keeps its answer and pins none.
    #[test]
    fn a_proof_is_pinned_once_and_not_after_its_answer() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(&crate::testing::hub_data(dir.path()))?;
        assert_eq!(pins.response(&[1; 32])?, None);
        assert_eq!(
            pins.pin_proof(&[1; 32], &request(1), b"first")?,
            Some(b"first".to_vec())
        );
        assert_eq!(
            pins.pin_proof(&[1; 32], &request(1), b"later")?,
            Some(b"first".to_vec())
        );
        assert!(pins.pin_proof(&[1; 32], &request(9), b"first").is_err());
        assert_eq!(
            pins.answer(&[1; 32], &request(1), || Ok(request(3)))?,
            request(3)
        );
        assert_eq!(pins.response(&[1; 32])?, Some(request(3)));

        pins.answer(&[2; 32], &request(1), || Ok(request(4)))?;
        assert_eq!(pins.pin_proof(&[2; 32], &request(1), b"late")?, None);
        assert_eq!(pins.response(&[2; 32])?, Some(request(4)));
        Ok(())
    }
}
