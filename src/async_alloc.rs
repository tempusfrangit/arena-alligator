use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::futures::OwnedNotified;

use crate::buffer::Buffer;
use crate::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use crate::{BuddyArena, FixedArena};

/// Policy for how `allocate_async` waits when the arena is full.
pub enum AsyncPolicy {
    /// Wake one waiter per slot free via `tokio::sync::Notify`.
    Notify,
    /// Lock-free LIFO waiter registry with per-waiter wake targeting.
    TreiberWaiters,
}

/// Wait strategy used by async arena allocation.
pub trait Waiter: Send + Sync + 'static {
    /// Future returned when a waiter registers interest in allocation progress.
    type Registration: WaitRegistration;

    /// Register a waiter before retrying allocation.
    fn register(&self) -> Self::Registration;

    /// Wake one waiter after allocation state changes.
    fn wake_one(&self);
}

/// Wait registration used by the shared retry loop.
pub trait WaitRegistration: Future<Output = ()> {
    /// Prepare the registration before the post-registration allocation retry.
    fn prepare(self: Pin<&mut Self>);

    /// Revoke the registration when the retry succeeds immediately.
    fn revoke(self: Pin<&mut Self>);
}

pub(crate) trait WakeOne: Send + Sync {
    fn wake_one(&self);
}

impl<W: Waiter> WakeOne for W {
    fn wake_one(&self) {
        Waiter::wake_one(self);
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
        self.inner.wake_one();
    }
}

/// Notify-based waiter policy.
#[derive(Clone, Default)]
pub struct NotifyWaiters {
    notify: Arc<tokio::sync::Notify>,
}

impl NotifyWaiters {
    /// Create a notify-based waiter policy.
    pub fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl Waiter for NotifyWaiters {
    type Registration = NotifyRegistration;

    fn register(&self) -> Self::Registration {
        NotifyRegistration {
            future: self.notify.clone().notified_owned(),
        }
    }

    fn wake_one(&self) {
        self.notify.notify_one();
    }
}

pub struct NotifyRegistration {
    future: OwnedNotified,
}

impl WaitRegistration for NotifyRegistration {
    fn prepare(self: Pin<&mut Self>) {
        let _ = self.project_future().enable();
    }

    fn revoke(self: Pin<&mut Self>) {}
}

impl Future for NotifyRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project_future().poll(cx)
    }
}

impl NotifyRegistration {
    fn project_future(self: Pin<&mut Self>) -> Pin<&mut OwnedNotified> {
        // SAFETY: pinning `NotifyRegistration` also pins its `future` field.
        unsafe { self.map_unchecked_mut(|this| &mut this.future) }
    }
}

struct WaiterNode {
    next: AtomicPtr<WaiterNode>,
    notify: Arc<tokio::sync::Notify>,
    revoked: AtomicBool,
}

struct TreiberStack {
    head: AtomicPtr<WaiterNode>,
}

// SAFETY: WaiterNode is behind Arc and only accessed through atomic operations.
// AtomicPtr and AtomicBool are Send+Sync; tokio::sync::Notify is Send+Sync.
unsafe impl Send for TreiberStack {}
unsafe impl Sync for TreiberStack {}

impl TreiberStack {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Push a waiter node onto the stack.
    /// `raw` must come from `Arc::into_raw`.
    fn push(&self, raw: *const WaiterNode) {
        let node = raw as *mut WaiterNode;
        loop {
            let head = self.head.load(Ordering::Relaxed);
            // SAFETY: node is valid (Arc keeps it alive) and no concurrent writer
            // touches next until the node is linked into the stack via CAS.
            unsafe {
                (*node).next.store(head, Ordering::Relaxed);
            }
            if self
                .head
                .compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Pop and wake the first non-revoked waiter. Skips tombstones.
    fn wake_one(&self) {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head.is_null() {
                return;
            }

            // SAFETY: head was pushed via Arc::into_raw and is valid while in the stack.
            let next = unsafe { (*head).next.load(Ordering::Relaxed) };
            if self
                .head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: reconstitute the Arc that was stored via Arc::into_raw.
                // Exactly one reconstitution per into_raw call.
                let node = unsafe { Arc::from_raw(head as *const WaiterNode) };
                if node.revoked.load(Ordering::Acquire) {
                    continue;
                }
                node.notify.notify_one();
                return;
            }
        }
    }
}

