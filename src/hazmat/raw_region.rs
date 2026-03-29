use std::fmt;
use std::mem::ManuallyDrop;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::ops::Range;

use bytes::Bytes;

use crate::BuddyArena;
use crate::FixedArena;
use crate::allocation::{AllocationKind, ArenaRef};
use crate::arena::InitPolicy;
use crate::error::AllocError;
use crate::handle::BufferHandle;
use crate::sync::Arc;

/// Hazmat raw-access wrapper around [`FixedArena`].
///
/// Derefs to [`FixedArena`] for normal allocation and adds
/// [`raw_alloc()`](Self::raw_alloc).
#[derive(Clone)]
pub struct RawFixedArena(pub(crate) FixedArena);

impl std::ops::Deref for RawFixedArena {
    type Target = FixedArena;
    fn deref(&self) -> &FixedArena {
        &self.0
    }
}

impl fmt::Debug for RawFixedArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RawFixedArena").field(&self.0).finish()
    }
}

impl RawFixedArena {
    /// Allocate a raw region from the arena.
    ///
    /// Returns a [`RawRegion`] with direct pointer access to one arena slot.
    ///
    /// The returned region exposes the slot's full visible capacity.
    /// [`InitPolicy::Zero`] clears the slot before it is returned.
    /// Dropping the region releases the slot back to the arena.
    pub fn raw_alloc(&self) -> Result<RawRegion, AllocError> {
        let inner = &self.0.inner;
        let Some(slot_idx) = inner.bitmap.try_alloc() else {
            inner.metrics.record_alloc_failure();
            return Err(AllocError::ArenaFull);
        };

        let offset = slot_idx * inner.slot_capacity;

        match inner.init_policy {
            InitPolicy::Zero => {
                if let Some(ref zeroed_bm) = inner.zeroed_bitmap
                    && !zeroed_bm.all_set_in_range(slot_idx, slot_idx + 1)
                {
                    // SAFETY: ptr+offset..ptr+offset+slot_capacity is within the arena
                    // allocation and exclusively owned by this slot (bitmap claim above).
                    unsafe {
                        crate::arena::zeroize_region(inner.ptr.add(offset), inner.slot_capacity)
                    };
                }
            }
            InitPolicy::Uninit => {}
        }

        inner.metrics.record_alloc_success(inner.slot_capacity);

        Ok(RawRegion::new(
            ArenaRef::Fixed(Arc::clone(inner)),
            AllocationKind::Fixed { slot_idx },
            inner.ptr,
            offset,
            inner.slot_capacity,
        ))
    }
}

/// Hazmat raw-access wrapper around [`BuddyArena`].
///
/// Derefs to [`BuddyArena`] for normal allocation and adds
/// [`raw_alloc()`](Self::raw_alloc).
#[derive(Clone)]
pub struct RawBuddyArena(pub(crate) BuddyArena);

impl std::ops::Deref for RawBuddyArena {
    type Target = BuddyArena;
    fn deref(&self) -> &BuddyArena {
        &self.0
    }
}

impl fmt::Debug for RawBuddyArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RawBuddyArena").field(&self.0).finish()
    }
}

impl RawBuddyArena {
    /// Allocate a raw region from the buddy arena.
    ///
    /// The request rounds up to the next power-of-two multiple of
    /// [`min_block_size()`](BuddyArena::min_block_size).
    ///
    /// Visible capacity follows the arena geometry. [`exact()`](crate::BuddyGeometry::exact)
    /// exposes the full allocated block. [`nearest()`](crate::BuddyGeometry::nearest)
    /// caps visible capacity to `len` even when allocator slack is larger.
    /// [`InitPolicy::Zero`] clears the allocated block before it is returned.
    /// Dropping the region releases the block back to the arena.
    pub fn raw_alloc(&self, len: NonZeroUsize) -> Result<RawRegion, AllocError> {
        let arena = &self.0;
        let inner = &arena.inner;

        let target_order = arena.order_for_request(len.get()).ok_or_else(|| {
            inner.metrics.record_alloc_failure();
            AllocError::ArenaFull
        })?;

        let (order, block_idx) = arena
            .try_allocate_from_summary(target_order)
            .or_else(|| arena.try_allocate_from_full_scan(target_order))
            .ok_or_else(|| {
                inner.metrics.record_alloc_failure();
                AllocError::ArenaFull
            })?;

        let (final_order, final_block_idx) = arena.split_down(order, block_idx, target_order);
        let block_size = arena.block_size(final_order);
        let offset = arena.block_offset(final_order, final_block_idx);

        match inner.init_policy {
            InitPolicy::Zero => {
                if let Some(ref zeroed_bm) = inner.zeroed_bitmap {
                    let order0_start = final_block_idx * (1 << final_order);
                    let order0_end = order0_start + (1 << final_order);
                    if !zeroed_bm.all_set_in_range(order0_start, order0_end) {
                        // SAFETY: ptr+offset..ptr+offset+block_size is within the arena
                        // allocation and exclusively owned (bitmap claim above).
                        unsafe {
                            crate::arena::zeroize_region(inner.ptr.add(offset), block_size);
                        }
                    }
                }
            }
            InitPolicy::Uninit => {}
        }

        inner.metrics.record_alloc_success(block_size);

        let user_capacity = if inner.cap_capacity {
            len.get().min(block_size)
        } else {
            block_size
        };

        Ok(RawRegion::new(
            ArenaRef::Buddy(Arc::clone(inner)),
            AllocationKind::Buddy {
                order: final_order,
                block_idx: final_block_idx,
            },
            inner.ptr,
            offset,
            user_capacity,
        ))
    }
}

