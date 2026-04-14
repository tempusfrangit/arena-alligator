use std::alloc::Layout;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arena_alligator::{
    BuddyArena, BuddyHint, BuildError, Dealloc, FixedArena, HeapDealloc, InitPolicy, NoDealloc,
    SlotSpec,
};
use bytes::BufMut;

/// Allocate a block on the heap and return (ptr, len, layout).
fn heap_block(size: usize, align: usize) -> (*mut u8, usize, Layout) {
    let layout = Layout::from_size_align(size, align).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    (ptr, size, layout)
}

// ── FixedArena::from_raw ─────────────────────────────────────────────

#[test]
fn fixed_from_raw_count_success() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Count(NonZeroUsize::new(4).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    assert_eq!(arena.slot_count(), 4);
    assert_eq!(arena.slot_capacity(), 1024);

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"hello from raw");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"hello from raw");
}

#[test]
fn fixed_from_raw_size_success() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Size(NonZeroUsize::new(512).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    assert_eq!(arena.slot_count(), 8);
    assert_eq!(arena.slot_capacity(), 512);
}

#[test]
fn fixed_from_raw_size_truncates_tail() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Size(NonZeroUsize::new(1000).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    assert_eq!(arena.slot_count(), 4);
    assert_eq!(arena.slot_capacity(), 1000);
}

#[test]
fn fixed_from_raw_null_pointer() {
    let result = unsafe {
        FixedArena::from_raw(
            std::ptr::null_mut(),
            4096,
            SlotSpec::Count(NonZeroUsize::new(4).unwrap()),
            NoDealloc,
        )
    }
    .build();

    assert_eq!(result.unwrap_err(), BuildError::NullPointer);
}

#[test]
fn fixed_from_raw_slot_exceeds_block() {
    let (ptr, len, layout) = heap_block(256, 8);
    let result = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Size(NonZeroUsize::new(512).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build();

    assert_eq!(result.unwrap_err(), BuildError::SlotSizeExceedsBacking);
    // Build failed -- caller retains ownership per the API contract.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

#[test]
fn fixed_from_raw_alignment_pads_down() {
    let (ptr, len, layout) = heap_block(4096, 64);
    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Count(NonZeroUsize::new(4).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .alignment(64)
    .build()
    .unwrap();

    assert_eq!(arena.slot_capacity() % 64, 0);
    assert_eq!(arena.slot_capacity(), 1024);
}

#[test]
fn fixed_from_raw_no_dealloc() {
    let block = vec![0u8; 4096].into_boxed_slice();
    let ptr = Box::into_raw(block) as *mut u8;

    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            4096,
            SlotSpec::Count(NonZeroUsize::new(2).unwrap()),
            NoDealloc,
        )
    }
    .build()
    .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"no dealloc test");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"no dealloc test");

    drop(bytes);
    drop(arena);

    // Clean up manually since NoDealloc won't free it.
    unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, 4096))) };
}

#[test]
fn fixed_from_raw_full_lifecycle() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Count(NonZeroUsize::new(1).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"first");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"first");

    assert!(arena.allocate().is_err(), "slot held by frozen Bytes");
    drop(bytes);

    let mut buf2 = arena.allocate().expect("slot freed after drop");
    buf2.put_slice(b"second");
    let bytes2 = buf2.freeze();
    assert_eq!(&bytes2[..], b"second");
}

#[test]
fn fixed_from_raw_dealloc_called_on_drop() {
    let called = Arc::new(AtomicBool::new(false));

    struct TrackingDealloc {
        called: Arc<AtomicBool>,
        layout: Layout,
    }

    // SAFETY: frees via std::alloc::dealloc with the stored layout.
    unsafe impl Dealloc for TrackingDealloc {
        unsafe fn dealloc(self, ptr: *mut u8, _len: usize) {
            self.called.store(true, Ordering::SeqCst);
            unsafe { std::alloc::dealloc(ptr, self.layout) };
        }
    }

    let layout = Layout::from_size_align(4096, 8).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());

    let dealloc = TrackingDealloc {
        called: Arc::clone(&called),
        layout,
    };

    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            4096,
            SlotSpec::Count(NonZeroUsize::new(2).unwrap()),
            dealloc,
        )
    }
    .build()
    .unwrap();

    assert!(!called.load(Ordering::SeqCst));
    drop(arena);
    assert!(
        called.load(Ordering::SeqCst),
        "dealloc must be called on arena drop"
    );
}

