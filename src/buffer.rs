use std::fmt;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use crate::arena::ArenaInner;

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
        }
    }

    /// Capacity of this buffer in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no bytes have been written.
    pub fn is_empty(&self) -> bool {
        self.len == 0
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
        self.inner.bitmap.free(self.slot_idx);
        // SAFETY: inner is valid and not yet dropped.
        unsafe { ManuallyDrop::drop(&mut self.inner) };
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
}
