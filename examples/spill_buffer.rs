//! Auto-spill: handle rare oversize payloads without panicking.

use std::num::NonZeroUsize;

use arena_alligator::FixedArena;
use bytes::BufMut;

fn main() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(32).unwrap(),
        NonZeroUsize::new(1024).unwrap(),
    )
    .auto_spill()
    .build()
    .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(&[0xAA; 512]);
    assert!(!buf.is_spilled());
    let small = buf.freeze();
    println!("small: {} bytes, arena-backed", small.len());

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(&[0xBB; 2048]);
    assert!(buf.is_spilled());
    let large = buf.freeze();
    println!("large: {} bytes, heap-backed after spill", large.len());

    let m = arena.metrics();
    println!("spills: {}, bytes_live: {}", m.spills, m.bytes_live);
}