impl Drop for TreiberStack {
    fn drop(&mut self) {
        let mut current = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        while !current.is_null() {
            // SAFETY: each node was pushed via Arc::into_raw
            let node = unsafe { Arc::from_raw(current as *const WaiterNode) };
            current = node.next.load(Ordering::Relaxed);
        }
    }
}

/// Lock-free LIFO waiter policy with per-waiter wake targeting.
pub struct TreiberWaiters {
    stack: Arc<TreiberStack>,
}

impl TreiberWaiters {
    /// Create a Treiber-based waiter policy.
    pub fn new() -> Self {
        Self {
            stack: Arc::new(TreiberStack::new()),
        }
    }
}

impl Default for TreiberWaiters {
    fn default() -> Self {
        Self::new()
    }
}

impl Waiter for TreiberWaiters {
    type Registration = TreiberRegistration;

    fn register(&self) -> Self::Registration {
        let node = Arc::new(WaiterNode {
            next: AtomicPtr::new(ptr::null_mut()),
            notify: Arc::new(tokio::sync::Notify::new()),
            revoked: AtomicBool::new(false),
        });

        TreiberRegistration {
            node: Arc::clone(&node),
            stack: Arc::clone(&self.stack),
            future: node.notify.clone().notified_owned(),
            published: false,
        }
    }

    fn wake_one(&self) {
        self.stack.wake_one();
    }
}

pub struct TreiberRegistration {
    node: Arc<WaiterNode>,
    stack: Arc<TreiberStack>,
    future: OwnedNotified,
    published: bool,
}

impl WaitRegistration for TreiberRegistration {
    fn prepare(mut self: Pin<&mut Self>) {
        let _ = self.as_mut().project_future().enable();
        // SAFETY: updating `published` does not move the pinned future field.
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if !this.published {
            this.stack.push(Arc::into_raw(Arc::clone(&this.node)));
            this.published = true;
        }
    }

    fn revoke(self: Pin<&mut Self>) {
        self.node.revoked.store(true, Ordering::Release);
    }
}

impl Future for TreiberRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project_future().poll(cx)
    }
}

impl TreiberRegistration {
    fn project_future(self: Pin<&mut Self>) -> Pin<&mut OwnedNotified> {
        // SAFETY: pinning `TreiberRegistration` also pins its `future` field.
        unsafe { self.map_unchecked_mut(|this| &mut this.future) }
    }
}

#[doc(hidden)]
pub enum BuiltInWaiters {
    Notify(NotifyWaiters),
    Treiber(TreiberWaiters),
}

impl Waiter for BuiltInWaiters {
    type Registration = BuiltInRegistration;

    fn register(&self) -> Self::Registration {
        match self {
            Self::Notify(waiters) => BuiltInRegistration::Notify(waiters.register()),
            Self::Treiber(waiters) => BuiltInRegistration::Treiber(waiters.register()),
        }
    }

    fn wake_one(&self) {
        match self {
            Self::Notify(waiters) => Waiter::wake_one(waiters),
            Self::Treiber(waiters) => Waiter::wake_one(waiters),
        }
    }
}

#[doc(hidden)]
pub enum BuiltInRegistration {
    Notify(NotifyRegistration),
    Treiber(TreiberRegistration),
}

impl WaitRegistration for BuiltInRegistration {
    fn prepare(self: Pin<&mut Self>) {
        // SAFETY: pinning the enum pins the active registration variant in place.
        unsafe {
            match self.get_unchecked_mut() {
                Self::Notify(registration) => Pin::new_unchecked(registration).prepare(),
                Self::Treiber(registration) => Pin::new_unchecked(registration).prepare(),
            }
        }
    }

    fn revoke(self: Pin<&mut Self>) {
        // SAFETY: pinning the enum pins the active registration variant in place.
        unsafe {
            match self.get_unchecked_mut() {
                Self::Notify(registration) => Pin::new_unchecked(registration).revoke(),
                Self::Treiber(registration) => Pin::new_unchecked(registration).revoke(),
            }
        }
    }
}

