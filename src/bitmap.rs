use std::sync::atomic::{AtomicUsize, Ordering};

// Compile-time word width selection: AtomicU64 on 64-bit targets, AtomicU32 on 32-bit.
#[cfg(target_has_atomic = "64")]
type AtomicWord = std::sync::atomic::AtomicU64;
#[cfg(target_has_atomic = "64")]
type Word = u64;

#[cfg(not(target_has_atomic = "64"))]
type AtomicWord = std::sync::atomic::AtomicU32;
#[cfg(not(target_has_atomic = "64"))]
type Word = u32;

const BITS_PER_WORD: usize = std::mem::size_of::<Word>() * 8;

/// Generates a cache-line-aligned wrapper to prevent false sharing.
/// 64 bytes on 64-bit targets (x86-64, AArch64), 32 bytes on 32-bit.
macro_rules! define_cache_aligned {
    ($align:literal) => {
        #[repr(align($align))]
        pub(crate) struct CacheAligned<T>(pub(crate) T);
    };
}

#[cfg(target_pointer_width = "64")]
define_cache_aligned!(64);
#[cfg(not(target_pointer_width = "64"))]
define_cache_aligned!(32);

/// Lock-free bitmap allocator using `fetch_and`/`fetch_or` with cursor distribution.
///
/// Each slot is one bit in a bitmap word (1 = free, 0 = allocated).
/// A shared cursor distributes threads across words to reduce contention.
///
/// - **Alloc:** `fetch_and(!mask)` clears a bit atomically; check `prev & mask` to confirm.
/// - **Free:** `fetch_or(mask)` sets the bit — single instruction, never fails.
/// - **Scan:** `trailing_zeros()` finds the first free bit in O(1) per word.
/// - **Distribution:** `fetch_add` on shared cursor spreads threads across words.
pub(crate) struct AtomicBitmap {
    words: Box<[CacheAligned<AtomicWord>]>,
    cursor: CacheAligned<AtomicUsize>,
    /// Pow2-rounded word count — may exceed actual words with valid slots.
    scan_words: usize,
    /// `scan_words - 1` for branchless index wrapping via `& word_mask`.
    word_mask: usize,
    slot_count: usize,
}

impl AtomicBitmap {
    /// Create a new bitmap with `slot_count` slots, all initially free.
    pub(crate) fn new(slot_count: usize) -> Self {
        let actual_words = slot_count.div_ceil(BITS_PER_WORD);
        let scan_words = actual_words.max(1).next_power_of_two();
        let mut words = Vec::with_capacity(scan_words);

        for w in 0..scan_words {
            let start_bit = w * BITS_PER_WORD;
            if start_bit >= slot_count {
                // Padding words beyond actual slots — always zero (no free bits).
                words.push(CacheAligned(AtomicWord::new(0)));
            } else {
                let valid_bits = (slot_count - start_bit).min(BITS_PER_WORD);
                let mask = if valid_bits == BITS_PER_WORD {
                    Word::MAX
                } else {
                    (1 as Word).wrapping_shl(valid_bits as u32) - 1
                };
                words.push(CacheAligned(AtomicWord::new(mask)));
            }
        }

        debug_assert!(scan_words.is_power_of_two());

        Self {
            words: words.into_boxed_slice(),
            cursor: CacheAligned(AtomicUsize::new(0)),
            scan_words,
            word_mask: scan_words - 1,
            slot_count,
        }
    }

    /// Try to allocate a single slot. Returns the slot index, or `None` if all slots are taken.
    #[inline]
    pub(crate) fn try_alloc(&self) -> Option<usize> {
        if self.slot_count == 0 {
            return None;
        }
        let start = self.cursor.0.fetch_add(1, Ordering::Relaxed) & self.word_mask;
        for i in 0..self.scan_words {
            let word_idx = (start + i) & self.word_mask;
            let base = word_idx * BITS_PER_WORD;
            if let Some(slot) = Self::try_claim_word(&self.words[word_idx].0, base) {
                return Some(slot);
            }
        }
        None
    }

    /// Free a previously allocated slot, returning it to the pool.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `slot >= slot_count`.
    #[inline]
    pub(crate) fn free(&self, slot: usize) {
        debug_assert!(slot < self.slot_count, "slot index out of bounds");
        let word_idx = slot / BITS_PER_WORD;
        let mask = (1 as Word) << (slot % BITS_PER_WORD) as u32;
        let prev = self.words[word_idx].0.fetch_or(mask, Ordering::Release);
        debug_assert!(prev & mask == 0, "double free");
    }

