# NUMA-Aware Deployment Pattern

This allocator is fast enough that NUMA placement usually dominates allocator internals on multi-socket machines.

Baseline rule: use one arena per NUMA node, and keep worker threads on that node.

## Page faulting and placement

The kernel allocates physical pages on the NUMA node where the faulting thread runs. The crate gives you three ways to control this:

**Pin the builder thread.** Call `build()` from a thread pinned to the target node. Pages are prefaulted on that node at build time.

```rust,ignore
// Thread pinned to node 0
let arena = FixedArena::builder(nz(1024), nz(4096))
    .page_size(PageSize::Auto)
    .build()?;
```

**Deferred faulting.** Call `build_unfaulted()`, move the `Unfaulted` wrapper to a pinned thread, then call `fault_pages()`.

```rust,ignore
let unfaulted = FixedArena::builder(nz(1024), nz(4096))
    .page_size(PageSize::Auto)
    .build_unfaulted()?;

// On pinned thread:
let arena = unfaulted.fault_pages();
```

**Kernel demand-faulting.** Skip the prefault walk entirely. The kernel faults pages on whichever thread touches them first (first-touch policy). Useful when workers are already pinned and each will naturally touch its own slice.

```rust,ignore
let arena = FixedArena::builder(nz(1024), nz(4096))
    .build_unfaulted()?
    .into_inner();
```

## Practical defaults

1. Create one allocator instance per NUMA node.
2. Partition worker pools by node (`node0-workers`, `node1-workers`, etc.).
3. Pin workers to CPUs local to their node.
4. Route requests to the local node first.
5. Only use cross-node fallback when the local node is exhausted, and cap fallback volume.

Do not treat all memory as one global pool unless your traffic is already cross-node and latency-insensitive.

## Why this works

- Local memory access avoids remote-hop latency and bandwidth contention.
- Per-node arenas reduce cache-line traffic on allocator metadata under high concurrency.
- Fallback preserves availability without making remote allocations the steady-state path.

## Cross-node fallback policy

Keep fallback explicit and bounded:

- Prefer local arena.
- On local allocation failure, try one remote node.
- Track fallback rate (`remote_allocations / total_allocations`).
- Alert if fallback stays elevated (for example, over 1-5% for sustained periods).

If fallback is persistently high, rebalance work by node or increase capacity on the hot node instead of widening fallback fan-out.

## Kubernetes notes

For predictable node-local behavior in k8s:

- Request whole CPUs where possible.
- Use CPU pinning-friendly settings (for example, dedicated CPU manager policies in clusters that support them).
- Keep process thread count aligned with assigned CPUs.
- Avoid overcommitting CPU on latency-critical pods.

Even inside a container, node-local CPU and memory placement still matter on dual-socket hosts.

## Minimal architecture sketch

```text
socket 0 / NUMA node 0
  workers 0..N (pinned) -> arena_node0

socket 1 / NUMA node 1
  workers N+1..M (pinned) -> arena_node1

on local exhaustion:
  arena_nodeX -> one-shot fallback -> arena_nodeY
```

Start simple. Validate local-first behavior in production metrics before adding more sophisticated routing.
