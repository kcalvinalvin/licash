//! Deadline budget of a hash-locked payment across hubs.
//!
//! A route is one timed lock per hub under a common hash, the recipient's
//! hub first. Every hub but the sender's issues its lock itself, of a note
//! it issued to itself, to its payee: the recipient, or the hub downstream,
//! a client of it. Its payee settles that lock only by revealing the
//! preimage to it or by activating the lock on Bitcoin, so the hub holds the
//! preimage soon after its own lock expires, whatever any other party does.
//! It then claims the lock one hub upstream, which pays it, and activates
//! that lock if its hub does not answer. The lock upstream only has to
//! outlive that activation.
//!
//! A party that pays at a hub without issuing the lock, a provider swapping
//! between hubs, has no such guarantee: the hub learns the preimage and can
//! keep it, and the payer obtains it only by requesting its refund on
//! Bitcoin, which the hub can void only by publishing the preimage. Its
//! lock elsewhere must outlive that hub's response window as well.
//!
//! A refund is owed on Bitcoin only once the lock expired at depth (under
//! a v4 Base), when the hub holds any preimage. A hub admits a lock only if
//! it expires, at depth, within its window, which bounds a lock's life.

/// One hub's timing for a lock it issues, in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hop {
    /// Response window `w` of the hub's recovery contracts, and the most
    /// blocks a lock it admits lives.
    pub window: u32,
    /// Blocks within which a transaction the party broadcasts confirms: an
    /// activation upstream, or a refund request and its claim body. It may
    /// first wait for the hub to answer offchain.
    pub confirm: u32,
    /// Blocks to read the preimage from Bitcoin.
    pub detect: u32,
    /// Blocks the party needs to act on the preimage upstream.
    pub margin: u32,
    /// Activation depth of the lock.
    pub depth: u32,
    /// Blocks the hub needs to produce and confirm its response.
    pub reaction: u32,
}

impl Hop {
    /// The lock expires, at depth and with the hub's reaction, within the
    /// window from `now`.
    pub fn admits(&self, now: u32, expiry: u32) -> bool {
        let Some(decided) = sum(&[expiry, self.depth, self.reaction]) else {
            return false;
        };
        expiry > now && now.checked_add(self.window).is_some_and(|by| decided <= by)
    }

    /// Height by which the issuer of a lock expiring at `expiry`, if the
    /// lock was paid, holds its preimage and has activated the lock
    /// upstream: its payee revealed the preimage to it before `expiry` or
    /// activated the lock below it.
    pub fn activated_by(&self, expiry: u32) -> Option<u32> {
        sum(&[expiry, self.detect, self.confirm])
    }

    /// Least expiry of the lock one hub upstream, which pays the issuer of
    /// the lock expiring at `expiry`.
    pub fn upstream_expiry(&self, expiry: u32) -> Option<u32> {
        sum(&[self.activated_by(expiry)?, self.margin, 1])
    }

    /// Blocks by which the lock upstream outlives the issuer's at least:
    /// `upstream_expiry(e) - e`.
    pub fn lead(&self) -> Option<u32> {
        sum(&[self.detect, self.confirm, self.margin, 1])
    }

    /// Height by which a payer of a lock it did not issue, expiring at
    /// `expiry`, whose refund request confirms within `confirm` blocks of
    /// it, has read the preimage or is owed the refund. None unless
    /// `confirm` exceeds the lock's depth: her refund is owed only on a
    /// chain past `expiry + depth`, so she requests it no earlier (filed
    /// before, it is owed nothing and given up).
    pub fn forced_by(&self, expiry: u32) -> Option<u32> {
        if self.confirm <= self.depth {
            return None;
        }
        sum(&[expiry, self.confirm, self.window, self.detect])
    }

    /// Least expiry of a lock elsewhere that pays that payer.
    pub fn forced_upstream_expiry(&self, expiry: u32) -> Option<u32> {
        sum(&[self.forced_by(expiry)?, self.margin, 1])
    }

    /// Blocks by which a lock elsewhere that pays that payer outlives the
    /// lock at least: `forced_upstream_expiry(e) - e`, if `confirm`
    /// exceeds the depth (which callers of it alone check).
    pub fn forced_lead(&self) -> Option<u32> {
        sum(&[self.confirm, self.window, self.detect, self.margin, 1])
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
/// set up at height `now`: every hub admits its lock, and every issuer has
/// activated the lock upstream, with margin, before it expires.
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
    fn upstream_locks_outlive_the_issuers_activation_and_windows_do_not_nest() {
        // Recipient's hub first, every hub with the same window.
        let route = [hop(24); 3];
        let e = expiries(105, &route).unwrap();
        assert_eq!(e, vec![105, 111, 117]);
        assert!(admissible(100, &e, &route));
        // The recipient's hub, paid at 104, reads an activation of its own
        // lock by 106 and its own activation upstream confirms by 108,
        // three blocks before that lock expires.
        assert_eq!(route[0].activated_by(e[0]), Some(108));
        // An upstream lock that expires before that activation can be
        // refunded first.
        assert!(!admissible(100, &[105, 108, 117], &route));
        // Each window only has to contain its own lock.
        let tight = [hop(8), hop(14), hop(20)];
        assert!(admissible(100, &e, &tight));
        let short = [hop(8), hop(14), hop(19)];
        assert!(!admissible(100, &e, &short));
        // A lock already expired, or overflowing heights, is never admitted.
        assert!(!hop(8).admits(105, 105));
        assert!(expiries(u32::MAX - 3, &route).is_none());
    }

    #[test]
    fn a_payer_that_did_not_issue_the_lock_waits_for_the_window() {
        let to = hop(24);
        // Its refund request confirms by 107, the hub answers by 131 and
        // the payer reads the preimage by 132.
        assert_eq!(to.forced_by(105), Some(132));
        assert_eq!(to.forced_upstream_expiry(105), Some(135));
        assert_eq!(to.forced_lead(), Some(30));
        assert_eq!(to.lead(), Some(6));
        assert_eq!(to.upstream_expiry(105), Some(111));
        // A refund is owed only past the lock's expiry at depth: a request
        // due no later is none.
        let early = Hop { depth: 2, ..to };
        assert_eq!(early.forced_by(105), None);
        assert_eq!(early.forced_upstream_expiry(105), None);
        // A lock that pays it elsewhere must outlive that window, which a
        // hub with the same window cannot admit.
        assert!(!hop(24).admits(100, 135));
        assert!(hop(40).admits(100, 135));
        assert!(to.forced_by(u32::MAX - 3).is_none());
    }
}
