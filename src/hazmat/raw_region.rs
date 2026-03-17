use std::fmt;
use std::ops::Range;

use crate::BuddyArena;
use crate::FixedArena;

/// Hazmat raw-access wrapper around [`FixedArena`].
///
/// Derefs to [`FixedArena`] for normal allocation and adds raw allocation.
#[derive(Clone)]
pub struct RawFixedArena(pub(crate) FixedArena);

impl std::ops::Deref for RawFixedArena {
    type Target = FixedArena;
    fn deref(&self) -> &FixedArena {
        &self.0
    }
}

impl fmt::Debug for RawFixedArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RawFixedArena").field(&self.0).finish()
    }
}

/// Hazmat raw-access wrapper around [`BuddyArena`].
///
/// Derefs to [`BuddyArena`] for normal allocation and adds raw allocation.
#[derive(Clone)]
pub struct RawBuddyArena(pub(crate) BuddyArena);

impl std::ops::Deref for RawBuddyArena {
    type Target = BuddyArena;
    fn deref(&self) -> &BuddyArena {
        &self.0
    }
}

impl fmt::Debug for RawBuddyArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RawBuddyArena").field(&self.0).finish()
    }
}

/// Error returned when a freeze range is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFreezeError {
    range_start: usize,
    range_end: usize,
    capacity: usize,
}

impl RawFreezeError {
    /// Range passed to `freeze`.
    pub fn range(&self) -> Range<usize> {
        self.range_start..self.range_end
    }

    /// Visible capacity of the raw region.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl fmt::Display for RawFreezeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.range_start > self.range_end {
            write!(
                f,
                "invalid freeze range: {}..{} (start > end)",
                self.range_start, self.range_end,
            )
        } else {
            write!(
                f,
                "freeze range {}..{} exceeds capacity {}",
                self.range_start, self.range_end, self.capacity,
            )
        }
    }
}

impl std::error::Error for RawFreezeError {}

/// Placeholder for the arena-backed raw region type.
#[derive(Debug)]
pub struct RawRegion {
    _priv: (),
}
