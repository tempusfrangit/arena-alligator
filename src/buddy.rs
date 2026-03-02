use std::alloc::Layout;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::bitmap::AtomicBitmap;
use crate::buffer::Buffer;
use crate::error::{AllocError, BuildError};
use crate::metrics::{BuddyArenaMetrics, MetricsState};

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BuddyArenaInner {
    pub(crate) ptr: *mut u8,
    layout: Layout,
    pub(crate) total_size: usize,
    pub(crate) min_block_size: usize,
    pub(crate) max_order: usize,
    pub(crate) free_bitmaps: Box<[AtomicBitmap]>,
    pub(crate) nonempty_orders: AtomicUsize,
    pub(crate) auto_spill: bool,
    pub(crate) metrics: MetricsState,
    #[cfg(feature = "async-alloc")]
    pub(crate) wake_handle: Option<crate::async_alloc::WakeHandle>,
}

// SAFETY: buddy allocations hand out disjoint blocks. Shared metadata access
// is synchronized through atomics in the per-order bitmaps and summary state.
unsafe impl Send for BuddyArenaInner {}
unsafe impl Sync for BuddyArenaInner {}

impl Drop for BuddyArenaInner {
    fn drop(&mut self) {
        // SAFETY: ptr and layout were produced by std::alloc::alloc in build().
        unsafe {
            std::alloc::dealloc(self.ptr, self.layout);
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
    pub fn builder(total_size: NonZeroUsize, min_block_size: NonZeroUsize) -> BuddyArenaBuilder {
        BuddyArenaBuilder {
            total_size,
            min_block_size,
            alignment: 1,
            auto_spill: false,
        }
    }

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
        let capacity = self.block_size(final_order);
        let offset = self.block_offset(final_order, final_block_idx);
        self.inner.metrics.record_alloc_success(capacity);

        Ok(Buffer::new_buddy(
            Arc::clone(&self.inner),
            final_order,
            final_block_idx,
            offset,
            capacity,
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn auto_spill_enabled(&self) -> bool {
        self.inner.auto_spill
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn nonempty_orders(&self) -> usize {
        self.inner
            .nonempty_orders
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn free_block_count(&self, order: usize) -> usize {
        self.inner.free_bitmaps[order].free_count()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_block_free(&self, order: usize, block_idx: usize) -> bool {
        self.inner.free_bitmaps[order].is_free(block_idx)
    }

    fn order_for_request(&self, len: usize) -> Option<usize> {
        let size = len.max(self.inner.min_block_size).next_power_of_two();
        if size > self.inner.total_size {
            return None;
        }
        Some(size.trailing_zeros() as usize - self.inner.min_block_size.trailing_zeros() as usize)
    }

    fn try_allocate_from_summary(&self, target_order: usize) -> Option<(usize, usize)> {
        let summary = self.inner.nonempty_orders.load(Ordering::Acquire);
        self.try_allocate_from_orders(target_order, Some(summary))
    }

    fn try_allocate_from_full_scan(&self, target_order: usize) -> Option<(usize, usize)> {
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

    fn split_down(
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

    fn block_size(&self, order: usize) -> usize {
        self.inner.min_block_size << order
    }

    fn block_offset(&self, order: usize, block_idx: usize) -> usize {
        block_idx * self.block_size(order)
    }
}

/// Builder for [`BuddyArena`].
pub struct BuddyArenaBuilder {
    total_size: NonZeroUsize,
    min_block_size: NonZeroUsize,
    alignment: usize,
    auto_spill: bool,
}

impl BuddyArenaBuilder {
    /// Alignment for arena backing and block boundaries.
    ///
    /// Must be a power of 2. The minimum block size must be a multiple of
    /// the chosen alignment.
    pub fn alignment(mut self, n: usize) -> Self {
        self.alignment = n;
        self
    }

    /// Enable auto-spill: overflow writes copy to heap after releasing
    /// the buddy block back to the arena.
    pub fn auto_spill(mut self) -> Self {
        self.auto_spill = true;
        self
    }

    /// Build the buddy arena metadata and backing allocation.
    pub fn build(self) -> Result<BuddyArena, BuildError> {
        #[cfg(feature = "async-alloc")]
        let inner = self.build_inner(None)?;
        #[cfg(not(feature = "async-alloc"))]
        let inner = self.build_inner()?;
        Ok(BuddyArena {
            inner: Arc::new(inner),
        })
    }

    fn build_inner(
        self,
        #[cfg(feature = "async-alloc")] waker: Option<crate::async_alloc::WakeHandle>,
    ) -> Result<BuddyArenaInner, BuildError> {
        if !self.alignment.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }

        let total_size = self.total_size.get();
        let min_block_size = self.min_block_size.get();

        let max_order = buddy_max_order(total_size, min_block_size, self.alignment)
            .ok_or(BuildError::InvalidGeometry)?;

        let layout = Layout::from_size_align(total_size, self.alignment)
            .map_err(|_| BuildError::SizeOverflow)?;

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
        // The builder starts with all per-order bitmaps empty, then publishes
        // the single max-order block as free initial state.
        free_bitmaps[max_order].free(0);

        let inner = BuddyArenaInner {
            ptr,
            layout,
            total_size,
            min_block_size,
            max_order,
            free_bitmaps: free_bitmaps.into_boxed_slice(),
            nonempty_orders: AtomicUsize::new(1usize << max_order),
            auto_spill: self.auto_spill,
            metrics: MetricsState::new(total_size),
            #[cfg(feature = "async-alloc")]
            wake_handle: waker,
        };

        Ok(inner)
    }
}

#[cfg(feature = "async-alloc")]
impl BuddyArenaBuilder {
    /// Build an async-capable buddy arena using notify-based waiters.
    pub fn build_async(self) -> Result<crate::async_alloc::AsyncBuddyArena, BuildError> {
        self.build_async_with(crate::async_alloc::NotifyWaiters::new())
    }

    /// Build an async-capable buddy arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncBuddyArena<W>, BuildError>
    where
        W: crate::async_alloc::Waiter,
    {
        let waiters = Arc::new(waiters);
        let inner = self.build_inner(Some(crate::async_alloc::WakeHandle::new(Arc::clone(
            &waiters,
        ))))?;

        Ok(crate::async_alloc::AsyncBuddyArena::new(
            BuddyArena {
                inner: Arc::new(inner),
            },
            waiters,
        ))
    }
}

impl BuddyArenaInner {
    pub(crate) fn block_size(&self, order: usize) -> usize {
        self.min_block_size << order
    }

    pub(crate) fn release_block(&self, mut order: usize, mut block_idx: usize) {
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
            wake_handle.wake();
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

fn buddy_max_order(total_size: usize, min_block_size: usize, alignment: usize) -> Option<usize> {
    if !min_block_size.is_power_of_two() {
        return None;
    }
    if min_block_size < alignment || total_size < min_block_size {
        return None;
    }
    if !total_size.is_multiple_of(min_block_size) {
        return None;
    }

    let blocks = total_size / min_block_size;
    if !blocks.is_power_of_two() {
        return None;
    }

    let max_order = blocks.trailing_zeros() as usize;
    if max_order >= usize::BITS as usize {
        return None;
    }

    Some(max_order)
}

fn blocks_at_order(max_order: usize, order: usize) -> usize {
    1usize << (max_order - order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn build_basic_buddy_arena() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
        assert_eq!(arena.total_size(), 4096);
        assert_eq!(arena.min_block_size(), 512);
        assert_eq!(arena.max_order(), 3);
        assert_eq!(arena.nonempty_orders(), 1 << 3);
        assert!(!arena.auto_spill_enabled());
    }

    #[test]
    fn build_rejects_non_power_of_two_min_block_size() {
        let err = BuddyArena::builder(nz(4096), nz(768)).build().unwrap_err();
        assert_eq!(err, BuildError::InvalidGeometry);
    }

    #[test]
    fn build_rejects_total_size_not_power_of_two_multiple() {
        let err = BuddyArena::builder(nz(6144), nz(1024)).build().unwrap_err();
        assert_eq!(err, BuildError::InvalidGeometry);
    }

    #[test]
    fn build_rejects_total_size_smaller_than_min_block() {
        let err = BuddyArena::builder(nz(256), nz(512)).build().unwrap_err();
        assert_eq!(err, BuildError::InvalidGeometry);
    }

    #[test]
    fn build_rejects_alignment_larger_than_min_block() {
        let err = BuddyArena::builder(nz(4096), nz(512))
            .alignment(1024)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidGeometry);
    }

    #[test]
    fn build_rejects_invalid_alignment() {
        let err = BuddyArena::builder(nz(4096), nz(512))
            .alignment(3)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn rounded_capacity_derivation() {
        assert_eq!(buddy_max_order(4096, 512, 1), Some(3));
        assert_eq!(buddy_max_order(8192, 512, 1), Some(4));
        assert_eq!(buddy_max_order(4096, 256, 256), Some(4));
        assert_eq!(blocks_at_order(3, 3), 1);
        assert_eq!(blocks_at_order(3, 2), 2);
        assert_eq!(blocks_at_order(3, 1), 4);
        assert_eq!(blocks_at_order(3, 0), 8);
    }

    #[test]
    fn initial_free_state_has_one_max_order_block() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
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
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
        let buf = arena.allocate(nz(700)).unwrap();
        assert_eq!(buf.capacity(), 1024);
    }

    #[test]
    fn allocate_exhausts_large_block() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
        let _buf = arena.allocate(nz(4096)).unwrap();
        assert_eq!(arena.allocate(nz(512)).unwrap_err(), AllocError::ArenaFull);
    }

    #[test]
    fn split_path_publishes_sibling_blocks() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
        let _buf = arena.allocate(nz(512)).unwrap();
        assert!(arena.is_block_free(2, 1));
        assert!(arena.is_block_free(1, 1));
        assert!(arena.is_block_free(0, 1));
    }

    #[test]
    fn coalesce_path_restores_top_block() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();
        let buf = arena.allocate(nz(512)).unwrap();
        drop(buf);
        assert_eq!(arena.free_block_count(arena.max_order()), 1);
        assert!(arena.is_block_free(arena.max_order(), 0));
    }

    #[test]
    fn metrics_track_allocate_free_and_failure() {
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();

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
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();

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
        let arena = BuddyArena::builder(nz(4096), nz(512)).build().unwrap();

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
}
