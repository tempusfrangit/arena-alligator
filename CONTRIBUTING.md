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

Benchmarks are optional for most PRs and should be run when allocator behavior, contention behavior, or performance-sensitive paths change.

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
