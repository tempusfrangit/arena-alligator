use std::fmt;
use std::ops::Deref;

use crate::buffer::Buffer;
use crate::FixedArena;

/// Policy for how `allocate_async` waits when the arena is full.
pub enum AsyncPolicy {
    /// Wake one waiter per slot free via `tokio::sync::Notify`.
    Notify,
}

/// Async-capable wrapper around [`FixedArena`].
///
/// Created via [`FixedArenaBuilder::build_async()`]. Provides
/// [`allocate_async()`](AsyncFixedArena::allocate_async) which parks
/// until a slot becomes available, while sync methods remain accessible
/// through `Deref<Target = FixedArena>`.
#[derive(Clone)]
pub struct AsyncFixedArena {
    inner: FixedArena,
}

impl AsyncFixedArena {
    pub(crate) fn new(inner: FixedArena) -> Self {
        Self { inner }
    }

    /// Allocate a buffer, waiting asynchronously if the arena is full.
    ///
    /// Returns once a slot becomes available. The bitmap is the source
    /// of truth; notifications are hints to retry.
    pub async fn allocate_async(&self) -> Buffer {
        let waker = self
            .inner
            .inner
            .waker
            .as_ref()
            .expect("allocate_async requires build_async()");
        match waker {
            WakerImpl::Notify(notify) => loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Ok(buf) = self.inner.allocate() {
                    return buf;
                }
                notified.await;
            },
        }
    }
}

impl Deref for AsyncFixedArena {
    type Target = FixedArena;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Debug for AsyncFixedArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncFixedArena")
            .field("inner", &self.inner)
            .finish()
    }
}

pub(crate) enum WakerImpl {
    Notify(tokio::sync::Notify),
}

impl WakerImpl {
    pub(crate) fn wake(&self) {
        match self {
            WakerImpl::Notify(notify) => notify.notify_one(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use bytes::BufMut;
    use tokio::time::{timeout, Duration};

    use crate::FixedArena;

    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[tokio::test]
    async fn allocate_async_basic() {
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"data");
        let bytes = buf.freeze();
        drop(bytes);
        let _buf2 = arena.allocate_async().await;
    }

    #[tokio::test]
    async fn allocate_async_waits_then_succeeds() {
        let arena = Arc::new(
            FixedArena::builder(nz(1), nz(32))
                .build_async(AsyncPolicy::Notify)
                .unwrap(),
        );
        let mut buf = arena.allocate_async().await;
        buf.put_slice(b"blocking");
        let bytes = buf.freeze();
        let arena2 = Arc::clone(&arena);
        let handle = tokio::spawn(async move {
            let buf = arena2.allocate_async().await;
            buf.capacity()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bytes);
        let cap = timeout(Duration::from_secs(2), handle)
            .await
            .expect("should not timeout")
            .expect("task should not panic");
        assert_eq!(cap, 32);
    }

    #[tokio::test]
    async fn sync_allocate_still_fast_fails() {
        let arena = FixedArena::builder(nz(1), nz(32))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[tokio::test]
    async fn multiple_waiters_all_served() {
        let arena = Arc::new(
            FixedArena::builder(nz(2), nz(32))
                .build_async(AsyncPolicy::Notify)
                .unwrap(),
        );
        let buf1 = arena.allocate().unwrap();
        let buf2 = arena.allocate().unwrap();
        let a1 = Arc::clone(&arena);
        let h1 = tokio::spawn(async move { a1.allocate_async().await.capacity() });
        let a2 = Arc::clone(&arena);
        let h2 = tokio::spawn(async move { a2.allocate_async().await.capacity() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(buf1);
        drop(buf2);
        let (r1, r2) = tokio::join!(
            timeout(Duration::from_secs(2), h1),
            timeout(Duration::from_secs(2), h2),
        );
        assert_eq!(r1.unwrap().unwrap(), 32);
        assert_eq!(r2.unwrap().unwrap(), 32);
    }

    #[tokio::test]
    async fn deref_exposes_sync_methods() {
        let arena = FixedArena::builder(nz(4), nz(64))
            .build_async(AsyncPolicy::Notify)
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }
}
