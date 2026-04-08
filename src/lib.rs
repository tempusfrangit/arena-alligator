#![no_std]
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
//! use core::num::NonZeroUsize;
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
//! [`InitPolicy::Zero`] zeroes memory on return to the arena and on first
//! allocation. Returned slots and blocks are scrubbed before being marked
//! free, preventing data leaks between callers. First allocations (cold
//! memory, never returned) are zeroed on the alloc path. Memory that has
//! been through a return-scrub cycle is no longer cold and the alloc-path
//! zero is skipped. All zeroing uses the [`zeroize`] crate
//! (compiler-guaranteed not elided).
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
//! # Preallocated memory handoff
//!
//! [`FixedArena::from_raw()`] and [`BuddyArena::from_raw()`] let callers hand
//! pre-existing memory to the arena. This is for mmap'd regions, shared
//! memory, static buffers, and similar cases where the backing storage is
//! provisioned elsewhere.
//!
//! For `&'static mut` buffers, prefer the safe
//! [`FixedArena::from_static()`] and [`BuddyArena::from_static()`]
//! wrappers. Use `from_raw()` for pointer/length regions and custom
//! deallocation strategies.
//!
//! These constructors are `unsafe` because the arena cannot validate pointer
//! provenance, exclusivity, or deallocation correctness. On the safe side of
//! the boundary, the resulting arenas use the same allocation, freeze, and
//! retention rules as the ordinary builder paths.
//!
//! Use [`NoDealloc`] when the caller retains responsibility for freeing the
//! backing region. Use [`HeapDealloc`] when the region came from
//! [`alloc::alloc::alloc`].
//!
//! # Async allocation
//!
//! With the `async-alloc` feature, [`AsyncFixedArena`] and [`AsyncBuddyArena`]
//! provide `allocate_async()` which waits until capacity is available.
//!
//! # `no_std`
//!
//! This crate is `#![no_std]` by default and depends only on `alloc` and
//! `core`. It works on targets with a global allocator and pointer-width
//! atomics, including embedded systems.
//!
//! ## Feature flags
//!
//! | Feature | Default | What it enables |
//! | ------- | ------- | --------------- |
//! | `std` | yes | Standard-library integrations and dependency features; required by `async-alloc` |
//! | `libc` | yes | Page size detection via `sysconf` on Unix |
//! | `async-alloc` | no | [`AsyncFixedArena`] / [`AsyncBuddyArena`] via tokio (implies `std`) |
//! | `hazmat-raw-access` | no | Raw pointer access to arena memory |
//!
//! For `no_std` usage, disable default features:
//!
//! ```toml
//! [dependencies]
//! arena-alligator = { version = "0.6", default-features = false }
//! ```

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod allocation;
mod arena;
mod bitmap;
mod buddy;
mod buffer;
mod dealloc;
mod error;
mod ext;
mod geometry;
mod handle;
mod metrics;
mod spec;
mod sync;

#[cfg(feature = "async-alloc")]
mod async_alloc;

#[cfg(feature = "hazmat-raw-access")]
pub mod hazmat;

pub use arena::{AutoSpill, RawBackedFixedArenaBuilder, Standard};
pub use arena::{FixedArena, FixedArenaBuilder, InitPolicy, PageSize, Unfaulted};
pub use buddy::{BuddyArena, BuddyArenaBuilder, RawBackedBuddyArenaBuilder};
pub use buffer::Buffer;
pub use dealloc::{Dealloc, HeapDealloc, NoDealloc};
pub use error::{AllocError, BufferFullError, BuildError};

#[cfg(feature = "hazmat-raw-access")]
pub use arena::HazmatRaw;
pub use ext::BytesExt;
pub use geometry::BuddyGeometry;
pub use metrics::{BuddyArenaMetrics, FixedArenaMetrics};
pub use spec::{BuddyHint, SlotSpec};

#[cfg(feature = "async-alloc")]
pub use async_alloc::{
    AsyncBuddyArena, AsyncFixedArena, BuddyWaiter, NotifyWaiters, WaitRegistration, Waiter,
};
