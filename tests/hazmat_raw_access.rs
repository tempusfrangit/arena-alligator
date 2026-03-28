#![cfg(feature = "hazmat-raw-access")]

use std::num::NonZeroUsize;

use bytes::BufMut;

use arena_alligator::hazmat::{RawBuddyArena, RawFixedArena};
use arena_alligator::{BuddyArena, BuddyGeometry, FixedArena};

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[test]
fn fixed_raw_alloc_concurrent_stress() {
    let arena: RawFixedArena = FixedArena::with_slot_capacity(nz(64), nz(128))
        .hazmat_raw_access()
        .build()
        .unwrap();

    std::thread::scope(|s| {
        for _ in 0..8 {
            let arena = &arena;
            s.spawn(move || {
                for i in 0..500u32 {
                    if let Ok(mut raw) = arena.raw_alloc() {
                        let ptr = raw.as_mut_ptr();
                        let data = i.to_le_bytes();
                        unsafe {
                            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, 4);
                        }
                        let bytes = unsafe { raw.freeze(0..4) }.unwrap();
                        assert_eq!(&bytes[..], &data);
                    }
                }
            });
        }
    });
}

#[test]
fn buddy_raw_alloc_concurrent_stress() {
    let arena: RawBuddyArena =
        BuddyArena::builder(BuddyGeometry::exact(nz(1024 * 1024), nz(256)).unwrap())
            .hazmat_raw_access()
            .build()
            .unwrap();

    std::thread::scope(|s| {
        for _ in 0..8 {
            let arena = &arena;
            s.spawn(move || {
                for i in 0..500u32 {
                    if let Ok(mut raw) = arena.raw_alloc(nz(256)) {
                        let ptr = raw.as_mut_ptr();
                        let data = i.to_le_bytes();
                        unsafe {
                            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, 4);
                        }
                        let bytes = unsafe { raw.freeze(0..4) }.unwrap();
                        assert_eq!(&bytes[..], &data);
                    }
                }
            });
        }
    });
}

#[test]
fn raw_alloc_and_buffer_coexist() {
    let arena = FixedArena::with_slot_capacity(nz(4), nz(64))
        .hazmat_raw_access()
        .build()
        .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"buffer");
    let bytes_buf = buf.freeze();

    let mut raw = arena.raw_alloc().unwrap();
    let ptr = raw.as_mut_ptr();
    unsafe { std::ptr::copy_nonoverlapping(b"raw".as_ptr(), ptr, 3) };
    let bytes_raw = unsafe { raw.freeze(0..3) }.unwrap();

    assert_eq!(&bytes_buf[..], b"buffer");
    assert_eq!(&bytes_raw[..], b"raw");
}

#[test]
fn freeze_subslice_skips_header() {
    let arena = FixedArena::with_slot_capacity(nz(1), nz(128))
        .hazmat_raw_access()
        .build()
        .unwrap();

    let mut raw = arena.raw_alloc().unwrap();
    let ptr = raw.as_mut_ptr();
    unsafe {
        let len_bytes = 5u32.to_le_bytes();
        std::ptr::copy_nonoverlapping(len_bytes.as_ptr(), ptr, 4);
        std::ptr::copy_nonoverlapping(b"hello".as_ptr(), ptr.add(4), 5);
    }

    let bytes = unsafe { raw.freeze(4..9) }.unwrap();
    assert_eq!(&bytes[..], b"hello");
}

#[test]
fn bytes_clone_retains_arena_backing() {
    let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
        .hazmat_raw_access()
        .build()
        .unwrap();

    let mut raw = arena.raw_alloc().unwrap();
    let ptr = raw.as_mut_ptr();
    unsafe { std::ptr::copy_nonoverlapping(b"retained".as_ptr(), ptr, 8) };
    let bytes = unsafe { raw.freeze(0..8) }.unwrap();

    let cloned = bytes.clone();
    drop(bytes);

    assert!(arena.raw_alloc().is_err());
    assert_eq!(&cloned[..], b"retained");

    drop(cloned);
    assert!(arena.raw_alloc().is_ok());
}
