use std::sync::Arc;

use crate::arena::ArenaInner;

/// Internal owner for frozen arena memory. Dropped when the last
/// `Bytes` clone/slice is dropped, freeing the slot back to the arena.
pub(crate) struct BufferHandle {
    inner: Arc<ArenaInner>,
    slot_idx: usize,
    offset: usize,
    len: usize,
}

impl BufferHandle {
    pub(crate) fn new(inner: Arc<ArenaInner>, slot_idx: usize, offset: usize, len: usize) -> Self {
        Self {
            inner,
            slot_idx,
            offset,
            len,
        }
    }
}

impl AsRef<[u8]> for BufferHandle {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: Arc<ArenaInner> keeps the allocation alive.
        // Data is immutable after freeze consumed the Buffer.
        unsafe { std::slice::from_raw_parts(self.inner.ptr.add(self.offset), self.len) }
    }
}

impl Drop for BufferHandle {
    fn drop(&mut self) {
        // Release in bitmap.free() pairs with AcqRel in try_alloc's fetch_and.
        self.inner.bitmap.free(self.slot_idx);
        #[cfg(feature = "async-alloc")]
        if let Some(waker) = &self.inner.waker {
            waker.wake();
        }
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