#[test]
fn fixed_from_raw_dealloc_not_called_on_build_failure() {
    let called = Arc::new(AtomicBool::new(false));

    struct TrackingDealloc(Arc<AtomicBool>);

    // SAFETY: no-op, only used to track whether dealloc was called.
    unsafe impl Dealloc for TrackingDealloc {
        unsafe fn dealloc(self, _ptr: *mut u8, _len: usize) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let result = unsafe {
        FixedArena::from_raw(
            std::ptr::null_mut(),
            4096,
            SlotSpec::Count(NonZeroUsize::new(4).unwrap()),
            TrackingDealloc(Arc::clone(&called)),
        )
    }
    .build();

    assert!(result.is_err());
    assert!(
        !called.load(Ordering::SeqCst),
        "dealloc must NOT be called when build fails"
    );
}

#[test]
fn fixed_from_raw_zero_policy_lifecycle() {
    let (ptr, len, layout) = heap_block(4096, 8);
    unsafe { std::ptr::write_bytes(ptr, 0xAA, len) };

    let arena = unsafe {
        FixedArena::from_raw(
            ptr,
            len,
            SlotSpec::Count(NonZeroUsize::new(1).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .init_policy(InitPolicy::Zero)
    .build()
    .unwrap();

    // Zero policy: allocate, write, freeze, drop, re-allocate should work.
    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"dirty data here!");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"dirty data here!");
    drop(bytes);

    // Slot returned and scrubbed. Re-allocate succeeds.
    let mut buf2 = arena
        .allocate()
        .expect("slot freed after zero-scrub return");
    buf2.put_slice(b"clean");
    let bytes2 = buf2.freeze();
    assert_eq!(&bytes2[..], b"clean");
}

// ── BuddyArena::from_raw ─────────��─────────────────���────────────────

#[test]
fn buddy_from_raw_success() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            len,
            BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    assert_eq!(arena.total_size(), 4096);
    assert_eq!(arena.min_block_size(), 512);

    let mut buf = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    buf.put_slice(b"buddy raw");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"buddy raw");
}

#[test]
fn buddy_from_raw_non_power_of_two_region() {
    let (ptr, len, layout) = heap_block(5000, 8);
    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            len,
            BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    assert_eq!(arena.total_size(), 4096);
    assert_eq!(arena.min_block_size(), 512);
}

#[test]
fn buddy_from_raw_dealloc_gets_original_len() {
    let called_with_len = Arc::new(std::sync::Mutex::new(0usize));

    struct LenTracker {
        len_cell: Arc<std::sync::Mutex<usize>>,
        layout: Layout,
    }

    // SAFETY: frees via std::alloc::dealloc, records len.
    unsafe impl Dealloc for LenTracker {
        unsafe fn dealloc(self, ptr: *mut u8, len: usize) {
            *self.len_cell.lock().unwrap() = len;
            unsafe { std::alloc::dealloc(ptr, self.layout) };
        }
    }

    let layout = Layout::from_size_align(5000, 8).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());

    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            5000,
            BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
            LenTracker {
                len_cell: Arc::clone(&called_with_len),
                layout,
            },
        )
    }
    .build()
    .unwrap();

    drop(arena);
    assert_eq!(
        *called_with_len.lock().unwrap(),
        5000,
        "dealloc must receive original len, not usable size"
    );
}

#[test]
fn buddy_from_raw_null_pointer() {
    let result = unsafe {
        BuddyArena::from_raw(
            std::ptr::null_mut(),
            4096,
            BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
            NoDealloc,
        )
    }
    .build();

    assert_eq!(result.unwrap_err(), BuildError::NullPointer);
}

