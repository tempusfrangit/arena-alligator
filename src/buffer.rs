use std::fmt;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use bytes::buf::UninitSlice;
use bytes::{BufMut, Bytes, BytesMut};

use crate::arena::ArenaInner;
use crate::error::BufferFullError;
use crate::handle::BufferHandle;

/// A writable buffer backed by arena memory.
///
/// Created by [`FixedArena::allocate()`]. Write data via `BufMut` trait methods,
/// then call [`freeze()`](Buffer::freeze) to produce immutable `Bytes`.
///
/// Dropping without freezing returns the slot to the arena.
pub struct Buffer {
    pub(crate) inner: ManuallyDrop<Arc<ArenaInner>>,
    pub(crate) slot_idx: usize,
    pub(crate) offset: usize,
    pub(crate) capacity: usize,
    pub(crate) len: usize,
    pub(crate) spilled: Option<BytesMut>,
}

impl Buffer {
    pub(crate) fn new(
        inner: Arc<ArenaInner>,
        slot_idx: usize,
        offset: usize,
        capacity: usize,
    ) -> Self {
        Self {
            inner: ManuallyDrop::new(inner),
            slot_idx,
            offset,
            capacity,
            len: 0,
            spilled: None,
        }
    }

    /// Capacity of this buffer in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        if let Some(buf) = &self.spilled {
            buf.len()
        } else {
            self.len
        }
    }

    /// Whether no bytes have been written.
    pub fn is_empty(&self) -> bool {
        if let Some(buf) = &self.spilled {
            buf.is_empty()
        } else {
            self.len == 0
        }
    }

    /// Whether this buffer has spilled to the heap.
    pub fn is_spilled(&self) -> bool {
        self.spilled.is_some()
    }

    /// Try to append `data`. Returns `Err` with remaining/requested sizes if it won't fit.
    pub fn try_put_slice(&mut self, data: &[u8]) -> Result<(), BufferFullError> {
        let remaining = self.remaining_mut();
        if remaining < data.len() {
            return Err(BufferFullError {
                remaining,
                requested: data.len(),
            });
        }
        self.put_slice(data);
        Ok(())
    }

    /// Check if `len` bytes will fit without overflow.
    pub fn will_fit(&self, len: usize) -> bool {
        self.remaining_mut() >= len
    }

    /// Freeze buffer into immutable `Bytes`.
    ///
    /// Consumes the buffer. The returned `Bytes` keeps the arena memory
    /// alive via `Arc`. When the last `Bytes` clone/slice drops, the
    /// slot is freed back to the arena.
    ///
    /// If the buffer has spilled to the heap, returns heap-backed `Bytes`
    /// (the arena slot was already freed during spill).
    pub fn freeze(mut self) -> Bytes {
        if let Some(spilled) = self.spilled.take() {
            // SAFETY: inner is valid, ManuallyDrop prevents double-drop.
            unsafe { ManuallyDrop::drop(&mut self.inner) };
            std::mem::forget(self);
            return spilled.freeze();
        }

        // SAFETY: inner is valid and not yet dropped.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        let slot_idx = self.slot_idx;
        let offset = self.offset;
        let len = self.len;

        std::mem::forget(self);

        let handle = BufferHandle::new(inner, slot_idx, offset, len);
        Bytes::from_owner(handle)
    }

    /// Explicitly abandon this buffer without freezing.
    ///
    /// Equivalent to `drop(buf)`. Exists for readability.
    pub fn abandon(self) {
        drop(self);
    }

    fn do_spill(&mut self) {
        tracing::warn!(
            slot = self.slot_idx,
            capacity = self.capacity,
            "arena buffer spilled to heap"
        );

        let mut buffer = BytesMut::with_capacity(self.len * 2);
        // SAFETY: ptr + offset is valid for self.len bytes (written data).
        let src = unsafe { std::slice::from_raw_parts(self.inner.ptr.add(self.offset), self.len) };
        buffer.extend_from_slice(src);

        self.inner.bitmap.free(self.slot_idx);
        #[cfg(feature = "async-alloc")]
        if let Some(waker) = &self.inner.waker {
            waker.wake();
        }

        self.spilled = Some(buffer);
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("slot_idx", &self.slot_idx)
            .field("offset", &self.offset)
            .field("capacity", &self.capacity)
            .field("len", &self.len)
            .finish()
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.spilled.is_none() {
            self.inner.bitmap.free(self.slot_idx);
            #[cfg(feature = "async-alloc")]
            if let Some(waker) = &self.inner.waker {
                waker.wake();
            }
        }
        // SAFETY: inner is valid and not yet dropped.
        unsafe { ManuallyDrop::drop(&mut self.inner) };
    }
}

