use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::sync::Notify;
use tokio::sync::futures::OwnedNotified;
use tokio::sync::oneshot;

use crate::buffer::Buffer;
use crate::sync::atomic::{AtomicUsize, Ordering};
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

    /// Revoke the registration when the retry succeeds immediately.
    fn revoke(self: Pin<&mut Self>);
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

struct WaiterEntry {
    state: AtomicU8,
    // SAFETY: accessed exactly once by whichever side wins the CAS on `state`.
    // See the safety contract in .plan/2026-03-06-buddy-wake-starvation.md.
    tx: UnsafeCell<Option<oneshot::Sender<usize>>>,
    timestamp: u64,
    #[allow(dead_code)]
    order: usize,
}

impl WaiterEntry {
    fn new(tx: oneshot::Sender<usize>, timestamp: u64, order: usize) -> Self {
        Self {
            state: AtomicU8::new(LIVE),
            tx: UnsafeCell::new(Some(tx)),
            timestamp,
            order,
        }
    }

    /// Take the oneshot sender out of this entry.
    ///
    /// # Safety
    ///
    /// Caller must have won the CAS on `state` (Live→Woken or Live→Revoked).
    /// This guarantees exclusive access — no other code path will call this.
    unsafe fn take_tx(&self) -> Option<oneshot::Sender<usize>> {
        unsafe { (*self.tx.get()).take() }
    }
}

// SAFETY: tx is only accessed by the CAS winner, so no data races.
// The oneshot::Sender is Send, and AtomicU8/u64/usize are Sync.
unsafe impl Send for WaiterEntry {}
unsafe impl Sync for WaiterEntry {}

// ---------------------------------------------------------------------------
// Buddy wake internals — per-order state + scoring
// ---------------------------------------------------------------------------

/// Sentinel: no waiters at this order.
const NO_WAITERS_TIMESTAMP: u64 = u64::MAX;

/// Maximum stale entries to pop per wake call (bounds free-path work).
const MAX_POPS_PER_WAKE: usize = 8;

/// Maximum consecutive wins by one order before it's banned.
const MAX_CONSECUTIVE: u32 = 10;

/// Nanosecond bonus per order level for scoring (higher order = looks older).
const ORDER_BONUS_NS: u64 = 50_000; // 50μs per order level

/// Per-waiter depth bonus in nanoseconds.
const DEPTH_BONUS_NS: u64 = 5_000; // 5μs per waiting peer

struct BuddyOrderSlot {
    queue: Mutex<std::collections::VecDeque<Arc<WaiterEntry>>>,
    count: AtomicUsize,
    head_timestamp: AtomicU64,
}

