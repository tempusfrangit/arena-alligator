use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

use tokio::sync::Notify;
use tokio::sync::futures::OwnedNotified;
use tokio::sync::oneshot;

use crate::buffer::Buffer;
use crate::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use crate::sync::cell::UnsafeCell;
use crate::{BuddyArena, FixedArena};

// ---------------------------------------------------------------------------
// Waiter traits
// ---------------------------------------------------------------------------

/// Wait strategy for fixed arena async allocation.
///
/// Fixed arenas have uniform slot sizes, so no order awareness is needed.
pub trait Waiter: Send + Sync + 'static {
    /// Future returned when a waiter registers interest in allocation progress.
    type Registration: WaitRegistration;

    /// Register a waiter before retrying allocation.
    fn register(&self) -> Self::Registration;

    /// Wake one waiter after a slot becomes available.
    fn wake(&self);
}

/// Wait strategy for buddy arena async allocation.
///
/// Buddy arenas have per-order block sizes. Waiters register at the order
/// matching their request, and wakes target the best candidate order via
/// scoring.
pub trait BuddyWaiter: Send + Sync + 'static {
    /// Future returned when a waiter registers interest.
    type Registration: WaitRegistration;

    /// Register a waiter at the given allocation order.
    fn register(&self, order: usize) -> Self::Registration;

    /// Wake the best-scoring waiter after a block at `freed_order` becomes available.
    fn wake(&self, freed_order: usize);
}

/// Registration returned by waiter traits.
pub trait WaitRegistration: Future<Output = ()> {
    /// Arm the registration before the post-registration allocation retry.
    fn prepare(self: Pin<&mut Self>);

    /// Revoke the registration when the post-registration retry succeeds
    /// before the allocation loop awaits the registration. The loop does not
    /// call this method after the registration completes.
    fn revoke(self: Pin<&mut Self>);

    /// Consume a completed wake after the fixed allocation loop retries.
    ///
    /// The loop calls this method after that allocation attempt, whether the
    /// attempt succeeds or fails. A failed retry shows that the slot is not
    /// available, so the drop must not forward the handled wake. A drop without
    /// a call to this method must keep the cancellation propagation that the
    /// implementation gives, if it has one.
    ///
    /// The default is a no-op for registrations that do not use an
    /// acknowledgement. An implementation that forwards completed wakes on drop
    /// must override this method. A wrapper around such a registration must call
    /// this method on the registration that it wraps.
    fn consume(self: Pin<&mut Self>) {}
}

// ---------------------------------------------------------------------------
// Wake handles (type-erased dispatch for arena inner structs)
// ---------------------------------------------------------------------------

pub(crate) trait WakeOne: Send + Sync {
    fn wake(&self);
}

impl<W: Waiter> WakeOne for W {
    fn wake(&self) {
        Waiter::wake(self);
    }
}

pub(crate) struct WakeHandle {
    inner: Arc<dyn WakeOne>,
}

impl WakeHandle {
    pub(crate) fn new<W: Waiter>(waiters: Arc<W>) -> Self {
        let inner: Arc<dyn WakeOne> = waiters;
        Self { inner }
    }

    pub(crate) fn wake(&self) {
        self.inner.wake();
    }
}

pub(crate) trait BuddyWakeOne: Send + Sync {
    fn wake(&self, freed_order: usize);
}

impl<W: BuddyWaiter> BuddyWakeOne for W {
    fn wake(&self, freed_order: usize) {
        BuddyWaiter::wake(self, freed_order);
    }
}

pub(crate) struct BuddyWakeHandle {
    inner: Arc<dyn BuddyWakeOne>,
}

impl BuddyWakeHandle {
    pub(crate) fn new<W: BuddyWaiter>(waiters: Arc<W>) -> Self {
        let inner: Arc<dyn BuddyWakeOne> = waiters;
        Self { inner }
    }

    pub(crate) fn wake(&self, freed_order: usize) {
        self.inner.wake(freed_order);
    }
}

// ---------------------------------------------------------------------------
// WaiterEntry — CAS-arbitrated shared state between queue and registration
// ---------------------------------------------------------------------------

const LIVE: u8 = 0;
const WOKEN: u8 = 1;
const REVOKED: u8 = 2;

// The sender type is generic only so that the loom model can use a payload that
// it can observe. Production code always uses the default.
struct WaiterEntry<T = oneshot::Sender<usize>> {
    state: AtomicU8,
    // SAFETY: accessed exactly once by whichever side wins the CAS on `state`.
    tx: UnsafeCell<Option<T>>,
    timestamp: u64,
}

impl<T> WaiterEntry<T> {
    fn new(tx: T, timestamp: u64) -> Self {
        Self {
            state: AtomicU8::new(LIVE),
            tx: UnsafeCell::new(Some(tx)),
            timestamp,
        }
    }

    /// Move the entry from LIVE to `to` (WOKEN or REVOKED) and take its
    /// sender. Only the caller that wins the transition gets `Some`. Each
    /// other caller gets `None` and does not touch the sender.
    fn claim(&self, to: u8) -> Option<T> {
        debug_assert!(to == WOKEN || to == REVOKED);
        self.state
            .compare_exchange(LIVE, to, AtomicOrdering::AcqRel, AtomicOrdering::Relaxed)
            .ok()?;
        // SAFETY: no code moves `state` back to LIVE, so the CAS above succeeds
        // for at most one caller in the lifetime of the entry. Only that caller
        // gets to this access.
        self.tx.with_mut(|tx| unsafe { (*tx).take() })
    }
}

// SAFETY: only the CAS winner accesses tx, and the winner can run on any
// thread, so the payload must be Send. No code shares a reference to the
// payload, so Sync is not necessary for the payload. The state atomic and
// timestamp are Sync.
unsafe impl<T: Send> Send for WaiterEntry<T> {}
unsafe impl<T: Send> Sync for WaiterEntry<T> {}

// ---------------------------------------------------------------------------
// Buddy wake internals — per-order state + scoring
// ---------------------------------------------------------------------------

/// Sentinel: no waiters at this order.
const NO_WAITERS_TIMESTAMP: u64 = u64::MAX;

/// Maximum wakes one wake call delivers across candidate orders.
const MAX_DELIVERIES_PER_WAKE: usize = 8;

/// Maximum consecutive wins by one order before it's banned.
const MAX_CONSECUTIVE: u32 = 10;

/// Nanosecond bonus per order level for scoring (higher order = looks older).
const ORDER_BONUS_NS: u64 = 50_000; // 50μs per order level

/// Per-waiter depth bonus in nanoseconds.
const DEPTH_BONUS_NS: u64 = 5_000; // 5μs per waiting peer

struct BuddyOrderSlot {
    queue: Mutex<alloc::collections::VecDeque<Arc<WaiterEntry>>>,
    count: AtomicUsize,
    head_timestamp: AtomicU64,
}

impl BuddyOrderSlot {
    fn new() -> Self {
        Self {
            queue: Mutex::new(alloc::collections::VecDeque::new()),
            count: AtomicUsize::new(0),
            head_timestamp: AtomicU64::new(NO_WAITERS_TIMESTAMP),
        }
    }
}

// ---------------------------------------------------------------------------
// NotifyWaiters — implements both Waiter (fixed) and BuddyWaiter (buddy)
// ---------------------------------------------------------------------------

struct FixedOrderSlot {
    notify: Arc<Notify>,
    count: AtomicUsize,
}

/// Per-order waiter system.
///
/// Fixed arenas use a single `Notify`-based FIFO (order 0 only).
/// Buddy arenas use per-order mutex queues with CAS-arbitrated oneshot
/// delivery and age-based scoring (with order and queue-depth bonuses plus
/// an anti-monopoly streak ban) to prevent starvation.
#[derive(Clone)]
pub struct NotifyWaiters {
    inner: Arc<NotifyWaitersInner>,
}

struct NotifyWaitersInner {
    // Fixed arena path (always has exactly one slot for order 0)
    fixed_slot: FixedOrderSlot,
    // Buddy arena path (empty vec for fixed-only arenas)
    buddy_orders: Box<[BuddyOrderSlot]>,
    // Packed streak state: upper 32 bits = last_winner order, lower 32 = streak count
    streak_state: AtomicU64,
    // Monotonic clock epoch for timestamps
    epoch: Instant,
}

impl NotifyWaiters {
    /// Create a waiter set with the given number of buddy orders.
    ///
    /// Fixed arenas use `num_orders = 1` (only the fixed slot is used).
    /// Buddy arenas use `num_orders = max_order + 1`.
    pub fn new(num_orders: usize) -> Self {
        assert!(num_orders > 0, "must have at least one order");
        let buddy_orders: Vec<BuddyOrderSlot> =
            (0..num_orders).map(|_| BuddyOrderSlot::new()).collect();
        Self {
            inner: Arc::new(NotifyWaitersInner {
                fixed_slot: FixedOrderSlot {
                    notify: Arc::new(Notify::new()),
                    count: AtomicUsize::new(0),
                },
                buddy_orders: buddy_orders.into_boxed_slice(),
                streak_state: AtomicU64::new(0),
                epoch: Instant::now(),
            }),
        }
    }

    fn now_ns(&self) -> u64 {
        self.inner.epoch.elapsed().as_nanos() as u64
    }
}

impl NotifyWaitersInner {
    // -- Buddy wake scoring --

    fn score_orders(&self, freed_order: usize) -> Vec<usize> {
        let max = freed_order.min(self.buddy_orders.len() - 1);
        let streak = self.streak_state.load(AtomicOrdering::Relaxed);
        let last_winner = (streak >> 32) as usize;
        let streak_count = streak as u32;

        // Collect all eligible orders first, then apply ban as post-filter.
        // Building candidates before ban check avoids order-dependent evaluation
        // where early-scanned orders see an empty candidate list.
        let mut candidates: Vec<(usize, u64)> = Vec::new();

        for order in 0..=max {
            let slot = &self.buddy_orders[order];
            let count = slot.count.load(Ordering::Acquire);
            if count == 0 {
                continue;
            }
            let ts = slot.head_timestamp.load(AtomicOrdering::Acquire);
            if ts == NO_WAITERS_TIMESTAMP {
                continue;
            }

            let effective_age = ts
                .saturating_sub((order as u64).saturating_mul(ORDER_BONUS_NS))
                .saturating_sub((count as u64).saturating_mul(DEPTH_BONUS_NS));

            candidates.push((order, effective_age));
        }

        // Streak ban: remove the streak winner if it has exceeded MAX_CONSECUTIVE
        // and there is at least one other candidate to serve instead.
        if streak_count >= MAX_CONSECUTIVE
            && candidates.len() > 1
            && candidates.iter().any(|(o, _)| *o == last_winner)
        {
            candidates.retain(|(o, _)| *o != last_winner);
        }

        // Sort by effective_age ascending (lowest = oldest = highest priority)
        candidates.sort_by_key(|&(_, age)| age);
        candidates.into_iter().map(|(order, _)| order).collect()
    }

