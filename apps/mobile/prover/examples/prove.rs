//! Prove a program with a job, resuming the one in `dir` if there: each
//! segment's time and the peak memory, then the receipt in
//! `dir/receipt.bin`. For measuring a phone over adb.
//!
//!   prove <program.bin> <input.bin> <po2> <dir>
use std::{fs, path::Path, time::Instant};

use anyhow::{bail, Result};
use licash_prover::Job;

/// The peak resident memory of this process, in MB, where Linux says.
fn peak() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb / 1024)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [program, input, po2, dir] = args.as_slice() else {
        bail!("usage: prove <program.bin> <input.bin> <po2> <dir>");
    };
    let dir = Path::new(dir);
    fs::create_dir_all(dir)?;
    let t = Instant::now();
    let mut job = match Job::open(dir)? {
        Some(job) => job,
        None => {
            let (program, input) = (fs::read(program)?, fs::read(input)?);
            let job = Job::start(dir, &program, &input, po2.parse()?)?;
            println!(
                "executed in {} ms, peak {} MB",
                t.elapsed().as_millis(),
                peak()
            );
            job
        }
    };
    let status = job.status();
    println!(
        "{} user cycles, {} segments, {} proven",
        status.cycles, status.segments, status.proven
    );
    while job.status().proven < status.segments {
        let t = Instant::now();
        job.step()?;
        let s = job.status();
        let ms = t.elapsed().as_millis();
        println!(
            "segment {} of {}: {ms} ms, peak {} MB",
            s.proven,
            s.segments,
            peak()
        );
    }
    let receipt = job.receipt()?;
    fs::write(dir.join("receipt.bin"), &receipt)?;
    println!(
        "verified succinct receipt of image {}, {} bytes, in {}",
        job.image_id(),
        receipt.len(),
        dir.join("receipt.bin").display()
    );
    Ok(())
}
