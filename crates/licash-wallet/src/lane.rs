//! Which requests may share connections and Tor circuits: a lane each.
//!
//! A hub, or an exit, sees what one connection or circuit carries together.
//! The wallet's requests run in lanes, set for a future and whatever it
//! awaits ([`in_lane`]), read by the transport as it asks
//! ([`current_lane`]): one over Tor keeps each lane's connections and
//! circuits to itself. Requests outside any lane share those of their
//! host, as before lanes.
//!
//! Lanes split what the hub could otherwise link only by its connection: a
//! payment taken from those taken with it, an extra code's payment on to
//! the wallet from the payments it took, one payment's delivery from
//! another's. What the requests say links them still: a spend of the notes
//! a take made, the inbox entry a payment on is taken by.
//!
//! What runs by itself is marked as such the same way ([`in_background`]):
//! no one waits on it, so a host that did not answer lately is passed over.

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    task::{Context, Poll},
};

use licash::crypto::tagged_hash;
use rand_core::{OsRng, RngCore};

/// Which requests a request may share connections and circuits with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Those in no lane, whatever lane it is in: for what tells the hub
    /// nothing of who asks, an inbox's public feed.
    Shared,
    /// Those of the operation it is in, else of a new one: the outermost
    /// operation's lane holds its every step.
    Operation,
    /// A new lane's own, within an operation too: for what the hub must
    /// not link to the rest of it.
    Fresh,
    /// Those of the same key's, in this process: the polls of one invoice,
    /// say, one after the other.
    Of([u8; 32]),
}

thread_local! {
    /// The lane of the future being polled on this thread: none outside
    /// any [`in_lane`], `Some(None)` in [`Lane::Shared`].
    static LANE: Cell<Option<Option<u64>>> = const { Cell::new(None) };
}

/// A future polled in its lane, the lane it was polled in put back after.
struct Scoped<F> {
    lane: Option<u64>,
    run: Pin<Box<F>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        struct Restore(Option<Option<u64>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                LANE.with(|lane| lane.set(self.0));
            }
        }
        let _restore = Restore(LANE.with(|lane| lane.replace(Some(self.lane))));
        self.run.as_mut().poll(cx)
    }
}

/// The last lane made: lanes of [`Lane::Of`] have the top bit set.
static MADE: AtomicU64 = AtomicU64::new(0);

/// Of the process, so that no lane of [`Lane::Of`] is the same from one
/// run to the next.
static SALT: OnceLock<[u8; 32]> = OnceLock::new();

/// `run` in `lane`, and whatever it awaits.
pub async fn in_lane<F: Future>(lane: Lane, run: F) -> F::Output {
    let id = match lane {
        Lane::Shared => None,
        Lane::Operation if current_lane().is_some() => return run.await,
        // Polled on whatever thread: a future's lane goes with it.
        Lane::Operation | Lane::Fresh => Some(MADE.fetch_add(1, Ordering::Relaxed) + 1),
        Lane::Of(key) => {
            let salt = SALT.get_or_init(|| {
                let mut salt = [0; 32];
                OsRng.fill_bytes(&mut salt);
                salt
            });
            let hash = tagged_hash("licash/lane", &[salt, &key]);
            let mut id = [0; 8];
            id.copy_from_slice(&hash[..8]);
            Some(u64::from_be_bytes(id) | 1 << 63)
        }
    };
    Scoped {
        lane: id,
        run: Box::pin(run),
    }
    .await
}

/// The lane of the request being asked, if any ([`in_lane`]).
pub fn current_lane() -> Option<u64> {
    LANE.with(Cell::get).flatten()
}

thread_local! {
    /// Whether the future being polled on this thread runs by itself, no
    /// one waiting on it ([`in_background`]).
    static BACKGROUND: Cell<bool> = const { Cell::new(false) };
}

/// A future polled in the background, whether it was put back after.
struct Unwaited<F>(Pin<Box<F>>);

impl<F: Future> Future for Unwaited<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                BACKGROUND.with(|b| b.set(self.0));
            }
        }
        let _restore = Restore(BACKGROUND.with(|b| b.replace(true)));
        self.0.as_mut().poll(cx)
    }
}

/// `run`, and whatever it awaits, as what runs by itself, no one waiting
/// on it: a transport may pass over at once a host that did not answer
/// lately ([`in_background_now`]), rather than wait on it again.
pub async fn in_background<F: Future>(run: F) -> F::Output {
    Unwaited(Box::pin(run)).await
}

/// What a synchronous caller blocks on runs in the background while this
/// is held ([`in_background`]): a runtime's `block_on` polls on its thread.
/// Not for async code, which may move between threads.
pub struct Background(bool);

impl Background {
    /// Until dropped, on this thread.
    pub fn enter() -> Self {
        Self(BACKGROUND.with(|b| b.replace(true)))
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        BACKGROUND.with(|b| b.set(self.0));
    }
}

/// Whether the request being asked runs in the background ([`in_background`]).
pub fn in_background_now() -> bool {
    BACKGROUND.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_background_holds_what_it_awaits_and_ends_with_it() {
        assert!(!in_background_now());
        let (inner, laned) = in_background(async {
            tokio::task::yield_now().await;
            let laned = in_lane(Lane::Fresh, async { in_background_now() }).await;
            (in_background_now(), laned)
        })
        .await;
        assert!(inner && laned);
        assert!(!in_background_now());
        {
            let _background = Background::enter();
            assert!(in_background_now());
            assert!(in_background(async { in_background_now() }).await);
            assert!(in_background_now());
        }
        assert!(!in_background_now());
    }

    #[tokio::test]
    async fn an_operation_holds_its_steps_and_a_fresh_lane_is_new_within_it() {
        assert_eq!(current_lane(), None);
        let (outer, inner, fresh, shared) = in_lane(Lane::Operation, async {
            let outer = current_lane();
            let inner = in_lane(Lane::Operation, async { current_lane() }).await;
            let fresh = in_lane(Lane::Fresh, async { current_lane() }).await;
            let shared = in_lane(Lane::Shared, async { current_lane() }).await;
            (outer, inner, fresh, shared)
        })
        .await;
        assert!(outer.is_some());
        assert_eq!(inner, outer);
        assert!(fresh.is_some() && fresh != outer);
        assert_eq!(shared, None);
        let again = in_lane(Lane::Operation, async { current_lane() }).await;
        assert!(again.is_some() && again != outer);
    }

    #[tokio::test]
    async fn a_keyed_lane_is_the_same_for_its_key_alone() {
        let of = |key| in_lane(Lane::Of([key; 32]), async { current_lane() });
        let (first, again, other) = (of(1).await, of(1).await, of(2).await);
        assert!(first.is_some());
        assert_eq!(first, again);
        assert_ne!(first, other);
        let fresh = in_lane(Lane::Fresh, async { current_lane() }).await;
        assert_ne!(fresh, first);
    }
}
