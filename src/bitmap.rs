use crate::sync::atomic::{AtomicUsize, Ordering};

type AtomicWord = AtomicUsize;
type Word = usize;

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
        Self::with_allocation_state(slot_count, true)
    }

    /// Create a new bitmap with `slot_count` slots, all initially allocated.
    pub(crate) fn new_empty(slot_count: usize) -> Self {
        Self::with_allocation_state(slot_count, false)
    }

    fn with_allocation_state(slot_count: usize, initially_free: bool) -> Self {
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
                let free_mask = if valid_bits == BITS_PER_WORD {
                    Word::MAX
                } else {
                    (1 as Word).wrapping_shl(valid_bits as u32) - 1
                };
                let initial = if initially_free { free_mask } else { 0 };
                words.push(CacheAligned(AtomicWord::new(initial)));
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

    pub(crate) fn try_claim_exact(&self, slot: usize) -> bool {
        debug_assert!(slot < self.slot_count, "slot index out of bounds");
        let word_idx = slot / BITS_PER_WORD;
        let mask = (1 as Word) << (slot % BITS_PER_WORD) as u32;
        let prev = self.words[word_idx].0.fetch_and(!mask, Ordering::AcqRel);
        prev & mask != 0
    }

    pub(crate) fn any_free(&self) -> bool {
        let actual_words = self.slot_count.div_ceil(BITS_PER_WORD);
        for word_idx in 0..actual_words {
            if self.words[word_idx].0.load(Ordering::Acquire) != 0 {
                return true;
            }
        }
        false
    }

    pub(crate) fn is_free(&self, slot: usize) -> bool {
        debug_assert!(slot < self.slot_count, "slot index out of bounds");
        let word_idx = slot / BITS_PER_WORD;
        let mask = (1 as Word) << (slot % BITS_PER_WORD) as u32;
        self.words[word_idx].0.load(Ordering::Acquire) & mask != 0
    }

    pub(crate) fn free_count(&self) -> usize {
        let mut total = 0usize;
        let actual_words = self.slot_count.div_ceil(BITS_PER_WORD);
        for word_idx in 0..actual_words {
            total += self.words[word_idx].0.load(Ordering::Acquire).count_ones() as usize;
        }
        total
    }

    /// Check if all bits in `start..end` are set (1 = set).
    ///
    /// Returns `true` for empty ranges (`start == end`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn all_set_in_range(&self, start: usize, end: usize) -> bool {
        debug_assert!(end <= self.slot_count);
        debug_assert!(start <= end);
        if start == end {
            return true;
        }

        let first_word = start / BITS_PER_WORD;
        let last_word = (end - 1) / BITS_PER_WORD;

        if first_word == last_word {
            let mask = range_mask(start % BITS_PER_WORD, end - first_word * BITS_PER_WORD);
            return self.words[first_word].0.load(Ordering::Acquire) & mask == mask;
        }

        // First partial word
        let first_bit = start % BITS_PER_WORD;
        let first_mask = !((1 as Word).wrapping_shl(first_bit as u32) - 1);
        if self.words[first_word].0.load(Ordering::Acquire) & first_mask != first_mask {
            return false;
        }

        // Full words in the middle
        for w in (first_word + 1)..last_word {
            if self.words[w].0.load(Ordering::Acquire) != Word::MAX {
                return false;
            }
        }

        // Last partial word
        let last_bit = end - last_word * BITS_PER_WORD;
        let last_mask = if last_bit >= BITS_PER_WORD {
            Word::MAX
        } else {
            (1 as Word).wrapping_shl(last_bit as u32) - 1
        };
        self.words[last_word].0.load(Ordering::Acquire) & last_mask == last_mask
    }

    /// Set all bits in `start..end`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_range(&self, start: usize, end: usize) {
        debug_assert!(end <= self.slot_count);
        debug_assert!(start <= end);
        if start == end {
            return;
        }

        let first_word = start / BITS_PER_WORD;
        let last_word = (end - 1) / BITS_PER_WORD;

        if first_word == last_word {
            let mask = range_mask(start % BITS_PER_WORD, end - first_word * BITS_PER_WORD);
            self.words[first_word].0.fetch_or(mask, Ordering::Release);
            return;
        }

        // First partial word
        let first_bit = start % BITS_PER_WORD;
        let first_mask = !((1 as Word).wrapping_shl(first_bit as u32) - 1);
        self.words[first_word]
            .0
            .fetch_or(first_mask, Ordering::Release);

        // Full words in the middle
        for w in (first_word + 1)..last_word {
            self.words[w].0.fetch_or(Word::MAX, Ordering::Release);
        }

        // Last partial word
        let last_bit = end - last_word * BITS_PER_WORD;
        let last_mask = if last_bit >= BITS_PER_WORD {
            Word::MAX
        } else {
            (1 as Word).wrapping_shl(last_bit as u32) - 1
        };
        self.words[last_word]
            .0
            .fetch_or(last_mask, Ordering::Release);
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

