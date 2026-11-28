//! Deterministic interleavings of the async allocation paths.
//!
//! Every test runs on a current-thread runtime with the clock paused. Most
//! tests poll `allocate_async` futures directly, and each future has its own
//! waker that counts wakes. Because of this, the test alone sets the order of
//! polls, frees, sync allocations and cancellations, and it can see which
//! waiters a free woke. The tests count retries through `allocations_failed`.
//! A parked waiter records one failed attempt when it queues. A woken waiter
//! that finds no free unit records each attempt that it made before it parks
//! again.
#![cfg(feature = "async-alloc")]

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use arena_alligator::{
    AsyncBuddyArena, AsyncFixedArena, BuddyArena, BuddyGeometry, Buffer, FixedArena,
};

const UNIT: usize = 512;

/// Per-wake delivery cap of the default buddy waiter.
const BUDDY_DELIVERY_CAP: usize = 8;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A future that the test polls only through explicit calls.
struct Driven<'a, T> {
    future: Option<BoxFuture<'a, T>>,
    count: Arc<WakeCount>,
    waker: Waker,
    seen: usize,
}

impl<'a, T> Driven<'a, T> {
    fn new(future: BoxFuture<'a, T>) -> Self {
        let count = Arc::new(WakeCount::default());
        let waker = Waker::from(Arc::clone(&count));
        Self {
            future: Some(future),
            count,
            waker,
            seen: 0,
        }
    }

    fn wakes(&self) -> usize {
        self.count.0.load(Ordering::Relaxed)
    }

    /// Returns true if a wake occurred after the last poll.
    fn woken(&self) -> bool {
        self.wakes() > self.seen
    }

    fn is_pending(&self) -> bool {
        self.future.is_some()
    }

    fn poll(&mut self) -> Option<T> {
        self.seen = self.wakes();
        let future = self
            .future
            .as_mut()
            .expect("polled after completion or cancel");
        let mut cx = Context::from_waker(&self.waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => {
                self.future = None;
                Some(value)
            }
            Poll::Pending => None,
        }
    }

    #[track_caller]
    fn park(&mut self) {
        assert!(self.poll().is_none(), "waiter completed instead of parking");
    }

    #[track_caller]
    fn complete(&mut self) -> T {
        self.poll().expect("waiter did not complete")
    }

    fn cancel(&mut self) {
        assert!(self.future.take().is_some(), "cancelled twice");
    }
}

/// Indices of the pending waiters that got a wake after their last poll.
fn woken<T>(waiters: &[Driven<'_, T>]) -> Vec<usize> {
    waiters
        .iter()
        .enumerate()
        .filter(|(_, w)| w.is_pending() && w.woken())
        .map(|(i, _)| i)
        .collect()
}

fn total_wakes<T>(waiters: &[Driven<'_, T>]) -> usize {
    waiters.iter().map(Driven::wakes).sum()
}

/// Poll the woken waiters until no woken waiter is left, as an executor does.
/// Returns the completions in poll order. A wake storm causes a failure at the
/// limit of 64 rounds, not a hang.
fn run_woken(waiters: &mut [Driven<'_, Buffer>]) -> Vec<(usize, Buffer)> {
    let mut done = Vec::new();
    for _ in 0..64 {
        let ready = woken(waiters);
        if ready.is_empty() {
            return done;
        }
        for i in ready {
            if let Some(buf) = waiters[i].poll() {
                done.push((i, buf));
            }
        }
    }
    panic!("waiters kept waking each other after 64 rounds");
}

/// One-unit allocation against either arena type.
trait TestArena: Clone + Send + Sync + 'static {
    /// Number of failed attempts that a woken waiter makes when a different
    /// allocation takes its unit first. The fixed loop retries one time on the
    /// wake and one more time after it re-registers. The buddy loop re-registers
    /// first and then retries one time.
    const LOST_RACE_ATTEMPTS: u64;

    fn alloc(&self) -> BoxFuture<'_, Buffer>;
    fn try_alloc(&self) -> Option<Buffer>;
    fn failed(&self) -> u64;
    fn bytes_live(&self) -> usize;
}

impl TestArena for AsyncFixedArena {
    const LOST_RACE_ATTEMPTS: u64 = 2;

    fn alloc(&self) -> BoxFuture<'_, Buffer> {
        Box::pin(self.allocate_async())
    }

    fn try_alloc(&self) -> Option<Buffer> {
        self.allocate().ok()
    }

    fn failed(&self) -> u64 {
        self.metrics().allocations_failed
    }

    fn bytes_live(&self) -> usize {
        self.metrics().bytes_live
    }
}

impl TestArena for AsyncBuddyArena {
    const LOST_RACE_ATTEMPTS: u64 = 1;

    fn alloc(&self) -> BoxFuture<'_, Buffer> {
        Box::pin(async move { self.allocate_async(nz(UNIT)).await.unwrap() })
    }

    fn try_alloc(&self) -> Option<Buffer> {
        self.allocate(nz(UNIT)).ok()
    }

    fn failed(&self) -> u64 {
        self.metrics().allocations_failed
    }

    fn bytes_live(&self) -> usize {
        self.metrics().bytes_live
    }
}

