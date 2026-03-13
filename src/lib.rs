#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

//! Arena allocator for building [`bytes::Bytes`] without copying the final
//! payload.
//!
//! Write into a [`Buffer`], call [`freeze()`](Buffer::freeze), and get back
//! `Bytes` backed by arena memory. The slot or block returns to the arena
//! when the last `Bytes` reference drops.
//!
//! # Arena selection
//!
//! [`FixedArena`] is the recommended high-throughput path when one slot size
//! covers the workload. Fixed uses uniform slots and a bitmap claim path.
//!
//! [`BuddyArena`] covers variable-size allocation from one shared region.
//! Requests are rounded up to powers of two, larger blocks split on demand,
//! and neighbors coalesce on release.
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
//! [`.auto_spill()`](FixedArenaBuilder::auto_spill) changes overflow writes
//! from panic-on-capacity to heap spill. The arena allocation is released as
//! soon as the spill happens.
//!
//! # Initialization policy
//!
//! The default policy is [`InitPolicy::Uninit`], which matches Rust's common
//! writable-uninitialized-memory model: newly allocated capacity is not
//! zero-filled, and only the bytes written become visible in the
//! frozen [`Bytes`](bytes::Bytes).
//!
//! [`InitPolicy::Zero`] clears reused arena memory before it is handed back to
//! a writer. That adds work on every allocation in exchange for a stronger
//! zero-on-allocate guarantee.
//!
//! # Frozen slice retention
//!
//! Freezing a buffer transfers ownership of the arena slot (or buddy block)
//! to the returned `Bytes`. Cloning or slicing that `Bytes` shares the
//! reference, so the arena memory stays pinned until every clone and slice is
//! dropped.
//!
//! [`BytesExt::into_owned()`] copies frozen bytes into fresh owned mutable
//! storage.
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