impl BuddyOrderSlot {
    fn new() -> Self {
        Self {
            queue: Mutex::new(std::collections::VecDeque::new()),
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
/// For fixed arenas: single `Notify`-based FIFO (order 0 only).
/// For buddy arenas: per-order mutex queues with CAS-arbitrated oneshot
/// delivery and 4-factor scoring to prevent starvation.
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

    // -- Buddy wake scoring --

    fn score_orders(&self, freed_order: usize) -> Vec<usize> {
        let max = freed_order.min(self.inner.buddy_orders.len() - 1);
        let streak = self.inner.streak_state.load(AtomicOrdering::Relaxed);
        let last_winner = (streak >> 32) as usize;
        let streak_count = streak as u32;

        // Collect all eligible orders first, then apply ban as post-filter.
        // Building candidates before ban check avoids order-dependent evaluation
        // where early-scanned orders see an empty candidate list.
        let mut candidates: Vec<(usize, u64)> = Vec::new();

        for order in 0..=max {
            let slot = &self.inner.buddy_orders[order];
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
        let current = self.inner.streak_state.load(AtomicOrdering::Relaxed);
        let last_winner = (current >> 32) as usize;
        let new = if winner_order == last_winner {
            let streak = (current as u32).saturating_add(1);
            ((winner_order as u64) << 32) | streak as u64
        } else {
            ((winner_order as u64) << 32) | 1u64
        };
        self.inner.streak_state.store(new, AtomicOrdering::Relaxed);
    }

    fn update_head_timestamp(
        &self,
        queue: &std::collections::VecDeque<Arc<WaiterEntry>>,
        order: usize,
    ) {
        let slot = &self.inner.buddy_orders[order];
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
        let mut pops: usize = 0;

        // Deliver one wake per candidate order. A freed block at order N can
        // serve waiters at multiple lower orders via splitting, so we wake
        // across orders rather than stopping at the first delivery.
        for order in candidates {
            let mut queue = self.inner.buddy_orders[order].queue.lock().unwrap();
            while let Some(entry) = queue.pop_front() {
                pops += 1;
                if pops > MAX_POPS_PER_WAKE {
                    return;
                }

                if entry.state.load(AtomicOrdering::Relaxed) != LIVE {
                    continue;
                }

                if entry
                    .state
                    .compare_exchange(LIVE, WOKEN, AtomicOrdering::AcqRel, AtomicOrdering::Relaxed)
                    .is_ok()
                {
                    self.inner.buddy_orders[order]
                        .count
                        .fetch_sub(1, Ordering::Release);
                    self.update_head_timestamp(&queue, order);
                    drop(queue);

                    // SAFETY: we won the CAS Live→Woken
                    let tx = unsafe { entry.take_tx() };
                    if let Some(tx) = tx
                        && tx.send(freed_order).is_ok()
                    {
                        self.update_streak(order);
                    }
                    // Move to next order (one delivery per order)
                    break;
                }
            }
        }
    }

    fn buddy_register(&self, order: usize) -> BuddyRegistration {
        let order = order.min(self.inner.buddy_orders.len() - 1);
        let (tx, rx) = oneshot::channel();
        let timestamp = self.now_ns();
        let entry = Arc::new(WaiterEntry::new(tx, timestamp, order));

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
        self.buddy_wake(freed_order);
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
        // If we consumed a Notify permit (poll returned Ready) but were dropped
        // before the allocation loop could retry, propagate the wake so the
        // next waiter isn't stalled. The OwnedNotified won't propagate on its
        // own because it was already polled to completion.
        // Only propagate when another waiter exists — otherwise the stored
        // permit creates a hot retry loop under full-arena conditions.
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
        if this.registered {
            if let Some(entry) = &this.entry
                && entry
                    .state
                    .compare_exchange(
                        LIVE,
                        REVOKED,
                        AtomicOrdering::AcqRel,
                        AtomicOrdering::Relaxed,
                    )
                    .is_ok()
            {
                // SAFETY: we won the CAS Live→Revoked
                let _tx = unsafe { entry.take_tx() };
                this.waiters.buddy_orders[this.order]
                    .count
                    .fetch_sub(1, Ordering::Release);
            }
            this.registered = false;
        }
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
        if self.registered
            && let Some(entry) = &self.entry
            && entry
                .state
                .compare_exchange(
                    LIVE,
                    REVOKED,
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Relaxed,
                )
                .is_ok()
        {
            // SAFETY: we won the CAS Live→Revoked
            let _tx = unsafe { entry.take_tx() };
            self.waiters.buddy_orders[self.order]
                .count
                .fetch_sub(1, Ordering::Release);
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
        registration.await;
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
/// Created via [`FixedArenaBuilder::build_async()`]. Provides
/// [`allocate_async()`](AsyncFixedArena::allocate_async) which parks
/// until a slot becomes available, while sync methods remain accessible
/// through `Deref<Target = FixedArena>`.
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
/// Created via [`BuddyArenaBuilder::build_async()`]. Provides
/// [`allocate_async()`](AsyncBuddyArena::allocate_async) which parks
/// until a large-enough block becomes available, while sync methods remain
/// accessible through `Deref<Target = BuddyArena>`.
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
    pub async fn allocate_async(&self, len: std::num::NonZeroUsize) -> Buffer {
        let order = self
            .order_for_request(len.get())
            .unwrap_or(self.max_order());
        allocate_with_buddy_waiter(self.waiters.as_ref(), order, || {
            self.inner.allocate(len).ok()
        })
        .await
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
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use bytes::BufMut;
    use tokio::time::{Duration, timeout};

    use crate::BuddyArena;
    use crate::BuddyGeometry;
    use crate::FixedArena;

    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

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

    #[tokio::test]
    async fn allocate_async_basic() {
        let arena = FixedArena::builder(nz(1), nz(32)).build_async().unwrap();
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"data");
        let bytes = buf.freeze();
        drop(bytes);
        let _buf2 = arena.allocate_async().await;
    }

    #[tokio::test]
    async fn allocate_async_waits_then_succeeds() {
        let arena = Arc::new(FixedArena::builder(nz(1), nz(32)).build_async().unwrap());
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"blocking");
        let bytes = buf.freeze();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2.allocate_async().await;
            buf.capacity()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bytes);
        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 32);
    }

    #[tokio::test]
    async fn sync_allocate_still_fast_fails() {
        let arena = FixedArena::builder(nz(1), nz(32)).build_async().unwrap();
        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[tokio::test]
    async fn multiple_waiters_all_served() {
        let arena = Arc::new(FixedArena::builder(nz(2), nz(32)).build_async().unwrap());
        let buf1 = arena.allocate().unwrap();
        let buf2 = arena.allocate().unwrap();
        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move { a1.allocate_async().await.capacity() });
        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move { a2.allocate_async().await.capacity() });
        tokio::time::sleep(Duration::from_millis(50)).await;
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
        let arena = FixedArena::builder(nz(4), nz(64)).build_async().unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[tokio::test]
    async fn fixed_cancellation_no_leak() {
        let arena = Arc::new(FixedArena::builder(nz(1), nz(32)).build_async().unwrap());
        let buf = arena.allocate().unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
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
        let w2 = Arc::clone(&waiters);
        let h_b = tokio::spawn(async move {
            let reg = Waiter::register(&*w2);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            reg.await;
        });

        // Let task B register and start awaiting
        tokio::task::yield_now().await;

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
        let w = Arc::clone(&waiters);
        let h = tokio::spawn(async move {
            let reg = Waiter::register(&*w);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            reg.await;
        });

        tokio::task::yield_now().await;

        // Wake the sole waiter
        Waiter::wake(&*waiters);
        let _ = h.await;

        // No waiters remain. If a stale permit was stored, the next
        // registration would resolve immediately (spurious wake).
        let w2 = Arc::clone(&waiters);
        let h2 = tokio::spawn(async move {
            let reg = Waiter::register(&*w2);
            tokio::pin!(reg);
            reg.as_mut().prepare();
            // This must NOT resolve immediately — no real wake happened
            reg.await;
        });

        tokio::task::yield_now().await;

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
            FixedArena::builder(nz(1), nz(32))
                .build_async_with(waiters.clone())
                .unwrap(),
        );
        let buf = arena.allocate().unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async().await.capacity() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 32);
        assert!(waiters.registrations() >= 1);
        assert!(waiters.wakes() >= 1);
    }

