use core::num::NonZeroUsize;

use crate::error::BuildError;

/// Validated buddy arena geometry.
///
/// Constructed via [`exact()`](Self::exact) for strict validation or
/// [`nearest()`](Self::nearest) for automatic adjustment. Once built,
/// the geometry is guaranteed valid for arena construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyGeometry {
    total_size: usize,
    min_block_size: usize,
    max_order: usize,
    alignment: usize,
    cap_capacity: bool,
}

impl BuddyGeometry {
    /// Validate exact buddy geometry.
    ///
    /// Returns `Err` if:
    /// - `min_block_size` is not a power of two
    /// - `total_size` is not a power-of-two multiple of `min_block_size`
    /// - `total_size < min_block_size`
    ///
    /// ```
    /// use core::num::NonZeroUsize;
    /// use arena_alligator::BuddyGeometry;
    ///
    /// let geo = BuddyGeometry::exact(
    ///     NonZeroUsize::new(4096).unwrap(),
    ///     NonZeroUsize::new(512).unwrap(),
    /// ).unwrap();
    /// assert_eq!(geo.total_size(), 4096);
    /// assert_eq!(geo.min_block_size(), 512);
    /// assert_eq!(geo.max_order(), 3); // 4096 / 512 = 8 = 2^3
    /// ```
    pub fn exact(
        total_size: NonZeroUsize,
        min_block_size: NonZeroUsize,
    ) -> Result<Self, BuildError> {
        let total = total_size.get();
        let min_block = min_block_size.get();

        let max_order =
            validate_buddy_geometry(total, min_block).ok_or(BuildError::InvalidGeometry)?;

        Ok(Self {
            total_size: total,
            min_block_size: min_block,
            max_order,
            alignment: 1,
            cap_capacity: false,
        })
    }

    /// Build valid buddy geometry by snapping inputs to nearest valid values.
    ///
    /// - `min_block_size` rounds up to next power of two
    /// - `total_size` rounds up to next power-of-two multiple of min_block_size
    ///
    /// Returns `Err(BuildError::SizeOverflow)` if snapped values overflow `usize`.
    ///
    /// Buffers allocated from arenas using `nearest` geometry report the
    /// requested capacity, not the full block size.
    ///
    /// ```
    /// use core::num::NonZeroUsize;
    /// use arena_alligator::BuddyGeometry;
    ///
    /// // 6000 snaps up to 8192, 768 snaps up to 1024
    /// let geo = BuddyGeometry::nearest(
    ///     NonZeroUsize::new(6000).unwrap(),
    ///     NonZeroUsize::new(768).unwrap(),
    /// ).unwrap();
    /// assert_eq!(geo.total_size(), 8192);
    /// assert_eq!(geo.min_block_size(), 1024);
    /// ```
    pub fn nearest(
        total_size: NonZeroUsize,
        min_block_size: NonZeroUsize,
    ) -> Result<Self, BuildError> {
        let min_block = min_block_size
            .get()
            .checked_next_power_of_two()
            .ok_or(BuildError::SizeOverflow)?;

        let (total, max_order) = snap_to_valid_geometry(total_size.get(), min_block)?;

        Ok(Self {
            total_size: total,
            min_block_size: min_block,
            max_order,
            alignment: 1,
            cap_capacity: true,
        })
    }

    /// Set alignment. Must be a power of two.
    ///
    /// For `exact` geometry, alignment must not exceed `min_block_size`
    /// (returns `Err(InvalidGeometry)` otherwise). For `nearest` geometry,
    /// `min_block_size` snaps up to the alignment and the total size is
    /// recalculated.
    pub fn with_alignment(self, alignment: NonZeroUsize) -> Result<Self, BuildError> {
        let a = alignment.get();
        if !a.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }
        if a <= self.min_block_size {
            return Ok(Self {
                alignment: a,
                ..self
            });
        }
        // alignment > min_block_size
        if !self.cap_capacity {
            return Err(BuildError::InvalidGeometry);
        }
        let min_block = a;
        let (total, max_order) = snap_to_valid_geometry(self.total_size, min_block)?;

        Ok(Self {
            total_size: total,
            min_block_size: min_block,
            max_order,
            alignment: a,
            cap_capacity: self.cap_capacity,
        })
    }

    /// Total bytes managed by this geometry.
    pub fn total_size(&self) -> usize {
        self.total_size
    }

    /// Smallest allocatable block size.
    pub fn min_block_size(&self) -> usize {
        self.min_block_size
    }

    /// Largest block order.
    pub fn max_order(&self) -> usize {
        self.max_order
    }

    /// Alignment for the backing allocation.
    pub fn alignment(&self) -> usize {
        self.alignment
    }

    /// Whether buffer capacity is capped to the requested size.
    pub(crate) fn cap_capacity(&self) -> bool {
        self.cap_capacity
    }
}

