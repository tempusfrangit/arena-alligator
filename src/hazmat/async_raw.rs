use alloc::sync::Arc;
use core::fmt;
use core::ops::Deref;

use super::{RawBuddyArena, RawFixedArena, RawRegion};
use crate::AllocError;
use crate::async_alloc::{
    BuddyWaiter, NotifyWaiters, Waiter, allocate_with_buddy_waiter, allocate_with_waiter,
};

/// Async wrapper around [`RawFixedArena`].
///
/// `build_async()` on a [`FixedArenaBuilder`](crate::FixedArenaBuilder) in
/// hazmat mode returns this type.
/// [`raw_alloc_async()`](AsyncRawFixedArena::raw_alloc_async) waits until a
/// slot is available. The sync methods stay available through
/// `Deref<Target = RawFixedArena>`.
///
/// ```
/// use arena_alligator::FixedArena;
/// use core::num::NonZeroUsize;
/// let arena = FixedArena::with_slot_capacity(
///     NonZeroUsize::new(4).unwrap(),
///     NonZeroUsize::new(64).unwrap(),
/// )
/// .hazmat_raw_access()
/// .build_async()
/// .unwrap();
/// let _ = arena.raw_alloc_async();
/// ```
///
/// An arena built without `.hazmat_raw_access()` has no `raw_alloc_async()`:
///
/// ```compile_fail
/// use arena_alligator::FixedArena;
/// use core::num::NonZeroUsize;
/// let arena = FixedArena::with_slot_capacity(
///     NonZeroUsize::new(4).unwrap(),
///     NonZeroUsize::new(64).unwrap(),
/// )
/// .build_async()
/// .unwrap();
/// let _ = arena.raw_alloc_async();
/// ```
pub struct AsyncRawFixedArena<W = NotifyWaiters> {
    inner: RawFixedArena,
    waiters: Arc<W>,
}

impl<W> AsyncRawFixedArena<W> {
    pub(crate) fn new(inner: RawFixedArena, waiters: Arc<W>) -> Self {
        Self { inner, waiters }
    }
}

impl<W: Waiter> AsyncRawFixedArena<W> {
    /// Allocate a raw region. If the arena is full, wait until a slot is
    /// available.
    ///
    /// The bitmap is the source of truth. A notification is a hint to try
    /// again.
    ///
    /// The region is the same as one from [`raw_alloc()`](RawFixedArena::raw_alloc).
    /// If the caller drops the future before it completes, the future has no
    /// region to release. If the caller drops it after a wake and before the
    /// next poll, the wake goes to the next waiter.
    pub async fn raw_alloc_async(&self) -> RawRegion {
        allocate_with_waiter(self.waiters.as_ref(), || self.inner.raw_alloc().ok()).await
    }
}

// A derived impl has the bound `W: Clone`. With that bound, `clone()` on an
// arena with a waiter that is not `Clone` resolves through `Deref` to the
// sync arena.
impl<W> Clone for AsyncRawFixedArena<W> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            waiters: Arc::clone(&self.waiters),
        }
    }
}

impl<W> Deref for AsyncRawFixedArena<W> {
    type Target = RawFixedArena;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<W> fmt::Debug for AsyncRawFixedArena<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncRawFixedArena")
            .field("inner", &self.inner)
            .finish()
    }
}

/// Async wrapper around [`RawBuddyArena`].
///
/// `build_async()` on a [`BuddyArenaBuilder`](crate::BuddyArenaBuilder) in
/// hazmat mode returns this type.
/// [`raw_alloc_async()`](AsyncRawBuddyArena::raw_alloc_async) waits until a
/// block that fits the request is available. The sync methods stay available
/// through `Deref<Target = RawBuddyArena>`.
///
/// ```
/// use arena_alligator::{BuddyArena, BuddyGeometry};
/// use core::num::NonZeroUsize;
/// let geometry = BuddyGeometry::exact(
///     NonZeroUsize::new(4096).unwrap(),
///     NonZeroUsize::new(512).unwrap(),
/// )
/// .unwrap();
/// let arena = BuddyArena::builder(geometry)
///     .hazmat_raw_access()
///     .build_async()
///     .unwrap();
/// let _ = arena.raw_alloc_async(NonZeroUsize::new(512).unwrap());
/// ```
///
/// An arena built without `.hazmat_raw_access()` has no `raw_alloc_async()`:
///
/// ```compile_fail
/// use arena_alligator::{BuddyArena, BuddyGeometry};
/// use core::num::NonZeroUsize;
/// let geometry = BuddyGeometry::exact(
///     NonZeroUsize::new(4096).unwrap(),
///     NonZeroUsize::new(512).unwrap(),
/// )
/// .unwrap();
/// let arena = BuddyArena::builder(geometry).build_async().unwrap();
/// let _ = arena.raw_alloc_async(NonZeroUsize::new(512).unwrap());
/// ```
pub struct AsyncRawBuddyArena<W = NotifyWaiters> {
    inner: RawBuddyArena,
    waiters: Arc<W>,
}

impl<W> AsyncRawBuddyArena<W> {
    pub(crate) fn new(inner: RawBuddyArena, waiters: Arc<W>) -> Self {
        Self { inner, waiters }
    }
}

impl<W: BuddyWaiter> AsyncRawBuddyArena<W> {
    /// Allocate a raw region. If no block fits, wait until one is available.
    ///
    /// The buddy bitmaps are the source of truth. A notification is a hint to
    /// try again after a free or a coalesce.
    ///
    /// Returns [`AllocError::RequestTooLarge`] immediately when `len` is larger
    /// than the arena capacity, because no free can satisfy such a request.
    ///
    /// The region is the same as one from [`raw_alloc()`](RawBuddyArena::raw_alloc).
    /// If the caller drops the future before it completes, the future has no
    /// region to release. If the caller drops it after a wake and before the
    /// next poll, the wake goes to another waiter.
    pub async fn raw_alloc_async(
        &self,
        len: core::num::NonZeroUsize,
    ) -> Result<RawRegion, AllocError> {
        // No free can satisfy this request, so a wait never ends. The sync
        // path records the failure and returns the error.
        let Some(order) = self.inner.order_for_request(len.get()) else {
            return self.inner.raw_alloc(len);
        };
        Ok(
            allocate_with_buddy_waiter(self.waiters.as_ref(), order, || {
                self.inner.raw_alloc(len).ok()
            })
            .await,
        )
    }
}

// Same reason as for `AsyncRawFixedArena`.
impl<W> Clone for AsyncRawBuddyArena<W> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            waiters: Arc::clone(&self.waiters),
        }
    }
}

impl<W> Deref for AsyncRawBuddyArena<W> {
    type Target = RawBuddyArena;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<W> fmt::Debug for AsyncRawBuddyArena<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncRawBuddyArena")
            .field("inner", &self.inner)
            .finish()
    }
}