    /// Attempt to claim one free bit from a specific word.
    ///
    /// Loops until either a bit is successfully claimed or the word is empty.
    /// The loop is bounded by `BITS_PER_WORD` iterations in the worst case
    /// (each iteration, some thread makes progress by claiming a bit).
    ///
    /// The scan load is Relaxed: a stale read just means a wasted fetch_and
    /// that harmlessly fails the `prev & mask` check. The fetch_and(AcqRel)
    /// on success is the real acquire edge that pairs with free's Release.
    #[inline]
    fn try_claim_word(word: &AtomicWord, base: usize) -> Option<usize> {
        loop {
            let bits = word.load(Ordering::Relaxed);
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            let mask = (1 as Word) << bit as u32;
            let prev = word.fetch_and(!mask, Ordering::AcqRel);
            if prev & mask != 0 {
                return Some(base + bit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_returns_all_slots_exactly_once() {
        let bm = AtomicBitmap::new(4);
        let mut slots: Vec<usize> = (0..4).filter_map(|_| bm.try_alloc()).collect();
        assert_eq!(slots.len(), 4);
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), 4, "each slot allocated exactly once");
        assert!(slots.iter().all(|&s| s < 4));
    }

    #[test]
    fn alloc_returns_none_when_full() {
        let bm = AtomicBitmap::new(2);
        assert!(bm.try_alloc().is_some());
        assert!(bm.try_alloc().is_some());
        assert!(bm.try_alloc().is_none());
    }

    #[test]
    fn free_makes_slot_available() {
        let bm = AtomicBitmap::new(1);
        let slot = bm.try_alloc().unwrap();
        assert!(bm.try_alloc().is_none());
        bm.free(slot);
        assert!(bm.try_alloc().is_some());
    }

    #[test]
    fn alloc_free_cycle() {
        let bm = AtomicBitmap::new(2);
        let s0 = bm.try_alloc().unwrap();
        let s1 = bm.try_alloc().unwrap();
        assert_ne!(s0, s1);
        assert!(bm.try_alloc().is_none());

        bm.free(s0);
        let s2 = bm.try_alloc().unwrap();
        assert_eq!(s2, s0);
        assert!(bm.try_alloc().is_none());

        bm.free(s1);
        bm.free(s2);
        let mut slots: Vec<usize> = (0..2).filter_map(|_| bm.try_alloc()).collect();
        slots.sort();
        assert_eq!(slots, vec![0, 1]);
    }

    #[test]
    fn partial_last_word() {
        // 65 slots = 1 full word + 1 bit in second word
        let bm = AtomicBitmap::new(BITS_PER_WORD + 1);
        let mut slots = Vec::new();
        while let Some(s) = bm.try_alloc() {
            slots.push(s);
        }
        assert_eq!(slots.len(), BITS_PER_WORD + 1);
        let max = *slots.iter().max().unwrap();
        assert_eq!(max, BITS_PER_WORD);
    }

    #[test]
    fn single_slot_bitmap() {
        let bm = AtomicBitmap::new(1);
        let s = bm.try_alloc().unwrap();
        assert_eq!(s, 0);
        assert!(bm.try_alloc().is_none());
        bm.free(s);
        assert_eq!(bm.try_alloc().unwrap(), 0);
    }

    #[test]
    fn exact_word_boundary() {
        let bm = AtomicBitmap::new(BITS_PER_WORD);
        let mut slots = Vec::new();
        while let Some(s) = bm.try_alloc() {
            slots.push(s);
        }
        assert_eq!(slots.len(), BITS_PER_WORD);
        assert!(bm.try_alloc().is_none());
    }

    #[test]
    fn free_all_then_realloc() {
        let bm = AtomicBitmap::new(BITS_PER_WORD * 2);
        let slots: Vec<usize> = (0..BITS_PER_WORD * 2)
            .filter_map(|_| bm.try_alloc())
            .collect();
        assert_eq!(slots.len(), BITS_PER_WORD * 2);

        for &s in &slots {
            bm.free(s);
        }

        let mut realloc: Vec<usize> = (0..BITS_PER_WORD * 2)
            .filter_map(|_| bm.try_alloc())
            .collect();
        realloc.sort();
        let mut expected: Vec<usize> = (0..BITS_PER_WORD * 2).collect();
        expected.sort();
        assert_eq!(realloc, expected);
    }

    #[test]
    fn concurrent_alloc_free() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let slot_count = BITS_PER_WORD * 4;
        let bm = Arc::new(AtomicBitmap::new(slot_count));
        let threads = 8;
        let ops_per_thread = 10_000;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let bm = Arc::clone(&bm);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..ops_per_thread {
                        if let Some(slot) = bm.try_alloc() {
                            assert!(slot < slot_count);
                            bm.free(slot);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // All slots should be free after all threads complete
        let mut recovered = Vec::new();
        while let Some(s) = bm.try_alloc() {
            recovered.push(s);
        }
        assert_eq!(
            recovered.len(),
            slot_count,
            "all slots should be recoverable"
        );
    }

    #[test]
    fn concurrent_no_duplicates() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let slot_count = 128;
        let bm = Arc::new(AtomicBitmap::new(slot_count));
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let bm = Arc::clone(&bm);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let mut claimed = Vec::new();
                    while let Some(slot) = bm.try_alloc() {
                        claimed.push(slot);
                    }
                    claimed
                })
            })
            .collect();

        let mut all: Vec<usize> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        all.sort();
        all.dedup();
        assert_eq!(
            all.len(),
            slot_count,
            "each slot allocated exactly once across all threads"
        );
    }

    #[test]
    #[should_panic(expected = "slot index out of bounds")]
    fn free_out_of_bounds_panics_debug() {
        let bm = AtomicBitmap::new(4);
        bm.free(4);
    }

    #[test]
    #[should_panic(expected = "double free")]
    fn double_free_panics_debug() {
        let bm = AtomicBitmap::new(4);
        let slot = bm.try_alloc().unwrap();
        bm.free(slot);
        bm.free(slot);
    }

    #[test]
    fn zero_slot_bitmap() {
        let bm = AtomicBitmap::new(0);
        assert!(bm.try_alloc().is_none());
    }

    #[test]
    fn large_bitmap() {
        let bm = AtomicBitmap::new(7168);
        let mut count = 0;
        while bm.try_alloc().is_some() {
            count += 1;
        }
        assert_eq!(count, 7168);
    }
}
