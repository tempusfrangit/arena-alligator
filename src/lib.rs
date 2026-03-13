#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Lock-free arena allocator producing [`bytes::Bytes`] via zero-copy freeze.
//!
//! Write into a [`Buffer`], call [`freeze()`](Buffer::freeze), and get back
//! `Bytes` backed by arena memory. The slot or block returns to the arena
//! when the last `Bytes` reference drops.
//!
//! # Allocator modes
//!
//! - [`FixedArena`]: uniform slot sizes and a simple bitmap claim path.
//! - [`BuddyArena`]: power-of-two blocks for variable-size requests.
//!
//! Both produce the same [`Buffer`] type with identical write and freeze
//! semantics.
//!
//! # Quick start
//!
//! ```
//! use std::num::NonZeroUsize;
//! use arena_alligator::FixedArena;
//! use bytes::BufMut;
//!
//! let arena = FixedArena::with_slot_capacity(
//!     NonZeroUsize::new(1024).unwrap(),
//!     NonZeroUsize::new(4096).unwrap(),
//! )
//!     .build()
//!     .unwrap();
//!
//! let mut buf = arena.allocate().unwrap();
//! buf.put_slice(b"hello");
//! let bytes = buf.freeze();
//! assert_eq!(&bytes[..], b"hello");
//! ```
//!
//! # Auto-spill
//!
//! Enable [`.auto_spill()`](FixedArenaBuilder::auto_spill) on the builder if
//! you want overflow writes to spill onto the heap instead of panicking. The
//! arena allocation is released as soon as the spill happens.
//!
//! # Frozen slice retention
//!
//! Freezing a buffer transfers ownership of the arena slot (or buddy block)
//! to the returned `Bytes`. Cloning or slicing that `Bytes` shares the
//! reference, so the arena memory stays pinned until every clone and slice is
//! dropped.
//!
//! If you need mutable owned storage after `freeze()`, use
//! [`BytesExt::into_owned()`]. It copies into a fresh `BytesMut`.
//!
//! # Async allocation
//!
//! With the `async-alloc` feature, [`AsyncFixedArena`] and [`AsyncBuddyArena`]
//! provide `allocate_async()` which waits until capacity is available.

mod allocation;
mod arena;
mod bitmap;
mod buddy;
mod buffer;
mod error;
mod ext;
mod geometry;
mod handle;
mod metrics;
mod sync;

#[cfg(feature = "async-alloc")]
mod async_alloc;

pub use arena::{FixedArena, FixedArenaBuilder, InitPolicy, PageSize, Unfaulted};
pub use buddy::{BuddyArena, BuddyArenaBuilder};
pub use buffer::Buffer;
pub use error::{AllocError, BufferFullError, BuildError};
pub use ext::BytesExt;
pub use geometry::BuddyGeometry;
pub use metrics::{BuddyArenaMetrics, FixedArenaMetrics};

#[cfg(feature = "async-alloc")]
pub use async_alloc::{
    AsyncBuddyArena, AsyncFixedArena, BuddyWaiter, NotifyWaiters, WaitRegistration, Waiter,
};