    fn update_streak(&self, winner_order: usize) {
        let current = self.streak_state.load(AtomicOrdering::Relaxed);
        let last_winner = (current >> 32) as usize;
        let new = if winner_order == last_winner {
            let streak = (current as u32).saturating_add(1);
            ((winner_order as u64) << 32) | streak as u64
        } else {
            ((winner_order as u64) << 32) | 1u64
        };
        self.streak_state.store(new, AtomicOrdering::Relaxed);
    }

    fn update_head_timestamp(
        &self,
        queue: &alloc::collections::VecDeque<Arc<WaiterEntry>>,
        order: usize,
    ) {
        let slot = &self.buddy_orders[order];
        // Scan past tombstones (WOKEN/REVOKED) to find the first live entry.
        // Without this, a tombstone at front would set NO_WAITERS_TIMESTAMP
        // even when live waiters exist further back, making them invisible
        // to scoring until an unrelated event repairs head state.
        for entry in queue.iter() {
            if entry.state.load(AtomicOrdering::Relaxed) == LIVE {
                slot.head_timestamp
                    .store(entry.timestamp, AtomicOrdering::Release);
                return;
            }
        }
        slot.head_timestamp
            .store(NO_WAITERS_TIMESTAMP, AtomicOrdering::Release);
    }

    fn buddy_wake(&self, freed_order: usize) {
        let candidates = self.score_orders(freed_order);
        let mut delivered: usize = 0;

        // Deliver one wake per candidate order. A freed block at order N can
        // serve waiters at multiple lower orders via splitting, so wakes fan
        // out across orders rather than stopping at the first delivery.
        for order in candidates {
            if delivered >= MAX_DELIVERIES_PER_WAKE {
                break;
            }
            let mut queue = self.buddy_orders[order].queue.lock().unwrap();
            // Only deliveries count against the bound. A revoked entry or a
            // failed send belongs to a cancelled registration. The loop pops each
            // such entry one time only, so the skips cost at most one pop per
            // cancellation. If the bound counts these entries, a run of
            // cancellations can hide a live waiter from each subsequent free
            // while the block is unused.
            while let Some(entry) = queue.pop_front() {
                if entry.state.load(AtomicOrdering::Relaxed) != LIVE {
                    continue;
                }

                if let Some(tx) = entry.claim(WOKEN) {
                    self.buddy_orders[order]
                        .count
                        .fetch_sub(1, Ordering::Release);
                    self.update_head_timestamp(&queue, order);
                    drop(queue);

                    if tx.send(freed_order).is_ok() {
                        self.update_streak(order);
                        delivered += 1;
                        break;
                    }
                    // A receiver cancellation can occur after the CAS succeeds. Then the
                    // send fails, and the loop retries the same order.
                    queue = self.buddy_orders[order].queue.lock().unwrap();
                }
            }
        }
    }
}

impl NotifyWaiters {
    fn buddy_register(&self, order: usize) -> BuddyRegistration {
        let order = order.min(self.inner.buddy_orders.len() - 1);
        let (tx, rx) = oneshot::channel();
        let timestamp = self.now_ns();
        let entry = Arc::new(WaiterEntry::new(tx, timestamp));

        BuddyRegistration {
            entry: Some(Arc::clone(&entry)),
            rx: Some(rx),
            waiters: Arc::clone(&self.inner),
            order,
            registered: false,
            pending_entry: Some(entry),
        }
    }
}

impl Waiter for NotifyWaiters {
    type Registration = NotifyRegistration;

    fn register(&self) -> NotifyRegistration {
        NotifyRegistration {
            future: self.inner.fixed_slot.notify.clone().notified_owned(),
            inner: Arc::clone(&self.inner),
            registered: false,
            woken: false,
        }
    }

    fn wake(&self) {
        if self.inner.fixed_slot.count.load(Ordering::Acquire) > 0 {
            self.inner.fixed_slot.notify.notify_one();
        }
    }
}

impl BuddyWaiter for NotifyWaiters {
    type Registration = BuddyRegistration;

    fn register(&self, order: usize) -> BuddyRegistration {
        self.buddy_register(order)
    }

    fn wake(&self, freed_order: usize) {
        self.inner.buddy_wake(freed_order);
    }
}

// ---------------------------------------------------------------------------
// NotifyRegistration — fixed arena path (unchanged Notify-based FIFO)
// ---------------------------------------------------------------------------

/// Registration future for fixed arena [`NotifyWaiters`].
pub struct NotifyRegistration {
    future: OwnedNotified,
    inner: Arc<NotifyWaitersInner>,
    registered: bool,
    woken: bool,
}

impl WaitRegistration for NotifyRegistration {
    fn prepare(self: Pin<&mut Self>) {
        let this = unsafe { self.get_unchecked_mut() };
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        let _ = future.enable();
        if !this.registered {
            this.inner.fixed_slot.count.fetch_add(1, Ordering::Release);
            this.registered = true;
        }
    }

    fn revoke(self: Pin<&mut Self>) {
        let this = unsafe { self.get_unchecked_mut() };
        if this.registered {
            this.inner.fixed_slot.count.fetch_sub(1, Ordering::Release);
            this.registered = false;
        }
        this.woken = false;
    }

    fn consume(self: Pin<&mut Self>) {
        // SAFETY: this write clears the wake flag and does not move the pinned
        // notification.
        unsafe { self.get_unchecked_mut() }.woken = false;
    }
}

impl Future for NotifyRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let poll = unsafe { Pin::new_unchecked(&mut this.future) }.poll(cx);
        if poll.is_ready() {
            if this.registered {
                this.inner.fixed_slot.count.fetch_sub(1, Ordering::Release);
                this.registered = false;
            }
            this.woken = true;
        }
        poll
    }
}

impl Drop for NotifyRegistration {
    fn drop(&mut self) {
        if self.registered {
            self.inner.fixed_slot.count.fetch_sub(1, Ordering::Release);
        }
        // A completed Notify future cannot forward its permit on cancellation. If
        // the drop occurs before the retry handles the wake, the drop forwards the
        // permit. The drop does not forward when no other waiter exists, because
        // `notify_one` then stores a stale permit.
        if self.woken && self.inner.fixed_slot.count.load(Ordering::Acquire) > 0 {
            self.inner.fixed_slot.notify.notify_one();
        }
    }
}

// ---------------------------------------------------------------------------
// BuddyRegistration — buddy arena path (CAS + oneshot)
// ---------------------------------------------------------------------------

/// Registration future for buddy arena [`NotifyWaiters`].
pub struct BuddyRegistration {
    entry: Option<Arc<WaiterEntry>>,
    rx: Option<oneshot::Receiver<usize>>,
    waiters: Arc<NotifyWaitersInner>,
    order: usize,
    registered: bool,
    // Entry waiting to be pushed into queue on prepare()
    pending_entry: Option<Arc<WaiterEntry>>,
}

impl BuddyRegistration {
    fn cancel(&mut self) {
        if !self.registered {
            return;
        }
        self.registered = false;
        let Some(entry) = &self.entry else {
            return;
        };
        let slot = &self.waiters.buddy_orders[self.order];
        // The queue lock serializes this cancellation with prepare and wake, so an
        // older repair cannot overwrite the timestamp of a newer queue state.
        let mut queue = slot.queue.lock().unwrap();
        let tx = if let Some(tx) = entry.claim(REVOKED) {
            slot.count.fetch_sub(1, Ordering::Release);
            // Cancellation takes each revoked sender under this lock. Because of this,
            // the removal of the stale prefix cannot invoke wakers or scan old
            // tombstones more than one time.
            while queue
                .front()
                .is_some_and(|head| head.state.load(AtomicOrdering::Relaxed) != LIVE)
            {
                queue.pop_front();
            }
            self.waiters.update_head_timestamp(&queue, self.order);
            Some(tx)
        } else {
            None
        };
        drop(queue);
        // The sender drop closes the channel, and that can invoke a waker that
        // reenters this queue.
        drop(tx);
    }
}

impl WaitRegistration for BuddyRegistration {
    fn prepare(self: Pin<&mut Self>) {
        let this = unsafe { self.get_unchecked_mut() };
        if !this.registered
            && let Some(entry) = this.pending_entry.take()
        {
            let slot = &this.waiters.buddy_orders[this.order];
            let mut queue = slot.queue.lock().unwrap();
            queue.push_back(entry);
            let prev = slot.count.fetch_add(1, Ordering::Release);
            if prev == 0
                && let Some(e) = &this.entry
            {
                slot.head_timestamp
                    .store(e.timestamp, AtomicOrdering::Release);
            }
            this.registered = true;
        }
    }

    fn revoke(self: Pin<&mut Self>) {
        let this = unsafe { self.get_unchecked_mut() };
        this.cancel();
    }
}

impl Future for BuddyRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(rx) = &mut this.rx {
            match Pin::new(rx).poll(cx) {
                Poll::Ready(_) => {
                    this.rx = None;
                    this.registered = false;
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(())
        }
    }
}

impl Drop for BuddyRegistration {
    fn drop(&mut self) {
        self.cancel();

        // A wake delivered but never polled (task cancelled between the
        // oneshot firing and its next poll, or an allocation that succeeded
        // without observing the delivery) would otherwise be swallowed,
        // stalling the remaining waiters until an unrelated free. Recover it
        // from the receiver and hand it onward. This propagates transitively
        // through a chain of cancelled waiters until it reaches a live one,
        // and it converges: a woken waiter that runs its retry drains the
        // receiver in poll, and a failed retry deliberately does NOT
        // re-broadcast. The failure proves the block is gone (buddy_wake
        // already fans out across candidate orders at free time), and
        // re-broadcasting from re-registering waiters turns one free into a
        // perpetual wake storm under saturation.
        if let Some(freed) = self.rx.take().and_then(|mut rx| rx.try_recv().ok()) {
            self.waiters.buddy_wake(freed);
        }
    }
}

// ---------------------------------------------------------------------------
// Allocation loops
// ---------------------------------------------------------------------------

async fn allocate_with_waiter<W, T, F>(waiters: &W, mut try_allocate: F) -> T
where
    W: Waiter,
    F: FnMut() -> Option<T>,
{
    loop {
        let registration = waiters.register();
        tokio::pin!(registration);
        registration.as_mut().prepare();
        if let Some(value) = try_allocate() {
            registration.as_mut().revoke();
            return value;
        }
        registration.as_mut().await;
        let value = try_allocate();
        registration.as_mut().consume();
        if let Some(value) = value {
            return value;
        }
    }
}

async fn allocate_with_buddy_waiter<W, T, F>(waiters: &W, order: usize, mut try_allocate: F) -> T
where
    W: BuddyWaiter,
    F: FnMut() -> Option<T>,
{
    loop {
        let registration = waiters.register(order);
        tokio::pin!(registration);
        registration.as_mut().prepare();
        if let Some(value) = try_allocate() {
            registration.as_mut().revoke();
            return value;
        }
        registration.await;
    }
}

// ---------------------------------------------------------------------------
// AsyncFixedArena
// ---------------------------------------------------------------------------

