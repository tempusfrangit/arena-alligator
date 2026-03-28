//! Hazmat fixed arena: write a header and freeze only the payload.

use std::num::NonZeroUsize;

use arena_alligator::FixedArena;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn main() {
    let arena = FixedArena::with_slot_capacity(nz(4), nz(128))
        .hazmat_raw_access()
        .build()
        .unwrap();

    let mut raw = arena.raw_alloc().unwrap();
    let ptr = raw.as_mut_ptr();
    let payload = b"hello";

    unsafe {
        let len = (payload.len() as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len.as_ptr(), ptr, 4);
        std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr.add(4), payload.len());
    }

    let bytes = unsafe { raw.freeze(4..4 + payload.len()) }.unwrap();
    println!("payload: {:?}", std::str::from_utf8(&bytes).unwrap());

    drop(bytes);
    assert!(arena.raw_alloc().is_ok());
}
