//! Buddy arena: variable-size allocations from a shared region.

use std::num::NonZeroUsize;

use arena_alligator::{BuddyArena, BuddyGeometry};
use bytes::BufMut;

fn main() {
    let geo = BuddyGeometry::exact(
        NonZeroUsize::new(1024 * 1024).unwrap(),
        NonZeroUsize::new(256).unwrap(),
    )
    .unwrap();
    let arena = BuddyArena::builder(geo).build().unwrap();

    let mut small = arena.allocate(NonZeroUsize::new(100).unwrap()).unwrap();
    small.put_slice(b"small payload");
    println!("requested 100 B, got {} B capacity", small.capacity());
    let small_bytes = small.freeze();

    let mut large = arena.allocate(NonZeroUsize::new(50_000).unwrap()).unwrap();
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
