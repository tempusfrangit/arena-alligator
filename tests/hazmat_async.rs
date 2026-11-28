//! Async allocation of hazmat raw regions.
//!
//! Most tests poll `raw_alloc_async` futures by hand. Each future has its own
//! waker that counts wakes, so the test alone sets the order of polls and
//! frees.
#![cfg(all(feature = "async-alloc", feature = "hazmat-raw-access"))]

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use arena_alligator::hazmat::{AsyncRawBuddyArena, AsyncRawFixedArena};
use arena_alligator::{
    AllocError, BuddyArena, BuddyGeometry, BuddyWaiter, FixedArena, InitPolicy, NotifyWaiters,
    Waiter,
};

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
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

/// A future that the test polls by hand.
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

    /// True if a wake came after the last poll.
    fn woken(&self) -> bool {
        self.wakes() > self.seen
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

/// Counts calls, so that a test can see which waiter the arena uses.
struct CountingWaiter {
    inner: NotifyWaiters,
    registers: Arc<AtomicUsize>,
    wakes: Arc<AtomicUsize>,
}

impl Waiter for CountingWaiter {
    type Registration = <NotifyWaiters as Waiter>::Registration;

    fn register(&self) -> Self::Registration {
        self.registers.fetch_add(1, Ordering::Relaxed);
        Waiter::register(&self.inner)
    }

    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        Waiter::wake(&self.inner);
    }
}

impl BuddyWaiter for CountingWaiter {
    type Registration = <NotifyWaiters as BuddyWaiter>::Registration;

    fn register(&self, order: usize) -> Self::Registration {
        self.registers.fetch_add(1, Ordering::Relaxed);
        BuddyWaiter::register(&self.inner, order)
    }

    fn wake(&self, freed_order: usize) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        BuddyWaiter::wake(&self.inner, freed_order);
    }
}

fn fixed(slots: usize) -> AsyncRawFixedArena {
    FixedArena::with_slot_capacity(nz(slots), nz(64))
        .hazmat_raw_access()
        .build_async()
        .unwrap()
}

#[test]
fn fixed_returns_at_once_when_a_slot_is_free() {
    let arena = fixed(1);
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async()));
    assert_eq!(waiter.complete().capacity(), 64);
}

#[test]
fn fixed_parks_until_the_region_drops() {
    let arena = fixed(1);
    let held = arena.raw_alloc().unwrap();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async()));
    waiter.park();
    assert!(!waiter.woken());
    assert_eq!(arena.metrics().allocations_failed, 1);
    drop(held);
    assert!(waiter.woken());
    assert_eq!(waiter.complete().capacity(), 64);
}

#[test]
fn fixed_wakes_when_the_last_bytes_slice_drops() {
    let arena = fixed(1);
    let mut held = arena.raw_alloc().unwrap();
    unsafe { held.as_mut_ptr().write_bytes(0xAB, 16) };
    let bytes = unsafe { held.freeze(0..16) }.unwrap();
    let tail = bytes.slice(8..);
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async()));
    waiter.park();
    drop(bytes);
    assert!(!waiter.woken());
    drop(tail);
    assert!(waiter.woken());
    waiter.complete();
}

#[test]
fn fixed_cancelled_waiter_passes_the_wake_on() {
    let arena = fixed(1);
    let held = arena.raw_alloc().unwrap();
    let mut first = Driven::new(Box::pin(arena.raw_alloc_async()));
    let mut second = Driven::new(Box::pin(arena.raw_alloc_async()));
    first.park();
    second.park();
    drop(held);
    assert!(first.woken());
    assert!(!second.woken());
    first.cancel();
    assert!(second.woken());
    second.complete();
}

#[test]
fn fixed_sync_methods_stay_available() {
    let arena = fixed(2);
    assert_eq!(arena.slot_count(), 2);
    let _region = arena.raw_alloc().unwrap();
    let _buffer = arena.allocate().unwrap();
    assert!(arena.raw_alloc().is_err());
}

