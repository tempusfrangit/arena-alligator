use std::alloc::Layout;
use std::fmt;
use std::num::NonZeroUsize;

use crate::bitmap::AtomicBitmap;
use crate::buffer::Buffer;
use crate::error::{AllocError, BuildError};
use crate::metrics::{FixedArenaMetrics, MetricsState};
use crate::sync::Arc;

pub(crate) struct ArenaInner {
    pub(crate) ptr: *mut u8,
    layout: Layout,
    pub(crate) slot_capacity: usize,
    pub(crate) slot_count: usize,
    pub(crate) bitmap: AtomicBitmap,
    pub(crate) auto_spill: bool,
    pub(crate) metrics: MetricsState,
    #[cfg(feature = "async-alloc")]
    pub(crate) wake_handle: Option<crate::async_alloc::WakeHandle>,
}

// SAFETY: Buffer discipline enforces exclusive access per slot:
// - Writing: one Buffer per slot index (bitmap claim enforced)
// - Frozen: immutable access through Bytes (buffer consumed by freeze)
// - No overlap between slots (each slot is at a distinct offset)
unsafe impl Send for ArenaInner {}
unsafe impl Sync for ArenaInner {}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        // SAFETY: ptr and layout were produced by std::alloc::alloc in build().
        unsafe {
            std::alloc::dealloc(self.ptr, self.layout);
        }
    }
}

/// Fixed-size slot arena allocator.
///
/// All slots have identical capacity. Allocation is lock-free via atomic
/// bitmap. Produces `bytes::Bytes` through [`Buffer::freeze()`].
///
/// Cheap to clone — clones share the same backing memory via `Arc`.
#[derive(Clone)]
pub struct FixedArena {
    pub(crate) inner: Arc<ArenaInner>,
}

impl fmt::Debug for FixedArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedArena")
            .field("slot_count", &self.inner.slot_count)
            .field("slot_capacity", &self.inner.slot_capacity)
            .finish()
    }
}

impl FixedArena {
    /// Create a builder. Both parameters are `NonZeroUsize` — zero slots
    /// or zero capacity are rejected at the type level.
    pub fn builder(slot_count: NonZeroUsize, slot_capacity: NonZeroUsize) -> FixedArenaBuilder {
        FixedArenaBuilder {
            slot_count,
            slot_capacity,
            alignment: 1,
            auto_spill: false,
        }
    }

    /// Number of slots in this arena.
    pub fn slot_count(&self) -> usize {
        self.inner.slot_count
    }

    /// Capacity of each slot in bytes (aligned).
    pub fn slot_capacity(&self) -> usize {
        self.inner.slot_capacity
    }

    /// Snapshot current allocator metrics.
    pub fn metrics(&self) -> FixedArenaMetrics {
        self.inner.metrics.fixed_snapshot()
    }

    /// Allocate a buffer. Returns `Err(AllocError::ArenaFull)` if all slots are in use.
    pub fn allocate(&self) -> Result<Buffer, AllocError> {
        let Some(slot_idx) = self.inner.bitmap.try_alloc() else {
            self.inner.metrics.record_alloc_failure();
            return Err(AllocError::ArenaFull);
        };

        let offset = slot_idx * self.inner.slot_capacity;
        self.inner
            .metrics
            .record_alloc_success(self.inner.slot_capacity);

        Ok(Buffer::new_fixed(
            Arc::clone(&self.inner),
            slot_idx,
            offset,
            self.inner.slot_capacity,
        ))
    }
}

/// Builder for [`FixedArena`].
pub struct FixedArenaBuilder {
    slot_count: NonZeroUsize,
    slot_capacity: NonZeroUsize,
    alignment: usize,
    auto_spill: bool,
}

impl FixedArenaBuilder {
    /// Alignment for arena backing, slot boundaries, and slot capacities.
    ///
    /// Must be a power of 2. Default: 1 (no alignment constraint).
    /// Use 4096 for O_DIRECT / DMA compatibility.
    pub fn alignment(mut self, n: usize) -> Self {
        self.alignment = n;
        self
    }

    /// Enable auto-spill: overflow writes copy to heap, freeing the arena slot.
    pub fn auto_spill(mut self) -> Self {
        self.auto_spill = true;
        self
    }

    /// Build the arena.
    pub fn build(self) -> Result<FixedArena, BuildError> {
        if !self.alignment.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }

        let slot_count = self.slot_count.get();
        let slot_capacity = self.slot_capacity.get();

        let aligned_capacity =
            align_up(slot_capacity, self.alignment).ok_or(BuildError::SizeOverflow)?;

        let total_size = slot_count
            .checked_mul(aligned_capacity)
            .ok_or(BuildError::SizeOverflow)?;

        let layout = Layout::from_size_align(total_size, self.alignment)
            .map_err(|_| BuildError::SizeOverflow)?;

        // SAFETY: layout has non-zero size (slot_count > 0, aligned_capacity > 0).
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        let inner = ArenaInner {
            ptr,
            layout,
            slot_capacity: aligned_capacity,
            slot_count,
            bitmap: AtomicBitmap::new(slot_count),
            auto_spill: self.auto_spill,
            metrics: MetricsState::new(total_size),
            #[cfg(feature = "async-alloc")]
            wake_handle: None,
        };

        Ok(FixedArena {
            inner: Arc::new(inner),
        })
    }
}

