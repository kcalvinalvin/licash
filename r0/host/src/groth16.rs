//! RISC Zero's Groth16 wrap, run natively: its `stark_verify` witness
//! generator piped into its gnark `prover`, with the setup they ship in
//! `risczero/risc0-groth16-prover` (its `/app/prover.sh`, without Docker).
//! The files are in [`crate::probe::groth16_dir`]: x86-64 only. It takes
//! about 7.7 GB of memory, whichever the device.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use risc0_groth16::{ProofJson, Seal};

/// What the wrap needs in its directory: both programs, the witness
/// generator's data, the constraint system and the proving key.
pub const FILES: [&str; 5] = [
    "stark_verify",
    "stark_verify.dat",
    "prover",
    "stark_verify.cs",
    "stark_verify_final.pk.dmp",
];

/// The shell half of `prover.sh`: the witness generated, then proved. The
/// image streams it through a FIFO; a file in the work directory instead
/// keeps the two from holding memory at once, and leaves nothing blocked
/// when either fails. `$1` the setup's directory, `$2` the work directory.
const SCRIPT: &str = r#"set -eu
ulimit -s 16384 2>/dev/null || ulimit -s unlimited 2>/dev/null || true
cd "$1"
rm -f "$2/witness.wtns" "$2/proof.part"
./stark_verify "$2/input.json" "$2/witness.wtns"
./prover stark_verify.cs stark_verify_final.pk.dmp "$2/witness.wtns" "$2/proof.part"
rm -f "$2/witness.wtns"
mv "$2/proof.part" "$2/proof.json"
"#;

/// The Groth16 seal of an `identity_p254` receipt's `seal` bytes, made in
/// `work` (kept there, and reused when run again).
pub fn wrap(setup: &Path, seal: &[u8], work: &Path) -> Result<Seal> {
    std::fs::create_dir_all(work)?;
    let proof = work.join("proof.json");
    if !proof.exists() {
        for file in FILES {
            ensure!(
                setup.join(file).exists(),
                "no Groth16 setup at {}: {file} is missing",
                setup.display()
            );
        }
        let input = risc0_groth16::prove::to_json(seal)?;
        crate::resumable::keep(&work.join("input.json"), input.as_bytes())?;
        let t = Instant::now();
        let status = Command::new("sh")
            .arg("-c")
            .arg(SCRIPT)
            .arg("groth16")
            .arg(setup)
            .arg(std::path::absolute(work)?)
            .status()
            .context("run the Groth16 prover")?;
        if !status.success() {
            bail!("the Groth16 prover failed: {status}");
        }
        println!("groth16: wrapped in {:.1} s", t.elapsed().as_secs_f64());
    }
    let json: ProofJson = serde_json::from_slice(&std::fs::read(&proof)?)
        .with_context(|| format!("{}", proof.display()))?;
    json.try_into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake setup whose programs pass on what they were given: the
    /// generator's witness reaches the prover, and the proof is left only
    /// when both succeed.
    #[test]
    fn the_witness_reaches_the_prover_and_a_failure_leaves_no_proof() {
        let setup = tempfile::tempdir().unwrap();
        let s = setup.path();
        for file in &FILES[1..] {
            std::fs::write(s.join(file), b"").unwrap();
        }
        let script = |path: &Path, body: &str| {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(&s.join("stark_verify"), r#"cat "$1" > "$2""#);
        script(
            &s.join("prover"),
            r#"grep -q seal "$3" && echo '{"ok":1}' > "$4""#,
        );
        let work = tempfile::tempdir().unwrap();
        let w = work.path().join("groth16");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join("input.json"), b"seal").unwrap();
        let run = |w: &Path| {
            Command::new("sh")
                .arg("-c")
                .arg(SCRIPT)
                .arg("groth16")
                .arg(s)
                .arg(w)
                .status()
                .unwrap()
        };
        assert!(run(&w).success());
        assert_eq!(
            std::fs::read(w.join("proof.json")).unwrap(),
            b"{\"ok\":1}\n"
        );
        assert!(!w.join("witness.wtns").exists());

        // A failed generator fails the wrap, and leaves no proof.
        std::fs::remove_file(w.join("proof.json")).unwrap();
        script(&s.join("stark_verify"), "exit 1");
        assert!(!run(&w).success());
        assert!(!w.join("proof.json").exists());

        // So does a failed prover.
        script(&s.join("stark_verify"), r#"cat "$1" > "$2""#);
        script(&s.join("prover"), "exit 1");
        assert!(!run(&w).success());
        assert!(!w.join("proof.json").exists());
    }

    #[test]
    fn a_missing_setup_is_named() {
        let setup = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let e = wrap(setup.path(), &[0; 4], work.path()).unwrap_err();
        assert!(e.to_string().contains("stark_verify is missing"), "{e}");
    }
}
