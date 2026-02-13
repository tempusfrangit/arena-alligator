#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Arena allocator producing `bytes::Bytes` via `from_owner`.

#[allow(dead_code)]
mod bitmap;
mod error;

pub use error::{AllocError, BufferFullError, BuildError};