/// Async-capable wrapper around [`FixedArena`].
///
/// Created via
/// [`FixedArenaBuilder::build_async()`](crate::FixedArenaBuilder::build_async).
/// [`allocate_async()`](AsyncFixedArena::allocate_async) parks until a slot
/// becomes available. Sync methods remain accessible through
/// `Deref<Target = FixedArena>`.
#[derive(Clone)]
pub struct AsyncFixedArena<W = NotifyWaiters> {
    inner: FixedArena,
    waiters: Arc<W>,
}

impl<W> AsyncFixedArena<W> {
    pub(crate) fn new(inner: FixedArena, waiters: Arc<W>) -> Self {
        Self { inner, waiters }
    }
}

impl<W: Waiter> AsyncFixedArena<W> {
    /// Allocate a buffer, waiting asynchronously if the arena is full.
    ///
    /// Returns once a slot becomes available. The bitmap is the source
    /// of truth; notifications are hints to retry.
    pub async fn allocate_async(&self) -> Buffer {
        allocate_with_waiter(self.waiters.as_ref(), || self.inner.allocate().ok()).await
    }
}

impl<W> Deref for AsyncFixedArena<W> {
    type Target = FixedArena;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<W> fmt::Debug for AsyncFixedArena<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncFixedArena")
            .field("inner", &self.inner)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// AsyncBuddyArena
// ---------------------------------------------------------------------------

/// Async-capable wrapper around [`BuddyArena`].
///
/// Created via
/// [`BuddyArenaBuilder::build_async()`](crate::BuddyArenaBuilder::build_async).
/// [`allocate_async()`](AsyncBuddyArena::allocate_async) parks until a
/// large-enough block becomes available. Sync methods remain accessible
/// through `Deref<Target = BuddyArena>`.
#[derive(Clone)]
pub struct AsyncBuddyArena<W = NotifyWaiters> {
    inner: BuddyArena,
    waiters: Arc<W>,
}

impl<W> AsyncBuddyArena<W> {
    pub(crate) fn new(inner: BuddyArena, waiters: Arc<W>) -> Self {
        Self { inner, waiters }
    }
}

impl<W: BuddyWaiter> AsyncBuddyArena<W> {
    /// Allocate a buffer, waiting asynchronously if the arena is full.
    ///
    /// The buddy bitmaps remain the source of truth; notifications are hints
    /// to retry after free or coalesce publishes a usable block.
    ///
    /// Returns [`AllocError::RequestTooLarge`](crate::AllocError::RequestTooLarge)
    /// immediately when `len` exceeds the arena's total capacity: no free or
    /// coalesce could ever satisfy it, so waiting would never return.
    pub async fn allocate_async(
        &self,
        len: core::num::NonZeroUsize,
    ) -> Result<Buffer, crate::AllocError> {
        // A request no block can ever satisfy must fail fast; parking would wait
        // forever. Defer to the sync path, which records the failure and returns
        // `RequestTooLarge`. Only transient `ArenaFull` is worth waiting on.
        let order = match self.order_for_request(len.get()) {
            Some(order) => order,
            None => return self.inner.allocate(len),
        };
        Ok(
            allocate_with_buddy_waiter(self.waiters.as_ref(), order, || {
                self.inner.allocate(len).ok()
            })
            .await,
        )
    }
}

impl<W> Deref for AsyncBuddyArena<W> {
    type Target = BuddyArena;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<W> fmt::Debug for AsyncBuddyArena<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncBuddyArena")
            .field("inner", &self.inner)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::{Wake, Waker};

    use bytes::BufMut;
    use tokio::time::{Duration, timeout};

    use crate::BuddyArena;
    use crate::BuddyGeometry;
    use crate::FixedArena;

    use super::*;

    // -- CountingWaiters: implements Waiter (fixed) and BuddyWaiter (buddy) --

    #[derive(Clone)]
    struct CountingWaiters {
        inner: NotifyWaiters,
        registrations: Arc<AtomicUsize>,
        wakes: Arc<AtomicUsize>,
    }

    impl CountingWaiters {
        fn new(num_orders: usize) -> Self {
            Self {
                inner: NotifyWaiters::new(num_orders),
                registrations: Arc::new(AtomicUsize::new(0)),
                wakes: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn registrations(&self) -> usize {
            self.registrations.load(AtomicOrdering::Relaxed)
        }

        fn wakes(&self) -> usize {
            self.wakes.load(AtomicOrdering::Relaxed)
        }

        async fn wait_for_registrations(&self, n: usize) {
            while self.registrations() < n {
                tokio::task::yield_now().await;
            }
        }
    }

    // Wrapper to project through to the correct inner registration type
    struct CountingFixedRegistration {
        inner: NotifyRegistration,
    }

    struct CountingBuddyRegistration {
        inner: BuddyRegistration,
    }

    impl Waiter for CountingWaiters {
        type Registration = CountingFixedRegistration;

        fn register(&self) -> Self::Registration {
            self.registrations.fetch_add(1, AtomicOrdering::Relaxed);
            CountingFixedRegistration {
                inner: Waiter::register(&self.inner),
            }
        }

        fn wake(&self) {
            self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
            Waiter::wake(&self.inner);
        }
    }

    impl BuddyWaiter for CountingWaiters {
        type Registration = CountingBuddyRegistration;

        fn register(&self, order: usize) -> Self::Registration {
            self.registrations.fetch_add(1, AtomicOrdering::Relaxed);
            CountingBuddyRegistration {
                inner: BuddyWaiter::register(&self.inner, order),
            }
        }

        fn wake(&self, freed_order: usize) {
            self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
            BuddyWaiter::wake(&self.inner, freed_order);
        }
    }

    impl WaitRegistration for CountingFixedRegistration {
        fn prepare(self: Pin<&mut Self>) {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.prepare();
        }

        fn revoke(self: Pin<&mut Self>) {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.revoke();
        }

        fn consume(self: Pin<&mut Self>) {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.consume();
        }
    }

    impl Future for CountingFixedRegistration {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll(cx)
        }
    }

    impl WaitRegistration for CountingBuddyRegistration {
        fn prepare(self: Pin<&mut Self>) {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.prepare();
        }

        fn revoke(self: Pin<&mut Self>) {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.revoke();
        }
    }

    impl Future for CountingBuddyRegistration {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll(cx)
        }
    }

    // -- Fixed arena tests --

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl WakeCounter {
        fn count(&self) -> usize {
            self.0.load(AtomicOrdering::Relaxed)
        }
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    #[cfg(not(loom))]
    #[test]
    fn fixed_failed_wake_retry_does_not_rebroadcast() {
        let waiters = CountingWaiters::new(1);
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build_async_with(waiters.clone())
        .unwrap();
        let held = arena.allocate().unwrap();
        let attempts = AtomicUsize::new(0);
        let allocate = || {
            allocate_with_waiter(&waiters, || {
                let attempt = attempts.fetch_add(1, AtomicOrdering::Relaxed);
                // The assert limits the work in one poll, so a self-wake loop fails quickly.
                assert!(attempt < 32, "allocation retry budget exhausted");
                arena.allocate().ok()
            })
        };
        let mut first = Box::pin(allocate());
        let mut second = Box::pin(allocate());
        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 2);

        drop(held);
        let stolen = arena.allocate().unwrap();
        assert_eq!(wakes.count(), 1);
        for _ in 0..8 {
            assert!(first.as_mut().poll(&mut cx).is_pending());
            assert!(second.as_mut().poll(&mut cx).is_pending());
        }
        assert!(
            attempts.load(AtomicOrdering::Relaxed) <= 4,
            "one stolen slot caused {} allocation attempts",
            attempts.load(AtomicOrdering::Relaxed)
        );
        assert_eq!(wakes.count(), 1, "handled wake reached a parked peer");

        drop(stolen);
        let Poll::Ready(buffer) = second.as_mut().poll(&mut cx) else {
            panic!("oldest waiter did not receive the next free slot");
        };
        drop(buffer);
        let Poll::Ready(buffer) = first.as_mut().poll(&mut cx) else {
            panic!("re-registered waiter did not receive the following free slot");
        };
        drop(buffer);
        assert_eq!(
            waiters.inner.inner.fixed_slot.count.load(Ordering::Acquire),
            0
        );
    }

    #[cfg(not(loom))]
    #[test]
    fn fixed_successful_wake_retry_consumes_registration() {
        let waiters = CountingWaiters::new(1);
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build_async_with(waiters.clone())
        .unwrap();
        let held = arena.allocate().unwrap();
        let mut first = Box::pin(arena.allocate_async());
        let mut second = Box::pin(arena.allocate_async());
        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());

        drop(held);
        let Poll::Ready(buffer) = first.as_mut().poll(&mut cx) else {
            panic!("woken waiter did not allocate the free slot");
        };
        assert_eq!(wakes.count(), 1, "successful retry re-broadcast its wake");
        assert_eq!(
            waiters.registrations(),
            2,
            "retry replaced its registration"
        );
        assert!(second.as_mut().poll(&mut cx).is_pending());

        drop(buffer);
        assert_eq!(wakes.count(), 2);
        assert!(second.as_mut().poll(&mut cx).is_ready());
        assert_eq!(
            waiters.inner.inner.fixed_slot.count.load(Ordering::Acquire),
            0
        );
    }

    #[cfg(not(loom))]
    #[test]
    fn fixed_cancelled_wake_propagates_before_and_after_poll() {
        for observe_wake in [false, true] {
            let waiters = NotifyWaiters::new(1);
            let mut first = Box::pin(Waiter::register(&waiters));
            let mut second = Box::pin(Waiter::register(&waiters));
            let mut third = Box::pin(Waiter::register(&waiters));
            let wakes = Arc::new(WakeCounter::default());
            let waker = Waker::from(Arc::clone(&wakes));
            let mut cx = Context::from_waker(&waker);
            for registration in [&mut first, &mut second, &mut third] {
                registration.as_mut().prepare();
                assert!(registration.as_mut().poll(&mut cx).is_pending());
            }

            Waiter::wake(&waiters);
            if observe_wake {
                assert!(first.as_mut().poll(&mut cx).is_ready());
            }
            drop(first);
            drop(second);
            assert!(third.as_mut().poll(&mut cx).is_ready());
            assert_eq!(wakes.count(), 3, "cancelled chain lost its wake");
            drop(third);
            assert_eq!(waiters.inner.fixed_slot.count.load(Ordering::Acquire), 0);

            let mut next = Box::pin(Waiter::register(&waiters));
            next.as_mut().prepare();
            assert!(next.as_mut().poll(&mut cx).is_pending());
        }
    }

