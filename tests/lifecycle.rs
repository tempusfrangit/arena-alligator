use arena_alligator::{AllocError, BuddyArena, BuddyGeometry, FixedArena};
use bytes::BufMut;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;

#[test]
fn freeze_bytes_slice_drop_lifecycle() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(64).unwrap(),
    )
    .build()
    .unwrap();

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
    let _recovered = arena
        .allocate()
        .expect("slot freed after all slices dropped");
}

#[test]
fn drop_without_freeze_returns_slot() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(64).unwrap(),
    )
    .build()
    .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"data that will be abandoned");

    assert!(arena.allocate().is_err(), "slot held by buffer");

    drop(buf);
    assert!(
        arena.allocate().is_ok(),
        "slot freed after drop without freeze"
    );
}

#[test]
fn auto_spill_freeze_path() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(8).unwrap(),
    )
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
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(4).unwrap(),
    )
    .auto_spill()
    .build()
    .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"12345");
    assert!(buf.is_spilled());

    // Slot already freed by spill, but we can still allocate after drop
    // to confirm no double-free or leak.
    drop(buf);
    assert!(
        arena.allocate().is_ok(),
        "slot freed after spilled buffer dropped"
    );
}

#[test]
fn exhaustion_returns_arena_full() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(48).unwrap(),
        NonZeroUsize::new(16).unwrap(),
    )
    .build()
    .unwrap();

    let mut buffers = Vec::with_capacity(48);
    for _ in 0..48 {
        buffers.push(arena.allocate().unwrap());
    }

    let err = arena.allocate().unwrap_err();
    assert_eq!(err, AllocError::ArenaFull);
}

#[test]
fn concurrent_allocate_free_stress() {
    use std::sync::Barrier;

    fn rand_bool(thread_id: u64, iteration: u64) -> bool {
        let mix = thread_id
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(iteration.wrapping_mul(0x6C62_272E_07BB_0143));
        mix & 1 == 0
    }

    let slot_count = 64;
    let arena = Arc::new(
        FixedArena::with_slot_capacity(
            NonZeroUsize::new(slot_count).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        )
        .build()
        .unwrap(),
    );
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
    assert_eq!(
        recovered.len(),
        slot_count,
        "all slots recovered at quiescence"
    );
}

#[test]
fn arena_dropped_while_bytes_live() {
    let bytes = {
        let arena = FixedArena::with_slot_capacity(
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        )
        .build()
        .unwrap();
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(b"persists after arena drop");
        buf.freeze()
    };

    assert_eq!(&bytes[..], b"persists after arena drop");
}

#[test]
fn alignment_capacity_rounded() {
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(100).unwrap(),
    )
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
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
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
    let arena = FixedArena::with_slot_capacity(
        NonZeroUsize::new(8).unwrap(),
        NonZeroUsize::new(32).unwrap(),
    )
    .build()
    .unwrap();

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

#[test]
fn buddy_drop_without_freeze_returns_space() {
    let arena = BuddyArena::builder(
        BuddyGeometry::exact(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(512).unwrap(),
        )
        .unwrap(),
    )
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(700).unwrap()).unwrap();
    buf.put_slice(b"buddy buffer");

    let _large = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
    assert_eq!(
        arena
            .allocate(NonZeroUsize::new(2048).unwrap())
            .unwrap_err(),
        AllocError::ArenaFull,
        "remaining space is too fragmented for another 2 KiB block"
    );

    drop(buf);

    assert!(
        arena.allocate(NonZeroUsize::new(2048).unwrap()).is_ok(),
        "dropping the 1 KiB block should restore a contiguous 2 KiB region"
    );
}

#[test]
fn buddy_mixed_size_churn_recovers_full_arena() {
    let arena = BuddyArena::builder(
        BuddyGeometry::exact(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(512).unwrap(),
        )
        .unwrap(),
    )
    .build()
    .unwrap();

    let a = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    let b = arena.allocate(NonZeroUsize::new(1500).unwrap()).unwrap();
    let c = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    let d = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    let e = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();

    assert_eq!(
        arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap_err(),
        AllocError::ArenaFull
    );

    drop(c);
    drop(a);
    assert_eq!(
        arena
            .allocate(NonZeroUsize::new(4096).unwrap())
            .unwrap_err(),
        AllocError::ArenaFull,
        "the remaining live allocations still block full-arena coalescing"
    );

    drop(d);
    drop(e);
    drop(b);

    let whole = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();
    assert_eq!(whole.capacity(), 4096);
}

#[test]
fn buddy_freeze_bytes_slice_drop_lifecycle() {
    let arena = BuddyArena::builder(
        BuddyGeometry::exact(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(512).unwrap(),
        )
        .unwrap(),
    )
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(700).unwrap()).unwrap();
    buf.put_slice(b"hello buddy world");
    let bytes = buf.freeze();

    let _other = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
    assert_eq!(
        arena
            .allocate(NonZeroUsize::new(2048).unwrap())
            .unwrap_err(),
        AllocError::ArenaFull,
        "the frozen 1 KiB block still prevents another 2 KiB coalesce"
    );

    let hello = bytes.slice(0..5);
    let world = bytes.slice(12..17);
    assert_eq!(&hello[..], b"hello");
    assert_eq!(&world[..], b"world");

    drop(bytes);
    assert_eq!(
        arena
            .allocate(NonZeroUsize::new(2048).unwrap())
            .unwrap_err(),
        AllocError::ArenaFull,
        "slices should keep the buddy block pinned after the root Bytes drops"
    );

    drop(hello);
    drop(world);
    assert!(
        arena.allocate(NonZeroUsize::new(2048).unwrap()).is_ok(),
        "the buddy block should release after the final slice drops"
    );
}

#[test]
fn buddy_auto_spill_freeze_path() {
    let arena = BuddyArena::builder(
        BuddyGeometry::exact(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(512).unwrap(),
        )
        .unwrap(),
    )
    .auto_spill()
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(700).unwrap()).unwrap();
    buf.put_slice(&vec![b'a'; 1024]);
    assert!(!buf.is_spilled());

    buf.put_slice(&vec![b'b'; 2048]);
    assert!(buf.is_spilled());
    assert!(
        arena.allocate(NonZeroUsize::new(4096).unwrap()).is_ok(),
        "spill should release the buddy block immediately"
    );

    let bytes = buf.freeze();
    assert_eq!(bytes.len(), 3072);
    assert!(
        arena.allocate(NonZeroUsize::new(4096).unwrap()).is_ok(),
        "freezing spilled bytes should not retain any buddy allocation"
    );
}

#[test]
fn buddy_auto_spill_drop_path() {
    let arena = BuddyArena::builder(
        BuddyGeometry::exact(
            NonZeroUsize::new(4096).unwrap(),
            NonZeroUsize::new(512).unwrap(),
        )
        .unwrap(),
    )
    .auto_spill()
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(700).unwrap()).unwrap();
    buf.put_slice(&vec![b'a'; 1024]);
    buf.put_slice(&vec![b'b'; 2048]);
    assert!(buf.is_spilled());

    let whole = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();
    assert_eq!(whole.capacity(), 4096);
    drop(whole);

    assert!(
        arena.allocate(NonZeroUsize::new(4096).unwrap()).is_ok(),
        "spilling should release the buddy block before the spilled buffer drops"
    );

    drop(buf);
}
