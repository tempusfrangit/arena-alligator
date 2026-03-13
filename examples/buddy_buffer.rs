//! Buddy arena: variable-size allocations from a shared region.

use std::num::NonZeroUsize;

use arena_alligator::{BuddyArena, BuddyGeometry};
use bytes::BufMut;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn main() {
    let geo = BuddyGeometry::exact(nz(1024 * 1024), nz(256)).unwrap();
    let arena = BuddyArena::builder(geo).build().unwrap();

    let mut small = arena.allocate(nz(100)).unwrap();
    small.put_slice(b"small payload");
    println!("requested 100 B, got {} B capacity", small.capacity());
    let small_bytes = small.freeze();

    let mut large = arena.allocate(nz(50_000)).unwrap();
    large.put_bytes(0xCD, 50_000);
    println!("requested 50000 B, got {} B capacity", large.capacity());
    let large_bytes = large.freeze();

    let m = arena.metrics();
    println!(
        "splits: {}, largest_free_block: {} B",
        m.splits, m.largest_free_block
    );

    drop(small_bytes);
    drop(large_bytes);

    let m = arena.metrics();
    println!(
        "after drop: coalesces: {}, largest_free_block: {} B",
        m.coalesces, m.largest_free_block
    );
}
