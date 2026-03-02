use arena_alligator::{AllocError, FixedArena};
use bytes::BufMut;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[test]
fn freeze_bytes_slice_drop_lifecycle() {
    let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"hello world");
    let bytes = buf.freeze();

    assert!(arena.allocate().is_err(), "slot held by frozen Bytes");

    let hello = bytes.slice(0..5);
    let world = bytes.slice(6..11);
    assert_eq!(&hello[..], b"hello");
    assert_eq!(&world[..], b"world");

    drop(bytes);
    assert!(arena.allocate().is_err(), "slices still hold the slot");

    drop(hello);
    drop(world);
    let _recovered = arena.allocate().expect("slot freed after all slices dropped");
}

#[test]
fn drop_without_freeze_returns_slot() {
    let arena = FixedArena::builder(nz(1), nz(64)).build().unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"data that will be abandoned");

    assert!(arena.allocate().is_err(), "slot held by buffer");

    drop(buf);
    assert!(arena.allocate().is_ok(), "slot freed after drop without freeze");
}

#[test]
fn auto_spill_freeze_path() {
    let arena = FixedArena::builder(nz(1), nz(8))
        .auto_spill()
        .build()
        .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"12345678");
    assert!(!buf.is_spilled());

    buf.put_slice(b"overflow!");
    assert!(buf.is_spilled());

    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"12345678overflow!");

    assert!(arena.allocate().is_ok(), "slot freed after spill");
}

#[test]
fn auto_spill_drop_path() {
    let arena = FixedArena::builder(nz(1), nz(4))
        .auto_spill()
        .build()
        .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"12345");
    assert!(buf.is_spilled());

    // Slot already freed by spill, but we can still allocate after drop
    // to confirm no double-free or leak.
    drop(buf);
    assert!(arena.allocate().is_ok(), "slot freed after spilled buffer dropped");
}

#[test]
fn exhaustion_returns_arena_full() {
    let arena = FixedArena::builder(nz(48), nz(16)).build().unwrap();

    let mut buffers = Vec::with_capacity(48);
    for _ in 0..48 {
        buffers.push(arena.allocate().unwrap());
    }

    let err = arena.allocate().unwrap_err();
    assert_eq!(err, AllocError::ArenaFull);
}

#[test]
fn concurrent_allocate_free_stress() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::Barrier;
    use std::time::Instant;

    fn rand_bool(thread_id: u64, iteration: u64) -> bool {
        let mut hasher = DefaultHasher::new();
        thread_id.hash(&mut hasher);
        iteration.hash(&mut hasher);
        Instant::now().hash(&mut hasher);
        hasher.finish() & 1 == 0
    }

    let slot_count = 64;
    let arena = Arc::new(FixedArena::builder(nz(slot_count), nz(32)).build().unwrap());
    let threads = 8;
    let iterations = 500;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let arena = Arc::clone(&arena);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..iterations {
                    if let Ok(mut buf) = arena.allocate() {
                        buf.put_slice(b"test");
                        if rand_bool(t as u64, i as u64) {
                            let _bytes = buf.freeze();
                        } else {
                            drop(buf);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let mut recovered = Vec::new();
    while let Ok(buf) = arena.allocate() {
        recovered.push(buf);
    }
    assert_eq!(recovered.len(), slot_count, "all slots recovered at quiescence");
}

#[test]
fn arena_dropped_while_bytes_live() {
    let bytes = {
        let arena = FixedArena::builder(nz(2), nz(64)).build().unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"persists after arena drop");
        buf.freeze()
    };

    assert_eq!(&bytes[..], b"persists after arena drop");
}

#[test]
fn alignment_capacity_rounded() {
    let arena = FixedArena::builder(nz(4), nz(100))
        .alignment(4096)
        .build()
        .unwrap();

    assert_eq!(arena.slot_capacity(), 4096);

    let mut frozen = Vec::with_capacity(4);
    for _ in 0..4 {
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"aligned");
        frozen.push(buf.freeze());
    }

    assert!(arena.allocate().is_err(), "all 4 slots in use");
}

#[test]
fn alignment_write_full_capacity() {
    let arena = FixedArena::builder(nz(1), nz(1))
        .alignment(512)
        .build()
        .unwrap();

    assert_eq!(arena.slot_capacity(), 512);

    let mut buf = arena.allocate().unwrap();
    let data = vec![0xABu8; 512];
    buf.put_slice(&data);

    let bytes = buf.freeze();
    assert_eq!(bytes.len(), 512);
    assert!(bytes.iter().all(|&b| b == 0xAB));
}

#[test]
fn mixed_freeze_and_abandon_all_slots_recover() {
    let arena = FixedArena::builder(nz(8), nz(32)).build().unwrap();

    let mut frozen = Vec::new();
    for i in 0..8 {
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(&[i as u8; 4]);
        if i % 2 == 0 {
            frozen.push(buf.freeze());
        } else {
            drop(buf);
        }
    }

    // 4 odd slots already freed, 4 even slots held by frozen Bytes
    let mut available = 0;
    let mut temp = Vec::new();
    while let Ok(buf) = arena.allocate() {
        available += 1;
        temp.push(buf);
    }
    assert_eq!(available, 4, "odd-indexed slots freed by abandon");
    drop(temp);

    drop(frozen);

    let mut recovered = Vec::new();
    while let Ok(buf) = arena.allocate() {
        recovered.push(buf);
    }
    assert_eq!(recovered.len(), 8, "all 8 slots recovered");
}