fn fixed_arena(units: usize) -> AsyncFixedArena {
    FixedArena::with_slot_capacity(nz(units), nz(UNIT))
        .build_async()
        .unwrap()
}

/// `units` must be a power of two. Every request is one minimum block, so all
/// waiters queue at order 0.
fn buddy_arena(units: usize) -> AsyncBuddyArena {
    BuddyArena::builder(BuddyGeometry::exact(nz(units * UNIT), nz(UNIT)).unwrap())
        .build_async()
        .unwrap()
}

fn fill<A: TestArena>(arena: &A) -> Vec<Buffer> {
    let mut held = Vec::new();
    while let Some(buf) = arena.try_alloc() {
        held.push(buf);
    }
    held
}

/// Asserts the state after the test releases all buffers. No bytes are live,
/// and the test can allocate each unit again. A fresh waiter parks with no
/// stale wake, and a free then serves it.
#[track_caller]
fn assert_settled<A: TestArena>(arena: &A, units: usize) {
    assert_eq!(arena.bytes_live(), 0, "capacity still accounted as live");
    let mut held = fill(arena);
    assert_eq!(held.len(), units, "capacity lost");

    let mut fresh = Driven::new(arena.alloc());
    fresh.park();
    assert!(!fresh.woken(), "fresh waiter saw a stale wake");
    drop(held.pop());
    assert!(fresh.woken(), "fresh waiter missed a free");
    drop(fresh.complete());
}

macro_rules! both_arenas {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[tokio::test(start_paused = true)]
                async fn fixed() {
                    super::$scenario(super::fixed_arena).await;
                }

                #[tokio::test(start_paused = true)]
                async fn buddy() {
                    super::$scenario(super::buddy_arena).await;
                }
            }
        )*
    };
}

both_arenas!(
    free_reaches_exactly_one_waiter,
    woken_waiter_loses_race_to_sync_allocate,
    lone_woken_waiter_loses_race_and_is_rewoken,
    cancelled_after_wake_before_poll_hands_wake_on,
    cancellations_at_front_middle_back,
    cancellations_then_frees_before_any_poll,
    saturated_free_causes_bounded_retries,
    timeouts_leave_arena_usable,
    deadline_and_wake_at_same_instant,
);

/// Each free wakes only one parked waiter, and the oldest waiter gets the first
/// wake. The other waiters stay parked with no wake. A subsequent free serves
/// each of them.
async fn free_reaches_exactly_one_waiter<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(2);
    let mut held = fill(&arena);
    let base = arena.failed();
    let mut waiters: Vec<_> = (0..4).map(|_| Driven::new(arena.alloc())).collect();
    for w in &mut waiters {
        w.park();
    }
    assert_eq!(arena.failed(), base + 4);

    for next in 0..4 {
        drop(held.remove(0));
        assert_eq!(
            woken(&waiters),
            [next],
            "free {next} woke the wrong waiters"
        );
        assert_eq!(total_wakes(&waiters), next + 1);
        for w in waiters.iter_mut().filter(|w| w.is_pending()) {
            if w.woken() {
                held.push(w.complete());
            } else {
                w.park();
            }
        }
        assert_eq!(arena.failed(), base + 4, "a parked waiter retried");
    }

    drop(held);
    assert_settled(&arena, 2);
}

