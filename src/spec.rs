use core::num::NonZeroUsize;

use crate::error::BuildError;

/// How to slice user-provided memory into fixed-size slots.
#[cfg_attr(not(test), allow(dead_code))]
pub enum SlotSpec {
    /// Slice into exactly this many equal-sized slots.
    /// Slot size is derived from `block_len / count`, aligned down.
    Count(NonZeroUsize),
    /// Slice into slots of this size.
    /// Slot count is derived from `block_len / size`.
    Size(NonZeroUsize),
}

impl SlotSpec {
    /// Resolve into (slot_count, slot_capacity) given the backing block
    /// length and alignment.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resolve(
        &self,
        block_len: usize,
        alignment: usize,
    ) -> Result<(usize, usize), BuildError> {
        match *self {
            SlotSpec::Count(count) => {
                let raw_size = block_len / count.get();
                let aligned_size = align_down(raw_size, alignment);
                if aligned_size == 0 {
                    return Err(BuildError::ZeroUsableSlots);
                }
                Ok((count.get(), aligned_size))
            }
            SlotSpec::Size(size) => {
                let aligned_size = align_down(size.get(), alignment);
                if aligned_size == 0 {
                    return Err(BuildError::ZeroUsableSlots);
                }
                if aligned_size > block_len {
                    return Err(BuildError::SlotSizeExceedsBacking);
                }
                let count = block_len / aligned_size;
                if count == 0 {
                    return Err(BuildError::ZeroUsableSlots);
                }
                Ok((count, aligned_size))
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn align_down(val: usize, align: usize) -> usize {
    val & !(align - 1)
}

/// Hint for deriving buddy geometry from user-provided memory.
#[cfg_attr(not(test), allow(dead_code))]
pub enum BuddyHint {
    /// Minimum allocation size. Max order derived from block length.
    MinAlloc(NonZeroUsize),
}

impl BuddyHint {
    /// Create a hint specifying the minimum allocation size.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn min_alloc(size: NonZeroUsize) -> Self {
        Self::MinAlloc(size)
    }

    /// Derive (min_block_size, max_order) from the hint and block length.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resolve(&self, block_len: usize) -> Result<(usize, usize), BuildError> {
        match *self {
            BuddyHint::MinAlloc(min) => {
                let min_block = min.get().next_power_of_two();
                let min_block = if min_block > block_len {
                    let down = prev_power_of_two(min.get());
                    if down == 0 || down > block_len {
                        return Err(BuildError::ZeroUsableSlots);
                    }
                    down
                } else {
                    min_block
                };

                let mut max_order = 0;
                while min_block << (max_order + 1) <= block_len {
                    max_order += 1;
                }

                let total_usable = min_block << max_order;
                if total_usable == 0 {
                    return Err(BuildError::ZeroUsableSlots);
                }

                Ok((min_block, max_order))
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn prev_power_of_two(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    1usize << (usize::BITS - 1 - n.leading_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn slot_spec_count_derives_size() {
        let spec = SlotSpec::Count(nz(8));
        let (count, size) = spec.resolve(1024, 1).unwrap();
        assert_eq!(count, 8);
        assert_eq!(size, 128);
    }

    #[test]
    fn slot_spec_size_derives_count() {
        let spec = SlotSpec::Size(nz(256));
        let (count, size) = spec.resolve(1024, 1).unwrap();
        assert_eq!(count, 4);
        assert_eq!(size, 256);
    }

    #[test]
    fn slot_spec_size_truncates_tail() {
        let spec = SlotSpec::Size(nz(300));
        let (count, size) = spec.resolve(1024, 1).unwrap();
        assert_eq!(count, 3);
        assert_eq!(size, 300);
    }

    #[test]
    fn slot_spec_size_exceeds_block() {
        let spec = SlotSpec::Size(nz(5000));
        let result = spec.resolve(4096, 1);
        assert_eq!(result, Err(BuildError::SlotSizeExceedsBacking));
    }

    #[test]
    fn slot_spec_count_with_alignment() {
        let spec = SlotSpec::Count(nz(4));
        let (count, size) = spec.resolve(4096, 64).unwrap();
        assert_eq!(count, 4);
        assert_eq!(size, 1024);
        assert_eq!(size % 64, 0);
    }

    #[test]
    fn slot_spec_zero_usable() {
        let spec = SlotSpec::Size(nz(100));
        let result = spec.resolve(50, 1);
        assert_eq!(result, Err(BuildError::SlotSizeExceedsBacking));
    }

    #[test]
    fn buddy_hint_basic() {
        let hint = BuddyHint::min_alloc(nz(512));
        let (min_block, max_order) = hint.resolve(4096).unwrap();
        assert_eq!(min_block, 512);
        assert_eq!(max_order, 3); // 512 * 2^3 = 4096
    }

    #[test]
    fn buddy_hint_snaps_up() {
        let hint = BuddyHint::min_alloc(nz(500));
        let (min_block, _) = hint.resolve(4096).unwrap();
        assert_eq!(min_block, 512); // snapped up to power of two
    }

    #[test]
    fn buddy_hint_too_large() {
        let hint = BuddyHint::min_alloc(nz(8192));
        let result = hint.resolve(4096);
        assert_eq!(result, Err(BuildError::ZeroUsableSlots));
    }

    #[test]
    fn buddy_hint_exact_fit() {
        let hint = BuddyHint::min_alloc(nz(4096));
        let (min_block, max_order) = hint.resolve(4096).unwrap();
        assert_eq!(min_block, 4096);
        assert_eq!(max_order, 0); // only one block
    }

    #[test]
    fn align_down_basic() {
        assert_eq!(align_down(1023, 64), 960);
        assert_eq!(align_down(1024, 64), 1024);
        assert_eq!(align_down(63, 64), 0);
    }

    #[test]
    fn prev_power_of_two_values() {
        assert_eq!(prev_power_of_two(0), 0);
        assert_eq!(prev_power_of_two(1), 1);
        assert_eq!(prev_power_of_two(500), 256);
        assert_eq!(prev_power_of_two(512), 512);
        assert_eq!(prev_power_of_two(1023), 512);
    }
}