#[test]
fn fixed_build_async_with_uses_the_given_waiter() {
    let registers = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let arena: AsyncRawFixedArena<CountingWaiter> = FixedArena::with_slot_capacity(nz(1), nz(64))
        .hazmat_raw_access()
        .build_async_with(CountingWaiter {
            inner: NotifyWaiters::new(1),
            registers: Arc::clone(&registers),
            wakes: Arc::clone(&wakes),
        })
        .unwrap();
    let held = arena.raw_alloc().unwrap();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async()));
    waiter.park();
    assert_eq!(registers.load(Ordering::Relaxed), 1);
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    drop(held);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(waiter.woken());
    waiter.complete();
}

#[test]
fn fixed_clone_keeps_the_async_type_and_the_waiter() {
    let registers = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let arena: AsyncRawFixedArena<CountingWaiter> = FixedArena::with_slot_capacity(nz(1), nz(64))
        .hazmat_raw_access()
        .build_async_with(CountingWaiter {
            inner: NotifyWaiters::new(1),
            registers: Arc::clone(&registers),
            wakes: Arc::clone(&wakes),
        })
        .unwrap();
    let clone: AsyncRawFixedArena<CountingWaiter> = arena.clone();
    let held = arena.raw_alloc().unwrap();
    let mut waiter = Driven::new(Box::pin(clone.raw_alloc_async()));
    waiter.park();
    assert_eq!(registers.load(Ordering::Relaxed), 1);
    drop(held);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(waiter.woken());
    waiter.complete();
}

#[test]
fn fixed_zero_policy_holds_on_the_wait_path() {
    let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
        .init_policy(InitPolicy::Zero)
        .hazmat_raw_access()
        .build_async()
        .unwrap();
    let mut held = arena.raw_alloc().unwrap();
    unsafe { held.as_mut_ptr().write_bytes(0xAB, 64) };
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async()));
    waiter.park();
    drop(held);
    let region = waiter.complete();
    for byte in region.as_uninit_slice() {
        assert_eq!(unsafe { byte.assume_init() }, 0);
    }
}

#[test]
fn fixed_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let arena = fixed(1);
    assert_send(&arena.raw_alloc_async());
}

#[test]
#[cfg_attr(miri, ignore = "multi-threaded runtime; too slow under Miri")]
fn fixed_tasks_share_two_slots() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let arena = fixed(2);
    runtime.block_on(async {
        let tasks: Vec<_> = (0..32u8)
            .map(|id| {
                let arena = arena.clone();
                tokio::spawn(async move {
                    for _ in 0..50 {
                        let mut region = arena.raw_alloc_async().await;
                        unsafe { region.as_mut_ptr().write_bytes(id, 8) };
                        let bytes = unsafe { region.freeze(0..8) }.unwrap();
                        assert!(bytes.iter().all(|b| *b == id));
                        drop(bytes);
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(30), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await
        .expect("tasks did not finish");
    });
    assert_eq!(arena.metrics().bytes_live, 0);
}

fn buddy() -> AsyncRawBuddyArena {
    let geometry = BuddyGeometry::exact(nz(4096), nz(512)).unwrap();
    BuddyArena::builder(geometry)
        .hazmat_raw_access()
        .build_async()
        .unwrap()
}

#[test]
fn buddy_too_large_fails_at_once() {
    let arena = buddy();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async(nz(8192))));
    assert!(matches!(
        waiter.complete(),
        Err(AllocError::RequestTooLarge)
    ));
    assert_eq!(arena.metrics().allocations_failed, 1);
}

#[test]
fn buddy_parks_until_blocks_coalesce() {
    let arena = buddy();
    let left = arena.raw_alloc(nz(2048)).unwrap();
    let right = arena.raw_alloc(nz(2048)).unwrap();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async(nz(4096))));
    waiter.park();
    drop(left);
    assert!(!waiter.woken());
    drop(right);
    assert!(waiter.woken());
    assert_eq!(waiter.complete().unwrap().capacity(), 4096);
}

#[test]
fn buddy_wakes_when_the_bytes_drops() {
    let arena = buddy();
    let mut held = arena.raw_alloc(nz(4096)).unwrap();
    unsafe { held.as_mut_ptr().write_bytes(0xCD, 8) };
    let bytes = unsafe { held.freeze(0..8) }.unwrap();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async(nz(512))));
    waiter.park();
    assert!(!waiter.woken());
    drop(bytes);
    assert!(waiter.woken());
    assert_eq!(waiter.complete().unwrap().capacity(), 512);
}

