use std::alloc::Layout;
use std::fmt;
use std::num::NonZeroUsize;

use crate::bitmap::AtomicBitmap;
use crate::buffer::Buffer;
use crate::error::{AllocError, BuildError};
use crate::metrics::{BuddyArenaMetrics, MetricsState};
use crate::sync::Arc;
use crate::sync::atomic::{AtomicUsize, Ordering};

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BuddyArenaInner {
    pub(crate) ptr: *mut u8,
    pub(crate) total_size: usize,
    dealloc_len: usize,
    pub(crate) min_block_size: usize,
    pub(crate) max_order: usize,
    pub(crate) free_bitmaps: Box<[AtomicBitmap]>,
    pub(crate) nonempty_orders: AtomicUsize,
    pub(crate) auto_spill: bool,
    pub(crate) cap_capacity: bool,
    pub(crate) init_policy: crate::arena::InitPolicy,
    pub(crate) metrics: MetricsState,
    /// Tracks which order-0 regions have been return-scrubbed. Only present
    /// when `init_policy == Zero`. Write-once: return path sets bits, alloc
    /// path only reads.
    pub(crate) zeroed_bitmap: Option<AtomicBitmap>,
    dealloc: crate::dealloc::ErasedDealloc,
    #[cfg(feature = "async-alloc")]
    pub(crate) wake_handle: Option<crate::async_alloc::BuddyWakeHandle>,
}

// SAFETY: buddy allocations hand out disjoint blocks. Shared metadata access
// is synchronized through atomics in the per-order bitmaps and summary state.
unsafe impl Send for BuddyArenaInner {}
unsafe impl Sync for BuddyArenaInner {}

impl Drop for BuddyArenaInner {
    fn drop(&mut self) {
        // SAFETY: ErasedDealloc::dealloc is called exactly once (here).
        unsafe {
            let dealloc =
                std::mem::replace(&mut self.dealloc, crate::dealloc::ErasedDealloc::noop());
            dealloc.dealloc(self.ptr, self.dealloc_len);
        }
    }
}

/// Buddy-backed arena allocator.
///
/// Memory is managed in power-of-two blocks over a fixed minimum block size.
/// The builder validates the arena geometry up front; allocation comes later.
#[derive(Clone)]
pub struct BuddyArena {
    pub(crate) inner: Arc<BuddyArenaInner>,
}

impl fmt::Debug for BuddyArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuddyArena")
            .field("total_size", &self.inner.total_size)
            .field("min_block_size", &self.inner.min_block_size)
            .field("max_order", &self.inner.max_order)
            .finish()
    }
}

impl BuddyArena {
    /// Create a builder for a buddy arena.
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use arena_alligator::{BuddyArena, BuddyGeometry};
    /// use bytes::BufMut;
    ///
    /// let geo = BuddyGeometry::exact(
    ///     NonZeroUsize::new(1024 * 1024).unwrap(),
    ///     NonZeroUsize::new(256).unwrap(),
    /// ).unwrap();
    /// let arena = BuddyArena::builder(geo).build().unwrap();
    ///
    /// let mut buf = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();
    /// buf.put_slice(b"hello buddy");
    /// let bytes = buf.freeze();
    /// assert_eq!(&bytes[..], b"hello buddy");
    /// ```
    pub fn builder(geometry: crate::geometry::BuddyGeometry) -> BuddyArenaBuilder {
        BuddyArenaBuilder {
            geometry,
            config: crate::arena::BuildConfig::new(),
            _mode: std::marker::PhantomData,
        }
    }

    /// Create a buddy arena from user-provided memory.
    ///
    /// This uses the caller's backing region instead of allocating a new one.
    /// Ownership transfers to the returned builder and then to the arena
    /// produced by [`build()`](RawBackedBuddyArenaBuilder::build). When the
    /// last arena reference drops, the backing region is released through
    /// `dealloc`.
    ///
    /// `hint` controls the derived buddy geometry. The final arena may use
    /// less than `len` bytes if the usable region must be rounded down to a
    /// power-of-two buddy layout.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to a valid, exclusively-owned allocation of at
    ///   least `len` bytes.
    /// - The region must not be accessed through any other mutable or shared
    ///   alias for the lifetime of the arena and any frozen [`Bytes`](bytes::Bytes)
    ///   derived from it.
    /// - The memory must remain valid until `D::dealloc` is called, which
    ///   happens when the last arena reference and last frozen `Bytes`
    ///   derived from it drop.
    /// - `dealloc` must correctly release the original region, or be
    ///   [`NoDealloc`](crate::NoDealloc) if the caller retains
    ///   responsibility for the backing memory.
    ///   For `&'static mut [u8]`, prefer the safe
    ///   [`from_static()`](Self::from_static) wrapper.
    ///
    /// If `build()` returns `Err`, the caller retains ownership of the
    /// memory and remains responsible for releasing it.
    pub unsafe fn from_raw<D: crate::dealloc::Dealloc>(
        ptr: *mut u8,
        len: usize,
        hint: crate::spec::BuddyHint,
        dealloc: D,
    ) -> RawBackedBuddyArenaBuilder<D> {
        RawBackedBuddyArenaBuilder {
            ptr,
            len,
            hint,
            dealloc,
            config: crate::arena::BuildConfig::new(),
        }
    }

