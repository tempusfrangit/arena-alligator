use std::fmt;

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

impl std::error::Error for AllocError {}

/// Builder configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// `slot_count * aligned_capacity` overflows `usize`.
    SizeOverflow,
    /// Alignment is not a power of 2.
    InvalidAlignment,
    /// Zero slots requested.
    ZeroSlots,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::SizeOverflow => write!(f, "total arena size overflows usize"),
            BuildError::InvalidAlignment => write!(f, "alignment must be a power of 2"),
            BuildError::ZeroSlots => write!(f, "slot count must be greater than zero"),
        }
    }
}

impl std::error::Error for BuildError {}

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

impl std::error::Error for BufferFullError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_error_display() {
        let err = AllocError::ArenaFull;
        assert_eq!(err.to_string(), "arena is full");
    }

    #[test]
    fn alloc_error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(AllocError::ArenaFull);
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
            BuildError::ZeroSlots.to_string(),
            "slot count must be greater than zero"
        );
    }

    #[test]
    fn build_error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(BuildError::ZeroSlots);
        assert!(err.to_string().contains("zero"));
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