/// Snap `total_hint` up to the nearest valid buddy geometry for `min_block`.
///
/// `min_block` must already be a power of two.
/// Returns `(total_size, max_order)`.
fn snap_to_valid_geometry(
    total_hint: usize,
    min_block: usize,
) -> Result<(usize, usize), BuildError> {
    let t = total_hint.max(min_block);
    let blocks = t
        .checked_add(min_block - 1)
        .ok_or(BuildError::SizeOverflow)?
        / min_block;
    let pow2_blocks = blocks
        .checked_next_power_of_two()
        .ok_or(BuildError::SizeOverflow)?;
    let total = pow2_blocks
        .checked_mul(min_block)
        .ok_or(BuildError::SizeOverflow)?;
    let max_order = pow2_blocks.trailing_zeros() as usize;
    Ok((total, max_order))
}

fn validate_buddy_geometry(total_size: usize, min_block_size: usize) -> Option<usize> {
    if !min_block_size.is_power_of_two() {
        return None;
    }
    if total_size < min_block_size {
        return None;
    }
    if !total_size.is_multiple_of(min_block_size) {
        return None;
    }
    let blocks = total_size / min_block_size;
    if !blocks.is_power_of_two() {
        return None;
    }
    let max_order = blocks.trailing_zeros() as usize;
    if max_order >= usize::BITS as usize {
        return None;
    }
    Some(max_order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::num::NonZeroUsize;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn exact_valid_geometry() {
        let geo = BuddyGeometry::exact(nz(4096), nz(512)).unwrap();
        assert_eq!(geo.total_size(), 4096);
        assert_eq!(geo.min_block_size(), 512);
        assert_eq!(geo.max_order(), 3);
        assert_eq!(geo.alignment(), 1);
    }

    #[test]
    fn exact_rejects_non_pow2_min_block() {
        assert_eq!(
            BuddyGeometry::exact(nz(4096), nz(768)).unwrap_err(),
            BuildError::InvalidGeometry,
        );
    }

    #[test]
    fn exact_rejects_non_pow2_multiple_total() {
        assert_eq!(
            BuddyGeometry::exact(nz(6144), nz(1024)).unwrap_err(),
            BuildError::InvalidGeometry,
        );
    }

    #[test]
    fn exact_rejects_total_smaller_than_min_block() {
        assert_eq!(
            BuddyGeometry::exact(nz(256), nz(512)).unwrap_err(),
            BuildError::InvalidGeometry,
        );
    }

    #[test]
    fn exact_with_alignment() {
        let geo = BuddyGeometry::exact(nz(4096), nz(512))
            .unwrap()
            .with_alignment(nz(512))
            .unwrap();
        assert_eq!(geo.alignment(), 512);
    }

    #[test]
    fn exact_rejects_alignment_larger_than_min_block() {
        let err = BuddyGeometry::exact(nz(4096), nz(512))
            .unwrap()
            .with_alignment(nz(1024))
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidGeometry);
    }

    #[test]
    fn exact_rejects_non_pow2_alignment() {
        let err = BuddyGeometry::exact(nz(4096), nz(512))
            .unwrap()
            .with_alignment(nz(3))
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn nearest_valid_inputs_unchanged() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(512)).unwrap();
        assert_eq!(geo.total_size(), 4096);
        assert_eq!(geo.min_block_size(), 512);
        assert_eq!(geo.max_order(), 3);
    }

    #[test]
    fn nearest_snaps_min_block_to_pow2() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(768)).unwrap();
        assert_eq!(geo.min_block_size(), 1024);
    }

    #[test]
    fn nearest_snaps_total_up_to_valid_multiple() {
        let geo = BuddyGeometry::nearest(nz(6000), nz(1024)).unwrap();
        assert_eq!(geo.total_size(), 8192);
        assert_eq!(geo.min_block_size(), 1024);
    }

    #[test]
    fn nearest_snaps_total_up_when_less_than_min_block() {
        let geo = BuddyGeometry::nearest(nz(256), nz(512)).unwrap();
        assert_eq!(geo.total_size(), 512);
        assert_eq!(geo.min_block_size(), 512);
    }

    #[test]
    fn nearest_with_alignment_adjusts() {
        let geo = BuddyGeometry::nearest(nz(4096), nz(512))
            .unwrap()
            .with_alignment(nz(1024))
            .unwrap();
        assert_eq!(geo.alignment(), 1024);
        assert_eq!(geo.min_block_size(), 1024);
        assert_eq!(geo.max_order(), 2);
    }

    #[test]
    fn nearest_overflow_returns_err() {
        let result = BuddyGeometry::nearest(nz(usize::MAX), nz(usize::MAX >> 1));
        assert_eq!(result.unwrap_err(), BuildError::SizeOverflow);
    }
}