/// A sync allocation takes the slot of a woken waiter before the waiter runs.
/// The waiter fails its retry and parks again behind the waiters that are
/// already queued. Its failed retry wakes no waiter. The next free goes to the
/// waiter that was behind it, and the free after that goes back to it.
async fn woken_waiter_loses_race_to_sync_allocate<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(1);
    let held = arena.try_alloc().unwrap();
    let base = arena.failed();
    let mut first = Driven::new(arena.alloc());
    let mut second = Driven::new(arena.alloc());
    first.park();
    second.park();

    drop(held);
    assert!(first.woken());
    assert!(!second.woken());
    let stolen = arena
        .try_alloc()
        .expect("slot should be free for the thief");

    let after_race = base + 2 + A::LOST_RACE_ATTEMPTS;
    first.park();
    assert_eq!(
        arena.failed(),
        after_race,
        "lost race retried more than once"
    );
    second.park();
    assert_eq!(arena.failed(), after_race, "an unwoken waiter retried");
    assert!(!second.woken(), "failed retry re-broadcast its wake");
    assert_eq!(first.wakes() + second.wakes(), 1);

    drop(stolen);
    assert!(second.woken(), "next free skipped the older waiter");
    assert!(!first.woken());
    let buf = second.complete();
    first.park();

    drop(buf);
    assert!(first.woken());
    drop(first.complete());
    assert_eq!(arena.failed(), after_race);
    assert_settled(&arena, 1);
}

/// With no other waiter, the next free wakes the waiter that lost the race
/// again.
async fn lone_woken_waiter_loses_race_and_is_rewoken<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(1);
    let held = arena.try_alloc().unwrap();
    let mut waiter = Driven::new(arena.alloc());
    waiter.park();

    drop(held);
    assert!(waiter.woken());
    let stolen = arena.try_alloc().unwrap();
    waiter.park();
    assert!(!waiter.woken(), "failed retry woke itself");

    drop(stolen);
    assert!(waiter.woken());
    drop(waiter.complete());
    assert_eq!(waiter.wakes(), 2);
    assert_settled(&arena, 1);
}

/// The test cancels a waiter after its wake but before it runs. The waiter must
/// forward the wake to the next waiter only.
async fn cancelled_after_wake_before_poll_hands_wake_on<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(1);
    let held = arena.try_alloc().unwrap();
    let base = arena.failed();
    let mut waiters: Vec<_> = (0..3).map(|_| Driven::new(arena.alloc())).collect();
    for w in &mut waiters {
        w.park();
    }

    drop(held);
    assert_eq!(woken(&waiters), [0]);
    waiters[0].cancel();
    assert_eq!(
        woken(&waiters),
        [1],
        "cancelled waiter's wake was not handed on"
    );
    let buf = waiters[1].complete();
    waiters[2].park();
    assert_eq!(arena.failed(), base + 3);

    drop(buf);
    assert_eq!(woken(&waiters), [2]);
    drop(waiters[2].complete());
    assert_eq!(total_wakes(&waiters), 3);
    assert_settled(&arena, 1);
}

/// The cancellation of parked waiters at the front, middle and back of the
/// queue wakes no waiter. Each subsequent free reaches the next survivor in
/// order.
async fn cancellations_at_front_middle_back<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(4);
    let mut held = fill(&arena);
    let base = arena.failed();
    let mut waiters: Vec<_> = (0..6).map(|_| Driven::new(arena.alloc())).collect();
    for w in &mut waiters {
        w.park();
    }
    for cancelled in [0, 3, 5] {
        waiters[cancelled].cancel();
    }
    // A cancelled buddy registration closes its own channel, and that can invoke
    // the waker of the cancelled waiter, so the assert examines only the survivors.
    assert_eq!(
        survivor_wakes(&waiters),
        0,
        "a cancellation woke a survivor"
    );

    let mut served = Vec::new();
    for _ in 0..3 {
        drop(held.pop());
        let ready = woken(&waiters);
        assert_eq!(ready.len(), 1, "one free woke {ready:?}");
        for (i, buf) in run_woken(&mut waiters) {
            served.push(i);
            held.push(buf);
        }
    }
    assert_eq!(served, [1, 2, 4]);
    assert_eq!(arena.failed(), base + 6);

    drop(held);
    assert_settled(&arena, 4);
}

const SURVIVORS: [usize; 3] = [1, 2, 4];

