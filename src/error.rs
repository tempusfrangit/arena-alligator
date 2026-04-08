use core::fmt;

/// Allocation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocError {
    /// All slots are in use.
    ArenaFull,
}

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocError::ArenaFull => write!(f, "arena is full"),
        }
    }
}

impl core::error::Error for AllocError {}

/// Builder configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// `slot_count * aligned_capacity` overflows `usize`.
    SizeOverflow,
    /// Alignment is not a power of 2.
    InvalidAlignment,
    /// Buddy arena geometry is invalid.
    InvalidGeometry,
    /// Requested slot size exceeds backing memory length.
    SlotSizeExceedsBacking,
    /// No usable slots or blocks fit in the provided backing memory.
    ZeroUsableSlots,
    /// Null pointer provided to `from_raw`.
    NullPointer,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::SizeOverflow => write!(f, "total arena size overflows usize"),
            BuildError::InvalidAlignment => write!(f, "alignment must be a power of 2"),
            BuildError::InvalidGeometry => {
                write!(
                    f,
                    "buddy arena geometry must be a power-of-two multiple of min block size"
                )
            }
            BuildError::SlotSizeExceedsBacking => {
                write!(f, "requested slot size exceeds backing memory length")
            }
            BuildError::ZeroUsableSlots => {
                write!(f, "no usable slots fit in the provided backing memory")
            }
            BuildError::NullPointer => write!(f, "null pointer provided to from_raw"),
        }
    }
}

impl core::error::Error for BuildError {}

/// Buffer capacity exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferFullError {
    /// Bytes remaining in the buffer.
    pub remaining: usize,
    /// Bytes that were requested.
    pub requested: usize,
}

impl fmt::Display for BufferFullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "buffer full: {} bytes requested, {} bytes remaining",
            self.requested, self.remaining,
        )
    }
}

impl core::error::Error for BufferFullError {}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn alloc_error_display() {
        let err = AllocError::ArenaFull;
        assert_eq!(err.to_string(), "arena is full");
    }

    #[test]
    fn alloc_error_is_std_error() {
        let err: Box<dyn core::error::Error> = Box::new(AllocError::ArenaFull);
        assert_eq!(err.to_string(), "arena is full");
    }

    #[test]
    fn build_error_display_variants() {
        assert_eq!(
            BuildError::SizeOverflow.to_string(),
            "total arena size overflows usize"
        );
        assert_eq!(
            BuildError::InvalidAlignment.to_string(),
            "alignment must be a power of 2"
        );
        assert_eq!(
            BuildError::InvalidGeometry.to_string(),
            "buddy arena geometry must be a power-of-two multiple of min block size"
        );
    }

    #[test]
    fn build_error_is_std_error() {
        let err: Box<dyn core::error::Error> = Box::new(BuildError::SizeOverflow);
        assert!(err.to_string().contains("overflows"));
    }

    #[test]
    fn buffer_full_error_display() {
        let err = BufferFullError {
            remaining: 10,
            requested: 50,
        };
        assert_eq!(
            err.to_string(),
            "buffer full: 50 bytes requested, 10 bytes remaining"
        );
    }
}
