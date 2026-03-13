use bytes::{Bytes, BytesMut};

/// Extension methods for converting [`Bytes`] into owned mutable storage.
pub trait BytesExt {
    /// Copy the contents to a new heap-backed `BytesMut`, consuming `self`.
    ///
    /// This always copies the payload into a fresh `BytesMut`.
    ///
    /// If `self` is the last handle to arena-backed storage, dropping it here
    /// releases the slot or block immediately. Clones and slices keep the arena
    /// allocation pinned until the last related `Bytes` is dropped.
    ///
    /// ```
    /// use bytes::BufMut;
    /// use arena_alligator::{FixedArena, BytesExt};
    /// use std::num::NonZeroUsize;
    ///
    /// let arena = FixedArena::with_slot_capacity(
    ///     NonZeroUsize::new(4).unwrap(),
    ///     NonZeroUsize::new(64).unwrap(),
    /// ).build().unwrap();
    ///
    /// let mut buf = arena.allocate().unwrap();
    /// buf.put_slice(b"hello");
    /// let frozen = buf.freeze();
    ///
    /// let mut owned = frozen.into_owned();
    /// owned.put_slice(b" world");
    /// assert_eq!(&owned[..], b"hello world");
    /// ```
    fn into_owned(self) -> BytesMut;
}

impl BytesExt for Bytes {
    fn into_owned(self) -> BytesMut {
        BytesMut::from(self.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use bytes::BufMut;

    use super::*;
    use crate::FixedArena;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn into_owned_frees_arena_slot() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello");
        let frozen = buf.freeze();

        assert!(arena.allocate().is_err());

        let owned = frozen.into_owned();
        assert_eq!(&owned[..], b"hello");
        assert!(arena.allocate().is_ok());
    }

    #[test]
    fn into_owned_returns_mutable_bytes() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello");
        let frozen = buf.freeze();

        let mut owned = frozen.into_owned();
        owned.put_slice(b" world");
        assert_eq!(&owned[..], b"hello world");
    }

    #[test]
    fn into_owned_works_on_non_arena_bytes() {
        let bytes = Bytes::from_static(b"static data");
        let owned = bytes.into_owned();
        assert_eq!(&owned[..], b"static data");
    }

    #[test]
    fn into_owned_does_not_free_while_other_clones_exist() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .build()
            .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"hello");
        let frozen = buf.freeze();
        let clone = frozen.clone();

        let owned = frozen.into_owned();
        assert_eq!(&owned[..], b"hello");
        assert!(arena.allocate().is_err());

        drop(clone);
        assert!(arena.allocate().is_ok());
    }
}
