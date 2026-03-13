use std::num::NonZeroUsize;

use crate::error::BuildError;

/// Validated buddy arena geometry.
///
/// Once built, the geometry is guaranteed valid for arena construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyGeometry {
    total_size: usize,
    min_block_size: usize,
    max_order: usize,
    alignment: usize,
}

impl BuddyGeometry {
    /// Validate exact buddy geometry.
    ///
    /// Returns `Err` if:
    /// - `min_block_size` is not a power of two
    /// - `total_size` is not a power-of-two multiple of `min_block_size`
    /// - `total_size < min_block_size`
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
        })
    }

    /// Set alignment. Must be a power of two and no larger than
    /// `min_block_size`.
    pub fn with_alignment(self, alignment: NonZeroUsize) -> Result<Self, BuildError> {
        let a = alignment.get();
        if !a.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }
        if a > self.min_block_size {
            return Err(BuildError::InvalidGeometry);
        }
        Ok(Self {
            alignment: a,
            ..self
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
    use std::num::NonZeroUsize;

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
}
