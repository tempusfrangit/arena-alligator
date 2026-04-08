<p align="center">
  <img src="docs/assets/logo.svg" alt="Arena Alligator" width="520">
</p>

<p align="center">
  <a href="https://crates.io/crates/arena-alligator"><img alt="crates.io" src="https://img.shields.io/crates/v/arena-alligator.svg"></a>
  <a href="https://docs.rs/arena-alligator"><img alt="docs.rs" src="https://docs.rs/arena-alligator/badge.svg"></a>
  <a href="https://github.com/tempusfrangit/arena-alligator/actions/workflows/ci.yaml"><img alt="CI" src="https://github.com/tempusfrangit/arena-alligator/actions/workflows/ci.yaml/badge.svg?branch=main"></a>
  <a href="https://deepwiki.com/tempusfrangit/arena-alligator"><img alt="DeepWiki" src="https://img.shields.io/badge/DeepWiki-reference-0A66C2"></a>
</p>

Arena allocator for building `bytes::Bytes` without copying the final payload.

Write into a `Buffer`, call `freeze()`, and hand off `Bytes` backed by arena memory. The slot or block returns to the arena when the last clone or slice drops.

`FixedArena` is the recommended high-throughput path when one slot size can cover the workload: uniform slots, a bitmap claim/release path, and predictable allocation cost.

`BuddyArena` is the variable-size allocator. Use it when request sizes swing enough that one fixed slot size would waste memory or spill too often. It rounds requests up to powers of two, splits larger blocks on demand, and coalesces neighbors on release.

## Quick start

```rust
use std::num::NonZeroUsize;
use arena_alligator::FixedArena;
use bytes::BufMut;

let arena = FixedArena::with_slot_capacity(
    NonZeroUsize::new(1024).unwrap(),
    NonZeroUsize::new(4096).unwrap(),
).build()?;

let mut buf = arena.allocate()?;
buf.put_slice(b"request payload");
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Which allocator should I use?

| Arena | Start here when | Why |
| ----- | ---------------- | --- |
| `FixedArena` | Most allocations fit one chosen slot size, or a small set of predictable slot sizes across separate arenas | Fastest path, simplest capacity planning, lowest allocator overhead |
| `BuddyArena` | Request sizes vary enough that fixed slots would waste memory or spill too often | One shared region, power-of-two block reuse, split/coalesce behavior for variable-size workloads |

Use `FixedArena` by default. Use `BuddyArena` only when variable-size allocation is a hard requirement.

## Buddy allocator example

```rust
use std::num::NonZeroUsize;
use arena_alligator::{BuddyArena, BuddyGeometry};
use bytes::BufMut;

let arena = BuddyArena::builder(
    BuddyGeometry::exact(
        NonZeroUsize::new(64 * 1024 * 1024).unwrap(),
        NonZeroUsize::new(256).unwrap(),
    )?,
)
.build()?;

let mut buf = arena.allocate(NonZeroUsize::new(8192).unwrap())?;
buf.put_slice(&vec![0u8; 8192]);
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Geometry choice

`BuddyGeometry::exact(...)` validates a geometry that is already chosen. Invalid inputs fail immediately.

`BuddyGeometry::nearest(...)` snaps the requested total size and minimum block size up to the nearest valid buddy geometry. Use it when the target shape is approximate and automatic adjustment is acceptable.

## Auto-spill

By default, writing past capacity panics. That follows the contract of a fixed-size `BufMut`: once capacity is exhausted, additional writes are an error unless a different behavior is selected explicitly.

With `auto_spill()`, the buffer copies its current contents to heap-backed storage, releases the arena allocation immediately, and keeps writing on the heap. `freeze()` still returns `Bytes`.