/// Error returned when a freeze range is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFreezeError {
    range_start: usize,
    range_end: usize,
    capacity: usize,
}

impl RawFreezeError {
    /// Range passed to `freeze`.
    pub fn range(&self) -> Range<usize> {
        self.range_start..self.range_end
    }

    /// Visible capacity of the raw region.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl fmt::Display for RawFreezeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.range_start > self.range_end {
            write!(
                f,
                "invalid freeze range: {}..{} (start > end)",
                self.range_start, self.range_end,
            )
        } else {
            write!(
                f,
                "freeze range {}..{} exceeds capacity {}",
                self.range_start, self.range_end, self.capacity,
            )
        }
    }
}

impl std::error::Error for RawFreezeError {}

/// Raw allocation from an arena.
///
/// Direct pointer access to arena memory without [`Buffer`](crate::Buffer),
/// `BufMut`, auto-spill, or length tracking. The caller tracks
/// initialization. [`freeze`](Self::freeze) is `unsafe`: the caller
/// guarantees the frozen range is initialized.
///
/// `freeze` returns ordinary [`Bytes`]. Clones, slices, and extension
/// traits on the result keep the backing allocation pinned.
pub struct RawRegion {
    owner: ManuallyDrop<ArenaRef>,
    allocation: AllocationKind,
    ptr: *mut u8,
    offset: usize,
    capacity: usize,
}

// SAFETY: RawRegion has exclusive access to its allocation. The raw pointer
// is anchored by the owning arena ref and only used within the allocation
// bounds described by offset/capacity.
unsafe impl Send for RawRegion {}

impl RawRegion {
    pub(crate) fn new(
        owner: ArenaRef,
        allocation: AllocationKind,
        ptr: *mut u8,
        offset: usize,
        capacity: usize,
    ) -> Self {
        Self {
            owner: ManuallyDrop::new(owner),
            allocation,
            ptr,
            offset,
            capacity,
        }
    }

    /// Capacity of this allocation in bytes (visible capacity).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Raw pointer to the start of the allocated region.
    pub fn as_ptr(&self) -> *const u8 {
        // SAFETY: offset is within the arena allocation.
        unsafe { self.ptr.add(self.offset) }
    }

    /// Mutable raw pointer to the start of the allocated region.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        // SAFETY: offset is within the arena allocation.
        unsafe { self.ptr.add(self.offset) }
    }

    /// The full allocation as a mutable slice of potentially uninitialized bytes.
    pub fn as_uninit_slice_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        // SAFETY: ptr+offset is valid for capacity bytes, exclusively accessed.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr.add(self.offset).cast::<MaybeUninit<u8>>(),
                self.capacity,
            )
        }
    }

    /// The full allocation as an immutable slice of potentially uninitialized bytes.
    pub fn as_uninit_slice(&self) -> &[MaybeUninit<u8>] {
        // SAFETY: ptr+offset is valid for capacity bytes.
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.add(self.offset).cast::<MaybeUninit<u8>>(),
                self.capacity,
            )
        }
    }

    /// Freeze a byte range into immutable [`Bytes`].
    ///
    /// Consumes the region. The slot or block returns to the arena when the
    /// last `Bytes` clone or slice drops. Common prefix case: `freeze(0..len)`.
    ///
    /// # Safety
    ///
    /// All bytes in `range` must be initialized.
    ///
    /// # Errors
    ///
    /// Returns [`RawFreezeError`] if `range.start > range.end` or
    /// `range.end > self.capacity()`.
    pub unsafe fn freeze(mut self, range: Range<usize>) -> Result<Bytes, RawFreezeError> {
        if range.start > range.end || range.end > self.capacity {
            return Err(RawFreezeError {
                range_start: range.start,
                range_end: range.end,
                capacity: self.capacity,
            });
        }

        self.owner.record_frozen();
        // SAFETY: owner is valid and taken exactly once during freeze.
        let owner = unsafe { ManuallyDrop::take(&mut self.owner) };
        let allocation = self.allocation;
        let ptr = self.ptr;
        let offset = self.offset + range.start;
        let len = range.end - range.start;

        std::mem::forget(self);

        let handle = BufferHandle::new(owner, allocation, ptr, offset, len);
        Ok(Bytes::from_owner(handle))
    }
}

