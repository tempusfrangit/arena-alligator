//! Async allocation: wait until capacity is available.

use std::num::NonZeroUsize;
use std::sync::Arc;

use arena_alligator::{AsyncPolicy, FixedArena};
use bytes::BufMut;

#[tokio::main]
async fn main() {
    let arena = Arc::new(
        FixedArena::builder(
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(256).unwrap(),
        )
        .build_async(AsyncPolicy::Notify)
        .unwrap(),
    );

    let mut buf1 = arena.allocate_async().await;
    buf1.put_slice(b"request 1");
    let bytes1 = buf1.freeze();

    let mut buf2 = arena.allocate_async().await;
    buf2.put_slice(b"request 2");
    let bytes2 = buf2.freeze();

    let arena2 = Arc::clone(&arena);
    let waiter = tokio::spawn(async move {
        let mut buf = arena2.allocate_async().await;
        buf.put_slice(b"waited for this");
        buf.freeze()
    });

    drop(bytes1);

    let result = waiter.await.unwrap();
    println!(
        "async allocation got: {}",
        std::str::from_utf8(&result).unwrap()
    );

    drop(bytes2);
    drop(result);

    let m = arena.metrics();
    println!(
        "allocations: {}, frees: {}, bytes_live: {}",
        m.allocations_ok, m.frees, m.bytes_live
    );
}