fn survivor_wakes<T>(waiters: &[Driven<'_, T>]) -> usize {
    SURVIVORS.iter().map(|&i| waiters[i].wakes()).sum()
}

/// The same queue shape, but all frees occur before any survivor runs. Each
/// survivor gets one wake only, and all survivors complete.
async fn cancellations_then_frees_before_any_poll<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(4);
    let mut held = fill(&arena);
    let base = arena.failed();
    let mut waiters: Vec<_> = (0..6).map(|_| Driven::new(arena.alloc())).collect();
    for w in &mut waiters {
        w.park();
    }
    for cancelled in [0, 3, 5] {
        waiters[cancelled].cancel();
    }

    for _ in 0..3 {
        drop(held.pop());
    }
    assert_eq!(woken(&waiters), [1, 2, 4]);
    let done = run_woken(&mut waiters);
    assert_eq!(done.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [1, 2, 4]);
    assert_eq!(survivor_wakes(&waiters), 3);
    assert_eq!(arena.failed(), base + 6);

    drop(done);
    drop(held);
    assert_settled(&arena, 4);
}

/// 32 waiters parked on a full arena. One free that a sync allocation steals
/// causes one wake and one lost race only. When the test polls each parked
/// waiter 4 times, no waiter retries. The free of the stolen unit then serves
/// one waiter only.
async fn saturated_free_causes_bounded_retries<A: TestArena>(make: fn(usize) -> A) {
    const WAITERS: usize = 32;
    let arena = make(4);
    let mut held = fill(&arena);
    let base = arena.failed();
    let mut waiters: Vec<_> = (0..WAITERS).map(|_| Driven::new(arena.alloc())).collect();
    for w in &mut waiters {
        w.park();
    }
    assert_eq!(arena.failed(), base + WAITERS as u64);

    drop(held.pop());
    let stolen = arena.try_alloc().unwrap();
    assert_eq!(total_wakes(&waiters), 1);
    assert!(run_woken(&mut waiters).is_empty());
    for _ in 0..4 {
        for w in &mut waiters {
            w.park();
        }
    }
    let after_race = base + WAITERS as u64 + A::LOST_RACE_ATTEMPTS;
    assert_eq!(total_wakes(&waiters), 1, "wake storm after a stolen free");
    assert_eq!(arena.failed(), after_race);

    drop(stolen);
    assert_eq!(total_wakes(&waiters), 2);
    let done = run_woken(&mut waiters);
    assert_eq!(done.len(), 1);
    assert_eq!(total_wakes(&waiters), 2);
    assert_eq!(arena.failed(), after_race);

    drop(done);
    drop(held);
    drop(waiters);
    assert_settled(&arena, 4);
}

/// `tokio::time::timeout` around `allocate_async`, which is the usual way for a
/// caller to cancel. Deadlines expire while the waiters are parked, and a free
/// serves one waiter between the expiries. After that, the arena is fully
/// usable and has no stale wakes.
async fn timeouts_leave_arena_usable<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(2);
    let mut held = fill(&arena);
    let base = arena.failed();
    let mut handles: Vec<_> = (1..=4u64)
        .map(|i| {
            let arena = arena.clone();
            tokio::spawn(async move { tokio::time::timeout(ms(10 * i), arena.alloc()).await.ok() })
        })
        .collect();
    while arena.failed() < base + 4 {
        tokio::task::yield_now().await;
    }

    // Time moves only by explicit advances. Because of this, a wake storm that
    // keeps the runtime busy makes the retry count below fail. The test does
    // not hang on a clock that never auto-advances.
    tokio::time::advance(ms(15)).await;
    assert!(handles.remove(0).await.unwrap().is_none());

    drop(held.pop());
    let served = handles
        .remove(0)
        .await
        .unwrap()
        .expect("waiter with a live deadline missed the free");
    tokio::time::advance(ms(30)).await;
    for handle in handles {
        assert!(handle.await.unwrap().is_none());
    }
    assert_eq!(arena.failed(), base + 4, "expired waiters retried");

    drop(served);
    drop(held);
    assert_settled(&arena, 2);
}

