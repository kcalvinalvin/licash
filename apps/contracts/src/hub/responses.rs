//! One-time answers shared by the daemon and the operator. The request is
//! committed before signing; a restart can only sign that same witness.
//! With it, the evidence the hub decided her claim on: its response, its
//! disclosure and its proof all cite that evidence, whatever the ledger
//! learns after.

use std::path::Path;

use anyhow::{ensure, Context, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::Transaction;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub(super) struct Responses(Connection);

impl Responses {
    pub fn open(data: &Path) -> Result<Self> {
        let mut c = crate::private_db(&data.join("responses.sqlite"))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                tx.execute_batch(
                    "CREATE TABLE responses (
                        id BLOB PRIMARY KEY,
                        request BLOB NOT NULL,
                        response BLOB,
                        evidence BLOB
                    ) STRICT;",
                )?;
                tx.pragma_update(None, "user_version", 2)?;
            }
            // Version 1 pinned no evidence: its responses were all decided
            // on none.
            1 => {
                tx.execute_batch("ALTER TABLE responses ADD COLUMN evidence BLOB;")?;
                tx.pragma_update(None, "user_version", 2)?;
            }
            2 => {}
            v => anyhow::bail!("responses.sqlite is of version {v}, unknown"),
        }
        tx.commit()?;
        Ok(Self(c))
    }

    /// The evidence `evidence` (its bytes) pinned for `request` of opening
    /// `id`, unless some was pinned first: what is pinned. `request` must
    /// be validated before this call.
    pub fn pin(&self, id: &[u8; 32], request: &Transaction, evidence: &[u8]) -> Result<Vec<u8>> {
        self.0.execute(
            "INSERT INTO responses (id, request, evidence) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
            params![id, serialize(request), evidence],
        )?;
        let (pinned, evidence): (Vec<u8>, Option<Vec<u8>>) = self.0.query_row(
            "SELECT request, evidence FROM responses WHERE id = ?1",
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
            .query_row("SELECT evidence FROM responses WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(row.map(Option::unwrap_or_default))
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
            "INSERT INTO responses (id, request) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![id, serialize(request)],
        )?;
        let (pinned, response): (Vec<u8>, Option<Vec<u8>>) = self.0.query_row(
            "SELECT request, response FROM responses WHERE id = ?1",
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
            "UPDATE responses SET response = ?2 WHERE id = ?1 AND response IS NULL",
            params![id, serialize(&response)],
        )?;
        let kept: Vec<u8> =
            self.0
                .query_row("SELECT response FROM responses WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
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
            .query_row("SELECT 1 FROM responses WHERE id = ?1", [id], |_| Ok(()))
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
        let pins = Responses::open(dir.path())?;
        let r = request(1);
        let answer = request(2);
        assert!(!pins.contains(&[1; 32])?);
        assert_eq!(pins.answer(&[1; 32], &r, || Ok(answer.clone()))?, answer);
        drop(pins);
        let pins = Responses::open(dir.path())?;
        assert_eq!(
            pins.answer(&[1; 32], &r, || panic!("signed twice"))?,
            answer
        );
        Ok(())
    }

    #[test]
    fn the_pin_survives_a_failed_signer_and_rejects_another_witness() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(dir.path())?;
        let r = request(1);
        let other = request(2);
        assert_eq!(r.compute_txid(), other.compute_txid());
        assert!(pins
            .answer(&[1; 32], &r, || anyhow::bail!("stopped"))
            .is_err());
        drop(pins);
        let pins = Responses::open(dir.path())?;
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
        let a = Responses::open(dir.path())?;
        let b = Responses::open(dir.path())?;
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
        let pins = Responses::open(dir.path())?;
        pins.0.pragma_update(None, "user_version", 3)?;
        drop(pins);
        assert!(Responses::open(dir.path())
            .err()
            .unwrap()
            .to_string()
            .contains("version 3"));
        Ok(())
    }

    /// Evidence is pinned once, with the request; a response of version 1
    /// reads back as decided on none, its answer kept.
    #[test]
    fn evidence_is_pinned_once_and_a_version_1_response_had_none() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pins = Responses::open(dir.path())?;
        assert_eq!(pins.evidence(&[1; 32])?, None);
        assert_eq!(pins.pin(&[1; 32], &request(1), b"first")?, b"first");
        assert_eq!(pins.pin(&[1; 32], &request(1), b"later")?, b"first");
        assert!(pins.pin(&[1; 32], &request(9), b"first").is_err());
        assert_eq!(pins.evidence(&[1; 32])?, Some(b"first".to_vec()));
        drop(pins);

        let old = tempfile::tempdir()?;
        let c = Connection::open(old.path().join("responses.sqlite"))?;
        c.execute_batch(
            "CREATE TABLE responses (
                id BLOB PRIMARY KEY,
                request BLOB NOT NULL,
                response BLOB
            ) STRICT;
            PRAGMA user_version = 1;",
        )?;
        c.execute(
            "INSERT INTO responses (id, request, response) VALUES (?1, ?2, ?3)",
            params![[2u8; 32], serialize(&request(1)), serialize(&request(2))],
        )?;
        drop(c);
        let pins = Responses::open(old.path())?;
        assert_eq!(pins.evidence(&[2; 32])?, Some(vec![]));
        assert_eq!(
            pins.answer(&[2; 32], &request(1), || panic!("signed again"))?,
            request(2)
        );
        Ok(())
    }
}
