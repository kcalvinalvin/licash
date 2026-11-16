//! A request R: the amount `b` the holder claims and her claim's digest E,
//! as her one-time signatures in the graph's `request` sign them.

/// What `request` signs: `b` and E. A [`crate::recovery::Publication`] is
/// authority only for the request that names its amount and digest.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub amount: u32,
    pub claim_digest: [u8; 32],
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Request(..)")
    }
}
