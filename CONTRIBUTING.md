# Contributing

Thanks for contributing to `arena-alligator`.

## Development workflow

This repository uses `mise` as the task runner. Run all project tasks through `mise`.

List tasks:

```sh
mise tasks --all
```

Most common commands:

```sh
mise run format:fix
mise run clippy
mise run test
```

## Local validation before PR

Run the full local validation path before opening a pull request:

```sh
mise run test
```

For faster iteration during development:

```sh
mise run test:nextest
mise run format
mise run clippy
```

## Benchmarks

Benchmarks are not required for most PRs. Run them when allocator behavior, contention behavior, or performance-sensitive paths change.

```sh
mise run bench
mise run bench:extreme
```

Benchmark documentation and latest published summaries:

- [Benchmark results](docs/benchmarks.md)

## Pull requests

Keep PRs focused and small. Include:

1. A concise problem statement.
2. The behavioral change.
3. Validation performed (tests/bench commands run).

If relevant, include before/after benchmark deltas and platform details (CPU, architecture, OS, thread count).

## Style

- Follow `rustfmt` output.
- Treat clippy warnings as errors for contributions.
- Prefer clear naming and small, composable functions over large multi-purpose blocks.

## Atomic and alignment invariants

Changes touching atomics, memory ordering, or alignment-sensitive structures must preserve the crate's concurrency and layout assumptions.

- Keep allocator synchronization lock-free and atomic-based (no mutex substitution in hot allocation paths).
- Treat `src/bitmap.rs` `CacheAligned<T>` wrappers as false-sharing guards; do not remove or reduce alignment without benchmark evidence.
- Keep slot/alignment inputs power-of-two validated for builders (`FixedArena`/`BuddyArena`), and preserve capacity rounding behavior.
- If changing atomic orderings, document why the new ordering is correct and which happens-before edge still protects safety/liveness.

Required evidence for these changes (in PR description):

1. A clear problem statement tied to a concrete invariant (safety, liveness, or performance regression).
2. A minimal reproducible case that demonstrates the issue before the change.
3. Exact commands used to reproduce, including feature flags, thread counts, and environment details.
4. Before/after output showing the issue is fixed (or regression removed) by the change.

Evidence must be reproducible by another contributor on comparable hardware/settings.

Required validation commands:

```sh
mise run test
mise run test:nextest
mise run clippy
```

If contention behavior or bitmap internals change, also run:

```sh
mise run bench
```
