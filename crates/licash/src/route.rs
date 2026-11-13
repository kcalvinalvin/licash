//! Deadline budget of a hash-locked payment across hubs.
//!
//! A route is one timed lock per hub under a common hash. The payer of a lock
//! that does not learn the preimage requests its refund on Bitcoin, and the
//! hub can void that request only by publishing the preimage in its response.
//! So by a bounded height the payer either holds the preimage or is paid its
//! refund, and the lock one hub upstream only has to outlive that height.
//!
//! A hub answers a refund request plainly only after it has seen the lock
//! expire unactivated, so it admits a lock only if that fits its response
//! window. Together the two rules bound a route: each hub's window must
//! exceed the sum of the windows downstream of it.

/// One hub's timing for a lock it issues, in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hop {
    /// Response window `w` of the payer's recovery contract at this hub.
    pub window: u32,
    /// Blocks after the lock's expiry by which the payer's request and
    /// claim body are confirmed. It may first wait for a cooperative refund.
    pub confirm: u32,
    /// Blocks for the payer to read the hub's response.
    pub detect: u32,
    /// Blocks the payer needs to act on the preimage upstream.
    pub margin: u32,
    /// Activation depth of the lock.
    pub depth: u32,
    /// Blocks the hub needs to produce and confirm its response.
    pub reaction: u32,
}

impl Hop {
    /// The hub can watch the lock expire, at depth, and still answer a
    /// request posted as early as `now`.
    pub fn admits(&self, now: u32, expiry: u32) -> bool {
        let Some(decided) = sum(&[expiry, self.depth, self.reaction]) else {
            return false;
        };
        expiry > now && now.checked_add(self.window).is_some_and(|by| decided <= by)
    }

    /// Height by which a payer whose refund request confirms within
    /// `confirm` blocks of `expiry` has read the preimage or is owed the
    /// refund.
    pub fn resolved_by(&self, expiry: u32) -> Option<u32> {
        sum(&[expiry, self.confirm, self.window, self.detect])
    }

    /// Least expiry of the lock one hub upstream.
    pub fn upstream_expiry(&self, expiry: u32) -> Option<u32> {
        sum(&[self.resolved_by(expiry)?, self.margin, 1])
    }
}

fn sum(terms: &[u32]) -> Option<u32> {
    terms
        .iter()
        .try_fold(0u32, |total, t| total.checked_add(*t))
}

/// Lock expiries for `route`, whose first hop is the recipient's hub and
/// whose last is the sender's. `terminal` is the recipient's lock expiry.
pub fn expiries(terminal: u32, route: &[Hop]) -> Option<Vec<u32>> {
    let mut out = vec![terminal; route.len().min(1)];
    for hop in route.iter().take(route.len().saturating_sub(1)) {
        out.push(hop.upstream_expiry(*out.last()?)?);
    }
    Some(out)
}

/// Whether locks expiring at `expiries` (recipient's hub first) are safe to
/// set up at height `now`: every hub admits its lock, and every payer has
/// resolved its outgoing lock, with margin, before its incoming lock expires.
pub fn admissible(now: u32, expiries: &[u32], route: &[Hop]) -> bool {
    expiries.len() == route.len()
        && route
            .iter()
            .zip(expiries)
            .all(|(hop, e)| hop.admits(now, *e))
        && route
            .iter()
            .zip(expiries)
            .zip(expiries.iter().skip(1))
            .all(|((hop, e), up)| hop.upstream_expiry(*e).is_some_and(|least| *up >= least))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(window: u32) -> Hop {
        Hop {
            window,
            confirm: 2,
            detect: 1,
            margin: 2,
            depth: 1,
            reaction: 2,
        }
    }

    #[test]
    fn upstream_locks_outlive_forced_disclosure_and_windows_nest() {
        // Recipient's hub first: windows 8, 24 and 60 blocks.
        let route = [hop(8), hop(24), hop(60)];
        let e = expiries(105, &route).unwrap();
        assert_eq!(e, vec![105, 119, 149]);
        assert!(admissible(100, &e, &route));
        // The forwarder that requests its refund by e[0] reads the preimage
        // by 116, three blocks before its own incoming lock expires.
        assert_eq!(route[0].resolved_by(e[0]), Some(116));
        // Expiry spacing alone is not enough: an upstream lock that expires
        // inside the downstream response window can be refunded first.
        assert!(!admissible(100, &[105, 116, 149], &route));
        // Nor is a window that cannot contain its own lock: equal windows
        // admit a single hub only.
        let flat = [hop(24), hop(24)];
        let e = expiries(105, &flat).unwrap();
        assert!(admissible(100, &e[..1], &flat[..1]));
        assert!(!admissible(100, &e, &flat));
        // Each window exceeds the sum of the windows downstream.
        assert!(route[1].window > route[0].window);
        assert!(route[2].window > route[0].window + route[1].window);
        let short = [hop(8), hop(24), hop(30)];
        assert!(!admissible(100, &expiries(105, &short).unwrap(), &short));
        // A lock already expired, or overflowing heights, is never admitted.
        assert!(!hop(8).admits(105, 105));
        assert!(expiries(u32::MAX - 3, &route).is_none());
    }
}
