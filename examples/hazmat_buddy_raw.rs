//! Hazmat buddy arena: visible capacity follows geometry, not block slack.

use std::num::NonZeroUsize;

use arena_alligator::{BuddyArena, BuddyGeometry};

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn main() {
    let exact = BuddyArena::builder(BuddyGeometry::exact(nz(4096), nz(256)).unwrap())
        .hazmat_raw_access()
        .build()
        .unwrap();
    let exact_raw = exact.raw_alloc(nz(300)).unwrap();
    println!("exact visible capacity: {}", exact_raw.capacity());
    drop(exact_raw);

    let nearest = BuddyArena::builder(BuddyGeometry::nearest(nz(4096), nz(256)).unwrap())
        .hazmat_raw_access()
        .build()
        .unwrap();
    let mut nearest_raw = nearest.raw_alloc(nz(300)).unwrap();
    println!("nearest visible capacity: {}", nearest_raw.capacity());

    unsafe {
        std::ptr::copy_nonoverlapping(b"buddy".as_ptr(), nearest_raw.as_mut_ptr(), 5);
    }
    let bytes = unsafe { nearest_raw.freeze(0..5) }.unwrap();
    println!("payload: {:?}", std::str::from_utf8(&bytes).unwrap());

    drop(bytes);
    assert!(nearest.raw_alloc(nz(300)).is_ok());
}
