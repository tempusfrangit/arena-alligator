use std::num::NonZeroUsize;
use std::thread;
use std::time::{Duration, Instant};

use arena_alligator::{AllocError, BuddyArena, FixedArena};
use bytes::{BufMut, Bytes};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

const FIXED_SLOT_CAPACITY: usize = 4096;
const FIXED_WRITE_LEN: usize = 1024;
const FIXED_SLOT_COUNT: usize = 65_536;

const BUDDY_TOTAL_SIZE: usize = 64 * 1024 * 1024;
const BUDDY_MIN_BLOCK: usize = 256;
const BUDDY_SIZES: [usize; 8] = [192, 768, 2048, 6144, 12_288, 3072, 512, 16_384];

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn contention_levels() -> Vec<usize> {
    let mut levels = vec![1, 4, thread::available_parallelism().map_or(8, usize::from)];
    levels.sort_unstable();
    levels.dedup();
    levels
}

fn fixed_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("fixed_roundtrip");
    let arena = FixedArena::builder(nz(FIXED_SLOT_COUNT), nz(FIXED_SLOT_CAPACITY))
        .build()
        .unwrap();

    for len in [256usize, 1024, 4096] {
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter(|| {
                let mut buf = arena.allocate().unwrap();
                buf.put_bytes(0xAB, len);
                let bytes = buf.freeze();
                black_box(bytes.len());
                drop(bytes);
            });
        });
    }

    group.finish();
}

fn buddy_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy_roundtrip");
    let arena = BuddyArena::builder(nz(BUDDY_TOTAL_SIZE), nz(BUDDY_MIN_BLOCK))
        .build()
        .unwrap();

    for len in [256usize, 1024, 4096, 16_384] {
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter(|| {
                let mut buf = arena.allocate(nz(len)).unwrap();
                buf.put_bytes(0xCD, len);
                let bytes = buf.freeze();
                black_box(bytes.len());
                drop(bytes);
            });
        });
    }

    group.finish();
}

fn fixed_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("fixed_contention");

    for hold_pct in [0usize, 75] {
        for threads in contention_levels() {
            let id = BenchmarkId::new(
                format!("slots_{FIXED_SLOT_COUNT}_hold_{hold_pct}pct"),
                threads,
            );
            group.throughput(Throughput::Elements(threads as u64));
            group.bench_with_input(id, &threads, |b, &threads| {
                let arena = FixedArena::builder(nz(FIXED_SLOT_COUNT), nz(FIXED_SLOT_CAPACITY))
                    .build()
                    .unwrap();
                let held = hold_fixed_capacity(&arena, hold_pct);

                b.iter_custom(|iters| {
                    let elapsed = run_fixed_contention(arena.clone(), threads, iters as usize);
                    black_box(&held);
                    elapsed
                });
            });
        }
    }

    group.finish();
}

fn buddy_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy_contention");

    for hold_pct in [0usize, 50] {
        for threads in contention_levels() {
            let id = BenchmarkId::new(
                format!("arena_{}mb_hold_{hold_pct}pct", BUDDY_TOTAL_SIZE >> 20),
                threads,
            );
            group.throughput(Throughput::Elements(threads as u64));
            group.bench_with_input(id, &threads, |b, &threads| {
                let arena = BuddyArena::builder(nz(BUDDY_TOTAL_SIZE), nz(BUDDY_MIN_BLOCK))
                    .build()
                    .unwrap();
                let held = hold_buddy_capacity(&arena, hold_pct);

                b.iter_custom(|iters| {
                    let elapsed = run_buddy_contention(arena.clone(), threads, iters as usize);
                    black_box(&held);
                    elapsed
                });
            });
        }
    }

    group.finish();
}

fn fixed_auto_spill(c: &mut Criterion) {
    let mut group = c.benchmark_group("fixed_auto_spill");
    let arena = FixedArena::builder(nz(FIXED_SLOT_COUNT), nz(1024))
        .auto_spill()
        .build()
        .unwrap();

    for len in [2048usize, 8192] {
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter(|| {
                let mut buf = arena.allocate().unwrap();
                buf.put_bytes(0xEF, len);
                let bytes = buf.freeze();
                black_box(bytes.len());
                drop(bytes);
            });
        });
    }

    group.finish();
}

fn hold_fixed_capacity(arena: &FixedArena, hold_pct: usize) -> Vec<Bytes> {
    let held_slots = FIXED_SLOT_COUNT * hold_pct / 100;
    let mut held = Vec::with_capacity(held_slots);

    for _ in 0..held_slots {
        let buf = arena.allocate().unwrap();
        held.push(buf.freeze());
    }

    held
}

fn hold_buddy_capacity(arena: &BuddyArena, hold_pct: usize) -> Vec<Bytes> {
    let target = BUDDY_TOTAL_SIZE * hold_pct / 100;
    let mut held = Vec::new();
    let mut used = 0usize;

    while used + 1024 <= target {
        let mut buf = arena.allocate(nz(1024)).unwrap();
        buf.put_bytes(0x11, 1024);
        used += buf.capacity();
        held.push(buf.freeze());
    }

    held
}

fn run_fixed_contention(arena: FixedArena, threads: usize, iters: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(threads);

    for thread_idx in 0..threads {
        let arena = arena.clone();
        let per_thread = per_thread_iters(iters, threads, thread_idx);
        handles.push(thread::spawn(move || {
            for _ in 0..per_thread {
                let mut buf = alloc_fixed_retry(&arena);
                buf.put_bytes(0x5A, FIXED_WRITE_LEN);
                let bytes = buf.freeze();
                black_box(bytes.len());
                drop(bytes);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    start.elapsed()
}

fn run_buddy_contention(arena: BuddyArena, threads: usize, iters: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(threads);

    for thread_idx in 0..threads {
        let arena = arena.clone();
        let per_thread = per_thread_iters(iters, threads, thread_idx);
        handles.push(thread::spawn(move || {
            let mut next = thread_idx % BUDDY_SIZES.len();
            for _ in 0..per_thread {
                let len = BUDDY_SIZES[next];
                next = (next + 1) % BUDDY_SIZES.len();

                let mut buf = alloc_buddy_retry(&arena, len);
                buf.put_bytes(0x7C, len);
                let bytes = buf.freeze();
                black_box(bytes.len());
                drop(bytes);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    start.elapsed()
}

fn alloc_fixed_retry(arena: &FixedArena) -> arena_alligator::Buffer {
    loop {
        match arena.allocate() {
            Ok(buf) => return buf,
            Err(AllocError::ArenaFull) => std::hint::spin_loop(),
        }
    }
}

fn alloc_buddy_retry(arena: &BuddyArena, len: usize) -> arena_alligator::Buffer {
    loop {
        match arena.allocate(nz(len)) {
            Ok(buf) => return buf,
            Err(AllocError::ArenaFull) => std::hint::spin_loop(),
        }
    }
}

fn per_thread_iters(total: usize, threads: usize, thread_idx: usize) -> usize {
    let base = total / threads;
    let extra = usize::from(thread_idx < total % threads);
    base + extra
}

criterion_group!(
    allocator_benches,
    fixed_roundtrip,
    buddy_roundtrip,
    fixed_contention,
    buddy_contention,
    fixed_auto_spill
);
criterion_main!(allocator_benches);