    // -- Buddy arena tests --

    #[tokio::test]
    async fn buddy_allocate_async_waits_then_succeeds() {
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf = arena.allocate(nz(2048)).unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2.allocate_async(nz(2048)).await;
            buf.capacity()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 2048);
    }

    #[tokio::test]
    async fn buddy_multiple_waiters_all_served() {
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf1 = arena.allocate(nz(2048)).unwrap();
        let buf2 = arena.allocate(nz(2048)).unwrap();

        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move { a1.allocate_async(nz(2048)).await.capacity() });
        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move { a2.allocate_async(nz(2048)).await.capacity() });

        tokio::time::sleep(Duration::from_millis(50)).await;
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
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf1 = arena.allocate(nz(2048)).unwrap();
        let buf2 = arena.allocate(nz(2048)).unwrap();

        let (small_tx, small_rx) = tokio::sync::oneshot::channel::<()>();

        let arena_large = Arc::clone(&arena);
        let large =
            tokio::spawn(async move { arena_large.allocate_async(nz(4096)).await.capacity() });
        tokio::task::yield_now().await;

        let arena_small = Arc::clone(&arena);
        let small = tokio::spawn(async move {
            let buf = arena_small.allocate_async(nz(512)).await;
            let cap = buf.capacity();
            small_rx.await.ok();
            drop(buf);
            cap
        });
        tokio::task::yield_now().await;

        drop(buf1);
        tokio::task::yield_now().await;

        drop(buf2);
        tokio::task::yield_now().await;

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

    #[tokio::test]
    async fn buddy_large_request_unblocks_after_coalesce() {
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf1 = arena.allocate(nz(2048)).unwrap();
        let buf2 = arena.allocate(nz(2048)).unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2.allocate_async(nz(4096)).await;
            buf.capacity()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(buf1);
        tokio::time::sleep(Duration::from_millis(25)).await;
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
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf = arena.allocate(nz(4096)).unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async(nz(512)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        drop(buf);
        let _buf2 = arena.allocate(nz(4096)).unwrap();
    }

    #[tokio::test]
    async fn buddy_custom_waiter_supported() {
        let waiters = CountingWaiters::new(4);
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async_with(waiters.clone())
                .unwrap(),
        );
        let buf = arena.allocate(nz(2048)).unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async(nz(2048)).await.capacity() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(buf);

        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 2048);
        assert!(waiters.registrations() >= 1);
        assert!(waiters.wakes() >= 1);
    }

    /// Multiple waiters at different orders are all served from one freed block
    /// via buddy splitting.
    #[tokio::test]
    async fn buddy_multi_order_waiters_served_via_split() {
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );
        let buf = arena.allocate(nz(4096)).unwrap();

        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move { a1.allocate_async(nz(2048)).await.capacity() });

        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move { a2.allocate_async(nz(512)).await.capacity() });

        tokio::task::yield_now().await;

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
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(8192), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );

        for _ in 0..20 {
            // Allocate individual blocks so each free wakes one waiter
            let mut bufs = Vec::new();
            while let Ok(buf) = arena.allocate(nz(512)) {
                bufs.push(buf);
            }

            let waiter_count = 4;
            let cancel_count = 2;
            let mut handles = Vec::new();
            for _ in 0..waiter_count {
                let a = Arc::clone(&arena);
                handles.push(tokio::spawn(async move { a.allocate_async(nz(512)).await }));
            }
            tokio::task::yield_now().await;

            // Cancel some waiters
            for h in handles.drain(..cancel_count) {
                h.abort();
                let _ = h.await;
            }
            tokio::task::yield_now().await;

            // Free enough blocks for remaining waiters (one per waiter)
            let remaining = waiter_count - cancel_count;
            for buf in bufs.drain(..remaining) {
                drop(buf);
                tokio::task::yield_now().await;
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
        for _ in 0..20 {
            let arena = Arc::new(
                BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(512)).unwrap())
                    .build_async()
                    .unwrap(),
            );
            let _buf = arena.allocate(nz(4096)).unwrap();

            let mut handles = Vec::new();
            for _ in 0..4 {
                let a = Arc::clone(&arena);
                handles.push(tokio::spawn(async move { a.allocate_async(nz(512)).await }));
            }
            tokio::task::yield_now().await;

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

    // -- Fairness regression tests --

    /// Under repeated small frees, a large waiter must eventually be served
    /// (not starved indefinitely). This tests the scoring system's time-based
    /// priority escalation.
    #[tokio::test]
    async fn buddy_fairness_large_not_starved_by_repeated_small() {
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(8192), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );

        // Fill the arena with small blocks
        let mut bufs = Vec::new();
        while let Ok(buf) = arena.allocate(nz(512)) {
            bufs.push(buf);
        }

        // Large waiter wants 4096
        let arena_large = Arc::clone(&arena);
        let large_handle =
            tokio::spawn(async move { arena_large.allocate_async(nz(4096)).await.capacity() });
        tokio::task::yield_now().await;

        // Small waiter wants 512
        let arena_small = Arc::clone(&arena);
        let (small_done_tx, small_done_rx) = tokio::sync::oneshot::channel::<()>();
        let small_handle = tokio::spawn(async move {
            let buf = arena_small.allocate_async(nz(512)).await;
            let cap = buf.capacity();
            // Hold until signaled
            small_done_rx.await.ok();
            drop(buf);
            cap
        });
        tokio::task::yield_now().await;

        // Free blocks one at a time — small should get served first (lower order),
        // but eventually enough blocks free to coalesce for large
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
        let arena = Arc::new(
            BuddyArena::builder(BuddyGeometry::exact(nz(8192), nz(512)).unwrap())
                .build_async()
                .unwrap(),
        );

        for round in 0..10 {
            let size = if round % 2 == 0 { 2048 } else { 512 };
            let buf = arena.allocate(nz(size)).unwrap();

            let a = Arc::clone(&arena);
            let handle = tokio::spawn(async move { a.allocate_async(nz(size)).await.capacity() });

            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(buf);

            let cap = timeout(Duration::from_secs(2), handle)
                .await
                .expect("waiter should not deadlock")
                .expect("task should not panic");
            assert_eq!(cap, size);
        }
    }
}