/// Build a mask for bits `start_bit..end_bit` within a single word.
#[cfg_attr(not(test), allow(dead_code))]
fn range_mask(start_bit: usize, end_bit: usize) -> Word {
    debug_assert!(start_bit <= end_bit);
    debug_assert!(end_bit <= BITS_PER_WORD);
    let full = if end_bit >= BITS_PER_WORD {
        Word::MAX
    } else {
        (1 as Word).wrapping_shl(end_bit as u32) - 1
    };
    full & !((1 as Word).wrapping_shl(start_bit as u32) - 1)
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
    fn empty_bitmap_starts_with_no_free_slots() {
        let bm = AtomicBitmap::new_empty(4);
        assert!(bm.try_alloc().is_none());
        assert_eq!(bm.free_count(), 0);
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

    #[test]
    fn all_set_in_range_empty_bitmap() {
        let bm = AtomicBitmap::new_empty(128);
        assert!(!bm.all_set_in_range(0, 4));
    }

    #[test]
    fn all_set_in_range_full_bitmap() {
        let bm = AtomicBitmap::new(128);
        assert!(bm.all_set_in_range(0, 4));
        assert!(bm.all_set_in_range(60, 68)); // spans word boundary
        assert!(bm.all_set_in_range(0, 128));
    }

    #[test]
    fn all_set_in_range_partial() {
        let bm = AtomicBitmap::new(128);
        bm.try_alloc(); // clears one bit
        assert!(!bm.all_set_in_range(0, 128));
    }

    #[test]
    fn set_range_then_check() {
        let bm = AtomicBitmap::new_empty(128);
        bm.set_range(4, 8);
        assert!(bm.all_set_in_range(4, 8));
        assert!(!bm.all_set_in_range(0, 4));
        assert!(!bm.all_set_in_range(0, 128));
    }

    #[test]
    fn set_range_spans_word_boundary() {
        let bm = AtomicBitmap::new_empty(128);
        bm.set_range(60, 68);
        assert!(bm.all_set_in_range(60, 68));
        assert!(!bm.all_set_in_range(59, 68));
        assert!(!bm.all_set_in_range(60, 69));
    }

    #[test]
    fn set_range_single_bit() {
        let bm = AtomicBitmap::new_empty(64);
        bm.set_range(7, 8);
        assert!(bm.is_free(7));
        assert!(!bm.is_free(6));
        assert!(!bm.is_free(8));
    }

    #[test]
    fn all_set_in_range_single_bit() {
        let bm = AtomicBitmap::new(64);
        assert!(bm.all_set_in_range(0, 1));
        bm.try_alloc();
        assert!(bm.all_set_in_range(32, 33));
    }

    #[test]
    fn all_set_in_range_empty_range() {
        let bm = AtomicBitmap::new_empty(64);
        assert!(bm.all_set_in_range(5, 5));
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use crate::sync::Arc;
    use crate::sync::atomic::{AtomicUsize, Ordering};
    use loom::thread;

    use super::AtomicBitmap;

    #[test]
    fn loom_alloc_free_race() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new(1));

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            let t1 = thread::spawn(move || {
                if let Some(slot) = a.try_alloc() {
                    a.free(slot);
                }
            });

            let t2 = thread::spawn(move || {
                if let Some(slot) = b.try_alloc() {
                    b.free(slot);
                }
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert_eq!(bitmap.free_count(), 1);
            assert!(bitmap.any_free());
        });
    }

    #[test]
    fn loom_single_slot_has_at_most_one_winner() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new(1));
            let winners = Arc::new(AtomicUsize::new(0));

            let b1 = Arc::clone(&bitmap);
            let w1 = Arc::clone(&winners);
            let t1 = thread::spawn(move || {
                if b1.try_alloc().is_some() {
                    w1.fetch_add(1, Ordering::Relaxed);
                }
            });

            let b2 = Arc::clone(&bitmap);
            let w2 = Arc::clone(&winners);
            let t2 = thread::spawn(move || {
                if b2.try_alloc().is_some() {
                    w2.fetch_add(1, Ordering::Relaxed);
                }
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert!(winners.load(Ordering::Relaxed) <= 1);
        });
    }

    #[test]
    fn loom_two_slot_allocation_counts_never_exceed_capacity() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(2);
        builder.check(|| {
            let bitmap = Arc::new(AtomicBitmap::new(2));
            let winners = Arc::new(AtomicUsize::new(0));

            let mut handles = Vec::new();
            for _ in 0..3 {
                let bm = Arc::clone(&bitmap);
                let w = Arc::clone(&winners);
                handles.push(thread::spawn(move || {
                    if bm.try_alloc().is_some() {
                        w.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }

            for handle in handles {
                handle.join().unwrap();
            }

            assert!(winners.load(Ordering::Relaxed) <= 2);
        });
    }

    /// try_claim_exact racing with try_alloc on the same slot.
    /// Models the buddy coalesce path where one thread reclaims a
    /// buddy block while another thread tries to allocate it.
    #[test]
    fn loom_try_claim_exact_vs_try_alloc() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new(2));

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            // Thread A: try_alloc (scans for any free bit)
            let t1 = thread::spawn(move || a.try_alloc());

            // Thread B: try_claim_exact on slot 0 (buddy coalesce path)
            let t2 = thread::spawn(move || b.try_claim_exact(0));

            let alloc_result = t1.join().unwrap();
            let claim_result = t2.join().unwrap();

            // At most one thread wins slot 0.
            // If claim won slot 0, alloc may get slot 1 or None.
            // If alloc won slot 0, claim fails and alloc got slot 0.
            match (alloc_result, claim_result) {
                (Some(0), true) => panic!("both won slot 0"),
                _ => {}
            }

            // Exactly 2 minus however many were taken should remain free.
            let taken = usize::from(alloc_result.is_some()) + usize::from(claim_result);
            assert_eq!(bitmap.free_count(), 2 - taken);
        });
    }

    /// try_claim_exact racing with free on the same slot.
    /// Models concurrent coalesce (claim buddy) while another thread
    /// returns the same block.
    #[test]
    fn loom_try_claim_exact_vs_free() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new(2));

            // Pre-allocate slot 0 so the free path can be exercised.
            let slot = bitmap.try_alloc().unwrap();
            assert_eq!(slot, 0);

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            // Thread A: free slot 0 (sets bit)
            let t1 = thread::spawn(move || a.free(0));

            // Thread B: try_claim_exact slot 0 (clears bit)
            let t2 = thread::spawn(move || b.try_claim_exact(0));

            t1.join().unwrap();
            let claimed = t2.join().unwrap();

            if claimed {
                // claim won: slot 0 was freed then immediately reclaimed,
                // so it should be allocated (bit clear).
                assert!(!bitmap.is_free(0));
            } else {
                // claim lost the race: free hadn't happened yet,
                // but free completed so slot 0 is now free.
                assert!(bitmap.is_free(0));
            }
        });
    }

    /// Two threads doing alloc-free cycles on a 2-slot bitmap.
    /// Verifies no slots are lost after concurrent recycling.
    #[test]
    fn loom_alloc_free_recycle() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new(2));

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            let t1 = thread::spawn(move || {
                if let Some(s) = a.try_alloc() {
                    a.free(s);
                }
            });

            let t2 = thread::spawn(move || {
                if let Some(s) = b.try_alloc() {
                    b.free(s);
                }
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert_eq!(bitmap.free_count(), 2);
        });
    }

    #[test]
    fn loom_set_range_vs_all_set_in_range() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new_empty(4));

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            let t1 = thread::spawn(move || {
                a.set_range(0, 2);
            });

            let t2 = thread::spawn(move || {
                b.set_range(2, 4);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert!(bitmap.all_set_in_range(0, 4));
        });
    }

    #[test]
    fn loom_set_range_concurrent_overlap() {
        loom::model(|| {
            let bitmap = Arc::new(AtomicBitmap::new_empty(2));

            let a = Arc::clone(&bitmap);
            let b = Arc::clone(&bitmap);

            let t1 = thread::spawn(move || {
                a.set_range(0, 2);
            });

            let t2 = thread::spawn(move || {
                b.set_range(0, 2);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert!(bitmap.all_set_in_range(0, 2));
        });
    }
}