```rust
# use std::num::NonZeroUsize;
# use arena_alligator::FixedArena;
# use bytes::BufMut;
let arena = FixedArena::with_slot_capacity(
    NonZeroUsize::new(1024).unwrap(),
    NonZeroUsize::new(1024).unwrap(),
).auto_spill().build()?;

let mut buf = arena.allocate()?;
buf.put_slice(&[0u8; 2048]);
assert!(buf.is_spilled());
let _bytes = buf.freeze();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Initialization policy

By default, arena allocations use `InitPolicy::Uninit`. That follows Rust's usual high-performance model for writable capacity: newly allocated bytes are not zeroed, and only written bytes become part of the frozen `Bytes`.

For security-sensitive workloads, `InitPolicy::Zero` zeroes memory on return to the arena and on first allocation. Returned slots and blocks are scrubbed before being marked free, preventing data leaks between callers. All zeroing uses the `zeroize` crate (compiler-guaranteed not elided).

```rust
# use std::num::NonZeroUsize;
# use arena_alligator::{FixedArena, InitPolicy};
let arena = FixedArena::with_slot_capacity(
    NonZeroUsize::new(1024).unwrap(),
    NonZeroUsize::new(4096).unwrap(),
)
    .init_policy(InitPolicy::Zero)
    .build()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Preallocated memory handoff

Use `from_raw()` when the backing region already exists and the arena should
build on top of it instead of allocating its own memory. This is the path for
shared memory, mmap'd regions, static buffers, and other externally-provisioned
storage.

For `&'static mut` buffers, prefer the safe `from_static()` wrapper. Keep
`from_raw()` for pointer/length regions and custom deallocation strategies.

The unsafe boundary is at construction time: the caller must provide a valid,
exclusive region and the correct deallocation strategy. After construction, the
arena uses the same safe `allocate()` / `freeze()` flow as the ordinary builder
path.

`SlotSpec` derives fixed-slot geometry from a caller-provided region.
`BuddyHint` derives buddy geometry from the same kind of region.
Use `NoDealloc` when the caller retains responsibility for freeing the backing
memory.

```rust
# use std::num::NonZeroUsize;
# use arena_alligator::{FixedArena, SlotSpec};
static mut BLOCK: [u8; 4096] = [0; 4096];
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[allow(static_mut_refs)]
let arena = FixedArena::from_static(unsafe { &mut BLOCK }, SlotSpec::Count(nz(4)))
    .build()?;

assert_eq!(arena.slot_count(), 4);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Hazmat raw access

For protocols or layouts that need direct pointer access, enable the
`hazmat-raw-access` feature and opt in per arena builder with
`.hazmat_raw_access()`.

`RawRegion` bypasses `Buffer` and `BufMut`. You write through raw pointers or
`MaybeUninit<u8>` slices, then call `unsafe freeze(range)` when the frozen range
is fully initialized.

Returned `Bytes` are ordinary `bytes::Bytes`: clones and slices keep the arena
backing allocation pinned until every reference drops. Keep this off the default
path unless you need the extra control.

```toml
[dependencies]
arena-alligator = { version = "0.6", features = ["hazmat-raw-access"] }
```

```rust
# use std::num::NonZeroUsize;
# use arena_alligator::FixedArena;
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

let arena = FixedArena::with_slot_capacity(nz(4), nz(128))
    .hazmat_raw_access()
    .build()?;

let mut raw = arena.raw_alloc()?;
let ptr = raw.as_mut_ptr();

unsafe {
    let len = 5u32.to_le_bytes();
    std::ptr::copy_nonoverlapping(len.as_ptr(), ptr, 4);
    std::ptr::copy_nonoverlapping(b"hello".as_ptr(), ptr.add(4), 5);
}

let payload = unsafe { raw.freeze(4..9) }?;
assert_eq!(&payload[..], b"hello");
# Ok::<(), Box<dyn std::error::Error>>(())
```

See [`docs/hazmat.md`](docs/hazmat.md) for the sharp edges and the visibility
rules for buddy raw allocations.

## Async allocation

With the `async-alloc` feature, both arena types support `allocate_async()`. The task waits until capacity becomes available instead of busy-looping or falling back to the heap.

```toml
[dependencies]
arena-alligator = { version = "0.6", features = ["async-alloc"] }
```

```rust,ignore
let arena = Arc::new(
    FixedArena::with_slot_capacity(
        NonZeroUsize::new(2).unwrap(),
        NonZeroUsize::new(256).unwrap(),
    )
        .build_async()
        .unwrap(),
);

