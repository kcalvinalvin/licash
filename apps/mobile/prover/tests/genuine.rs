//! A genuine proof by a job, cut off halfway and resumed: hours and GBs,
//! so run explicitly, on a program and input built locally:
//!
//!   LICASH_PROVER_PROGRAM=r0/guests/native-claim.bin \
//!   LICASH_PROVER_INPUT=tmp/inputs/native-honest-holder-c0.bin \
//!   cargo test -p licash-prover --release --test genuine -- --ignored
//!
//! `LICASH_PROVER_PO2` sets the segment size, 18 if unset.
use licash_prover::Job;

#[test]
#[ignore = "hours of proving: run explicitly"]
fn a_job_cut_off_halfway_proves_a_receipt_that_verifies() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} unset"));
    let program = std::fs::read(var("LICASH_PROVER_PROGRAM")).unwrap();
    let input = std::fs::read(var("LICASH_PROVER_INPUT")).unwrap();
    let po2 = std::env::var("LICASH_PROVER_PO2").map_or(18, |p| p.parse().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let mut job = Job::start(dir.path(), &program, &input, po2).unwrap();
    let segments = job.status().segments;
    while job.status().proven < segments / 2 {
        job.step().unwrap();
    }
    let journal = job.journal();
    drop(job);
    let mut job = Job::open(dir.path()).unwrap().expect("the job");
    assert_eq!(job.status().proven, segments / 2);
    while job.status().proven < segments {
        job.step().unwrap();
    }
    // Of the program given, saying what executing it said.
    let bytes = job.receipt().unwrap();
    let receipt: risc0_zkvm::Receipt = bincode::deserialize(&bytes).unwrap();
    let image_id = risc0_zkvm::compute_image_id(&program).unwrap();
    receipt.verify(image_id).unwrap();
    assert_eq!(receipt.journal.bytes, journal);
    assert!(receipt.inner.succinct().is_ok());
    eprintln!(
        "{segments} segments of 2^{po2} cycles: a receipt of {} bytes",
        bytes.len()
    );
}
