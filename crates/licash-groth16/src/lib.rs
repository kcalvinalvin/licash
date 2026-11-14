//! A RISC Zero 3.1 Groth16 receipt's check over substrate-bn: whether a
//! proof's points prove that the guest `image_id` halted with exit code 0
//! and wrote `journal`. The holder guest checks the Hub's counter proof
//! with it, with RISC Zero's accelerated substrate-bn patched in.
//!
//! The points are compressed as ln-gap's native check reads them
//! (`lngap_zk::groth16`, BitVMX's verifier's input): 128 bytes, A, B, C,
//! each x little-endian with the top bit of its last byte set iff y is
//! odd; B as x.c0 then x.c1, the flag from y.c0. The verifying key, the
//! control root and id, and the claim's hashing are RISC Zero's
//! (risc0-groth16 3.0.6, risc0-zkvm 3.1.0; Apache-2.0), as there; this
//! crate's tests check that the two agree.

#![no_std]

use sha2::{Digest, Sha256};
use substrate_bn::{pairing_batch, AffineG1, AffineG2, Fq, Fq2, Fr, Gt, G1, G2};

/// The points' length: A and C 32 bytes, B 64.
pub const POINTS_LEN: usize = 128;

/// RISC Zero 3.1's recursion control root (`ALLOWED_CONTROL_ROOT`).
const CONTROL_ROOT: [u8; 32] = hex32("e6feef13db7aed4a731c7b0852de1f5c89b137726073f625d05333184d3a551d");
/// RISC Zero 3.1's BN254 identity control id.
const BN254_CONTROL_ID: [u8; 32] = hex32("de81d091ba14062acb5023b3b36deee95624e3fd47475cb7ff64aed5f9eac20a");

/// The verifying key (risc0-groth16 3.0.6), decimal; G2 coordinates as
/// `(c1, c0)`.
const ALPHA: [&str; 2] = ["16428432848801857252194528405604668803277877773566238944394625302971855135431", "16846502678714586896801519656441059708016666274385668027902869494772365009666"];
const BETA: [&str; 4] = [
    "3182164110458002340215786955198810119980427837186618912744689678939861918171",
    "16348171800823588416173124589066524623406261996681292662100840445103873053252",
    "4920802715848186258981584729175884379674325733638798907835771393452862684714",
    "19687132236965066906216944365591810874384658708175106803089633851114028275753",
];
const GAMMA: [&str; 4] = [
    "11559732032986387107991004021392285783925812861821192530917403151452391805634",
    "10857046999023057135944570762232829481370756359578518086990519993285655852781",
    "4082367875863433681332203403145435568316851327593401208105741076214120093531",
    "8495653923123431417604973247489272438418190587263600148770280649306958101930",
];
const DELTA: [&str; 4] = [
    "15616655295497688016920922285786248029410410634174255119755032442279507261612",
    "15776672394616990683767400715354533957990327248900631669473166016258279232588",
    "15603769682755083227316504195454390494226357053638046843688553601570183264395",
    "6666730711543397578682474175129345128534747409410156889826997147291122518312",
];
const IC: [[&str; 2]; 6] = [
    ["18663918579753010382002747204100316475180663666125247285366810668134508578264", "17189072794432247265173990289507014998311702955803346313941168113828133843434"],
    ["10995657406716404592042449088731908804822551193011116024160605390904112699922", "16840054500543522560166231854936072122401577171365452702445604851845250658027"],
    ["17223746139751910493708334502747005647365937123243899090309734281916083451762", "4092210736358501801028887881307062309243791997444282548134854129284894411044"],
    ["3765213192101990618643617040373059738050185238026301923348610794767426428033", "12210223264334159465299028585581460715942724522871187397965100355295734194591"],
    ["10211980968769599651826920300372241897389214017084266564307091587192484525770", "6627619408770540710319105693121261544533216204749218194792386277379533604252"],
    ["13454640350766589059799184485082639692652215534373479852236969801352010275184", "8731676129773990833613417922371950977268622282768299686490610454524284784606"],
];

