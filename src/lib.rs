#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Arena allocator producing `bytes::Bytes` via `from_owner`.

mod error;

pub use error::{AllocError, BuildError, BufferFullError};