    /// Build a buddy arena from a `&'static mut` buffer with [`NoDealloc`](crate::NoDealloc).
    ///
    /// This is a safe convenience wrapper over [`from_raw()`](Self::from_raw)
    /// for static buffers (e.g. linker-placed memory in embedded/no_std).
    /// The static lifetime guarantees the memory outlives the arena, and
    /// [`NoDealloc`](crate::NoDealloc) matches the fact that static memory
    /// must not be freed.
    pub fn from_static(
        buf: &'static mut [u8],
        hint: crate::spec::BuddyHint,
    ) -> RawBackedBuddyArenaBuilder<crate::dealloc::NoDealloc> {
        // SAFETY: static lifetime guarantees the memory outlives the arena
        // and all derived Bytes. Exclusive &mut ensures no aliasing.
        // NoDealloc is correct because static memory must not be freed.
        unsafe { Self::from_raw(buf.as_mut_ptr(), buf.len(), hint, crate::dealloc::NoDealloc) }
    }
}

impl BuddyArena {
    /// Total bytes managed by this arena.
    pub fn total_size(&self) -> usize {
        self.inner.total_size
    }

    /// Smallest allocatable block size in bytes.
    pub fn min_block_size(&self) -> usize {
        self.inner.min_block_size
    }

    /// Largest block order in this arena.
    pub fn max_order(&self) -> usize {
        self.inner.max_order
    }

    /// Snapshot current allocator metrics.
    pub fn metrics(&self) -> BuddyArenaMetrics {
        self.inner
            .metrics
            .buddy_snapshot(self.inner.largest_free_block())
    }
}

