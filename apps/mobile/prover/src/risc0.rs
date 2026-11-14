//! The RISC Zero zkVM a job proves with, on the CPU.
use anyhow::{anyhow, bail, ensure, Context, Result};
use risc0_zkvm::{
    compute_image_id, get_prover_server, Digest, ExecutorEnv, ExecutorImpl, ExitCode, InnerReceipt,
    ProverOpts, Receipt, ReceiptClaim, Segment, SegmentRef, SuccinctReceipt, VerifierContext,
};

use crate::{Backend, Executed};

pub(crate) struct Risc0;

/// A segment handed over, written where only the job finds it.
struct Written;

impl SegmentRef for Written {
    fn resolve(&self) -> Result<Segment> {
        bail!("a segment is read back from its file")
    }
}

impl Backend for Risc0 {
    fn execute(
        &self,
        program: &[u8],
        input: &[u8],
        po2: u32,
        segment: &mut (dyn FnMut(Vec<u8>) -> Result<()> + Send),
    ) -> Result<Executed> {
        // As the Licash programs read their input: its length, then its
        // bytes in words.
        let words: Vec<u32> = input
            .chunks(4)
            .map(|c| {
                let mut w = [0u8; 4];
                w[..c.len()].copy_from_slice(c);
                u32::from_le_bytes(w)
            })
            .collect();
        let length = u32::try_from(input.len()).context("an input of 4 GiB or more")?;
        let mut env = ExecutorEnv::builder();
        env.segment_limit_po2(po2);
        env.write(&length)?.write_slice(&words);
        let mut executor = ExecutorImpl::from_elf(env.build()?, program)?;
        let session = executor.run_with_callback(|s| {
            segment(bincode::serialize(&s)?)?;
            Ok(Box::new(Written))
        })?;
        // Receipts it verified would have to be proven too.
        ensure!(
            session.assumptions.is_empty(),
            "the program verifies other receipts, which a job does not prove"
        );
        ensure!(
            session.exit_code == ExitCode::Halted(0),
            "the program exited with {:?}",
            session.exit_code
        );
        Ok(Executed {
            image_id: compute_image_id(program)?.as_bytes().to_vec(),
            journal: session.journal.map(|j| j.bytes).unwrap_or_default(),
            cycles: session.user_cycles,
        })
    }

    fn prove(&self, segment: &[u8]) -> Result<Vec<u8>> {
        let segment: Segment = bincode::deserialize(segment).context("reading a segment")?;
        let prover = get_prover_server(&ProverOpts::succinct())?;
        let receipt = prover.prove_segment(&VerifierContext::default(), &segment)?;
        drop(segment);
        Ok(bincode::serialize(&prover.lift(&receipt)?)?)
    }

    fn join(&self, left: &[u8], right: &[u8]) -> Result<Vec<u8>> {
        let left: SuccinctReceipt<ReceiptClaim> = bincode::deserialize(left)?;
        let right: SuccinctReceipt<ReceiptClaim> = bincode::deserialize(right)?;
        let prover = get_prover_server(&ProverOpts::succinct())?;
        Ok(bincode::serialize(&prover.join(&left, &right)?)?)
    }

    fn receipt(&self, image_id: &[u8], journal: &[u8], joined: &[u8]) -> Result<Vec<u8>> {
        let joined: SuccinctReceipt<ReceiptClaim> = bincode::deserialize(joined)?;
        let image_id =
            Digest::try_from(image_id).map_err(|_| anyhow!("an image id of 32 bytes"))?;
        let receipt = Receipt::new(InnerReceipt::Succinct(joined), journal.to_vec());
        receipt
            .verify(image_id)
            .context("the receipt proven does not verify")?;
        Ok(bincode::serialize(&receipt)?)
    }
}