let buf = arena.allocate_async().await;
```

## Metrics

`metrics()` snapshots allocator state. Fixed reports allocation, failure, spill, and live-capacity counters. Buddy adds split, coalesce, and largest-free-block data so fragmentation pressure is visible directly.

In load tests, watch `spill_count` and `largest_free_block` over time to catch pressure and fragmentation early.

## Owned mutable bytes

`BytesExt::into_owned()` is the explicit handoff from arena-backed frozen bytes to owned mutable heap storage. Unlike `auto_spill()`, which changes storage implicitly on write overflow, `into_owned()` makes the copy at the point where the caller chooses to leave arena-backed storage:

```rust
use std::num::NonZeroUsize;
use arena_alligator::{FixedArena, BytesExt};
use bytes::BufMut;

let arena = FixedArena::with_slot_capacity(
    NonZeroUsize::new(4).unwrap(),
    NonZeroUsize::new(64).unwrap(),
).build()?;

let mut buf = arena.allocate()?;
buf.put_slice(b"hello");
let frozen = buf.freeze();

let mut owned = frozen.into_owned();
// Arena slot is freed; owned is heap-backed and mutable
owned.put_slice(b" world");
assert_eq!(&owned[..], b"hello world");
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Examples

Start with `fixed_buffer`, then run `buddy_buffer` for the variable-size path.

| Example | What it shows |
| ------- | ------------- |
| [`fixed_buffer`](examples/fixed_buffer.rs) | Allocate, write, freeze, and send across threads |
| [`buddy_buffer`](examples/buddy_buffer.rs) | Variable-size allocations with split and coalesce |
| [`spill_buffer`](examples/spill_buffer.rs) | Auto-spill to heap when a buffer outgrows slot capacity |
| [`hazmat_fixed_raw`](examples/hazmat_fixed_raw.rs) | Raw fixed-slot access with header/payload freeze |
| [`hazmat_buddy_raw`](examples/hazmat_buddy_raw.rs) | Raw buddy access with visible-capacity behavior |
| [`async_alloc`](examples/async_alloc.rs) | Wait for capacity with `allocate_async()` |
| [`treiber_waker`](examples/treiber_waker.rs) | Custom `Waiter` impl using a lock-free Treiber stack |

```sh
mise run examples
```

## Benchmarks

Benchmark summary tables and local Criterion HTML report links are in
[`docs/benchmarks.md`](docs/benchmarks.md).
That page includes both the Apple M4 Max baseline and a real-hardware k8s run summary.

Run benchmarks with:

```sh
mise run bench
mise run bench:extreme
```

`bench:extreme` enables an additional high-thread contention point (`40` threads by default).
Override via `ARENA_BENCH_EXTREME_THREADS=<n>`.

## Development tasks

This repository uses [`mise`](https://mise.jdx.dev/) as its task runner. Install it from the official guide: <https://mise.jdx.dev/installing-mise.html>.

Common commands in this repository:

```sh
mise run test
mise run format:fix
mise run clippy
mise run examples
mise run bench
```

List all available tasks with:

```sh
mise tasks --all
```

## Validation

The crate is exercised under standard tests, doctests, examples, and targeted concurrency validation:

- `miri` checks unsafe code paths for undefined behavior regressions.
- `loom` models the sync and async coordination paths under many thread interleavings.
- CI also runs formatting, clippy, docs, examples, and MSRV coverage.

## Deployment guides

- [NUMA-aware deployment pattern](docs/numa.md): per-node arenas, thread pinning, and bounded cross-node fallback.

## Changelog

Release notes are in [CHANGELOG.md](CHANGELOG.md).

## Status

As of `0.6.0`, the API is stabilized. Any future API changes will ship with adapters rather than break the contract directly.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development workflow and PR expectations.

## License

MIT