#[cfg(feature = "async-alloc")]
impl FixedArenaBuilder {
    /// Build an async-capable arena with the given wait policy.
    pub fn build_async(
        self,
        policy: crate::async_alloc::AsyncPolicy,
    ) -> Result<crate::async_alloc::AsyncFixedArena, BuildError> {
        let waiters = match policy {
            crate::async_alloc::AsyncPolicy::Notify => {
                crate::async_alloc::BuiltInWaiters::Notify(crate::async_alloc::NotifyWaiters::new())
            }
            crate::async_alloc::AsyncPolicy::TreiberWaiters => {
                crate::async_alloc::BuiltInWaiters::Treiber(
                    crate::async_alloc::TreiberWaiters::new(),
                )
            }
        };

        self.build_async_with(waiters)
    }

    /// Build an async-capable arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncFixedArena<W>, BuildError>
    where
        W: crate::async_alloc::Waiter,
    {
        if !self.alignment.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }

        let slot_count = self.slot_count.get();
        let slot_capacity = self.slot_capacity.get();

        let aligned_capacity =
            align_up(slot_capacity, self.alignment).ok_or(BuildError::SizeOverflow)?;

        let total_size = slot_count
            .checked_mul(aligned_capacity)
            .ok_or(BuildError::SizeOverflow)?;

        let layout = Layout::from_size_align(total_size, self.alignment)
            .map_err(|_| BuildError::SizeOverflow)?;

        // SAFETY: layout has non-zero size (slot_count > 0, aligned_capacity > 0).
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        let waiters = std::sync::Arc::new(waiters);

        let inner = ArenaInner {
            ptr,
            layout,
            slot_capacity: aligned_capacity,
            slot_count,
            bitmap: AtomicBitmap::new(slot_count),
            auto_spill: self.auto_spill,
            metrics: MetricsState::new(total_size),
            wake_handle: Some(crate::async_alloc::WakeHandle::new(std::sync::Arc::clone(
                &waiters,
            ))),
        };

        Ok(crate::async_alloc::AsyncFixedArena::new(
            FixedArena {
                inner: Arc::new(inner),
            },
            waiters,
        ))
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    let rounded = value.checked_add(alignment - 1)?;
    Some(rounded & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn build_basic_arena() {
        let arena = FixedArena::builder(nz(4), nz(64)).build().unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[test]
    fn build_invalid_alignment_fails() {
        let err = FixedArena::builder(nz(4), nz(64))
            .alignment(3)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn build_zero_alignment_fails() {
        let err = FixedArena::builder(nz(4), nz(64))
            .alignment(0)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn metrics_track_allocate_free_and_failure() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();

        let initial = arena.metrics();
        assert_eq!(initial.bytes_reserved, 64);
        assert_eq!(initial.bytes_live, 0);

        let buf = arena.allocate().unwrap();
        let after_alloc = arena.metrics();
        assert_eq!(after_alloc.allocations_ok, 1);
        assert_eq!(after_alloc.allocations_failed, 0);
        assert_eq!(after_alloc.bytes_live, 64);

        assert_eq!(arena.allocate().unwrap_err(), AllocError::ArenaFull);
        let after_fail = arena.metrics();
        assert_eq!(after_fail.allocations_failed, 1);
        assert_eq!(after_fail.bytes_live, 64);

        drop(buf);
        let after_free = arena.metrics();
        assert_eq!(after_free.frees, 1);
        assert_eq!(after_free.bytes_live, 0);
    }

    #[test]
    fn build_size_overflow_fails() {
        let err = FixedArena::builder(nz(usize::MAX), nz(2))
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::SizeOverflow);
    }

    #[test]
    fn alignment_rounding_overflow_fails() {
        let err = FixedArena::builder(nz(1), nz(usize::MAX))
            .alignment(2)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::SizeOverflow);
    }

    #[test]
    fn alignment_rounds_capacity_up() {
        let arena = FixedArena::builder(nz(2), nz(100))
            .alignment(64)
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 128);
    }

    #[test]
    fn alignment_4096_rounds_up() {
        let arena = FixedArena::builder(nz(4), nz(100))
            .alignment(4096)
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 4096);
    }

    #[test]
    fn clone_shares_inner() {
        let arena = FixedArena::builder(nz(2), nz(64)).build().unwrap();
        let arena2 = arena.clone();
        assert_eq!(arena.slot_count(), arena2.slot_count());
        assert_eq!(arena.slot_capacity(), arena2.slot_capacity());
    }

    #[test]
    fn allocate_and_drop() {
        let arena = FixedArena::builder(nz(2), nz(64)).build().unwrap();

        let buf1 = arena.allocate().unwrap();
        let buf2 = arena.allocate().unwrap();
        assert!(arena.allocate().is_err(), "arena should be full");

        drop(buf1);
        let _buf3 = arena.allocate().unwrap();
        drop(buf2);
    }

    #[test]
    fn allocate_full_returns_arena_full() {
        let arena = FixedArena::builder(nz(1), nz(32)).build().unwrap();

        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[test]
    fn drop_returns_slot() {
        let arena = FixedArena::builder(nz(1), nz(32)).build().unwrap();

        let buf = arena.allocate().unwrap();
        drop(buf);
        assert!(
            arena.allocate().is_ok(),
            "slot should be available after drop"
        );
    }
}