    #[test]
    fn fixed_custom_registration_revoke_only_before_completion() {
        struct CustomWaiter;

        struct CustomRegistration {
            polls: usize,
        }

        impl Waiter for CustomWaiter {
            type Registration = CustomRegistration;

            fn register(&self) -> Self::Registration {
                CustomRegistration { polls: 0 }
            }

            fn wake(&self) {}
        }

        impl WaitRegistration for CustomRegistration {
            fn prepare(self: Pin<&mut Self>) {}

            fn revoke(self: Pin<&mut Self>) {
                assert!(self.polls < 2, "revoke called after completion");
            }
        }

        impl Future for CustomRegistration {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                self.polls += 1;
                if self.polls == 1 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }

        for successful_attempt in [1, 2, 4] {
            let mut attempts = 0;
            let mut allocation = Box::pin(allocate_with_waiter(&CustomWaiter, || {
                attempts += 1;
                assert!(attempts <= successful_attempt, "retry budget exhausted");
                (attempts == successful_attempt).then_some(())
            }));
            let mut cx = Context::from_waker(Waker::noop());
            let mut completed = false;
            for _ in 0..4 {
                if allocation.as_mut().poll(&mut cx).is_ready() {
                    completed = true;
                    break;
                }
            }
            assert!(completed, "custom registration did not complete");
        }
    }

