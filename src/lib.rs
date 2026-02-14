#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Arena allocator producing `bytes::Bytes` via `from_owner`.

mod arena;
mod bitmap;
mod buffer;
mod error;

pub use arena::{FixedArena, FixedArenaBuilder};
pub use buffer::Buffer;
pub use error::{AllocError, BufferFullError, BuildError};