impl BuddyArena {
    /// Allocate a buddy-backed buffer with at least `len` bytes of capacity.
    pub fn allocate(&self, len: NonZeroUsize) -> Result<Buffer, AllocError> {
        let target_order = self.order_for_request(len.get()).ok_or_else(|| {
            self.inner.metrics.record_alloc_failure();
            AllocError::ArenaFull
        })?;

        let (order, block_idx) = self
            .try_allocate_from_summary(target_order)
            .or_else(|| self.try_allocate_from_full_scan(target_order))
            .ok_or_else(|| {
                self.inner.metrics.record_alloc_failure();
                AllocError::ArenaFull
            })?;

        let (final_order, final_block_idx) = self.split_down(order, block_idx, target_order);
        let block_size = self.block_size(final_order);
        let offset = self.block_offset(final_order, final_block_idx);

        match self.inner.init_policy {
            crate::arena::InitPolicy::Zero => {
                if let Some(ref zeroed_bm) = self.inner.zeroed_bitmap {
                    let order0_start = final_block_idx * (1 << final_order);
                    let order0_end = order0_start + (1 << final_order);
                    if !zeroed_bm.all_set_in_range(order0_start, order0_end) {
                        // SAFETY: ptr+offset..ptr+offset+block_size is within the arena
                        // allocation and exclusively owned (bitmap claim above).
                        unsafe {
                            crate::arena::zeroize_region(self.inner.ptr.add(offset), block_size);
                        }
                    }
                }
            }
            crate::arena::InitPolicy::Uninit => {}
        }

        self.inner.metrics.record_alloc_success(block_size);

        let user_capacity = if self.inner.cap_capacity {
            len.get().min(block_size)
        } else {
            block_size
        };

        Ok(Buffer::new_buddy(
            crate::allocation::ArenaRef::Buddy(self.inner.clone()),
            self.inner.ptr,
            self.inner.auto_spill,
            final_order,
            final_block_idx,
            offset,
            user_capacity,
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn auto_spill_enabled(&self) -> bool {
        self.inner.auto_spill
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn nonempty_orders(&self) -> usize {
        self.inner.nonempty_orders.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn free_block_count(&self, order: usize) -> usize {
        self.inner.free_bitmaps[order].free_count()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_block_free(&self, order: usize, block_idx: usize) -> bool {
        self.inner.free_bitmaps[order].is_free(block_idx)
    }

    pub(crate) fn order_for_request(&self, len: usize) -> Option<usize> {
        let size = len.max(self.inner.min_block_size).next_power_of_two();
        if size > self.inner.total_size {
            return None;
        }
        Some(size.trailing_zeros() as usize - self.inner.min_block_size.trailing_zeros() as usize)
    }

    pub(crate) fn try_allocate_from_summary(&self, target_order: usize) -> Option<(usize, usize)> {
        let summary = self.inner.nonempty_orders.load(Ordering::Acquire);
        self.try_allocate_from_orders(target_order, Some(summary))
    }

    pub(crate) fn try_allocate_from_full_scan(
        &self,
        target_order: usize,
    ) -> Option<(usize, usize)> {
        self.try_allocate_from_orders(target_order, None)
    }

    fn try_allocate_from_orders(
        &self,
        target_order: usize,
        summary: Option<usize>,
    ) -> Option<(usize, usize)> {
        for order in target_order..=self.inner.max_order {
            if let Some(bits) = summary
                && bits & (1usize << order) == 0
            {
                continue;
            }
            if let Some(block_idx) = self.inner.free_bitmaps[order].try_alloc() {
                self.inner.maybe_clear_summary(order);
                return Some((order, block_idx));
            }
        }
        None
    }

    pub(crate) fn split_down(
        &self,
        mut order: usize,
        mut block_idx: usize,
        target_order: usize,
    ) -> (usize, usize) {
        let mut split_steps = 0u64;
        while order > target_order {
            let child_order = order - 1;
            let left_child = block_idx * 2;
            let right_child = left_child + 1;

            self.inner
                .nonempty_orders
                .fetch_or(1usize << child_order, Ordering::Release);
            self.inner.free_bitmaps[child_order].free(right_child);

            order = child_order;
            block_idx = left_child;
            split_steps += 1;
        }

        if split_steps > 0 {
            self.inner.metrics.record_splits(split_steps);
        }

        (order, block_idx)
    }

    pub(crate) fn block_size(&self, order: usize) -> usize {
        self.inner.min_block_size << order
    }

    pub(crate) fn block_offset(&self, order: usize, block_idx: usize) -> usize {
        block_idx * self.block_size(order)
    }
}

/// Builder for a buddy arena backed by user-provided memory.
///
/// Created via [`BuddyArena::from_raw()`].
///
/// This mirrors the ordinary buddy builder for post-construction policy
/// knobs, but derives geometry from a caller-owned region plus a
/// [`BuddyHint`](crate::BuddyHint).
pub struct RawBackedBuddyArenaBuilder<D: crate::dealloc::Dealloc> {
    ptr: *mut u8,
    len: usize,
    hint: crate::spec::BuddyHint,
    dealloc: D,
    config: crate::arena::BuildConfig,
}

impl<D: crate::dealloc::Dealloc> RawBackedBuddyArenaBuilder<D> {
    /// Set the initialization policy for allocated buffers.
    ///
    /// [`InitPolicy::Zero`](crate::InitPolicy::Zero) zeroes visible block
    /// capacity on first allocation and on return, matching the standard
    /// buddy builder path.
    pub fn init_policy(mut self, policy: crate::arena::InitPolicy) -> Self {
        self.config.init_policy = policy;
        self
    }

    /// Set the page size used for prefaulting.
    ///
    /// This only affects [`build()`](Self::build) prefault behavior. It does
    /// not change the caller-provided backing region or the derived geometry.
    pub fn page_size(mut self, policy: crate::arena::PageSize) -> Self {
        self.config.page_size = policy;
        self
    }

    /// Build the arena from user-provided memory.
    ///
    /// [`BuddyHint`](crate::BuddyHint) derives the minimum block size and
    /// maximum order from the supplied region. Any tail bytes outside the
    /// final power-of-two buddy geometry are left unused.
    pub fn build(self) -> Result<BuddyArena, BuildError> {
        if self.ptr.is_null() {
            return Err(BuildError::NullPointer);
        }

        let page_size = self.config.page_size.resolve();
        let (min_block_size, max_order) = self.hint.resolve(self.len)?;

        let mut free_bitmaps = Vec::with_capacity(max_order + 1);
        for order in 0..=max_order {
            let block_count = blocks_at_order(max_order, order);
            free_bitmaps.push(AtomicBitmap::new_empty(block_count));
        }
        free_bitmaps[max_order].free(0);

        let order0_count = blocks_at_order(max_order, 0);
        let total_usable = min_block_size << max_order;

        let zeroed_bitmap = match self.config.init_policy {
            crate::arena::InitPolicy::Zero => Some(AtomicBitmap::new_empty(order0_count)),
            crate::arena::InitPolicy::Uninit => None,
        };

        let inner = BuddyArenaInner {
            ptr: self.ptr,
            total_size: total_usable,
            dealloc_len: self.len,
            min_block_size,
            max_order,
            free_bitmaps: free_bitmaps.into_boxed_slice(),
            nonempty_orders: AtomicUsize::new(1usize << max_order),
            auto_spill: false,
            cap_capacity: false,
            init_policy: self.config.init_policy,
            metrics: MetricsState::new(total_usable),
            zeroed_bitmap,
            dealloc: crate::dealloc::ErasedDealloc::new(self.dealloc),
            #[cfg(feature = "async-alloc")]
            wake_handle: None,
        };

        let arena = BuddyArena {
            inner: Arc::new(inner),
        };

        if let Some(ps) = page_size {
            crate::arena::prefault_region(arena.inner.ptr, total_usable, ps);
        }

        Ok(arena)
    }
}

/// Builder for [`BuddyArena`].
///
/// The `Mode` parameter controls which build targets are available:
///
/// - [`Standard`](crate::Standard) (default): builds [`BuddyArena`]. Can
///   transition to [`AutoSpill`](crate::AutoSpill) or
///   [`HazmatRaw`](crate::HazmatRaw).
/// - [`AutoSpill`](crate::AutoSpill): builds [`BuddyArena`] with heap
///   overflow fallback.
/// - [`HazmatRaw`](crate::HazmatRaw): builds
///   [`RawBuddyArena`](crate::hazmat::RawBuddyArena) with raw pointer
///   access. Requires `hazmat-raw-access` feature.
pub struct BuddyArenaBuilder<Mode = crate::arena::Standard> {
    geometry: crate::geometry::BuddyGeometry,
    config: crate::arena::BuildConfig,
    _mode: std::marker::PhantomData<Mode>,
}

impl<Mode> BuddyArenaBuilder<Mode> {
    /// Set the initialization policy for allocated buffers.
    ///
    /// Default: [`InitPolicy::Uninit`](crate::InitPolicy::Uninit). When set
    /// to [`InitPolicy::Zero`](crate::InitPolicy::Zero), every call to
    /// [`BuddyArena::allocate()`] writes zeroes across the block before
    /// returning the buffer.
    pub fn init_policy(mut self, policy: crate::arena::InitPolicy) -> Self {
        self.config.init_policy = policy;
        self
    }

    /// Set the page size used for prefaulting.
    ///
    /// Default: [`Auto`](crate::PageSize::Auto) on Unix with the `libc`
    /// feature, [`Unknown`](crate::PageSize::Unknown) otherwise.
    ///
    /// When set to [`Auto`](crate::PageSize::Auto) or
    /// [`Size`](crate::PageSize::Size), [`build()`](Self::build) touches
    /// every page at build time. Use
    /// [`build_unfaulted()`](Self::build_unfaulted) to defer the walk
    /// (e.g. for NUMA placement).
    pub fn page_size(mut self, policy: crate::arena::PageSize) -> Self {
        self.config.page_size = policy;
        self
    }

    fn build_raw(
        self,
        #[cfg(feature = "async-alloc")] waker: Option<crate::async_alloc::BuddyWakeHandle>,
    ) -> Result<BuddyArena, BuildError> {
        let total_size = self.geometry.total_size();
        let min_block_size = self.geometry.min_block_size();
        let max_order = self.geometry.max_order();
        let alignment = self.geometry.alignment();

        let layout =
            Layout::from_size_align(total_size, alignment).map_err(|_| BuildError::SizeOverflow)?;

        // SAFETY: layout has non-zero size and valid alignment.
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        let mut free_bitmaps = Vec::with_capacity(max_order + 1);
        for order in 0..=max_order {
            let block_count = blocks_at_order(max_order, order);
            free_bitmaps.push(AtomicBitmap::new_empty(block_count));
        }
        free_bitmaps[max_order].free(0);

        let order0_count = blocks_at_order(max_order, 0);
        let zeroed_bitmap = match self.config.init_policy {
            crate::arena::InitPolicy::Zero => Some(AtomicBitmap::new_empty(order0_count)),
            crate::arena::InitPolicy::Uninit => None,
        };

        let inner = BuddyArenaInner {
            ptr,
            total_size,
            dealloc_len: total_size,
            min_block_size,
            max_order,
            free_bitmaps: free_bitmaps.into_boxed_slice(),
            nonempty_orders: AtomicUsize::new(1usize << max_order),
            auto_spill: self.config.auto_spill,
            cap_capacity: self.geometry.cap_capacity(),
            init_policy: self.config.init_policy,
            metrics: MetricsState::new(total_size),
            zeroed_bitmap,
            dealloc: crate::dealloc::ErasedDealloc::new(crate::dealloc::HeapDealloc::new(layout)),
            #[cfg(feature = "async-alloc")]
            wake_handle: waker,
        };

        Ok(BuddyArena {
            inner: Arc::new(inner),
        })
    }

    fn build_buddy(self) -> Result<BuddyArena, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_raw(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        if let Some(ps) = page_size {
            crate::arena::prefault_region(arena.inner.ptr, arena.inner.total_size, ps);
        }
        Ok(arena)
    }

    fn build_buddy_unfaulted(self) -> Result<crate::arena::Unfaulted<BuddyArena>, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_raw(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        let total_size = arena.inner.total_size;
        Ok(crate::arena::Unfaulted::new(
            arena.inner.ptr,
            total_size,
            page_size,
            arena,
        ))
    }

    #[cfg(feature = "async-alloc")]
    fn build_buddy_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncBuddyArena<W>, BuildError>
    where
        W: crate::async_alloc::BuddyWaiter,
    {
        let page_size = self.config.page_size.resolve();
        let waiters = std::sync::Arc::new(waiters);
        let arena = self.build_raw(Some(crate::async_alloc::BuddyWakeHandle::new(
            std::sync::Arc::clone(&waiters),
        )))?;

        if let Some(ps) = page_size {
            crate::arena::prefault_region(arena.inner.ptr, arena.inner.total_size, ps);
        }

        Ok(crate::async_alloc::AsyncBuddyArena::new(arena, waiters))
    }
}

impl BuddyArenaBuilder<crate::arena::Standard> {
    /// Transition to [`AutoSpill`](crate::AutoSpill) mode. Overflow writes
    /// copy to heap, freeing the buddy block.
    ///
    /// Mutually exclusive with
    /// [`hazmat_raw_access()`](Self::hazmat_raw_access) at compile time.
    pub fn auto_spill(self) -> BuddyArenaBuilder<crate::arena::AutoSpill> {
        BuddyArenaBuilder {
            geometry: self.geometry,
            config: crate::arena::BuildConfig {
                auto_spill: true,
                ..self.config
            },
            _mode: std::marker::PhantomData,
        }
    }

    /// Build the buddy arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<BuddyArena, BuildError> {
        self.build_buddy()
    }

    /// Build the arena without prefaulting. Returns an
    /// [`Unfaulted`](crate::Unfaulted) wrapper.
    ///
    /// See [`Unfaulted`](crate::Unfaulted) for the three consumption
    /// paths: explicit fault, demand-fault, or direct allocate.
    pub fn build_unfaulted(self) -> Result<crate::arena::Unfaulted<BuddyArena>, BuildError> {
        self.build_buddy_unfaulted()
    }
}

impl BuddyArenaBuilder<crate::arena::AutoSpill> {
    /// Build the buddy arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<BuddyArena, BuildError> {
        self.build_buddy()
    }

    /// Build the arena without prefaulting. Returns an
    /// [`Unfaulted`](crate::Unfaulted) wrapper.
    pub fn build_unfaulted(self) -> Result<crate::arena::Unfaulted<BuddyArena>, BuildError> {
        self.build_buddy_unfaulted()
    }
}

#[cfg(feature = "hazmat-raw-access")]
impl BuddyArenaBuilder<crate::arena::Standard> {
    /// Transition to [`HazmatRaw`](crate::HazmatRaw) mode.
    ///
    /// Mutually exclusive with [`auto_spill()`](Self::auto_spill) at compile
    /// time.
    pub fn hazmat_raw_access(self) -> BuddyArenaBuilder<crate::arena::HazmatRaw> {
        BuddyArenaBuilder {
            geometry: self.geometry,
            config: self.config,
            _mode: std::marker::PhantomData,
        }
    }
}

#[cfg(feature = "hazmat-raw-access")]
impl BuddyArenaBuilder<crate::arena::HazmatRaw> {
    /// Build the buddy arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<crate::hazmat::RawBuddyArena, BuildError> {
        self.build_buddy().map(crate::hazmat::RawBuddyArena)
    }

    /// Build the arena without prefaulting.
    pub fn build_unfaulted(
        self,
    ) -> Result<crate::arena::Unfaulted<crate::hazmat::RawBuddyArena>, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_raw(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        let total_size = arena.inner.total_size;
        Ok(crate::arena::Unfaulted::new(
            arena.inner.ptr,
            total_size,
            page_size,
            crate::hazmat::RawBuddyArena(arena),
        ))
    }
}

#[cfg(feature = "async-alloc")]
impl BuddyArenaBuilder<crate::arena::Standard> {
    /// Build an async-capable buddy arena using the default per-order notify waiter.
    pub fn build_async(self) -> Result<crate::async_alloc::AsyncBuddyArena, BuildError> {
        let max_order = self.geometry.max_order();
        self.build_buddy_async_with(crate::async_alloc::NotifyWaiters::new(max_order + 1))
    }

    /// Build an async-capable buddy arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncBuddyArena<W>, BuildError>
    where
        W: crate::async_alloc::BuddyWaiter,
    {
        self.build_buddy_async_with(waiters)
    }
}

#[cfg(feature = "async-alloc")]
impl BuddyArenaBuilder<crate::arena::AutoSpill> {
    /// Build an async-capable buddy arena using the default per-order notify waiter.
    pub fn build_async(self) -> Result<crate::async_alloc::AsyncBuddyArena, BuildError> {
        let max_order = self.geometry.max_order();
        self.build_buddy_async_with(crate::async_alloc::NotifyWaiters::new(max_order + 1))
    }

    /// Build an async-capable buddy arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncBuddyArena<W>, BuildError>
    where
        W: crate::async_alloc::BuddyWaiter,
    {
        self.build_buddy_async_with(waiters)
    }
}

impl BuddyArenaInner {
    pub(crate) fn block_size(&self, order: usize) -> usize {
        self.min_block_size << order
    }

    pub(crate) fn release_block(&self, mut order: usize, mut block_idx: usize) {
        if self.init_policy == crate::arena::InitPolicy::Zero {
            let block_size = self.block_size(order);
            let offset = block_idx * block_size;
            // SAFETY: block is exclusively owned (not yet freed). ptr+offset is valid.
            unsafe {
                crate::arena::zeroize_region(self.ptr.add(offset), block_size);
            }
            if let Some(ref zeroed_bm) = self.zeroed_bitmap {
                let order0_start = block_idx * (1 << order);
                let order0_end = order0_start + (1 << order);
                zeroed_bm.set_range(order0_start, order0_end);
            }
        }

        while order < self.max_order {
            let buddy_idx = block_idx ^ 1;
            if !self.free_bitmaps[order].try_claim_exact(buddy_idx) {
                break;
            }
            self.maybe_clear_summary(order);
            block_idx /= 2;
            order += 1;
            self.metrics.record_coalesce();
        }

        self.nonempty_orders
            .fetch_or(1usize << order, Ordering::Release);
        self.free_bitmaps[order].free(block_idx);
        #[cfg(feature = "async-alloc")]
        if let Some(wake_handle) = &self.wake_handle {
            wake_handle.wake(order);
        }
    }

    fn maybe_clear_summary(&self, order: usize) {
        if !self.free_bitmaps[order].any_free() {
            self.nonempty_orders
                .fetch_and(!(1usize << order), Ordering::AcqRel);
        }
    }

    fn largest_free_block(&self) -> usize {
        for order in (0..=self.max_order).rev() {
            if self.free_bitmaps[order].any_free() {
                return self.block_size(order);
            }
        }
        0
    }
}

fn blocks_at_order(max_order: usize, order: usize) -> usize {
    1usize << (max_order - order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::BuddyGeometry;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn geo(total: usize, min_block: usize) -> BuddyGeometry {
        BuddyGeometry::exact(nz(total), nz(min_block)).unwrap()
    }

    #[test]
    fn build_basic_buddy_arena() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        assert_eq!(arena.total_size(), 4096);
        assert_eq!(arena.min_block_size(), 512);
        assert_eq!(arena.max_order(), 3);
        assert_eq!(arena.nonempty_orders(), 1 << 3);
        assert!(!arena.auto_spill_enabled());
    }

    #[test]
    fn blocks_at_order_derivation() {
        assert_eq!(blocks_at_order(3, 3), 1);
        assert_eq!(blocks_at_order(3, 2), 2);
        assert_eq!(blocks_at_order(3, 1), 4);
        assert_eq!(blocks_at_order(3, 0), 8);
    }

    #[test]
    fn initial_free_state_has_one_max_order_block() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        for order in 0..arena.max_order() {
            assert_eq!(arena.free_block_count(order), 0);
            assert!(!arena.is_block_free(order, 0));
        }

        assert_eq!(arena.nonempty_orders(), 1 << arena.max_order());
        assert_eq!(arena.free_block_count(arena.max_order()), 1);
        assert!(arena.is_block_free(arena.max_order(), 0));
    }

    #[test]
    fn allocate_rounds_up_request_size() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let buf = arena.allocate(nz(700)).unwrap();
        assert_eq!(buf.capacity(), 1024);
    }

    #[test]
    fn allocate_exhausts_large_block() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let _buf = arena.allocate(nz(4096)).unwrap();
        assert_eq!(arena.allocate(nz(512)).unwrap_err(), AllocError::ArenaFull);
    }