#[test]
fn buddy_from_raw_too_large_hint() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let result = unsafe {
        BuddyArena::from_raw(
            ptr,
            len,
            BuddyHint::min_alloc(NonZeroUsize::new(8192).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build();

    assert_eq!(result.unwrap_err(), BuildError::ZeroUsableSlots);
    // Build failed -- caller retains ownership per the API contract.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

#[test]
fn buddy_from_raw_full_lifecycle() {
    let (ptr, len, layout) = heap_block(4096, 8);
    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            len,
            BuddyHint::min_alloc(NonZeroUsize::new(1024).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .build()
    .unwrap();

    let mut buf1 = arena.allocate(NonZeroUsize::new(1024).unwrap()).unwrap();
    buf1.put_slice(b"block-a");
    let bytes1 = buf1.freeze();

    let mut buf2 = arena.allocate(NonZeroUsize::new(1024).unwrap()).unwrap();
    buf2.put_slice(b"block-b");
    let bytes2 = buf2.freeze();

    assert_eq!(&bytes1[..], b"block-a");
    assert_eq!(&bytes2[..], b"block-b");

    drop(bytes1);
    drop(bytes2);

    // Re-allocate a coalesced block after returning both.
    let mut buf3 = arena.allocate(NonZeroUsize::new(2048).unwrap()).unwrap();
    buf3.put_slice(b"coalesced");
    let bytes3 = buf3.freeze();
    assert_eq!(&bytes3[..], b"coalesced");
}

#[test]
fn buddy_from_raw_zero_policy_lifecycle() {
    let (ptr, len, layout) = heap_block(4096, 8);
    unsafe { std::ptr::write_bytes(ptr, 0xBB, len) };

    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            len,
            BuddyHint::min_alloc(NonZeroUsize::new(4096).unwrap()),
            HeapDealloc::new(layout),
        )
    }
    .init_policy(InitPolicy::Zero)
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(4096).unwrap()).unwrap();
    buf.put_slice(b"after zero");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"after zero");
    drop(bytes);

    // Re-allocate after return-scrub.
    let mut buf2 = arena
        .allocate(NonZeroUsize::new(4096).unwrap())
        .expect("block freed after zero-scrub return");
    buf2.put_slice(b"clean");
    let bytes2 = buf2.freeze();
    assert_eq!(&bytes2[..], b"clean");
}

#[test]
fn buddy_from_raw_no_dealloc() {
    let block = vec![0u8; 4096].into_boxed_slice();
    let ptr = Box::into_raw(block) as *mut u8;

    let arena = unsafe {
        BuddyArena::from_raw(
            ptr,
            4096,
            BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
            NoDealloc,
        )
    }
    .build()
    .unwrap();

    let mut buf = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    buf.put_slice(b"no dealloc buddy");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"no dealloc buddy");

    drop(bytes);
    drop(arena);

    unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, 4096))) };
}

// ── from_static ──────────────────────────────────────────────────────

#[test]
fn fixed_from_static() {
    static mut BLOCK: [u8; 4096] = [0u8; 4096];

    #[allow(static_mut_refs)]
    let arena = FixedArena::from_static(
        unsafe { &mut BLOCK },
        SlotSpec::Count(NonZeroUsize::new(4).unwrap()),
    )
    .build()
    .unwrap();

    assert_eq!(arena.slot_count(), 4);
    assert_eq!(arena.slot_capacity(), 1024);

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"static fixed");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"static fixed");
}

#[test]
fn buddy_from_static() {
    static mut BLOCK: [u8; 4096] = [0u8; 4096];

    #[allow(static_mut_refs)]
    let arena = BuddyArena::from_static(
        unsafe { &mut BLOCK },
        BuddyHint::min_alloc(NonZeroUsize::new(512).unwrap()),
    )
    .build()
    .unwrap();

    assert_eq!(arena.total_size(), 4096);

    let mut buf = arena.allocate(NonZeroUsize::new(512).unwrap()).unwrap();
    buf.put_slice(b"static buddy");
    let bytes = buf.freeze();
    assert_eq!(&bytes[..], b"static buddy");
}
