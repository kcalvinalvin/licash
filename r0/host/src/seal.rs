//! RISC Zero's seal in ln-gap/BitVMX's compressed-point convention.
//! This is the encoder from Lichen's r0/host/src/bitvmx.rs. In particular,
//! its parity bit is not arkworks' compressed-point sign convention.

use anyhow::{ensure, Result};

pub fn compress(seal: &[u8]) -> Result<Vec<u8>> {
    ensure!(seal.len() == 256, "a Groth16 seal must be 256 bytes");
    let w = |i: usize| -> [u8; 32] { seal[32 * i..32 * (i + 1)].try_into().unwrap() };
    let odd = |be: [u8; 32]| be[31] & 1 == 1;
    let le = |mut x: [u8; 32], flag: bool| -> Vec<u8> {
        if flag {
            x[0] |= 0x80;
        }
        x.reverse();
        x.to_vec()
    };
    let mut out = le(w(0), odd(w(1)));
    out.extend(le(w(3), false));
    out.extend(le(w(2), odd(w(5))));
    out.extend(le(w(6), odd(w(7))));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_checked() {
        for n in [0, 128, 255, 257] {
            assert!(compress(&vec![0; n]).is_err());
        }
    }

    #[test]
    fn coordinate_order_endianness_and_parity() {
        let mut input = [0u8; 256];
        for i in 0..8 {
            input[i * 32 + 31] = i as u8 + 1;
        }
        let out = compress(&input).unwrap();
        assert_eq!(out.len(), 128);
        assert_eq!([out[0], out[32], out[64], out[96]], [1, 4, 3, 7]);
        assert_eq!([out[31], out[63], out[95], out[127]], [0, 0, 0, 0]);
        input[63] = 3;
        input[191] = 5;
        input[255] = 7;
        let out = compress(&input).unwrap();
        assert_eq!([out[31], out[63], out[95], out[127]], [128, 0, 128, 128]);
    }
}
