use crate::allocation::{AllocationKind, ArenaRef};

/// Internal owner for frozen arena memory. Dropped when the last
/// `Bytes` clone/slice is dropped, freeing the slot back to the arena.
pub(crate) struct BufferHandle {
    inner: ArenaRef,
    allocation: AllocationKind,
    ptr: *mut u8,
    offset: usize,
    len: usize,
}

// SAFETY: BufferHandle only exposes immutable reads after freeze. The owning
// arena ref keeps the backing memory alive until the final frozen owner drops.
unsafe impl Send for BufferHandle {}

impl BufferHandle {
    pub(crate) fn new(
        inner: ArenaRef,
        allocation: AllocationKind,
        ptr: *mut u8,
        offset: usize,
        len: usize,
    ) -> Self {
        Self {
            inner,
            allocation,
            ptr,
            offset,
            len,
        }
    }
}

impl AsRef<[u8]> for BufferHandle {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the owning arena ref keeps the allocation alive.
        // Data is immutable after freeze consumed the Buffer.
        unsafe { std::slice::from_raw_parts(self.ptr.add(self.offset), self.len) }
    }
}

impl Drop for BufferHandle {
    fn drop(&mut self) {
        self.inner.release(self.allocation);
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use bytes::BufMut;

    use crate::FixedArena;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn freeze_produces_correct_bytes() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello world");
        let bytes = buf.freeze();
        assert_eq!(&bytes[..], b"hello world");
    }

    #[test]
    fn freeze_slot_freed_after_bytes_drop() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();

        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"data");
        let bytes = buf.freeze();

        assert!(arena.allocate().is_err());

        drop(bytes);
        assert!(arena.allocate().is_ok());
    }

    #[test]
    fn bytes_slice_is_zero_copy() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello world");
        let bytes = buf.freeze();

        let hello = bytes.slice(0..5);
        let world = bytes.slice(6..11);
        assert_eq!(&hello[..], b"hello");
        assert_eq!(&world[..], b"world");
    }

    #[test]
    fn arena_dropped_while_bytes_alive() {
        let bytes = {
            let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
            let mut buf = arena.allocate().unwrap();
            buf.put_slice(b"persists");
            buf.freeze()
        };
        assert_eq!(&bytes[..], b"persists");
    }

    #[test]
    fn abandon_returns_slot() {
        let arena = FixedArena::builder(nz(1), nz(32)).build().unwrap();
        let buf = arena.allocate().unwrap();
        buf.abandon();
        assert!(arena.allocate().is_ok());
    }

    #[test]
    fn freeze_empty_buffer() {
        let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();
        let buf = arena.allocate().unwrap();
        let bytes = buf.freeze();
        assert_eq!(bytes.len(), 0);
        assert!(bytes.is_empty());
    }
}