#[test]
fn buddy_cancelled_waiter_passes_the_wake_on() {
    let arena = buddy();
    let held = arena.raw_alloc(nz(4096)).unwrap();
    let mut first = Driven::new(Box::pin(arena.raw_alloc_async(nz(4096))));
    let mut second = Driven::new(Box::pin(arena.raw_alloc_async(nz(4096))));
    first.park();
    second.park();
    drop(held);
    assert!(first.woken());
    assert!(!second.woken());
    first.cancel();
    assert!(second.woken());
    assert_eq!(second.complete().unwrap().capacity(), 4096);
}

#[test]
fn buddy_sync_methods_stay_available() {
    let arena = buddy();
    assert_eq!(arena.total_size(), 4096);
    let _region = arena.raw_alloc(nz(512)).unwrap();
    let _buffer = arena.allocate(nz(512)).unwrap();
}

#[test]
fn buddy_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let arena = buddy();
    assert_send(&arena.raw_alloc_async(nz(512)));
}

#[test]
fn buddy_build_async_with_uses_the_given_waiter() {
    let registers = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let geometry = BuddyGeometry::exact(nz(4096), nz(512)).unwrap();
    // Orders 0 to 3 for 512-byte blocks in 4096 bytes.
    let arena: AsyncRawBuddyArena<CountingWaiter> = BuddyArena::builder(geometry)
        .hazmat_raw_access()
        .build_async_with(CountingWaiter {
            inner: NotifyWaiters::new(4),
            registers: Arc::clone(&registers),
            wakes: Arc::clone(&wakes),
        })
        .unwrap();
    let held = arena.raw_alloc(nz(4096)).unwrap();
    let mut waiter = Driven::new(Box::pin(arena.raw_alloc_async(nz(512))));
    waiter.park();
    assert_eq!(registers.load(Ordering::Relaxed), 1);
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    drop(held);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(waiter.woken());
    waiter.complete().unwrap();
}

#[test]
fn buddy_clone_keeps_the_async_type_and_the_waiter() {
    let registers = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let geometry = BuddyGeometry::exact(nz(4096), nz(512)).unwrap();
    // Orders 0 to 3 for 512-byte blocks in 4096 bytes.
    let arena: AsyncRawBuddyArena<CountingWaiter> = BuddyArena::builder(geometry)
        .hazmat_raw_access()
        .build_async_with(CountingWaiter {
            inner: NotifyWaiters::new(4),
            registers: Arc::clone(&registers),
            wakes: Arc::clone(&wakes),
        })
        .unwrap();
    let clone: AsyncRawBuddyArena<CountingWaiter> = arena.clone();
    let held = arena.raw_alloc(nz(4096)).unwrap();
    let mut waiter = Driven::new(Box::pin(clone.raw_alloc_async(nz(4096))));
    waiter.park();
    assert_eq!(registers.load(Ordering::Relaxed), 1);
    drop(held);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(waiter.woken());
    assert_eq!(waiter.complete().unwrap().capacity(), 4096);
}

#[test]
#[cfg_attr(miri, ignore = "multi-threaded runtime; too slow under Miri")]
fn buddy_tasks_share_one_arena() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let arena = buddy();
    runtime.block_on(async {
        let tasks: Vec<_> = (0..32usize)
            .map(|id| {
                let arena = arena.clone();
                tokio::spawn(async move {
                    for _ in 0..50 {
                        let len = 512usize << (id % 3);
                        let mut region = arena.raw_alloc_async(nz(len)).await.unwrap();
                        unsafe { region.as_mut_ptr().write_bytes(id as u8, 8) };
                        let bytes = unsafe { region.freeze(0..8) }.unwrap();
                        assert!(bytes.iter().all(|b| *b == id as u8));
                        drop(bytes);
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(30), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await
        .expect("tasks did not finish");
    });
    assert_eq!(arena.metrics().bytes_live, 0);
}
