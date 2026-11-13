//! Curve points as the verifier handles them, and multi-scalar
//! multiplication: the statement's only expensive operations.
//!
//! In the RISC Zero guest every curve addition and doubling is one call to
//! the bigint2 accelerator, while `k256`'s own field arithmetic (the modular
//! inversion in every projective-to-affine conversion, the square root in
//! every decompression) runs through the slower `sys_bigint`. So the guest
//! keeps points affine in the accelerator's representation (`Pt`),
//! compresses them without an inversion, and multiplies with a Strauss MSM
//! (4-bit windows: 252 shared doublings, 14 table additions plus at most 64
//! additions per term). Natively `Pt` wraps a `k256` projective point and
//! `msm` is the plain sum, so both sides compute the same function.

use k256::{AffinePoint, ProjectivePoint, Scalar};

#[cfg(all(target_os = "zkvm", target_arch = "riscv32"))]
mod imp {
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    use k256::{AffinePoint, ProjectivePoint, Scalar};
    use risc0_bigint2::ec::{AffinePoint as R0Point, Secp256k1Curve};

    use crate::crypto::Point;
    use crate::Error;

    type Raw = R0Point<8, Secp256k1Curve>;

    #[derive(Clone, Copy)]
    pub struct Pt(Raw);

    fn words(bytes_be: &[u8]) -> [u32; 8] {
        let mut w = [0u32; 8];
        for (i, chunk) in bytes_be.chunks(4).enumerate() {
            w[7 - i] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        w
    }

    fn bytes_be(w: &[u32; 8]) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, word) in w.iter().rev().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
        }
        b
    }

    impl Pt {
        pub fn identity() -> Pt {
            Pt(Raw::IDENTITY)
        }

        pub fn is_identity(&self) -> bool {
            self.0.is_identity()
        }

        pub fn from_affine(a: &AffinePoint) -> Pt {
            let enc = a.to_encoded_point(false);
            match (enc.x(), enc.y()) {
                (Some(x), Some(y)) => Pt(Raw::new_unchecked(words(x), words(y))),
                _ => Pt::identity(),
            }
        }

        /// Decompress (one square root, in `k256`).
        pub fn from_point(p: &Point) -> Result<Pt, Error> {
            p.to_affine().map(|a| Pt::from_affine(&a))
        }

        /// Compress: the parity of `y` and `x`, no inversion. The identity
        /// has no SEC1 compressed form; it is 33 zero bytes here.
        pub fn to_point(&self) -> Point {
            let Some(xy) = self.0.as_u32s() else {
                return Point([0u8; 33]);
            };
            let mut out = [0u8; 33];
            out[0] = 0x02 | (xy[1][0] & 1) as u8;
            out[1..].copy_from_slice(&bytes_be(&xy[0]));
            Point(out)
        }

        pub fn to_projective(&self) -> ProjectivePoint {
            if self.is_identity() {
                return ProjectivePoint::IDENTITY;
            }
            self.to_point()
                .to_projective()
                .expect("the accelerator returns points on the curve")
        }

        pub fn add(&self, other: &Pt) -> Pt {
            let mut out = Raw::IDENTITY;
            self.0.add(&other.0, &mut out);
            Pt(out)
        }
    }

    /// `sum k_i P_i`, Strauss with 4-bit windows, in place.
    pub fn msm(terms: &[(Pt, Scalar)]) -> Pt {
        let mut tables: Vec<[Raw; 16]> = Vec::with_capacity(terms.len());
        let mut scalars: Vec<[u8; 32]> = Vec::with_capacity(terms.len());
        for (p, k) in terms {
            let mut t = [Raw::IDENTITY; 16];
            t[1] = p.0;
            for i in 2..16 {
                let (lo, hi) = t.split_at_mut(i);
                lo[i - 1].add(&p.0, &mut hi[0]);
            }
            tables.push(t);
            scalars.push(k.to_bytes().into());
        }
        let mut a = Raw::IDENTITY;
        let mut b = Raw::IDENTITY;
        let (mut acc, mut tmp) = (&mut a, &mut b);
        for w in 0..64 {
            if w > 0 && !acc.is_identity() {
                for _ in 0..4 {
                    acc.double(tmp);
                    core::mem::swap(&mut acc, &mut tmp);
                }
            }
            for (t, k) in tables.iter().zip(&scalars) {
                let byte = k[w / 2];
                let nib = if w % 2 == 0 { byte >> 4 } else { byte & 0xf } as usize;
                if nib != 0 {
                    acc.add(&t[nib], tmp);
                    core::mem::swap(&mut acc, &mut tmp);
                }
            }
        }
        Pt(*acc)
    }
}

#[cfg(not(all(target_os = "zkvm", target_arch = "riscv32")))]
mod imp {
    use k256::{AffinePoint, ProjectivePoint, Scalar};

    use crate::crypto::Point;
    use crate::Error;

    #[derive(Clone, Copy)]
    pub struct Pt(ProjectivePoint);

    impl Pt {
        pub fn identity() -> Pt {
            Pt(ProjectivePoint::IDENTITY)
        }

        pub fn is_identity(&self) -> bool {
            self.0 == ProjectivePoint::IDENTITY
        }

        pub fn from_affine(a: &AffinePoint) -> Pt {
            Pt(ProjectivePoint::from(*a))
        }

        pub fn from_point(p: &Point) -> Result<Pt, Error> {
            p.to_affine().map(|a| Pt::from_affine(&a))
        }

        pub fn to_point(&self) -> Point {
            if self.is_identity() {
                return Point([0u8; 33]);
            }
            Point::from_projective(&self.0)
        }

        pub fn to_projective(&self) -> ProjectivePoint {
            self.0
        }

        pub fn add(&self, other: &Pt) -> Pt {
            Pt(self.0 + other.0)
        }
    }

    pub fn msm(terms: &[(Pt, Scalar)]) -> Pt {
        Pt(terms
            .iter()
            .fold(ProjectivePoint::IDENTITY, |acc, (p, k)| acc + p.0 * k))
    }
}

pub use imp::{msm, Pt};

impl Pt {
    pub fn generator() -> Pt {
        Pt::from_affine(&AffinePoint::GENERATOR)
    }

    pub fn from_projective(p: &ProjectivePoint) -> Pt {
        Pt::from_affine(&p.to_affine())
    }
}

/// `k G`.
pub fn gen_mul(k: &Scalar) -> Pt {
    msm(&[(Pt::generator(), *k)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{random_scalar, Point};

    #[test]
    fn msm_matches_naive_and_points_roundtrip() {
        let (a, b, c) = (random_scalar(), random_scalar(), random_scalar());
        let p = ProjectivePoint::GENERATOR * random_scalar();
        let q = ProjectivePoint::GENERATOR * random_scalar();
        let want = ProjectivePoint::GENERATOR * a + p * b + q * c;
        let got = msm(&[
            (Pt::generator(), a),
            (Pt::from_projective(&p), b),
            (Pt::from_projective(&q), c),
        ]);
        assert_eq!(got.to_projective(), want);
        assert_eq!(gen_mul(&a).to_projective(), ProjectivePoint::GENERATOR * a);
        assert!(msm(&[]).is_identity());
        let cp = Point::from_projective(&p);
        assert_eq!(Pt::from_point(&cp).unwrap().to_point(), cp);
        assert_eq!(
            Pt::from_projective(&p)
                .add(&Pt::from_projective(&(-p)))
                .to_point(),
            Point([0u8; 33])
        );
    }
}