impl fmt::Debug for RawRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawRegion")
            .field("allocation", &self.allocation)
            .field("offset", &self.offset)
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Drop for RawRegion {
    fn drop(&mut self) {
        self.owner.release(self.allocation);
        // SAFETY: owner is valid unless taken during freeze (which forgets self).
        unsafe { ManuallyDrop::drop(&mut self.owner) };
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::{BuddyArena, BuddyGeometry, FixedArena};

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn buddy_geo() -> BuddyGeometry {
        BuddyGeometry::exact(nz(4096), nz(512)).unwrap()
    }

    #[test]
    fn raw_region_capacity() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        assert_eq!(raw.capacity(), 64);
    }

    #[test]
    fn raw_region_freeze_prefix() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc().unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::copy_nonoverlapping(b"hello".as_ptr(), ptr, 5) };
        let bytes = unsafe { raw.freeze(0..5) }.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[test]
    fn raw_region_freeze_subslice() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc().unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::copy_nonoverlapping(b"XXhelloXX".as_ptr(), ptr, 9) };
        let bytes = unsafe { raw.freeze(2..7) }.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[test]
    fn raw_region_freeze_empty() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        let bytes = unsafe { raw.freeze(0..0) }.unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn raw_region_freeze_out_of_bounds() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        let err = unsafe { raw.freeze(0..65) }.unwrap_err();
        assert_eq!(err.capacity(), 64);
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn raw_region_freeze_inverted_range() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        let err = unsafe { raw.freeze(5..3) }.unwrap_err();
        assert_eq!(err.range(), 5..3);
    }

    #[test]
    fn raw_region_drop_releases_slot() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        assert!(arena.raw_alloc().is_err());
        drop(raw);
        assert!(arena.raw_alloc().is_ok());
    }

    #[test]
    fn raw_region_freeze_releases_on_bytes_drop() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc().unwrap();
        unsafe { raw.as_mut_ptr().write_bytes(0xAB, 8) };
        let bytes = unsafe { raw.freeze(0..8) }.unwrap();
        assert!(arena.raw_alloc().is_err());
        drop(bytes);
        assert!(arena.raw_alloc().is_ok());
    }

    #[test]
    fn raw_region_arena_dropped_while_bytes_alive() {
        let bytes = {
            let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
                .hazmat_raw_access()
                .build()
                .unwrap();
            let mut raw = arena.raw_alloc().unwrap();
            let ptr = raw.as_mut_ptr();
            unsafe { std::ptr::copy_nonoverlapping(b"persists".as_ptr(), ptr, 8) };
            unsafe { raw.freeze(0..8) }.unwrap()
        };
        assert_eq!(&bytes[..], b"persists");
    }

    #[test]
    fn raw_region_bytes_slicing_retains_arena() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc().unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::copy_nonoverlapping(b"hello world".as_ptr(), ptr, 11) };
        let bytes = unsafe { raw.freeze(0..11) }.unwrap();
        let hello = bytes.slice(0..5);
        drop(bytes);
        assert!(arena.raw_alloc().is_err());
        assert_eq!(&hello[..], b"hello");
        drop(hello);
        assert!(arena.raw_alloc().is_ok());
    }

    // --- Buddy raw_alloc tests ---

    #[test]
    fn buddy_raw_region_capacity() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc(nz(512)).unwrap();
        assert_eq!(raw.capacity(), 512);
    }

    #[test]
    fn buddy_raw_region_rounds_up() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc(nz(700)).unwrap();
        // exact geometry exposes full block capacity
        assert_eq!(raw.capacity(), 1024);
    }

    #[test]
    fn buddy_raw_region_cap_capacity() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(512)).unwrap();
        let arena = BuddyArena::builder(geo)
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc(nz(700)).unwrap();
        assert_eq!(raw.capacity(), 700);
    }

    #[test]
    fn buddy_raw_region_freeze_prefix() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc(nz(512)).unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::copy_nonoverlapping(b"buddy".as_ptr(), ptr, 5) };
        let bytes = unsafe { raw.freeze(0..5) }.unwrap();
        assert_eq!(&bytes[..], b"buddy");
    }

    #[test]
    fn buddy_raw_region_drop_releases_block() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc(nz(4096)).unwrap();
        assert!(arena.raw_alloc(nz(512)).is_err());
        drop(raw);
        assert!(arena.raw_alloc(nz(512)).is_ok());
    }

    #[test]
    fn buddy_raw_region_freeze_releases_on_bytes_drop() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc(nz(4096)).unwrap();
        unsafe { raw.as_mut_ptr().write_bytes(0xAB, 8) };
        let bytes = unsafe { raw.freeze(0..8) }.unwrap();
        assert!(arena.raw_alloc(nz(512)).is_err());
        drop(bytes);
        assert!(arena.raw_alloc(nz(512)).is_ok());
    }

    #[test]
    fn buddy_raw_region_coalesces_on_drop() {
        let arena = BuddyArena::builder(buddy_geo())
            .hazmat_raw_access()
            .build()
            .unwrap();
        let r1 = arena.raw_alloc(nz(2048)).unwrap();
        let r2 = arena.raw_alloc(nz(2048)).unwrap();
        drop(r1);
        drop(r2);
        assert!(arena.raw_alloc(nz(4096)).is_ok());
    }

    // --- Metrics and InitPolicy tests ---

    use crate::InitPolicy;

    #[test]
    fn fixed_raw_alloc_metrics_track_lifecycle() {
        let arena = FixedArena::with_slot_capacity(nz(2), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        let m = arena.metrics();
        assert_eq!(m.allocations_ok, 1);
        assert_eq!(m.bytes_live, 64);

        drop(raw);
        let m = arena.metrics();
        assert_eq!(m.frees, 1);
        assert_eq!(m.bytes_live, 0);
    }

    #[test]
    fn fixed_raw_alloc_metrics_track_freeze() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let mut raw = arena.raw_alloc().unwrap();
        unsafe { raw.as_mut_ptr().write_bytes(0, 8) };
        let bytes = unsafe { raw.freeze(0..8) }.unwrap();

        let m = arena.metrics();
        assert_eq!(m.frozen, 1);
        assert_eq!(m.frees, 0);
        assert_eq!(m.bytes_live, 64);

        drop(bytes);
        let m = arena.metrics();
        assert_eq!(m.frees, 1);
        assert_eq!(m.bytes_live, 0);
    }

    #[test]
    fn fixed_raw_alloc_metrics_track_failure() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let _raw = arena.raw_alloc().unwrap();
        let _ = arena.raw_alloc();

        let m = arena.metrics();
        assert_eq!(m.allocations_ok, 1);
        assert_eq!(m.allocations_failed, 1);
    }

    #[test]
    fn fixed_raw_alloc_zero_policy_zeroes_memory() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .init_policy(InitPolicy::Zero)
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc().unwrap();
        let slice = raw.as_uninit_slice();
        for byte in slice {
            assert_eq!(unsafe { byte.assume_init() }, 0);
        }
    }

    #[test]
    fn buddy_raw_alloc_zero_policy_zeroes_memory() {
        let arena = BuddyArena::builder(buddy_geo())
            .init_policy(InitPolicy::Zero)
            .hazmat_raw_access()
            .build()
            .unwrap();
        let raw = arena.raw_alloc(nz(512)).unwrap();
        let slice = raw.as_uninit_slice();
        for byte in &slice[..512] {
            assert_eq!(unsafe { byte.assume_init() }, 0);
        }
    }

    #[test]
    fn fixed_raw_alloc_zero_policy_zeroes_on_return() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .init_policy(InitPolicy::Zero)
            .hazmat_raw_access()
            .build()
            .unwrap();

        let mut raw = arena.raw_alloc().unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::write_bytes(ptr, 0xAB, 64) };
        let bytes = unsafe { raw.freeze(0..64) }.unwrap();
        drop(bytes);

        let raw = arena.raw_alloc().unwrap();
        let slice = raw.as_uninit_slice();
        for byte in slice {
            assert_eq!(unsafe { byte.assume_init() }, 0);
        }
    }

    #[test]
    fn buddy_raw_alloc_zero_policy_zeroes_on_return() {
        let arena = BuddyArena::builder(buddy_geo())
            .init_policy(InitPolicy::Zero)
            .hazmat_raw_access()
            .build()
            .unwrap();

        let mut raw = arena.raw_alloc(nz(512)).unwrap();
        let ptr = raw.as_mut_ptr();
        unsafe { std::ptr::write_bytes(ptr, 0xAB, 512) };
        let bytes = unsafe { raw.freeze(0..512) }.unwrap();
        drop(bytes);

        let raw = arena.raw_alloc(nz(512)).unwrap();
        let slice = raw.as_uninit_slice();
        for byte in &slice[..512] {
            assert_eq!(unsafe { byte.assume_init() }, 0);
        }
    }
}