impl Future for BuiltInRegistration {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: pinning the enum pins the active registration variant in place.
        unsafe {
            match self.get_unchecked_mut() {
                Self::Notify(registration) => Pin::new_unchecked(registration).poll(cx),
                Self::Treiber(registration) => Pin::new_unchecked(registration).poll(cx),
            }
        }
    }
}

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

/// Async-capable wrapper around [`FixedArena`].
///
/// Created via [`FixedArenaBuilder::build_async()`]. Provides
/// [`allocate_async()`](AsyncFixedArena::allocate_async) which parks
/// until a slot becomes available, while sync methods remain accessible
/// through `Deref<Target = FixedArena>`.
#[derive(Clone)]
pub struct AsyncFixedArena<W = BuiltInWaiters> {
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

impl<W: Waiter> AsyncBuddyArena<W> {
    /// Allocate a buffer, waiting asynchronously if the arena is full.
    ///
    /// The buddy bitmaps remain the source of truth; notifications are hints
    /// to retry after free or coalesce publishes a usable block.
    pub async fn allocate_async(&self, len: std::num::NonZeroUsize) -> Buffer {
        allocate_with_waiter(self.waiters.as_ref(), || self.inner.allocate(len).ok()).await
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use bytes::BufMut;
    use tokio::time::{Duration, timeout};

    use crate::BuddyArena;
    use crate::FixedArena;

    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[derive(Clone)]
    struct CountingWaiters {
        inner: NotifyWaiters,
        registrations: Arc<AtomicUsize>,
        wakes: Arc<AtomicUsize>,
    }

    impl CountingWaiters {
        fn new() -> Self {
            Self {
                inner: NotifyWaiters::new(),
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

    struct CountingRegistration {
        inner: NotifyRegistration,
    }

    impl Waiter for CountingWaiters {
        type Registration = CountingRegistration;

        fn register(&self) -> Self::Registration {
            self.registrations.fetch_add(1, AtomicOrdering::Relaxed);
            CountingRegistration {
                inner: self.inner.register(),
            }
        }

        fn wake_one(&self) {
            self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
            Waiter::wake_one(&self.inner);
        }
    }

    impl WaitRegistration for CountingRegistration {
        fn prepare(self: Pin<&mut Self>) {
            self.project_inner().prepare();
        }

        fn revoke(self: Pin<&mut Self>) {
            self.project_inner().revoke();
        }
    }

    impl Future for CountingRegistration {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.project_inner().poll(cx)
        }
    }

    impl CountingRegistration {
        fn project_inner(self: Pin<&mut Self>) -> Pin<&mut NotifyRegistration> {
            // SAFETY: pinning `CountingRegistration` also pins its inner registration.
            unsafe { self.map_unchecked_mut(|this| &mut this.inner) }
        }
    }

    #[tokio::test]
    async fn allocate_async_basic() {
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"data");
        let bytes = buf.freeze();
        drop(bytes);
        let _buf2 = arena.allocate_async().await;
    }

    #[tokio::test]
    async fn allocate_async_waits_then_succeeds() {
        let arena = Arc::new(
            FixedArena::builder(nz(1), nz(32))
                .build_async(AsyncPolicy::Notify)
                .unwrap(),
        );
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
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[tokio::test]
    async fn multiple_waiters_all_served() {
        let arena = Arc::new(
            FixedArena::builder(nz(2), nz(32))
                .build_async(AsyncPolicy::Notify)
                .unwrap(),
        );
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
        let arena = FixedArena::builder(nz(4), nz(64))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[tokio::test]
    async fn treiber_allocate_async_basic() {
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::TreiberWaiters)
            .unwrap();
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"data");
        let bytes = buf.freeze();
        drop(bytes);
        let _buf2 = arena.allocate_async().await;
    }

    #[tokio::test]
    async fn treiber_waits_then_succeeds() {
        let arena = Arc::new(
            FixedArena::builder(nz(1), nz(32))
                .build_async(AsyncPolicy::TreiberWaiters)
                .unwrap(),
        );
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
    async fn treiber_multiple_waiters() {
        let arena = Arc::new(
            FixedArena::builder(nz(2), nz(32))
                .build_async(AsyncPolicy::TreiberWaiters)
                .unwrap(),
        );
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
    async fn treiber_cancellation_no_leak() {
        let arena = Arc::new(
            FixedArena::builder(nz(1), nz(32))
                .build_async(AsyncPolicy::TreiberWaiters)
                .unwrap(),
        );
        let buf = arena.allocate().unwrap();

        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move { arena2.allocate_async().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        drop(buf);

        let _buf2 = arena.allocate().unwrap();
    }

    #[tokio::test]
    async fn treiber_sync_still_fast_fails() {
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::TreiberWaiters)
            .unwrap();
        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[tokio::test]
    async fn buddy_allocate_async_waits_then_succeeds() {
        let arena = Arc::new(
            BuddyArena::builder(nz(4096), nz(512))
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
            BuddyArena::builder(nz(4096), nz(512))
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

    #[tokio::test]
    async fn buddy_large_request_unblocks_after_coalesce() {
        let arena = Arc::new(
            BuddyArena::builder(nz(4096), nz(512))
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
            BuddyArena::builder(nz(4096), nz(512))
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
    async fn fixed_custom_waiter_supported() {
        let waiters = CountingWaiters::new();
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

    #[tokio::test]
    async fn buddy_custom_waiter_supported() {
        let waiters = CountingWaiters::new();
        let arena = Arc::new(
            BuddyArena::builder(nz(4096), nz(512))
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
}

#[cfg(all(test, loom, feature = "async-alloc"))]
mod loom_tests {
    use std::ptr;

    use loom::thread;

    use super::*;

    fn new_node(revoked: bool) -> Arc<WaiterNode> {
        Arc::new(WaiterNode {
            next: AtomicPtr::new(ptr::null_mut()),
            notify: Arc::new(tokio::sync::Notify::new()),
            revoked: AtomicBool::new(revoked),
        })
    }

    #[test]
    fn loom_treiber_push_and_wake_drains_stack() {
        loom::model(|| {
            let stack = Arc::new(TreiberStack::new());

            let s1 = Arc::clone(&stack);
            let t1 = thread::spawn(move || {
                let node = new_node(false);
                s1.push(Arc::into_raw(node));
            });

            let s2 = Arc::clone(&stack);
            let t2 = thread::spawn(move || {
                let node = new_node(false);
                s2.push(Arc::into_raw(node));
            });

            t1.join().unwrap();
            t2.join().unwrap();

            stack.wake_one();
            stack.wake_one();

            assert!(stack.head.load(Ordering::Acquire).is_null());
        });
    }

    #[test]
    fn loom_treiber_skips_revoked_waiters() {
        loom::model(|| {
            let stack = TreiberStack::new();

            let live = new_node(false);
            let revoked = new_node(true);

            stack.push(Arc::into_raw(live));
            stack.push(Arc::into_raw(revoked));

            stack.wake_one();

            assert!(stack.head.load(Ordering::Acquire).is_null());
        });
    }

    #[test]
    fn loom_treiber_revoke_race_is_safe() {
        loom::model(|| {
            let stack = Arc::new(TreiberStack::new());
            let node = new_node(false);

            let stack_for_push = Arc::clone(&stack);
            let node_for_push = Arc::clone(&node);
            let t_push = thread::spawn(move || {
                stack_for_push.push(Arc::into_raw(node_for_push));
            });

            let node_for_revoke = Arc::clone(&node);
            let t_revoke = thread::spawn(move || {
                node_for_revoke.revoked.store(true, Ordering::Release);
            });

            t_push.join().unwrap();
            t_revoke.join().unwrap();

            stack.wake_one();

            assert!(stack.head.load(Ordering::Acquire).is_null());
        });
    }

    /// Push and wake racing concurrently. The pushed node must
    /// either be woken or remain in the stack for drop cleanup.
    #[test]
    fn loom_treiber_concurrent_push_and_wake() {
        loom::model(|| {
            let stack = Arc::new(TreiberStack::new());

            // Seed one node so wake_one has something to pop.
            let seed = new_node(false);
            stack.push(Arc::into_raw(seed));

            let s1 = Arc::clone(&stack);
            let t_push = thread::spawn(move || {
                let node = new_node(false);
                s1.push(Arc::into_raw(node));
            });

            let s2 = Arc::clone(&stack);
            let t_wake = thread::spawn(move || {
                s2.wake_one();
            });

            t_push.join().unwrap();
            t_wake.join().unwrap();

            // Drain whatever remains — no leaks, no double-frees.
            stack.wake_one();
        });
    }
}
