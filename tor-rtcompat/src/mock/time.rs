//! Functionality for simulating the passage of time in unit tests.
//!
//! We do this by providing [`MockSleepProvider`], a "SleepProvider"
//! instance that can simulate timeouts and retries without requiring
//! the actual system clock to advance.

#![allow(clippy::missing_docs_in_private_items)]

use std::{
    cmp::{Eq, Ordering, PartialEq, PartialOrd},
    collections::BinaryHeap,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Waker},
    time::{Duration, Instant, SystemTime},
};

use futures::Future;

use crate::SleepProvider;

/// A dummy [`SleepProvider`] instance for testing.
///
/// The MockSleepProvider ignores the current time, and instead keeps
/// its own view of the current `Instant` and `SystemTime`.  You
/// can advance them in-step by calling `advance()`, and you can simulate
/// jumps in the system clock by calling `jump()`.
///
/// This is *not* for production use.
pub struct MockSleepProvider {
    /// The shared backend for this MockSleepProvider and its futures.
    state: Arc<Mutex<SleepSchedule>>,
}

/// Shared backend for sleep provider and Sleeping futures.
struct SleepSchedule {
    /// What time do we pretend it is (monotonic)?  This value only
    /// moves forward.
    instant: Instant,
    /// What time do we pretend it is (wall clock)? This value can move
    /// in any way, but usually moves in step with `instant`.
    wallclock: SystemTime,
    /// Priority queue of events, in the order that we should wake them.
    sleepers: BinaryHeap<SleepEntry>,
}

/// An entry telling us when to wake which future up.
struct SleepEntry {
    /// The time at which this entry should wake
    when: Instant,
    /// The Waker to call when the instant has passed.
    waker: Waker,
}

/// A future returned by [`MockSystemTime::sleep()`].
pub struct Sleeping {
    /// The instant when we should become ready.
    when: Instant,
    /// The schedule to queue ourselves in if we're polled before we're ready.
    provider: Weak<Mutex<SleepSchedule>>,
}

impl MockSleepProvider {
    /// Create a new MockSleepProvider, starting at a given wall-clock time.
    pub fn new(wallclock: SystemTime) -> Self {
        let instant = Instant::now();
        let sleepers = BinaryHeap::new();
        let state = SleepSchedule {
            instant,
            wallclock,
            sleepers,
        };
        MockSleepProvider {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Advance the simulated timeline forward by `dur`.
    ///
    /// Calling this function will wake any pending futures as appropriate.
    pub fn advance(&self, dur: Duration) {
        // It's not so great to unwrap here in general, but since this is
        // only testing code we don't really care.
        let mut state = self.state.lock().unwrap();
        state.wallclock += dur;
        state.instant += dur;
        state.fire();
    }

    /// Simulate a discontinuity in the system clock, by jumping to
    /// `new_wallclock`.
    pub fn jump_to(&self, new_wallclock: SystemTime) {
        let mut state = self.state.lock().unwrap();
        state.wallclock = new_wallclock;
    }
}

impl SleepSchedule {
    /// Wake any pending events that are ready according to the
    /// current simulated time.
    fn fire(&mut self) {
        use std::collections::binary_heap::PeekMut;

        let now = self.instant;
        while let Some(top) = self.sleepers.peek_mut() {
            if now < top.when {
                return;
            }

            PeekMut::pop(top).waker.wake();
        }
    }

    /// Add a new SleepEntry to this schedule.
    fn push(&mut self, ent: SleepEntry) {
        self.sleepers.push(ent);
    }
}

impl SleepProvider for MockSleepProvider {
    type SleepFuture = Sleeping;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture {
        let when = self.state.lock().unwrap().instant + duration;

        Sleeping {
            when,
            provider: Arc::downgrade(&self.state),
        }
    }
}

impl PartialEq for SleepEntry {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when
    }
}
impl Eq for SleepEntry {}
impl PartialOrd for SleepEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SleepEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.when.cmp(&other.when).reverse()
    }
}

impl Future for Sleeping {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(provider) = Weak::upgrade(&self.provider) {
            let mut provider = provider.lock().unwrap();
            let now = provider.instant;

            if now >= self.when {
                return Poll::Ready(());
            }

            let entry = SleepEntry {
                when: self.when,
                waker: cx.waker().clone(),
            };

            provider.push(entry);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_with_runtime;

    #[test]
    fn time_moves_on() {
        test_with_runtime(|_| async {
            use futures::channel::oneshot;
            use std::sync::atomic::AtomicBool;
            use std::sync::atomic::Ordering;

            let sp = MockSleepProvider::new(SystemTime::now());
            let one_hour = Duration::new(3600, 0);

            let (s1, r1) = oneshot::channel();
            let (s2, r2) = oneshot::channel();
            let (s3, r3) = oneshot::channel();

            let b1 = AtomicBool::new(false);
            let b2 = AtomicBool::new(false);
            let b3 = AtomicBool::new(false);

            let real_start = Instant::now();

            futures::join!(
                async {
                    sp.sleep(one_hour).await;
                    b1.store(true, Ordering::SeqCst);
                    s1.send(()).unwrap();
                },
                async {
                    sp.sleep(one_hour * 3).await;
                    b2.store(true, Ordering::SeqCst);
                    s2.send(()).unwrap();
                },
                async {
                    sp.sleep(one_hour * 5).await;
                    b3.store(true, Ordering::SeqCst);
                    s3.send(()).unwrap();
                },
                async {
                    sp.advance(one_hour * 2);
                    r1.await.unwrap();
                    assert_eq!(true, b1.load(Ordering::SeqCst));
                    assert_eq!(false, b2.load(Ordering::SeqCst));
                    assert_eq!(false, b3.load(Ordering::SeqCst));

                    sp.advance(one_hour * 2);
                    r2.await.unwrap();
                    assert_eq!(true, b1.load(Ordering::SeqCst));
                    assert_eq!(true, b2.load(Ordering::SeqCst));
                    assert_eq!(false, b3.load(Ordering::SeqCst));

                    sp.advance(one_hour * 2);
                    r3.await.unwrap();
                    assert_eq!(true, b1.load(Ordering::SeqCst));
                    assert_eq!(true, b2.load(Ordering::SeqCst));
                    assert_eq!(true, b3.load(Ordering::SeqCst));
                    let real_end = Instant::now();

                    assert!(real_end - real_start < one_hour);
                }
            );
        })
    }
}
