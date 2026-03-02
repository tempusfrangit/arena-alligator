#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Arena allocator producing `bytes::Bytes` via `from_owner`.

mod allocation;
mod arena;
mod bitmap;
mod buddy;
mod buffer;
mod error;
mod handle;

#[cfg(feature = "async-alloc")]
mod async_alloc;

pub use arena::{FixedArena, FixedArenaBuilder};
pub use buddy::{BuddyArena, BuddyArenaBuilder};
pub use buffer::Buffer;
pub use error::{AllocError, BufferFullError, BuildError};

#[cfg(feature = "async-alloc")]
pub use async_alloc::{
    AsyncBuddyArena, AsyncFixedArena, AsyncPolicy, NotifyWaiters, TreiberWaiters, WaitRegistration,
    Waiter,
};
