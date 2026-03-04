<p align="center">
  <img src="docs/assets/logo.svg" alt="Arena Alligator" width="520">
</p>

Lock-free arena allocator that produces `bytes::Bytes` via zero-copy freeze.

Write into a `Buffer`, freeze it into `Bytes`, and let the arena reclaim the backing memory when the last reference drops. The common path avoids a copy on freeze and avoids a fresh heap allocation per buffer.

```rust
use std::num::NonZeroUsize;
use arena_alligator::FixedArena;
use bytes::BufMut;

let arena = FixedArena::builder(
    NonZeroUsize::new(1024).unwrap(),
    NonZeroUsize::new(4096).unwrap(),
).build()?;

let mut buf = arena.allocate()?;
buf.put_slice(b"request payload");
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Allocator modes

`FixedArena` uses uniform slots. Allocation is a single bitmap claim. Use it when buffer sizes are predictable.

`BuddyArena` uses power-of-two blocks from one contiguous region. Requests are rounded up, larger blocks split on demand, and free blocks coalesce on release. Use it when sizes vary.

```rust
use std::num::NonZeroUsize;
use arena_alligator::BuddyArena;
use bytes::BufMut;

let arena = BuddyArena::builder(
    NonZeroUsize::new(64 * 1024 * 1024).unwrap(),
    NonZeroUsize::new(256).unwrap(),
).build()?;

let mut buf = arena.allocate(NonZeroUsize::new(8192).unwrap())?;
buf.put_slice(&vec![0u8; 8192]);
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Auto-spill

By default, writing past buffer capacity panics. With `auto_spill()`, the buffer copies its current contents to heap-backed storage, frees the arena allocation immediately, and continues writing on the heap. `freeze()` still returns `Bytes`.

```rust
# use std::num::NonZeroUsize;
# use arena_alligator::FixedArena;
# use bytes::BufMut;
let arena = FixedArena::builder(
    NonZeroUsize::new(1024).unwrap(),
    NonZeroUsize::new(1024).unwrap(),
).auto_spill().build()?;

let mut buf = arena.allocate()?;
buf.put_slice(&[0u8; 2048]);
assert!(buf.is_spilled());
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Async allocation

With the `async-alloc` feature, both arena types support `allocate_async()`. The task waits until capacity is available and then retries against the live bitmap state.

```toml
[dependencies]
arena-alligator = { version = "0.3", features = ["async-alloc"] }
```

```rust,ignore
let arena = FixedArena::builder(nz(1024), nz(4096))
    .build_async(AsyncPolicy::Notify)?;

let buf = arena.allocate_async().await;
```

## Metrics

Both arena types expose a `metrics()` snapshot. Fixed reports allocation, failure, spill, and live-capacity counters. Buddy adds split, coalesce, and largest-free-block data.

## Status

Pre-1.0. The crate is usable now, but the API may still move before it settles.

## License

MIT
