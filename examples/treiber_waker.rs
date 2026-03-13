//! Custom waiter backed by a Treiber stack.
//!
//! This example implements [`Waiter`] and [`WaitRegistration`] with a
//! stack of parked wakers rather than the built-in `NotifyWaiters`.
//!
//! `crossbeam_epoch` handles node reclamation so the example concentrates on
//! the waiter contract rather than raw-pointer lifetime management.
//!
//! A Treiber stack funnels park/wake traffic through a hot atomic on the
//! stack head. Under real contention that CAS becomes a bottleneck, while
//! `NotifyWaiters` spreads the work through `tokio::sync::Notify`. That is the
//! preferred production path.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crossbeam_epoch::{self as epoch, Atomic, Owned};

use arena_alligator::{AsyncFixedArena, FixedArena, WaitRegistration, Waiter};
use bytes::BufMut;

// ---------------------------------------------------------------------------
// Entry: shared state between stack node and registration
// ---------------------------------------------------------------------------

const LIVE: u8 = 0;
const WOKEN: u8 = 1;
const REVOKED: u8 = 2;

struct Entry {
    status: AtomicU8,
    waker: Mutex<Option<Waker>>,
}

// ---------------------------------------------------------------------------
// Treiber stack with epoch-based reclamation
// ---------------------------------------------------------------------------

struct Node {
    entry: Arc<Entry>,
    next: Atomic<Node>,
}

struct TreiberStack {
    head: Atomic<Node>,
}

impl TreiberStack {
    fn new() -> Self {
        Self {
            head: Atomic::null(),
        }
    }

    fn push(&self, entry: Arc<Entry>) {
        let mut node = Owned::new(Node {
            entry,
            next: Atomic::null(),
        });
        let guard = epoch::pin();
        loop {
            let head = self.head.load(Ordering::Acquire, &guard);
            node.next.store(head, Ordering::Relaxed);
            match self.head.compare_exchange(
                head,
                node,
                Ordering::AcqRel,
                Ordering::Acquire,
                &guard,
            ) {
                Ok(_) => return,
                Err(err) => {
                    // Reuse the allocation on CAS failure.
                    node = err.new;
                }
            }
        }
    }

    fn pop_and_wake(&self) {
        let guard = epoch::pin();
        loop {
            let head = self.head.load(Ordering::Acquire, &guard);
            let head_ref = match unsafe { head.as_ref() } {
                Some(r) => r,
                None => return,
            };
            let next = head_ref.next.load(Ordering::Acquire, &guard);
            if self
                .head
                .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire, &guard)
                .is_ok()
            {
                let entry = &head_ref.entry;
                // SAFETY: after winning the CAS this pop owns the node, and
                // epoch pinning keeps the storage alive until reclamation.
                unsafe { guard.defer_destroy(head) };

                match entry.status.compare_exchange(
                    LIVE,
                    WOKEN,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        if let Some(waker) = entry.waker.lock().unwrap().take() {
                            waker.wake();
                        }
                        return;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
        }
    }
}

impl Drop for TreiberStack {
    fn drop(&mut self) {
        // Drop runs after the last Arc<TreiberStack>, so no concurrent access
        // remains and the list can be walked without pinning.
        unsafe {
            let guard = epoch::unprotected();
            let mut current = self.head.load(Ordering::Relaxed, guard);
            while let Some(node) = current.as_ref() {
                let next = node.next.load(Ordering::Relaxed, guard);
                drop(current.into_owned());
                current = next;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TreiberWaiter
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TreiberWaiter {
    stack: Arc<TreiberStack>,
}

impl TreiberWaiter {
    fn new() -> Self {
        Self {
            stack: Arc::new(TreiberStack::new()),
        }
    }
}

impl Waiter for TreiberWaiter {
    type Registration = TreiberRegistration;

    fn register(&self) -> TreiberRegistration {
        TreiberRegistration {
            stack: Arc::clone(&self.stack),
            entry: None,
            armed: false,
        }
    }

    fn wake(&self) {
        self.stack.pop_and_wake();
    }
}

// ---------------------------------------------------------------------------
// TreiberRegistration
//
// All fields are Unpin, so Pin<&mut Self> uses get_mut().
// ---------------------------------------------------------------------------

struct TreiberRegistration {
    stack: Arc<TreiberStack>,
    entry: Option<Arc<Entry>>,
    armed: bool,
}

impl WaitRegistration for TreiberRegistration {
    fn prepare(self: Pin<&mut Self>) {
        self.get_mut().armed = true;
    }

    fn revoke(self: Pin<&mut Self>) {
        self.get_mut().do_revoke();
    }
}

impl TreiberRegistration {
    fn do_revoke(&mut self) {
        if let Some(entry) = self.entry.take() {
            let _ =
                entry
                    .status
                    .compare_exchange(LIVE, REVOKED, Ordering::AcqRel, Ordering::Relaxed);
        }
        self.armed = false;
    }
}

impl Future for TreiberRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if !this.armed {
            return Poll::Ready(());
        }

        if let Some(entry) = &this.entry {
            if entry.status.load(Ordering::Acquire) == WOKEN {
                this.entry = None;
                this.armed = false;
                return Poll::Ready(());
            }
            // Refresh the task waker before the second state check.
            *entry.waker.lock().unwrap() = Some(cx.waker().clone());
            if entry.status.load(Ordering::Acquire) == WOKEN {
                this.entry = None;
                this.armed = false;
                return Poll::Ready(());
            }
        } else {
            let entry = Arc::new(Entry {
                status: AtomicU8::new(LIVE),
                waker: Mutex::new(Some(cx.waker().clone())),
            });
            this.entry = Some(Arc::clone(&entry));
            this.stack.push(entry);
        }

        Poll::Pending
    }
}

impl Drop for TreiberRegistration {
    fn drop(&mut self) {
        self.do_revoke();
    }
}

// ---------------------------------------------------------------------------
// Main
//
// current_thread + yield_now() makes the parked path deterministic.
// ---------------------------------------------------------------------------

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let arena: AsyncFixedArena<TreiberWaiter> = FixedArena::with_slot_capacity(nz(2), nz(256))
        .build_async_with(TreiberWaiter::new())
        .unwrap();
    let arena = Arc::new(arena);

    // Fill both slots.
    let mut buf1 = arena.allocate_async().await;
    let mut buf2 = arena.allocate_async().await;
    buf1.put_slice(b"slot one");
    buf2.put_slice(b"slot two");

    let a = Arc::clone(&arena);
    let handle = tokio::spawn(async move {
        // Park until a slot frees up.
        let mut buf = a.allocate_async().await;
        buf.put_slice(b"waited for this");
        let bytes = buf.freeze();
        println!(
            "treiber waker allocation got: {}",
            std::str::from_utf8(&bytes).unwrap()
        );
    });

    // Yield so the spawned task runs, fails allocation, and parks.
    tokio::task::yield_now().await;

    // Free a slot so the spawned task can proceed
    drop(buf1.freeze());

    handle.await.unwrap();
    drop(buf2.freeze());

    let m = arena.metrics();
    println!(
        "allocations: {}, frees: {}, bytes_live: {}",
        m.allocations_ok, m.frees, m.bytes_live
    );
}