    #[test]
    fn split_path_publishes_sibling_blocks() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let _buf = arena.allocate(nz(512)).unwrap();
        assert!(arena.is_block_free(2, 1));
        assert!(arena.is_block_free(1, 1));
        assert!(arena.is_block_free(0, 1));
    }

    #[test]
    fn coalesce_path_restores_top_block() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let buf = arena.allocate(nz(512)).unwrap();
        drop(buf);
        assert_eq!(arena.free_block_count(arena.max_order()), 1);
        assert!(arena.is_block_free(arena.max_order(), 0));
    }

    #[test]
    fn metrics_track_allocate_free_and_failure() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();

        let initial = arena.metrics();
        assert_eq!(initial.bytes_reserved, 4096);
        assert_eq!(initial.bytes_live, 0);

        let buf = arena.allocate(nz(700)).unwrap();
        let after_alloc = arena.metrics();
        assert_eq!(after_alloc.allocations_ok, 1);
        assert_eq!(after_alloc.allocations_failed, 0);
        assert_eq!(after_alloc.bytes_live, 1024);

        let other = arena.allocate(nz(2048)).unwrap();
        assert_eq!(arena.allocate(nz(2048)).unwrap_err(), AllocError::ArenaFull);
        let after_fail = arena.metrics();
        assert_eq!(after_fail.allocations_failed, 1);
        assert_eq!(after_fail.bytes_live, 3072);

        drop(buf);
        let after_free = arena.metrics();
        assert_eq!(after_free.frees, 1);
        assert_eq!(after_free.bytes_live, 2048);
        drop(other);
    }

    #[test]
    fn metrics_track_splits_and_largest_free_block() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();

        let initial = arena.metrics();
        assert_eq!(initial.splits, 0);
        assert_eq!(initial.coalesces, 0);
        assert_eq!(initial.largest_free_block, 4096);

        let buf = arena.allocate(nz(700)).unwrap();
        let after_split = arena.metrics();
        assert_eq!(after_split.splits, 2);
        assert_eq!(after_split.coalesces, 0);
        assert_eq!(after_split.largest_free_block, 2048);

        drop(buf);
        let after_free = arena.metrics();
        assert_eq!(after_free.coalesces, 2);
        assert_eq!(after_free.largest_free_block, 4096);
    }

    #[test]
    fn metrics_track_partial_coalesce() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();

        let left = arena.allocate(nz(2048)).unwrap();
        let right = arena.allocate(nz(2048)).unwrap();
        let full = arena.metrics();
        assert_eq!(full.largest_free_block, 0);

        drop(left);
        let half_free = arena.metrics();
        assert_eq!(half_free.coalesces, 0);
        assert_eq!(half_free.largest_free_block, 2048);

        drop(right);
        let fully_free = arena.metrics();
        assert_eq!(fully_free.coalesces, 1);
        assert_eq!(fully_free.largest_free_block, 4096);
    }

    #[test]
    fn build_unfaulted_then_fault_pages() {
        let unfaulted = BuddyArena::builder(geo(4096, 512))
            .page_size(crate::arena::PageSize::Size(nz(4096)))
            .build_unfaulted()
            .unwrap();
        let arena = unfaulted.fault_pages();
        assert_eq!(arena.total_size(), 4096);
        let _buf = arena.allocate(nz(512)).unwrap();
    }

    #[test]
    fn build_unfaulted_into_inner() {
        let unfaulted = BuddyArena::builder(geo(4096, 512))
            .page_size(crate::arena::PageSize::Unknown)
            .build_unfaulted()
            .unwrap();
        let arena = unfaulted.into_inner();
        assert_eq!(arena.total_size(), 4096);
        let _buf = arena.allocate(nz(512)).unwrap();
    }

    #[test]
    fn init_policy_zero_fills_block() {
        use bytes::BufMut;

        let arena = BuddyArena::builder(geo(1024, 1024))
            .init_policy(crate::arena::InitPolicy::Zero)
            .page_size(crate::arena::PageSize::Unknown)
            .build()
            .unwrap();

        // Write non-zero data, freeze, drop to return the block.
        let mut buf = arena.allocate(nz(512)).unwrap();
        buf.put_slice(&[0xAB; 512]);
        let bytes = buf.freeze();
        drop(bytes);

        // Re-allocate; zero policy should have cleared it.
        let buf = arena.allocate(nz(512)).unwrap();
        let block = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 1024) };
        assert!(block.iter().all(|&b| b == 0), "block should be zeroed");
    }

    #[test]
    fn nearest_caps_capacity_to_requested() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(512)).unwrap();
        let arena = BuddyArena::builder(geo).build().unwrap();
        let buf = arena.allocate(nz(700)).unwrap();
        assert_eq!(buf.capacity(), 700);
    }

    #[test]
    fn exact_exposes_full_block_capacity() {
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let buf = arena.allocate(nz(700)).unwrap();
        assert_eq!(buf.capacity(), 1024);
    }

    #[test]
    fn nearest_write_up_to_requested_then_full() {
        use bytes::BufMut;
        let geo = BuddyGeometry::nearest(nz(4096), nz(512)).unwrap();
        let arena = BuddyArena::builder(geo).build().unwrap();
        let mut buf = arena.allocate(nz(700)).unwrap();
        buf.put_slice(&vec![0xAB; 700]);
        assert_eq!(buf.len(), 700);
        assert_eq!(buf.remaining_mut(), 0);
    }

    #[test]
    fn exact_write_up_to_full_block() {
        use bytes::BufMut;
        let arena = BuddyArena::builder(geo(4096, 512)).build().unwrap();
        let mut buf = arena.allocate(nz(700)).unwrap();
        buf.put_slice(&vec![0xAB; 1024]);
        assert_eq!(buf.len(), 1024);
        assert_eq!(buf.remaining_mut(), 0);
    }

    #[test]
    fn nearest_metrics_reflect_block_size() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(512)).unwrap();
        let arena = BuddyArena::builder(geo).build().unwrap();
        let _buf = arena.allocate(nz(700)).unwrap();
        let m = arena.metrics();
        assert_eq!(m.bytes_live, 1024);
    }

    #[test]
    fn zero_policy_zeroes_on_return() {
        use bytes::BufMut;

        let arena = BuddyArena::builder(geo(4096, 512))
            .init_policy(crate::arena::InitPolicy::Zero)
            .page_size(crate::arena::PageSize::Unknown)
            .build()
            .unwrap();

        let mut buf = arena.allocate(nz(512)).unwrap();
        buf.put_slice(&[0xAB; 512]);
        let bytes = buf.freeze();
        drop(bytes);

        let buf = arena.allocate(nz(512)).unwrap();
        let block = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 512) };
        assert!(
            block.iter().all(|&b| b == 0),
            "block should be zeroed from return path"
        );
    }

    #[test]
    fn zero_policy_higher_order_checks_order0_bits() {
        let arena = BuddyArena::builder(geo(4096, 512))
            .init_policy(crate::arena::InitPolicy::Zero)
            .page_size(crate::arena::PageSize::Unknown)
            .build()
            .unwrap();

        // Allocate min-block, return it (zeroes + sets 1 order-0 bit)
        let buf = arena.allocate(nz(512)).unwrap();
        drop(buf);

        // Allocate a larger block that spans multiple order-0 regions.
        // Some order-0 bits are set (returned), some are not (cold from split siblings).
        // The alloc path should zeroize because not all bits are set.
        let buf = arena.allocate(nz(4096)).unwrap();
        let block = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 4096) };
        assert!(block.iter().all(|&b| b == 0), "block should be zeroed");
    }
}