const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => c - b'a' + 10,
    }
}

const fn hex32(s: &str) -> [u8; 32] {
    let s = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nib(s[2 * i]) << 4 | nib(s[2 * i + 1]);
        i += 1;
    }
    out
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// RISC Zero's tagged struct hash (`risc0_binfmt::tagged_struct`), for at
/// most four digests and two words.
fn tagged(tag: &str, down: &[[u8; 32]], data: &[u32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(sha256(&[tag.as_bytes()]));
    for d in down {
        h.update(d);
    }
    for w in data {
        h.update(w.to_le_bytes());
    }
    h.update((down.len() as u16).to_le_bytes());
    h.finalize().into()
}

/// The digest of RISC Zero's receipt claim for a guest that halted with
/// exit code 0 (`ReceiptClaim::ok(image_id, journal)`).
pub fn claim_digest(image_id: &[u8; 32], journal: &[u8]) -> [u8; 32] {
    let zero = [0u8; 32];
    let post = tagged("risc0.SystemState", &[zero], &[0]);
    let output = tagged("risc0.Output", &[sha256(&[journal]), zero], &[]);
    tagged("risc0.ReceiptClaim", &[zero, *image_id, post, output], &[0, 0])
}

fn dec(s: &str) -> Fq {
    Fq::from_str(s).expect("a field element")
}

fn g1_const(c: [&str; 2]) -> G1 {
    AffineG1::new(dec(c[0]), dec(c[1])).expect("a G1 point").into()
}

fn g2_const(c: [&str; 4]) -> G2 {
    AffineG2::new(Fq2::new(dec(c[1]), dec(c[0])), Fq2::new(dec(c[3]), dec(c[2]))).expect("a G2 point").into()
}

/// A scalar from little-endian bytes (at most 32), below the order.
fn fr_le(b: &[u8]) -> Option<Fr> {
    let mut be = [0u8; 32];
    for (i, x) in b.iter().enumerate() {
        be[31 - i] = *x;
    }
    Fr::from_slice(&be).ok()
}

/// A base field element from 32 little-endian bytes, below the modulus.
fn fq_le(b: &[u8]) -> Option<Fq> {
    let mut be = [0u8; 32];
    for (i, x) in b.iter().enumerate() {
        be[31 - i] = *x;
    }
    Fq::from_slice(&be).ok()
}

fn odd(y: Fq) -> bool {
    y.into_u256().get_bit(0).expect("bit 0")
}

/// The x of a compressed point (32 little-endian bytes) and its flag.
fn x_and_flag(b: &[u8]) -> Option<(Fq, bool)> {
    let mut x = [0u8; 32];
    x.copy_from_slice(b);
    let flag = x[31] & 0x80 != 0;
    x[31] &= 0x7f;
    Some((fq_le(&x)?, flag))
}

fn g1(b: &[u8]) -> Option<G1> {
    let (x, flag) = x_and_flag(b)?;
    let y = (x * x * x + G1::b()).sqrt()?;
    AffineG1::new(x, if odd(y) == flag { y } else { -y }).ok().map(Into::into)
}

fn g2(b: &[u8]) -> Option<G2> {
    let (c1, flag) = x_and_flag(&b[32..])?;
    let x = Fq2::new(fq_le(&b[..32])?, c1);
    let y = (x * x * x + G2::b()).sqrt()?;
    AffineG2::new(x, if odd(y.real()) == flag { y } else { -y }).ok().map(Into::into)
}

/// The proof's public inputs: the control root's halves, the claim's
/// halves (each half a little-endian integer), the BN254 control id.
fn inputs(claim: &[u8; 32]) -> Option<[Fr; 5]> {
    Some([fr_le(&CONTROL_ROOT[..16])?, fr_le(&CONTROL_ROOT[16..])?, fr_le(&claim[..16])?, fr_le(&claim[16..])?, fr_le(&BN254_CONTROL_ID)?])
}

/// Whether `points` (A, B, C compressed: [`POINTS_LEN`] bytes) prove that
/// the guest `image_id` halted with exit code 0 and wrote `journal`.
/// Points that don't decode (off the curve or its subgroup, a coordinate
/// beyond the field) don't.
pub fn verify(image_id: &[u8; 32], points: &[u8], journal: &[u8]) -> bool {
    let run = || -> Option<bool> {
        if points.len() != POINTS_LEN {
            return None;
        }
        let (a, b, c) = (g1(&points[..32])?, g2(&points[32..96])?, g1(&points[96..])?);
        let ic = IC.map(g1_const);
        let mut vk_x = ic[0];
        for (x, base) in inputs(&claim_digest(image_id, journal))?.iter().zip(&ic[1..]) {
            vk_x = vk_x + *base * *x;
        }
        // e(A, B) = e(alpha, beta) e(vk_x, gamma) e(C, delta)
        let pairs = [(a, b), (-g1_const(ALPHA), g2_const(BETA)), (-vk_x, g2_const(GAMMA)), (-c, g2_const(DELTA))];
        Some(pairing_batch(&pairs) == Gt::one())
    };
    run().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn decode(hexes: &[&str]) -> Vec<Vec<u8>> {
        hexes.iter().map(|h| hex::decode(h.trim()).unwrap()).collect()
    }

    /// Real RISC Zero 3.1 receipts as BitVMX's verifier takes them: ln-gap's
    /// withdrawal guest's on (9, 42) (its `withdraw-9-42-deployment.hex`),
    /// and a Licash counter guest's (image `362f03e3...`) on a regtest
    /// stale-backup counter, the seal the holder guest checks.
    fn inputs() -> Vec<Vec<u8>> {
        decode(&[include_str!("../tests/data/withdraw-9-42.hex"), include_str!("../tests/data/counter-stale.hex")])
    }

    /// Lichen's BABE guest's RISC Zero 3.0 receipts on a return of 60 with
    /// memo 42: 3.1's control root and key refuse them, as ln-gap's do.
    #[test]
    fn refuses_risc_zero_3_0_receipts() {
        for input in decode(&[include_str!("../tests/data/babe-claim-60-42.hex"), include_str!("../tests/data/babe-60-42.hex")]) {
            let (id, points, journal) = lngap_zk::groth16::split_input(&input).unwrap();
            assert!(!verify(&id, points, journal));
            assert!(lngap_zk::groth16::verify(&id, points, journal).is_err());
        }
    }

    #[test]
    fn agrees_with_ln_gaps_check() {
        for input in inputs() {
            let (id, points, journal) = lngap_zk::groth16::split_input(&input).unwrap();
            assert_eq!(claim_digest(&id, journal), lngap_zk::groth16::claim_digest(&id, journal));
            assert!(lngap_zk::groth16::verify(&id, points, journal).is_ok());
            assert!(verify(&id, points, journal));
            // each point, its flag, the image id, the journal: refused by
            // both
            let flip = |at: usize, x: u8| {
                let mut p = points.to_vec();
                p[at] ^= x;
                p
            };
            for (what, p) in [("A", flip(0, 1)), ("A's flag", flip(31, 0x80)), ("B.c0", flip(32, 1)), ("B's flag", flip(95, 0x80)), ("C", flip(96, 1)), ("C's flag", flip(127, 0x80))] {
                assert!(!verify(&id, &p, journal), "{what}");
                assert!(lngap_zk::groth16::verify(&id, &p, journal).is_err(), "{what}, ln-gap's");
            }
            let mut other = id;
            other[0] ^= 1;
            assert!(!verify(&other, points, journal));
            let mut j = journal.to_vec();
            j[0] ^= 1;
            assert!(!verify(&id, points, &j));
            assert!(!verify(&id, &points[..127], journal) && !verify(&id, &[0xff; 128], journal));
        }
    }
}