    #[tokio::test]
    async fn allocate_async_basic() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build_async()
        .unwrap();
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"data");
        let bytes = buf.freeze();
        drop(bytes);
        let _buf2 = arena.allocate_async().await;
    }

    #[tokio::test]
    async fn allocate_async_waits_then_succeeds() {
        let waiters = CountingWaiters::new(1);
        let arena = Arc::new(
            FixedArena::with_slot_capacity(
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(32).unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"blocking");
        let bytes = buf.freeze();
        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2.allocate_async().await;
            buf.capacity()
        });
        waiters.wait_for_registrations(baseline + 1).await;
        drop(bytes);
        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 32);
    }

    #[tokio::test]
    async fn sync_allocate_still_fast_fails() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build_async()
        .unwrap();
        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[tokio::test]
    async fn multiple_waiters_all_served() {
        let waiters = CountingWaiters::new(1);
        let arena = Arc::new(
            FixedArena::with_slot_capacity(
                NonZeroUsize::new(2).unwrap(),
                NonZeroUsize::new(32).unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf1 = arena.allocate().unwrap();
        let buf2 = arena.allocate().unwrap();
        let baseline = waiters.registrations();
        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move { a1.allocate_async().await.capacity() });
        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move { a2.allocate_async().await.capacity() });
        waiters.wait_for_registrations(baseline + 2).await;
        drop(buf1);
        drop(buf2);
        let (r1, r2) = tokio::join!(
            timeout(Duration::from_secs(2), h1),
            timeout(Duration::from_secs(2), h2),
        );
        assert_eq!(r1.unwrap().unwrap(), 32);
        assert_eq!(r2.unwrap().unwrap(), 32);
    }

    #[tokio::test]
    async fn deref_exposes_sync_methods() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        )
        .build_async()
        .unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[tokio::test]
    async fn fixed_cancellation_no_leak() {
        let waiters = CountingWaiters::new(1);
        let arena = Arc::new(
            FixedArena::with_slot_capacity(
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(32).unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate().unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async().await });
        waiters.wait_for_registrations(baseline + 1).await;
        handle.abort();
        let _ = handle.await;

        drop(buf);
        let _buf2 = arena.allocate().unwrap();
    }

    /// A woken registration that is dropped without allocating must propagate
    /// the wake to the next waiter. Without this, the Notify permit is consumed
    /// and the second waiter stalls forever.
    #[tokio::test(flavor = "current_thread")]
    async fn fixed_woken_drop_propagates_to_next_waiter() {
        let waiters = Arc::new(NotifyWaiters::new(1));

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

        // Task A: registers, gets woken, then drops registration without
        // allocating — simulates a cancelled task that consumed the permit.
        let w = Arc::clone(&waiters);
        let h_a = tokio::spawn(async move {
            let reg = Waiter::register(&*w);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            ready_tx.send(()).ok();
            reg.await;
            // Drop reg without doing anything — permit consumed, no allocation
        });

        // Wait for task A to register
        ready_rx.await.ok();

        // Task B: registers after A
        let (b_ready_tx, b_ready_rx) = tokio::sync::oneshot::channel::<()>();
        let w2 = Arc::clone(&waiters);
        let h_b = tokio::spawn(async move {
            let reg = Waiter::register(&*w2);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            b_ready_tx.send(()).ok();
            reg.await;
        });

        // Wait for task B to register and prepare
        b_ready_rx.await.ok();

        // One wake — task A gets the permit (first in Notify queue)
        Waiter::wake(&*waiters);

        // Let task A run to completion (consumes permit, drops registration)
        let _ = h_a.await;

        // Task B must complete — propagation from A's drop must re-notify
        timeout(Duration::from_secs(2), h_b)
            .await
            .expect("task B must not stall when A drops after wake")
            .expect("task B should not panic");
    }

    /// When the woken waiter is the last waiter (count == 0), dropping it
    /// must NOT store a stale permit. A stale permit would cause the next
    /// registrant to immediately wake, fail allocation, drop, re-notify —
    /// creating a hot retry loop.
    #[tokio::test(flavor = "current_thread")]
    async fn fixed_last_waiter_woken_drop_no_stale_permit() {
        let waiters = Arc::new(NotifyWaiters::new(1));

        // Single waiter — no peers
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let w = Arc::clone(&waiters);
        let h = tokio::spawn(async move {
            let reg = Waiter::register(&*w);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            ready_tx.send(()).ok();
            reg.await;
        });

        ready_rx.await.ok();

        // Wake the sole waiter
        Waiter::wake(&*waiters);
        let _ = h.await;

        // No waiters remain. If a stale permit was stored, the next
        // registration would resolve immediately (spurious wake).
        let (ready_tx2, ready_rx2) = tokio::sync::oneshot::channel::<()>();
        let w2 = Arc::clone(&waiters);
        let h2 = tokio::spawn(async move {
            let reg = Waiter::register(&*w2);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            ready_tx2.send(()).ok();
            // This must NOT resolve immediately — no real wake happened
            reg.await;
        });

        ready_rx2.await.ok();

        // h2 should still be pending (no stale permit)
        assert!(!h2.is_finished(), "stale permit caused spurious wake");

        // Clean up: wake h2 so it completes
        Waiter::wake(&*waiters);
        timeout(Duration::from_secs(1), h2)
            .await
            .expect("cleanup wake should work")
            .expect("no panic");
    }

    #[tokio::test]
    async fn fixed_custom_waiter_supported() {
        let waiters = CountingWaiters::new(1);
        let arena = Arc::new(
            FixedArena::with_slot_capacity(
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(32).unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate().unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async().await.capacity() });
        waiters.wait_for_registrations(baseline + 1).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 32);
        assert!(waiters.registrations() > baseline);
        assert!(waiters.wakes() >= 1);
    }

    // -- Buddy arena tests --

    #[tokio::test]
    async fn buddy_allocate_async_waits_then_succeeds() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2
                .allocate_async(NonZeroUsize::new(2048).unwrap())
                .await
                .unwrap();
            buf.capacity()
        });

        waiters.wait_for_registrations(baseline + 1).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 2048);
    }

    #[tokio::test]
    async fn buddy_allocate_async_oversized_fails_fast() {
        let arena = BuddyArena::builder(
            BuddyGeometry::exact(
                NonZeroUsize::new(4096).unwrap(),
                NonZeroUsize::new(512).unwrap(),
            )
            .unwrap(),
        )
        .build_async()
        .unwrap();

        // A request larger than the whole arena can never be satisfied by any
        // free or coalesce, so it must return immediately rather than park.
        let err = timeout(
            Duration::from_secs(1),
            arena.allocate_async(NonZeroUsize::new(8192).unwrap()),
        )
        .await
        .expect("oversized request must not hang")
        .unwrap_err();
        assert_eq!(err, crate::AllocError::RequestTooLarge);
    }

    #[tokio::test]
    async fn buddy_multiple_waiters_all_served() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf1 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
        let buf2 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move {
            a1.allocate_async(NonZeroUsize::new(2048).unwrap())
                .await
                .unwrap()
                .capacity()
        });
        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move {
            a2.allocate_async(NonZeroUsize::new(2048).unwrap())
                .await
                .unwrap()
                .capacity()
        });

        waiters.wait_for_registrations(baseline + 2).await;
        drop(buf1);
        drop(buf2);

        let (r1, r2) = tokio::join!(
            timeout(Duration::from_secs(2), h1),
            timeout(Duration::from_secs(2), h2),
        );
        assert_eq!(r1.unwrap().unwrap(), 2048);
        assert_eq!(r2.unwrap().unwrap(), 2048);
    }

    /// Per-order wake prevents the starvation scenario where a large waiter
    /// is starved because a small waiter steals the coalesced block.
    #[tokio::test]
    async fn buddy_large_waiter_not_starved_by_small() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf1 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
        let buf2 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();

        let (small_tx, small_rx) = tokio::sync::oneshot::channel::<()>();
        let (small_ready_tx, small_ready_rx) = tokio::sync::oneshot::channel::<()>();

        let baseline = waiters.registrations();
        let arena_large = Arc::clone(&arena);
        let large = tokio::spawn(async move {
            arena_large
                .allocate_async(NonZeroUsize::new(4096).unwrap())
                .await
                .unwrap()
                .capacity()
        });
        waiters.wait_for_registrations(baseline + 1).await;

        let arena_small = Arc::clone(&arena);
        let small = tokio::spawn(async move {
            let buf = arena_small
                .allocate_async(NonZeroUsize::new(512).unwrap())
                .await
                .unwrap();
            let cap = buf.capacity();
            small_ready_tx.send(()).ok();
            small_rx.await.ok();
            drop(buf);
            cap
        });
        waiters.wait_for_registrations(baseline + 2).await;

        drop(buf1);
        // Small waiter is woken (freed order 2 satisfies order 0) and allocates.
        small_ready_rx.await.ok();

        drop(buf2);

        small_tx.send(()).ok();

        let large_cap = timeout(Duration::from_secs(2), large)
            .await
            .expect("large waiter should not starve")
            .expect("task should not panic");
        assert_eq!(large_cap, 4096);

        let small_cap = timeout(Duration::from_secs(2), small)
            .await
            .expect("small waiter should complete")
            .expect("task should not panic");
        assert_eq!(small_cap, 512);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn buddy_large_request_unblocks_after_coalesce() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf1 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
        let buf2 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2
                .allocate_async(NonZeroUsize::new(4096).unwrap())
                .await
                .unwrap();
            buf.capacity()
        });

        waiters.wait_for_registrations(baseline + 1).await;
        drop(buf1);
        // Freed order 2 only wakes orders 0..=2; the order-3 waiter is not
        // woken, so it remains registered and the handle stays pending.
        tokio::task::yield_now().await;
        assert!(!handle.is_finished());
        drop(buf2);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 4096);
    }

    #[tokio::test]
    async fn buddy_cancellation_does_not_leak() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle =
            tokio::spawn(
                async move { arena2.allocate_async(NonZeroUsize::new(512).unwrap()).await },
            );
        waiters.wait_for_registrations(baseline + 1).await;
        handle.abort();
        let _ = handle.await;

        drop(buf);
        let _buf2 = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn buddy_custom_waiter_supported() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            arena2
                .allocate_async(NonZeroUsize::new(2048).unwrap())
                .await
                .unwrap()
                .capacity()
        });
        waiters.wait_for_registrations(baseline + 1).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 2048);
        assert!(waiters.registrations() > baseline);
        assert!(waiters.wakes() >= 1);
    }

    /// Multiple waiters at different orders are all served from one freed block
    /// via buddy splitting.
    #[tokio::test]
    async fn buddy_multi_order_waiters_served_via_split() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );
        let buf = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();

        let baseline = waiters.registrations();
        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move {
            a1.allocate_async(NonZeroUsize::new(2048).unwrap())
                .await
                .unwrap()
                .capacity()
        });

        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move {
            a2.allocate_async(NonZeroUsize::new(512).unwrap())
                .await
                .unwrap()
                .capacity()
        });

        waiters.wait_for_registrations(baseline + 2).await;

        drop(buf);

        let (r1, r2) = tokio::join!(
            timeout(Duration::from_secs(2), h1),
            timeout(Duration::from_secs(2), h2),
        );
        assert_eq!(r1.unwrap().unwrap(), 2048);
        assert_eq!(r2.unwrap().unwrap(), 512);
    }

    // -- Race-heavy cancellation/wake interleaving tests --

    /// Many concurrent cancellations interleaved with wakes must not corrupt
    /// the count invariant or double-take tx.
    #[tokio::test]
    async fn buddy_cancel_wake_interleaving_count_invariant() {
        let waiters = CountingWaiters::new(5);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(8192).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );

        for _ in 0..20 {
            // Allocate individual blocks so each free wakes one waiter
            let mut bufs = Vec::new();
            while let Ok(buf) = arena.allocate(NonZeroUsize::new(512).unwrap()) {
                bufs.push(buf);
            }

            let waiter_count = 4;
            let cancel_count = 2;
            let baseline = waiters.registrations();
            let mut handles = Vec::new();
            for _ in 0..waiter_count {
                let a = Arc::clone(&arena);
                handles.push(tokio::spawn(async move {
                    a.allocate_async(NonZeroUsize::new(512).unwrap())
                        .await
                        .unwrap()
                }));
            }
            waiters
                .wait_for_registrations(baseline + waiter_count)
                .await;

            // Cancel some waiters (h.await ensures abort completes)
            for h in handles.drain(..cancel_count) {
                h.abort();
                let _ = h.await;
            }

            // Free enough blocks for remaining waiters
            let remaining = waiter_count - cancel_count;
            for buf in bufs.drain(..remaining) {
                drop(buf);
            }

            for h in handles {
                let buf = timeout(Duration::from_secs(2), h)
                    .await
                    .expect("waiter should complete")
                    .expect("task should not panic");
                drop(buf);
            }

            // Free remaining blocks
            drop(bufs);
        }
    }

    /// Teardown while waiters are live must not panic or leak.
    #[tokio::test]
    async fn buddy_teardown_with_live_waiters() {
        let waiters = CountingWaiters::new(4);
        for _ in 0..20 {
            let arena = Arc::new(
                BuddyArena::builder(
                    BuddyGeometry::exact(
                        NonZeroUsize::new(4096).unwrap(),
                        NonZeroUsize::new(512).unwrap(),
                    )
                    .unwrap(),
                )
                .build_async_with(waiters.clone())
                .unwrap(),
            );
            let _buf = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();

            let baseline = waiters.registrations();
            let mut handles = Vec::new();
            for _ in 0..4 {
                let a = Arc::clone(&arena);
                handles.push(tokio::spawn(async move {
                    a.allocate_async(NonZeroUsize::new(512).unwrap())
                        .await
                        .unwrap()
                }));
            }
            waiters.wait_for_registrations(baseline + 4).await;

            // Drop arena while waiters are still registered — they hold Arc clones
            // so no UB, but the waiters will never complete
            drop(arena);
            drop(_buf);

            // Abort all waiters — this exercises the Drop path with Live state
            for h in handles {
                h.abort();
                let _ = h.await;
            }
        }
    }

    #[cfg(not(loom))]
    fn revoked_entry() -> Arc<WaiterEntry> {
        let tombstone = Arc::new(WaiterEntry::new(oneshot::channel::<usize>().0, 0));
        tombstone.state.store(REVOKED, AtomicOrdering::Relaxed);
        tombstone
    }

    /// Revoked entries in front of a live waiter must not count against the
    /// wake bound. If they count, a run of at least `MAX_DELIVERIES_PER_WAKE`
    /// cancellations stops each free before the live waiter, and the block
    /// stays free.
    #[cfg(not(loom))]
    #[test]
    fn buddy_wake_reaches_live_waiter_behind_tombstones() {
        for tombstones in [
            MAX_DELIVERIES_PER_WAKE - 1,
            MAX_DELIVERIES_PER_WAKE,
            MAX_DELIVERIES_PER_WAKE + 1,
            64,
            1000,
        ] {
            let waiters = NotifyWaiters::new(1);
            let slot = &waiters.inner.buddy_orders[0];
            let (live_tx, mut live_rx) = oneshot::channel::<usize>();
            let live = Arc::new(WaiterEntry::new(live_tx, 1));
            {
                let mut queue = slot.queue.lock().unwrap();
                for _ in 0..tombstones {
                    queue.push_back(revoked_entry());
                }
                queue.push_back(Arc::clone(&live));
                slot.count.store(1, Ordering::Release);
                slot.head_timestamp.store(1, AtomicOrdering::Release);
            }

            waiters.inner.buddy_wake(0);

            assert_eq!(
                live_rx.try_recv().ok(),
                Some(0),
                "live waiter behind {tombstones} tombstones was not woken"
            );
            assert_eq!(live.state.load(AtomicOrdering::Relaxed), WOKEN);
            assert_eq!(slot.count.load(Ordering::Acquire), 0);
            assert!(slot.queue.lock().unwrap().is_empty());
            assert_eq!(
                slot.head_timestamp.load(AtomicOrdering::Acquire),
                NO_WAITERS_TIMESTAMP
            );
        }
    }

    /// Tombstone skips must not remove the fan-out bound. The test puts a
    /// tombstone run in front of a live waiter at more candidate orders than
    /// the bound. One wake call delivers a number of wakes equal to the bound.
    /// Each waiter that it does not reach stays queued with its count unchanged.
    #[cfg(not(loom))]
    #[test]
    fn buddy_wake_delivery_bound_holds_across_tombstone_runs() {
        let orders = MAX_DELIVERIES_PER_WAKE + 2;
        let waiters = NotifyWaiters::new(orders);
        let mut live = Vec::new();
        for order in 0..orders {
            let slot = &waiters.inner.buddy_orders[order];
            let (tx, rx) = oneshot::channel::<usize>();
            let entry = Arc::new(WaiterEntry::new(tx, order as u64));
            let mut queue = slot.queue.lock().unwrap();
            for _ in 0..MAX_DELIVERIES_PER_WAKE {
                queue.push_back(revoked_entry());
            }
            queue.push_back(Arc::clone(&entry));
            slot.count.store(1, Ordering::Release);
            slot.head_timestamp
                .store(order as u64, AtomicOrdering::Release);
            live.push((entry, rx));
        }

        waiters.inner.buddy_wake(orders - 1);

        let mut delivered = 0;
        for (order, (entry, rx)) in live.iter_mut().enumerate() {
            let slot = &waiters.inner.buddy_orders[order];
            if rx.try_recv().is_ok() {
                delivered += 1;
                assert_eq!(entry.state.load(AtomicOrdering::Relaxed), WOKEN);
                assert_eq!(slot.count.load(Ordering::Acquire), 0);
            } else {
                assert_eq!(entry.state.load(AtomicOrdering::Relaxed), LIVE);
                assert_eq!(slot.count.load(Ordering::Acquire), 1);
                let queue = slot.queue.lock().unwrap();
                assert!(queue.iter().any(|queued| Arc::ptr_eq(queued, entry)));
            }
        }
        assert_eq!(delivered, MAX_DELIVERIES_PER_WAKE);
    }

    #[cfg(not(loom))]
    #[test]
    fn buddy_wake_failed_send_reaches_same_order_successor() {
        struct CheckUnlocked {
            waiters: Arc<NotifyWaitersInner>,
            wakes: AtomicUsize,
        }

        impl std::task::Wake for CheckUnlocked {
            fn wake(self: Arc<Self>) {
                assert!(
                    self.waiters.buddy_orders[0].queue.try_lock().is_ok(),
                    "delivery invoked a waker while holding the queue lock"
                );
                self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }

        let waiters = NotifyWaiters::new(2);
        let slot = &waiters.inner.buddy_orders[0];
        let mut failed = waiters.buddy_register(0);
        let mut revoked = waiters.buddy_register(0);
        let mut successor = waiters.buddy_register(0);
        let mut remaining = waiters.buddy_register(0);
        for reg in [&mut failed, &mut revoked, &mut successor, &mut remaining] {
            Pin::new(reg).prepare();
        }
        Pin::new(&mut revoked).revoke();
        let probe = Arc::new(CheckUnlocked {
            waiters: Arc::clone(&waiters.inner),
            wakes: AtomicUsize::new(0),
        });
        let waker = std::task::Waker::from(Arc::clone(&probe));
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut successor).poll(&mut cx).is_pending());

        // Keep the entry live so the wake wins the CAS but cannot deliver.
        drop(failed.rx.take());
        waiters.inner.buddy_wake(1);

        assert_eq!(
            failed
                .entry
                .as_ref()
                .unwrap()
                .state
                .load(AtomicOrdering::Relaxed),
            WOKEN
        );
        assert_eq!(
            successor.rx.as_mut().unwrap().try_recv().ok(),
            Some(1),
            "failed delivery must offer the freed order to the same-order successor"
        );
        assert_eq!(probe.wakes.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(slot.count.load(Ordering::Acquire), 1);
        assert_eq!(slot.queue.lock().unwrap().len(), 1);
        assert_eq!(
            slot.head_timestamp.load(AtomicOrdering::Acquire),
            remaining.entry.as_ref().unwrap().timestamp
        );
        assert_eq!(waiters.inner.streak_state.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(
            remaining.rx.as_mut().unwrap().try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );

        drop(failed);
        drop(revoked);
        drop(successor);
        assert_eq!(slot.count.load(Ordering::Acquire), 1);
        assert_eq!(
            remaining
                .entry
                .as_ref()
                .unwrap()
                .state
                .load(AtomicOrdering::Relaxed),
            LIVE
        );
        drop(remaining);
        assert_eq!(slot.count.load(Ordering::Acquire), 0);
    }

    /// A failed send is a cancellation that occurs after the wake wins the CAS.
    /// Like a revoked entry, it must not count against the wake bound. If it
    /// counts, a run of failed sends hides the same-order survivor from this
    /// free.
    #[cfg(not(loom))]
    #[test]
    fn buddy_wake_failed_sends_do_not_hide_survivor() {
        for earlier_delivery in [false, true] {
            let waiters = NotifyWaiters::new(2);
            let mut earlier = waiters.buddy_register(0);
            if earlier_delivery {
                Pin::new(&mut earlier).prepare();
                waiters.inner.buddy_orders[0]
                    .head_timestamp
                    .store(0, AtomicOrdering::Release);
            }
            waiters.inner.streak_state.store(3, AtomicOrdering::Relaxed);

            let slot = &waiters.inner.buddy_orders[1];
            let mut failed = Vec::new();
            let (tx, mut survivor_rx) = oneshot::channel();
            let timestamp = ORDER_BONUS_NS * 4 + DEPTH_BONUS_NS * MAX_DELIVERIES_PER_WAKE as u64;
            let survivor = Arc::new(WaiterEntry::new(
                tx,
                timestamp + MAX_DELIVERIES_PER_WAKE as u64,
            ));
            {
                let mut queue = slot.queue.lock().unwrap();
                for i in 0..MAX_DELIVERIES_PER_WAKE {
                    let (tx, rx) = oneshot::channel();
                    drop(rx);
                    let entry = Arc::new(WaiterEntry::new(tx, timestamp + i as u64));
                    queue.push_back(Arc::clone(&entry));
                    failed.push(entry);
                }
                queue.push_back(Arc::clone(&survivor));
                slot.count
                    .store(MAX_DELIVERIES_PER_WAKE + 1, Ordering::Release);
                slot.head_timestamp
                    .store(timestamp, AtomicOrdering::Release);
            }
            let expected_orders: &[usize] = if earlier_delivery { &[0, 1] } else { &[1] };
            assert_eq!(waiters.inner.score_orders(1).as_slice(), expected_orders);

            waiters.inner.buddy_wake(1);

            for entry in &failed {
                assert_eq!(entry.state.load(AtomicOrdering::Relaxed), WOKEN);
            }
            assert_eq!(
                survivor_rx.try_recv().ok(),
                Some(1),
                "failed sends hid the survivor from this free"
            );
            assert_eq!(survivor.state.load(AtomicOrdering::Relaxed), WOKEN);
            if earlier_delivery {
                assert_eq!(earlier.rx.as_mut().unwrap().try_recv().ok(), Some(1));
            }
            assert_eq!(
                waiters.inner.buddy_orders[0].count.load(Ordering::Acquire),
                0
            );
            assert_eq!(slot.count.load(Ordering::Acquire), 0);
            assert!(slot.queue.lock().unwrap().is_empty());
            assert_eq!(
                slot.head_timestamp.load(AtomicOrdering::Acquire),
                NO_WAITERS_TIMESTAMP
            );
            assert_eq!(
                waiters.inner.streak_state.load(AtomicOrdering::Relaxed),
                (1u64 << 32) | 1
            );
        }
    }

    #[cfg(not(loom))]
    #[test]
    fn buddy_wake_failed_send_without_successor_preserves_streak() {
        let waiters = NotifyWaiters::new(2);
        let slot = &waiters.inner.buddy_orders[1];
        let mut failed = waiters.buddy_register(1);
        Pin::new(&mut failed).prepare();
        drop(failed.rx.take());
        waiters.inner.streak_state.store(3, AtomicOrdering::Relaxed);

        waiters.inner.buddy_wake(1);

        assert_eq!(
            failed
                .entry
                .as_ref()
                .unwrap()
                .state
                .load(AtomicOrdering::Relaxed),
            WOKEN
        );
        assert_eq!(slot.count.load(Ordering::Acquire), 0);
        assert!(slot.queue.lock().unwrap().is_empty());
        assert_eq!(
            slot.head_timestamp.load(AtomicOrdering::Acquire),
            NO_WAITERS_TIMESTAMP
        );
        assert_eq!(waiters.inner.streak_state.load(AtomicOrdering::Relaxed), 3);
        drop(failed);
        assert_eq!(slot.count.load(Ordering::Acquire), 0);
    }

    // -- Wake propagation regression tests --

    /// A registration dropped while holding an undelivered wake (cancelled
    /// after the oneshot fired but before it was polled) must propagate the
    /// freed order to the next waiter instead of swallowing it.
    #[cfg(not(loom))]
    #[test]
    fn buddy_unpolled_wake_drop_propagates_to_next_waiter() {
        let waiters = NotifyWaiters::new(1);

        let mut reg1 = waiters.buddy_register(0);
        let mut reg2 = waiters.buddy_register(0);
        let entry1 = Arc::clone(reg1.entry.as_ref().unwrap());
        let entry2 = Arc::clone(reg2.entry.as_ref().unwrap());
        Pin::new(&mut reg1).prepare();
        Pin::new(&mut reg2).prepare();

        waiters.inner.buddy_wake(0);
        assert_eq!(entry1.state.load(AtomicOrdering::Relaxed), WOKEN);
        assert_eq!(entry2.state.load(AtomicOrdering::Relaxed), LIVE);

        drop(reg1);

        assert_eq!(
            entry2.state.load(AtomicOrdering::Relaxed),
            WOKEN,
            "unconsumed wake lost on drop"
        );
    }

    /// A registration dropped after observing its wake (polled Ready) must
    /// NOT re-broadcast it. In the allocation loop this drop point is only
    /// reachable after a failed retry, and the failed retry proves the block
    /// is gone; re-broadcasting from re-registering waiters turns one free
    /// into a perpetual wake storm among parked waiters under saturation.
    #[cfg(not(loom))]
    #[test]
    fn buddy_polled_wake_drop_does_not_rebroadcast() {
        let waiters = NotifyWaiters::new(1);

        let mut reg1 = waiters.buddy_register(0);
        let mut reg2 = waiters.buddy_register(0);
        let entry2 = Arc::clone(reg2.entry.as_ref().unwrap());
        Pin::new(&mut reg1).prepare();
        Pin::new(&mut reg2).prepare();

        waiters.inner.buddy_wake(0);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(Pin::new(&mut reg1).poll(&mut cx).is_ready());

        drop(reg1);

        assert_eq!(
            entry2.state.load(AtomicOrdering::Relaxed),
            LIVE,
            "observed wake re-broadcast: propagation does not converge"
        );
    }

    /// A wake that lands unpolled after revoke (the waiter allocated without
    /// ever observing the delivery) is recovered from the receiver at drop
    /// and re-routed to the next waiter.
    #[cfg(not(loom))]
    #[test]
    fn buddy_drop_recovers_wake_unobserved_by_revoke() {
        let waiters = NotifyWaiters::new(1);

        let mut reg1 = waiters.buddy_register(0);
        let mut reg2 = waiters.buddy_register(0);
        let entry2 = Arc::clone(reg2.entry.as_ref().unwrap());
        Pin::new(&mut reg1).prepare();
        Pin::new(&mut reg2).prepare();

        // Wake delivered but never polled; the fast path then succeeds and
        // revokes without ever observing the delivery.
        waiters.inner.buddy_wake(0);
        Pin::new(&mut reg1).revoke();

        drop(reg1);

        assert_eq!(
            entry2.state.load(AtomicOrdering::Relaxed),
            WOKEN,
            "wake delivered after revoke was lost instead of re-routed"
        );
        assert_eq!(reg2.rx.as_mut().unwrap().try_recv().ok(), Some(0));
    }

    /// An unobserved wake tunnels through a chain of cancelled waiters until
    /// it reaches a live one. The chain consumes one queue entry per hop and
    /// cancelled waiters do not re-register, so it always terminates.
    #[cfg(not(loom))]
    #[test]
    fn buddy_cancelled_chain_propagates_transitively() {
        let waiters = NotifyWaiters::new(1);

        let mut reg1 = waiters.buddy_register(0);
        let mut reg2 = waiters.buddy_register(0);
        let mut reg3 = waiters.buddy_register(0);
        let entry2 = Arc::clone(reg2.entry.as_ref().unwrap());
        let entry3 = Arc::clone(reg3.entry.as_ref().unwrap());
        Pin::new(&mut reg1).prepare();
        Pin::new(&mut reg2).prepare();
        Pin::new(&mut reg3).prepare();

        // Delivered to reg1, which dies without polling: reg2 inherits.
        waiters.inner.buddy_wake(0);
        drop(reg1);
        assert_eq!(entry2.state.load(AtomicOrdering::Relaxed), WOKEN);

        // reg2 also dies without polling: the wake must reach reg3.
        drop(reg2);
        assert_eq!(
            entry3.state.load(AtomicOrdering::Relaxed),
            WOKEN,
            "wake lost inside a cancelled-waiter chain"
        );
    }

    /// End-to-end storm regression: one free against always-failing waiters
    /// must produce a bounded number of retries, not a self-sustaining wake
    /// loop among re-registering waiters.
    #[tokio::test(flavor = "current_thread")]
    async fn buddy_single_free_causes_bounded_retries() {
        let waiters = Arc::new(NotifyWaiters::new(1));
        let retries = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let w = Arc::clone(&waiters);
            let r = Arc::clone(&retries);
            tokio::spawn(async move {
                allocate_with_buddy_waiter(w.as_ref(), 0, move || {
                    r.fetch_add(1, AtomicOrdering::Relaxed);
                    None::<()>
                })
                .await
            });
        }
        while waiters.inner.buddy_orders[0].count.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
        let baseline = retries.load(AtomicOrdering::Relaxed);

        waiters.inner.buddy_wake(0);
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }

        let after_wake = retries.load(AtomicOrdering::Relaxed) - baseline;
        assert!(
            after_wake <= 4,
            "wake storm: {after_wake} retries from a single free"
        );
    }

    /// End-to-end through the allocation loop: a waiter cancelled after its
    /// wake was delivered but before its task ran hands the wake to the next
    /// waiter instead of stalling it until an unrelated free.
    #[tokio::test(flavor = "current_thread")]
    async fn buddy_cancelled_after_delivery_hands_wake_to_next_waiter() {
        let waiters = Arc::new(NotifyWaiters::new(1));
        let available = Arc::new(AtomicUsize::new(0));

        // Waiter A parks first (front of the queue).
        let wa = Arc::clone(&waiters);
        let a =
            tokio::spawn(
                async move { allocate_with_buddy_waiter(wa.as_ref(), 0, || None::<()>).await },
            );
        while waiters.inner.buddy_orders[0].count.load(Ordering::Acquire) < 1 {
            tokio::task::yield_now().await;
        }

        // Waiter B succeeds once a block is available.
        let wb = Arc::clone(&waiters);
        let avail = Arc::clone(&available);
        let b = tokio::spawn(async move {
            allocate_with_buddy_waiter(wb.as_ref(), 0, move || {
                (avail.load(AtomicOrdering::Acquire) > 0).then_some(())
            })
            .await
        });
        while waiters.inner.buddy_orders[0].count.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }

        // Deliver to A, then cancel A before it can poll: current_thread
        // runtime with no yield in between, so A never observes the wake.
        available.store(1, AtomicOrdering::Release);
        waiters.inner.buddy_wake(0);
        a.abort();
        let _ = a.await;

        timeout(Duration::from_secs(5), b)
            .await
            .expect("recovered wake never reached the next waiter")
            .unwrap();
    }

    /// Two waiters that always fail, with a run of `MAX_DELIVERIES_PER_WAKE * 4`
    /// tombstones between them. Each free must cause one retry only. The woken
    /// waiter re-queues behind the other waiter, so the next free must reach the
    /// other waiter behind the tombstones. A free must not fan out into more
    /// wakes among the parked waiters.
    #[tokio::test(flavor = "current_thread")]
    async fn buddy_each_free_past_tombstones_causes_one_retry() {
        let waiters = Arc::new(NotifyWaiters::new(1));
        let retries = Arc::new(AtomicUsize::new(0));
        let spawn_failing = |waiters: &Arc<NotifyWaiters>| {
            let w = Arc::clone(waiters);
            let r = Arc::clone(&retries);
            tokio::spawn(async move {
                allocate_with_buddy_waiter(w.as_ref(), 0, move || {
                    r.fetch_add(1, AtomicOrdering::Relaxed);
                    None::<()>
                })
                .await
            })
        };
        let slot = &waiters.inner.buddy_orders[0];

        let first = spawn_failing(&waiters);
        while slot.count.load(Ordering::Acquire) < 1 {
            tokio::task::yield_now().await;
        }
        // The live head stops the prefix removal, so these cancellations stay in
        // the queue as tombstones.
        let tombstones = MAX_DELIVERIES_PER_WAKE * 4;
        for _ in 0..tombstones {
            let mut registration = waiters.buddy_register(0);
            Pin::new(&mut registration).prepare();
            drop(registration);
        }
        let second = spawn_failing(&waiters);
        while slot.count.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(slot.queue.lock().unwrap().len(), tombstones + 2);
        let baseline = retries.load(AtomicOrdering::Relaxed);

        for free in 1..=6 {
            waiters.inner.buddy_wake(0);
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                retries.load(AtomicOrdering::Relaxed) - baseline,
                free,
                "free {free} did not cause exactly one retry"
            );
            assert_eq!(slot.count.load(Ordering::Acquire), 2);
        }

        first.abort();
        second.abort();
    }

    /// End-to-end tests where a woken waiter does not take the block. A run of
    /// cancelled waiters is between it and the next live waiter. The free that
    /// follows, or the wake recovered from the cancelled waiter, must reach the
    /// live waiter with no additional free.
    #[cfg(not(loom))]
    mod buddy_tombstone_run {
        use tokio::task::JoinHandle;

        use super::*;

        const BLOCK: usize = 8192;

        fn nz(n: usize) -> NonZeroUsize {
            NonZeroUsize::new(n).unwrap()
        }

        fn spawn_waiter(arena: &AsyncBuddyArena) -> JoinHandle<()> {
            let arena = arena.clone();
            tokio::spawn(async move {
                drop(arena.allocate_async(nz(BLOCK)).await.unwrap());
            })
        }

        // Each parked waiter records one failed allocation after it queues.
        async fn wait_for_failures(arena: &AsyncBuddyArena, expected: u64) {
            while arena.metrics().allocations_failed < expected {
                tokio::task::yield_now().await;
            }
        }

        /// Queue `[live, n cancelled, live]` behind a held single-block arena.
        async fn stage(n: usize) -> (AsyncBuddyArena, Buffer, JoinHandle<()>, JoinHandle<()>) {
            let arena =
                BuddyArena::builder(BuddyGeometry::exact(nz(BLOCK), nz(BLOCK / 2)).unwrap())
                    .build_async()
                    .unwrap();
            let held = arena.allocate(nz(BLOCK)).unwrap();

            let first = spawn_waiter(&arena);
            wait_for_failures(&arena, 1).await;
            let cancelled: Vec<_> = (0..n).map(|_| spawn_waiter(&arena)).collect();
            wait_for_failures(&arena, n as u64 + 1).await;
            for handle in &cancelled {
                handle.abort();
            }
            for handle in cancelled {
                assert!(handle.await.unwrap_err().is_cancelled());
            }
            let live = spawn_waiter(&arena);
            wait_for_failures(&arena, n as u64 + 2).await;
            (arena, held, first, live)
        }

        /// A sync allocation takes the block before the woken waiter, and the
        /// waiter re-queues at the back. The free of that sync allocation must
        /// reach the waiter behind the run.
        #[tokio::test(flavor = "current_thread")]
        async fn requeued_waiter_does_not_strand_successor() {
            for n in [MAX_DELIVERIES_PER_WAKE, 64] {
                let (arena, held, first, live) = stage(n).await;

                drop(held);
                let stolen = arena.allocate(nz(BLOCK)).unwrap();
                wait_for_failures(&arena, n as u64 + 3).await;
                drop(stolen);

                timeout(Duration::from_secs(2), live)
                    .await
                    .unwrap_or_else(|_| panic!("waiter behind {n} cancellations stranded"))
                    .unwrap();
                timeout(Duration::from_secs(2), first)
                    .await
                    .expect("re-queued waiter never served")
                    .unwrap();
            }
        }

        /// The test cancels the woken waiter before it polls. The wake that its
        /// drop recovers must reach the waiter behind the run.
        #[tokio::test(flavor = "current_thread")]
        async fn cancelled_delivery_reaches_successor() {
            for n in [MAX_DELIVERIES_PER_WAKE, 64] {
                let (_arena, held, first, live) = stage(n).await;

                drop(held);
                first.abort();
                assert!(first.await.unwrap_err().is_cancelled());

                timeout(Duration::from_secs(2), live)
                    .await
                    .unwrap_or_else(|_| panic!("waiter behind {n} cancellations stranded"))
                    .unwrap();
            }
        }
    }

    // -- Fairness regression tests --

    /// Under repeated small frees, a large waiter must eventually be served
    /// (not starved indefinitely). This tests the scoring system's time-based
    /// priority escalation.
    #[tokio::test(flavor = "current_thread")]
    async fn buddy_fairness_large_not_starved_by_repeated_small() {
        let waiters = CountingWaiters::new(5);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(8192).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );

        // Fill the arena with small blocks
        let mut bufs = Vec::new();
        while let Ok(buf) = arena.allocate(NonZeroUsize::new(512).unwrap()) {
            bufs.push(buf);
        }

        // Large waiter wants 4096
        let base = waiters.registrations();
        let arena_large = Arc::clone(&arena);
        let large_handle = tokio::spawn(async move {
            arena_large
                .allocate_async(NonZeroUsize::new(4096).unwrap())
                .await
                .unwrap()
                .capacity()
        });
        waiters.wait_for_registrations(base + 1).await;

        // Small waiter wants 512
        let arena_small = Arc::clone(&arena);
        let (small_done_tx, small_done_rx) = tokio::sync::oneshot::channel::<()>();
        let small_handle = tokio::spawn(async move {
            let buf = arena_small
                .allocate_async(NonZeroUsize::new(512).unwrap())
                .await
                .unwrap();
            let cap = buf.capacity();
            // Hold until signaled
            small_done_rx.await.ok();
            drop(buf);
            cap
        });
        waiters.wait_for_registrations(base + 2).await;

        // Free blocks one at a time. On current_thread, yield_now() is
        // deterministic: each yield gives the woken task exactly one poll
        // cycle to process its allocation attempt before the next free.
        for buf in bufs.drain(..) {
            drop(buf);
            tokio::task::yield_now().await;
        }

        // Signal small to release its buffer
        small_done_tx.send(()).ok();

        let large_cap = timeout(Duration::from_secs(5), large_handle)
            .await
            .expect("large waiter must not starve")
            .expect("task should not panic");
        assert_eq!(large_cap, 4096);

        let small_cap = timeout(Duration::from_secs(2), small_handle)
            .await
            .expect("small waiter should complete")
            .expect("task should not panic");
        assert_eq!(small_cap, 512);
    }

    /// Mixed small/large free patterns over many iterations should serve
    /// all waiters without deadlock.
    #[tokio::test]
    async fn buddy_fairness_mixed_sizes_no_deadlock() {
        let waiters = CountingWaiters::new(5);
        let arena = Arc::new(
            BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(8192).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build_async_with(waiters.clone())
            .unwrap(),
        );

        for round in 0..10 {
            let size = if round % 2 == 0 { 2048 } else { 512 };
            let buf = arena.allocate(NonZeroUsize::new(size).unwrap()).unwrap();

            let baseline = waiters.registrations();
            let a = Arc::clone(&arena);
            let handle = tokio::spawn(async move {
                a.allocate_async(NonZeroUsize::new(size).unwrap())
                    .await
                    .unwrap()
                    .capacity()
            });

            waiters.wait_for_registrations(baseline + 1).await;
            drop(buf);

            let cap = timeout(Duration::from_secs(2), handle)
                .await
                .expect("waiter should not deadlock")
                .expect("task should not panic");
            assert_eq!(cap, size);
        }
    }

    #[cfg(not(loom))]
    mod buddy_cancellation {
        use super::*;

        fn register_at(waiters: &NotifyWaiters, order: usize, timestamp: u64) -> BuddyRegistration {
            let mut registration = waiters.buddy_register(order);
            registration.entry = None;
            let entry = registration.pending_entry.as_mut().unwrap();
            Arc::get_mut(entry).unwrap().timestamp = timestamp;
            registration.entry = Some(Arc::clone(entry));
            Pin::new(&mut registration).prepare();
            registration
        }

        fn cancel(registration: &mut Option<BuddyRegistration>, revoke: bool) {
            if revoke {
                Pin::new(registration.as_mut().unwrap()).revoke();
            } else {
                drop(registration.take());
            }
        }

        fn assert_slot(waiters: &NotifyWaiters, count: usize, timestamp: u64) {
            let slot = &waiters.inner.buddy_orders[0];
            assert_eq!(slot.count.load(Ordering::Acquire), count);
            assert_eq!(slot.head_timestamp.load(AtomicOrdering::Acquire), timestamp);
        }

        fn head_cancellation(revoke: bool) {
            let waiters = NotifyWaiters::new(2);
            let mut head = Some(register_at(&waiters, 0, 100_000));
            let mut next = register_at(&waiters, 0, 900_000);
            let _competitor = register_at(&waiters, 1, 500_000);
            assert_eq!(waiters.inner.score_orders(1), [0, 1]);

            cancel(&mut head, revoke);

            assert_slot(&waiters, 1, 900_000);
            assert_eq!(waiters.inner.score_orders(1), [1, 0]);
            drop(head);
            assert_slot(&waiters, 1, 900_000);
            waiters.inner.buddy_wake(0);
            assert_eq!(next.rx.as_mut().unwrap().try_recv(), Ok(0));
            assert_slot(&waiters, 0, NO_WAITERS_TIMESTAMP);
        }

        #[test]
        fn revoke_head_repairs_timestamp_and_score() {
            head_cancellation(true);
        }

        #[test]
        fn drop_head_repairs_timestamp_and_score() {
            head_cancellation(false);
        }

        fn nonhead_cancellation(revoke: bool) {
            let waiters = NotifyWaiters::new(1);
            let _head = register_at(&waiters, 0, 100);
            let mut middle = Some(register_at(&waiters, 0, 100));
            let _tail = register_at(&waiters, 0, 300);

            cancel(&mut middle, revoke);

            assert_slot(&waiters, 2, 100);
            let queue = waiters.inner.buddy_orders[0].queue.lock().unwrap();
            assert_eq!(queue.len(), 3, "nonhead cancellation should stay lazy");
            assert_eq!(queue[1].state.load(AtomicOrdering::Acquire), REVOKED);
            drop(queue);
            drop(middle);
            assert_slot(&waiters, 2, 100);
        }

        #[test]
        fn revoke_nonhead_preserves_timestamp() {
            nonhead_cancellation(true);
        }

        #[test]
        fn drop_nonhead_preserves_timestamp() {
            nonhead_cancellation(false);
        }

        fn last_cancellation(revoke: bool) {
            let waiters = NotifyWaiters::new(1);
            let mut last = Some(register_at(&waiters, 0, 100));

            cancel(&mut last, revoke);

            assert_slot(&waiters, 0, NO_WAITERS_TIMESTAMP);
            assert!(
                waiters.inner.buddy_orders[0]
                    .queue
                    .lock()
                    .unwrap()
                    .is_empty()
            );
            let _replacement = register_at(&waiters, 0, 200);
            drop(last);
            assert_slot(&waiters, 1, 200);
        }

        #[test]
        fn revoke_last_resets_timestamp() {
            last_cancellation(true);
        }

        #[test]
        fn drop_last_resets_timestamp() {
            last_cancellation(false);
        }

        fn tombstone_chain(revoke: bool) {
            let waiters = NotifyWaiters::new(1);
            let mut head = Some(register_at(&waiters, 0, 100));
            for timestamp in 0..(MAX_DELIVERIES_PER_WAKE * 2) {
                let mut tombstone = Some(register_at(&waiters, 0, timestamp as u64));
                cancel(&mut tombstone, revoke);
            }
            let mut survivor = register_at(&waiters, 0, 300);
            assert_slot(&waiters, 2, 100);

            cancel(&mut head, revoke);

            assert_slot(&waiters, 1, 300);
            assert_eq!(waiters.inner.buddy_orders[0].queue.lock().unwrap().len(), 1);
            waiters.inner.buddy_wake(0);
            assert_eq!(survivor.rx.as_mut().unwrap().try_recv(), Ok(0));
            assert_slot(&waiters, 0, NO_WAITERS_TIMESTAMP);
        }

        #[test]
        fn revoke_head_clears_tombstone_chain() {
            tombstone_chain(true);
        }

        #[test]
        fn drop_head_clears_tombstone_chain() {
            tombstone_chain(false);
        }

        struct OnWake(Mutex<Option<Box<dyn FnOnce() + Send>>>);

        impl std::task::Wake for OnWake {
            fn wake(self: Arc<Self>) {
                if let Some(callback) = self.0.lock().unwrap().take() {
                    callback();
                }
            }
        }

        #[test]
        fn sender_callbacks_observe_repair_and_can_reenter() {
            for revoke in [false, true] {
                let waiters = NotifyWaiters::new(1);
                let mut head = Some(register_at(&waiters, 0, 100));
                let next = Arc::new(Mutex::new(Some(register_at(&waiters, 0, 200))));
                let result = Arc::new(Mutex::new(None));
                let callback_waiters = waiters.clone();
                let callback_result = Arc::clone(&result);
                let callback_next = Arc::clone(&next);
                let waker = std::task::Waker::from(Arc::new(OnWake(Mutex::new(Some(Box::new(
                    move || {
                        if callback_waiters.inner.buddy_orders[0]
                            .queue
                            .try_lock()
                            .is_err()
                        {
                            return;
                        }
                        let slot = &callback_waiters.inner.buddy_orders[0];
                        let observed = (
                            slot.count.load(Ordering::Acquire),
                            slot.head_timestamp.load(AtomicOrdering::Acquire),
                        );
                        drop(callback_next.lock().unwrap().take());
                        let replacement = register_at(&callback_waiters, 0, 300);
                        *callback_result.lock().unwrap() = Some((observed, replacement));
                    },
                ))))));
                let mut cx = Context::from_waker(&waker);
                assert!(Pin::new(head.as_mut().unwrap()).poll(&mut cx).is_pending());

                cancel(&mut head, revoke);

                let result = result.lock().unwrap();
                let (observed, _) = result
                    .as_ref()
                    .expect("callback must run with queue unlocked");
                assert_eq!(*observed, (1, 200));
                assert_slot(&waiters, 1, 300);
            }
        }

        #[test]
        fn concurrent_updates_during_sender_drop_are_not_overwritten() {
            for revoke in [false, true] {
                let waiters = NotifyWaiters::new(1);
                let mut head = Some(register_at(&waiters, 0, 100));
                let next = register_at(&waiters, 0, 200);
                let (entered_tx, entered_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let callback_waiters = waiters.clone();
                let waker = std::task::Waker::from(Arc::new(OnWake(Mutex::new(Some(Box::new(
                    move || {
                        let slot = &callback_waiters.inner.buddy_orders[0];
                        let snapshot = slot.queue.try_lock().ok().map(|_queue| {
                            (
                                slot.count.load(Ordering::Acquire),
                                slot.head_timestamp.load(AtomicOrdering::Acquire),
                            )
                        });
                        entered_tx.send(snapshot).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    },
                ))))));
                let mut cx = Context::from_waker(&waker);
                assert!(Pin::new(head.as_mut().unwrap()).poll(&mut cx).is_pending());

                let cancellation = std::thread::spawn(move || {
                    cancel(&mut head, revoke);
                    head
                });
                let observed = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let replacement = if observed.is_some() {
                    drop(next);
                    let mut delivered = register_at(&waiters, 0, 300);
                    waiters.inner.buddy_wake(0);
                    assert_eq!(delivered.rx.as_mut().unwrap().try_recv(), Ok(0));
                    Some(register_at(&waiters, 0, 400))
                } else {
                    None
                };
                release_tx.send(()).unwrap();
                drop(cancellation.join().unwrap());

                assert_eq!(observed, Some((1, 200)), "repair must precede callbacks");
                assert_slot(&waiters, 1, 400);
                drop(replacement);
                assert_slot(&waiters, 0, NO_WAITERS_TIMESTAMP);
            }
        }
    }
}

