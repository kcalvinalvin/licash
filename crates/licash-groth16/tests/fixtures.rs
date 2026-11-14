//! Keep the receipt vectors (from Lichen d01bb5b) byte-identical, not just equivalent after trimming.

use sha2::{Digest, Sha256};

#[test]
fn receipt_vectors_keep_their_original_bytes() {
    for (bytes, expected) in [
        (
            include_bytes!("data/babe-60-42.hex").as_slice(),
            "80440bcb18b384c15bc56fd0b21775d0a2a95762b75471c87e1698a36189ded3",
        ),
        (
            include_bytes!("data/babe-claim-60-42.hex").as_slice(),
            "a96b9f3e893f519c51aeac89f074248f23637624451869191ff3e8e0b55ddee6",
        ),
        (
            include_bytes!("data/withdraw-9-42.hex").as_slice(),
            "f0df9f71c53bf01c1db95db74dafac66beb1bcfff261fcf7a3881af63ec06e46",
        ),
        (
            include_bytes!("data/counter-stale.hex").as_slice(),
            "1412aa7b8bd935f27a2f60224c6ced5d92539683bb421c2452b83f6202eda2a5",
        ),
    ] {
        assert_eq!(hex::encode(Sha256::digest(bytes)), expected);
    }
}
