# Changelog

## 0.7.0

`0.7.0` breaks the API contract that `0.6.0` established: breaking changes were to ship with adapters instead. That is not possible here. `AsyncBuddyArena::allocate_async()` promised to always return a `Buffer`, but a request larger than the arena can ever satisfy never resolves and the future hangs forever. An adapter over that signature would either have to invent a `Buffer` that cannot exist or keep the hang.

Breaking changes:

- `AsyncBuddyArena::allocate_async()` now returns `Result<Buffer, AllocError>`. Requests that can never fit fail fast with `RequestTooLarge`; transient exhaustion still parks the caller until capacity frees.
- `AllocError` separates permanent from transient failure: `RequestTooLarge` means the request can never fit, `ArenaFull` remains the retryable signal. `AllocError`, `BuildError`, and `BufferFullError` are now `#[non_exhaustive]`, so future variants will not require another break.
- The arena builders are typestate-parameterized: `FixedArenaBuilder<Mode = Standard>` and `BuddyArenaBuilder<Mode = Standard>`, with `auto_spill()` moving the builder into its `AutoSpill` mode instead of returning `Self`. Fluent chains compile unchanged. Code that names a builder type explicitly, or binds the result of `auto_spill()` to the unparameterized type, must update.

Fixes:

- Raw-backed builders (`from_raw()` / `from_static()`) never write to caller-provided memory during build; the backing region survives `build()` byte-for-byte.
- `buddy_wake` no longer strands a live waiter when stale queue entries exhaust the per-wake pop bound.
- A buddy waiter cancelled between wake delivery and its next poll no longer swallows the wake. The wake is recovered on drop and handed to the next waiter, through a chain of cancelled waiters if needed.
- Oversized buddy requests that would overflow power-of-two rounding fail with `RequestTooLarge` instead of panicking in debug builds or computing a corrupt block order in release builds.

New features:

- The `hazmat-raw-access` feature adds `RawFixedArena`, `RawBuddyArena`, and `RawRegion` for direct pointer access when callers need to bypass `Buffer` and `BufMut`.
- `FixedArena` and `BuddyArena` support preallocated-memory handoff through `from_raw()`, plus safe `from_static()` helpers for `&'static mut` buffers in embedded and other caller-managed setups.
- The crate supports `#![no_std]` + `alloc` builds, with CI coverage for `--no-default-features`; the `async-alloc` path remains opt-in behind `std`.
- `InitPolicy::Zero` is a stronger security guarantee: arena memory is scrubbed on return to the free pool and cold memory is zeroed on first allocation, applied consistently across fixed, buddy, and hazmat raw-access paths through the `zeroize` crate.

## 0.6.0

This release rounds out the public allocator API and improves documentation after `0.5.x`.

- `FixedArena` is now presented more clearly as the recommended fast path when one slot size covers the workload.
- `BuddyArena` now has a better front door for variable-size workloads through `BuddyGeometry`, including `exact()` and `nearest()` constructors.
- Builder APIs were cleaned up so the common fixed-arena path reads more directly, especially in examples and docs.
- Allocation policy controls expanded with `PageSize`, prefault support, `Unfaulted`, and `InitPolicy` for workloads that care about page-fault timing or zeroing behavior.
- `BytesExt::into_owned()` was added for the handoff case where frozen bytes should move back to owned mutable heap storage.
- Public docs gained more doctests, repaired doc links, and an explicit docs CI job.
- The repository examples now include a Treiber-stack waiter example for custom async wake behavior, and examples are exercised in CI.

## 0.5.x - 2026-03-06

`0.5.0` was the production-hardening release. `0.5.1` followed immediately with release polish and documentation cleanup.

- Loom coverage was added and the sync/atomic layer was abstracted so the allocator could be exercised under loom.
- Miri and stronger CI coverage were added, including improved main-branch concurrency handling.
- Buddy async wake delivery was fixed to avoid starvation under contention, and fixed-arena waiter registration cleanup was corrected so permits were not stranded after cancellation.
- Benchmark coverage expanded substantially, including published result summaries and an extreme-contention mode.
- Deployment and operator-facing docs improved with a NUMA guide, a CONTRIBUTING guide, and clearer README framing for async allocation and examples.
- `0.5.1` itself mostly packaged the line cleanly: versioning, release workflow fixes, benchmark-doc cleanup, and README/API-stability clarification.

## 0.4.0 - 2026-03-02

`0.4.0` focused on documentation packaging rather than allocator behavior.

- docs.rs builds were switched to `all-features`, which makes the async API visible in published docs instead of looking partially missing.
- Cargo metadata was bumped for the release with the matching lockfile refresh.

## 0.3.0 - 2026-03-02

`0.3.0` was the first fully shaped public release line for the crate.

- Fixed-slot allocation, buddy allocation, zero-copy freeze into `Bytes`, and auto-spill were all present.
- Async allocation support covered both allocator families.
- Metrics landed for both the fixed and buddy allocators.
- Benchmarks, a usage guide, and examples were added so the crate shipped with performance and operational context rather than only API surface.
- CI and publish workflows were introduced to make tagged releases reproducible.