/// A deadline and a wake both occur before the waiter runs. When the select of
/// the caller prefers the deadline, the select drops the woken allocation. That
/// drop must forward the wake to the next waiter. With `timeout`, which polls
/// the allocation first, the waiter keeps the slot and no other waiter gets a
/// wake.
async fn deadline_and_wake_at_same_instant<A: TestArena>(make: fn(usize) -> A) {
    let arena = make(1);

    let held = arena.try_alloc().unwrap();
    let deadline_first: BoxFuture<'_, Option<Buffer>> = Box::pin(async {
        tokio::select! {
            biased;
            () = tokio::time::sleep(ms(10)) => None,
            buf = arena.alloc() => Some(buf),
        }
    });
    let mut cancelled = Driven::new(deadline_first);
    let mut next = Driven::new(arena.alloc());
    cancelled.park();
    next.park();
    tokio::time::advance(ms(10)).await;
    assert!(cancelled.woken());
    drop(held);
    assert!(!next.woken());
    assert!(
        cancelled.complete().is_none(),
        "deadline should win the select"
    );
    assert!(next.woken(), "wake dropped with the cancelled allocation");
    drop(next.complete());

    let held = arena.try_alloc().unwrap();
    let mut timed = Driven::new(Box::pin(tokio::time::timeout(ms(10), arena.alloc())));
    let mut next = Driven::new(arena.alloc());
    timed.park();
    next.park();
    tokio::time::advance(ms(10)).await;
    drop(held);
    let buf = timed
        .complete()
        .expect("timeout polls the allocation before the deadline");
    assert!(!next.woken(), "a served wake was handed on as well");
    next.park();

    drop(buf);
    assert!(next.woken());
    drop(next.complete());
    assert_settled(&arena, 1);
}

// -- Buddy fan-out --

fn buddy_orders(max_order: u32) -> AsyncBuddyArena {
    BuddyArena::builder(BuddyGeometry::exact(nz(UNIT << max_order), nz(UNIT)).unwrap())
        .build_async()
        .unwrap()
}

/// Waiters for 4096, 2048, 1024 and 512 bytes parked on a held 8192-byte
/// block. The free of that block wakes one waiter per order, and all of the
/// waiters fit.
#[tokio::test(start_paused = true)]
async fn buddy_free_fans_out_to_lower_orders() {
    let arena = buddy_orders(4);
    let held = arena.allocate(nz(UNIT << 4)).unwrap();
    let base = arena.failed();
    let mut waiters: Vec<Driven<'_, Buffer>> = (0..4u32)
        .rev()
        .map(|order| -> Driven<'_, Buffer> {
            let arena = &arena;
            Driven::new(Box::pin(async move {
                arena.allocate_async(nz(UNIT << order)).await.unwrap()
            }))
        })
        .collect();
    for w in &mut waiters {
        w.park();
    }

    drop(held);
    assert_eq!(woken(&waiters), [0, 1, 2, 3]);
    let done = run_woken(&mut waiters);
    assert_eq!(done.len(), 4);
    assert_eq!(arena.failed(), base + 4);
    assert_eq!(arena.metrics().bytes_live, UNIT * 15);

    drop(done);
    assert_eq!(arena.metrics().largest_free_block, UNIT << 4);
}

/// 10 waiters at orders 9 to 0, which together fit in one order-10 block. The
/// free of that block delivers a number of wakes equal to the per-wake cap, and
/// each woken waiter fits. The order scores decide which orders get a wake,
/// and this test does not assert it. The 2 waiters that are left over stay queued,
/// and the frees of the served waiters reach them.
#[tokio::test(start_paused = true)]
async fn buddy_fan_out_is_bounded_by_delivery_cap() {
    let arena = buddy_orders(10);
    let held = arena.allocate(nz(UNIT << 10)).unwrap();
    let base = arena.failed();
    let mut waiters: Vec<Driven<'_, Buffer>> = (0..10u32)
        .rev()
        .map(|order| -> Driven<'_, Buffer> {
            let arena = &arena;
            Driven::new(Box::pin(async move {
                arena.allocate_async(nz(UNIT << order)).await.unwrap()
            }))
        })
        .collect();
    for w in &mut waiters {
        w.park();
    }

    drop(held);
    assert_eq!(woken(&waiters).len(), BUDDY_DELIVERY_CAP);
    let done = run_woken(&mut waiters);
    assert_eq!(done.len(), BUDDY_DELIVERY_CAP);
    assert_eq!(arena.failed(), base + 10, "a woken waiter did not fit");
    let left_over: Vec<_> = (0..waiters.len())
        .filter(|&i| waiters[i].is_pending())
        .collect();
    assert_eq!(left_over.len(), 10 - BUDDY_DELIVERY_CAP);
    assert!(woken(&waiters).is_empty());

    drop(done);
    assert_eq!(woken(&waiters), left_over);
    let rest = run_woken(&mut waiters);
    assert_eq!(rest.len(), left_over.len());
    assert_eq!(arena.failed(), base + 10);

    drop(rest);
    assert_eq!(arena.metrics().bytes_live, 0);
    assert_eq!(arena.metrics().largest_free_block, UNIT << 10);
}