// SAFETY: we uphold BufMut's contract — advance_mut only called after
// writing to chunk_mut, and we track len correctly.
unsafe impl BufMut for Buffer {
    fn remaining_mut(&self) -> usize {
        if let Some(buf) = &self.spilled {
            return buf.remaining_mut();
        }
        if self.inner.auto_spill {
            usize::MAX
        } else {
            self.capacity - self.len
        }
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        if let Some(buf) = &mut self.spilled {
            // SAFETY: caller guarantees cnt bytes were initialized in chunk_mut.
            unsafe { buf.advance_mut(cnt) };
            return;
        }
        assert!(
            self.len + cnt <= self.capacity,
            "arena buffer overflow: {} + {} > {}",
            self.len,
            cnt,
            self.capacity,
        );
        self.len += cnt;
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        if self.spilled.is_some() {
            return self.spilled.as_mut().unwrap().chunk_mut();
        }
        if self.inner.auto_spill && self.len >= self.capacity {
            self.do_spill();
            return self.spilled.as_mut().unwrap().chunk_mut();
        }
        // SAFETY: ptr + offset + len is within the slot's allocated region.
        let ptr = unsafe { self.inner.ptr.add(self.offset + self.len) };
        let remaining = self.capacity - self.len;
        // SAFETY: ptr is valid for remaining bytes, exclusively accessed by this Buffer.
        unsafe { UninitSlice::from_raw_parts_mut(ptr, remaining) }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::FixedArena;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn buffer_reports_capacity_and_empty() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let buf = arena.allocate().unwrap();
        assert_eq!(buf.capacity(), 64);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
    }

    use bytes::BufMut;

    #[test]
    fn put_slice_and_len() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello");
        assert_eq!(buf.len(), 5);
        assert!(!buf.is_empty());
        assert_eq!(buf.remaining_mut(), 59);
    }

    #[test]
    fn put_slice_fills_exactly() {
        let arena = FixedArena::builder(nz(1), nz(8)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345678");
        assert_eq!(buf.len(), 8);
        assert_eq!(buf.remaining_mut(), 0);
    }

    #[test]
    #[should_panic(expected = "advance out of bounds")]
    fn put_slice_overflow_panics() {
        let arena = FixedArena::builder(nz(1), nz(4)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345");
    }

    #[test]
    fn try_put_slice_ok() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        assert!(buf.try_put_slice(b"hello").is_ok());
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn try_put_slice_full() {
        let arena = FixedArena::builder(nz(1), nz(4)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        let err = buf.try_put_slice(b"12345").unwrap_err();
        assert_eq!(err.remaining, 4);
        assert_eq!(err.requested, 5);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn will_fit() {
        let arena = FixedArena::builder(nz(1), nz(10)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        assert!(buf.will_fit(10));
        assert!(!buf.will_fit(11));
        buf.put_slice(b"12345");
        assert!(buf.will_fit(5));
        assert!(!buf.will_fit(6));
    }

    #[test]
    fn multiple_writes_accumulate() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello ");
        buf.put_slice(b"world");
        assert_eq!(buf.len(), 11);
    }

    #[test]
    fn auto_spill_on_overflow() {
        let arena = FixedArena::builder(nz(1), nz(8))
            .auto_spill()
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345678");
        assert!(!buf.is_spilled());
        buf.put_slice(b"overflow!");
        assert!(buf.is_spilled());
        assert_eq!(buf.len(), 17);
    }

    #[test]
    fn auto_spill_freeze_produces_valid_bytes() {
        let arena = FixedArena::builder(nz(1), nz(4))
            .auto_spill()
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345678");
        assert!(buf.is_spilled());
        let bytes = buf.freeze();
        assert_eq!(&bytes[..], b"12345678");
    }

    #[test]
    fn auto_spill_frees_arena_slot_immediately() {
        let arena = FixedArena::builder(nz(1), nz(4))
            .auto_spill()
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345");
        let _buf2 = arena.allocate().unwrap();
    }

    #[test]
    fn auto_spill_remaining_mut_is_usize_max() {
        let arena = FixedArena::builder(nz(1), nz(8))
            .auto_spill()
            .build()
            .unwrap();
        let buf = arena.allocate().unwrap();
        assert_eq!(buf.remaining_mut(), usize::MAX);
    }

    #[test]
    fn auto_spill_abandon_frees_slot() {
        let arena = FixedArena::builder(nz(1), nz(4))
            .auto_spill()
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"12345");
        drop(buf);
        assert!(arena.allocate().is_ok());
    }
}
