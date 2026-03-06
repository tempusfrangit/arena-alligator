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
arena-alligator = { version = "0.4", features = ["async-alloc"] }
```

```rust,ignore
let arena = Arc::new(
    FixedArena::builder(nz(2), nz(256))
        .build_async()
        .unwrap(),
);

let buf = arena.allocate_async().await;
```

## Metrics

Use `metrics()` on `FixedArena` and `BuddyArena` to snapshot allocator state. Fixed reports allocation, failure, spill, and live-capacity counters. Buddy adds split, coalesce, and largest-free-block data.

In load tests, watch `spill_count` and `largest_free_block` over time to catch pressure and fragmentation early.

## Examples

If you're new to the crate, run `fixed_buffer` first, then `buddy_buffer` for variable-size behavior.

| Example | What it shows |
| ------- | ------------- |
| [`fixed_buffer`](examples/fixed_buffer.rs) | Allocate, write, freeze, and send across threads |
| [`buddy_buffer`](examples/buddy_buffer.rs) | Variable-size allocations with split and coalesce |
| [`spill_buffer`](examples/spill_buffer.rs) | Auto-spill to heap when a buffer outgrows slot capacity |
| [`async_alloc`](examples/async_alloc.rs) | Wait for capacity with `allocate_async()` |

```sh
cargo run --example fixed_buffer
cargo run --example async_alloc --features async-alloc
```

## Benchmarks

Benchmark summary tables and local Criterion HTML report links are in
[`docs/benchmarks.md`](docs/benchmarks.md).
That page now includes both the Apple M4 Max baseline and a real-hardware
k8s run summary (normal + extreme modes).

Run benchmarks with:

```sh
mise run bench
mise run bench:extreme
```

`bench:extreme` enables an additional high-thread contention point (`40` threads by default).
Override via `ARENA_BENCH_EXTREME_THREADS=<n>`.

## Deployment guides

- [NUMA-aware deployment pattern](docs/numa.md): per-node arenas, thread pinning, and bounded cross-node fallback.

## Status

Pre-1.0. The crate is usable now, but the API may still move before it settles.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development workflow and PR expectations.

## License

MIT
