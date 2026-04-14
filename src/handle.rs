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
        unsafe { core::slice::from_raw_parts(self.ptr.add(self.offset), self.len) }
    }
}

impl Drop for BufferHandle {
    fn drop(&mut self) {
        self.inner.release(self.allocation);
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;

    use bytes::BufMut;

    use crate::{BuddyArena, BuddyGeometry, FixedArena};

    #[test]
    fn freeze_produces_correct_bytes() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        )
        .build()
        .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello world");
        let bytes = buf.freeze();
        assert_eq!(&bytes[..], b"hello world");
    }

    #[test]
    fn abandon_returns_slot() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build()
        .unwrap();
        let buf = arena.allocate().unwrap();
        buf.abandon();
        assert!(arena.allocate().is_ok());
    }

    #[test]
    fn freeze_empty_buffer() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        )
        .build()
        .unwrap();
        let buf = arena.allocate().unwrap();
        let bytes = buf.freeze();
        assert_eq!(bytes.len(), 0);
        assert!(bytes.is_empty());
    }

    #[test]
    fn buddy_freeze_produces_correct_bytes() {
        let arena = BuddyArena::builder(
            BuddyGeometry::exact(
                NonZeroUsize::new(4096).unwrap(),
                NonZeroUsize::new(512).unwrap(),
            )
            .unwrap(),
        )
        .build()
        .unwrap();
        let mut buf = arena.allocate(NonZeroUsize::new(700).unwrap()).unwrap();
        buf.put_slice(b"buddy hello");
        let bytes = buf.freeze();
        assert_eq!(&bytes[..], b"buddy hello");
    }

    #[test]
    fn buddy_arena_dropped_while_bytes_alive() {
        let bytes = {
            let arena = BuddyArena::builder(
                BuddyGeometry::exact(
                    NonZeroUsize::new(4096).unwrap(),
                    NonZeroUsize::new(512).unwrap(),
                )
                .unwrap(),
            )
            .build()
            .unwrap();
            let mut buf = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
            buf.put_slice(b"buddy persists");
            buf.freeze()
        };
        assert_eq!(&bytes[..], b"buddy persists");
    }

    #[test]
    fn freeze_metrics_track_retained_capacity() {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        )
        .build()
        .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"metrics");

        let bytes = buf.freeze();
        let frozen = arena.metrics();
        assert_eq!(frozen.frozen, 1);
        assert_eq!(frozen.frees, 0);
        assert_eq!(frozen.bytes_live, 64);

        drop(bytes);
        let after_drop = arena.metrics();
        assert_eq!(after_drop.frees, 1);
        assert_eq!(after_drop.bytes_live, 0);
    }
}