/// Loom models of the `WaiterEntry` state machine alone. The models examine the
/// LIVE to WOKEN and LIVE to REVOKED transitions, and that only the winner
/// takes the sender. They also examine that the holder that releases its
/// reference last drops the entry. The queue, the oneshot channel and tokio
/// are out of scope, because loom cannot see into them.
#[cfg(all(test, loom))]
mod loom_tests {
    use alloc::vec::Vec;

    use loom::thread;

    use super::{LIVE, REVOKED, WOKEN, WaiterEntry};
    use crate::sync::Arc;
    use crate::sync::atomic::{AtomicUsize, Ordering};

    /// Takes the place of the oneshot sender. It counts drops, so a sender that
    /// the entry hands out two times, or a leaked sender, gives a count other
    /// than 1.
    struct Token(Arc<AtomicUsize>);

    impl Drop for Token {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn entry() -> (Arc<WaiterEntry<Token>>, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let entry = Arc::new(WaiterEntry::new(Token(Arc::clone(&drops)), 0));
        (entry, drops)
    }

    /// Starts one thread per item in `claims`. Each thread claims the given
    /// transition, or for `None` only drops its reference. The function drops
    /// the reference of the caller first, so the thread that releases its
    /// reference last drops the entry. Returns the transition that each thread
    /// won, if any, and the drop count of the payload.
    fn race(claims: &[Option<u8>]) -> (Vec<Option<u8>>, usize) {
        let (entry, drops) = entry();
        let handles: Vec<_> = claims
            .iter()
            .map(|&to| {
                let entry = Arc::clone(&entry);
                thread::spawn(move || {
                    let to = to?;
                    entry.claim(to).map(|tx| {
                        drop(tx);
                        to
                    })
                })
            })
            .collect();
        drop(entry);
        let winners = handles.into_iter().map(|h| h.join().unwrap()).collect();
        (winners, drops.load(Ordering::Relaxed))
    }

