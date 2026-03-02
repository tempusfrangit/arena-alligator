use std::alloc::Layout;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use crate::bitmap::AtomicBitmap;
use crate::error::BuildError;

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
        };

        Ok(BuddyArena {
            inner: Arc::new(inner),
        })
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
}