    #[test]
    fn loom_waiter_entry_wake_vs_cancel_hands_out_sender_once() {
        loom::model(|| {
            let (entry, drops) = entry();
            let wake = {
                let entry = Arc::clone(&entry);
                thread::spawn(move || entry.claim(WOKEN).map(drop).is_some())
            };
            let cancel = {
                let entry = Arc::clone(&entry);
                thread::spawn(move || entry.claim(REVOKED).map(drop).is_some())
            };
            let woke = wake.join().unwrap();
            let revoked = cancel.join().unwrap();

            assert!(woke != revoked, "exactly one side must win");
            let expected = if woke { WOKEN } else { REVOKED };
            assert_eq!(entry.state.load(Ordering::Relaxed), expected);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(entry.claim(WOKEN).is_none());
            assert!(entry.claim(REVOKED).is_none());
            drop(entry);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        });
    }

    /// Wake, cancel and a holder that only drops its reference. The main thread
    /// releases its reference first, so any of the 3 threads can drop the entry.
    #[test]
    fn loom_waiter_entry_wake_cancel_and_drop() {
        loom::model(|| {
            let (winners, drops) = race(&[Some(WOKEN), Some(REVOKED), None]);
            let won: Vec<_> = winners.into_iter().flatten().collect();
            assert_eq!(won.len(), 1, "sender handed out {} times", won.len());
            assert_eq!(drops, 1);
        });
    }

    /// No thread claims the entry. The holder that releases its reference last
    /// drops the entry, and the sender drops one time with it.
    #[test]
    fn loom_waiter_entry_unclaimed_sender_dropped_with_entry() {
        loom::model(|| {
            let (winners, drops) = race(&[None, None]);
            assert!(winners.iter().all(Option::is_none));
            assert_eq!(drops, 1);
        });
    }

    /// Two claims for the same transition. Production code never does this,
    /// because an entry leaves the queue one time only, under the queue lock.
    /// In this case too, `claim` must hand out the sender one time only.
    #[test]
    fn loom_waiter_entry_duplicate_wake_claims_hand_out_once() {
        loom::model(|| {
            let (winners, drops) = race(&[Some(WOKEN), Some(WOKEN)]);
            assert_eq!(winners.iter().flatten().count(), 1);
            assert_eq!(drops, 1);
        });
    }

    #[test]
    fn loom_waiter_entry_starts_live() {
        loom::model(|| {
            let (entry, drops) = entry();
            assert_eq!(entry.state.load(Ordering::Relaxed), LIVE);
            drop(entry);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        });
    }
}
