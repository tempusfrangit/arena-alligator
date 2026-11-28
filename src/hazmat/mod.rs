//! Low-level zero-copy access to arena memory.
//!
//! Available with the `hazmat-raw-access` feature. Types here expose
//! raw pointers and unsafe freeze semantics.
//!
//! Freezing a raw region returns ordinary [`bytes::Bytes`]. Clones,
//! slices, and downstream `Bytes` helpers keep the arena backing
//! allocation pinned until the final reference drops.
//!
//! With the `async-alloc` feature, this module also contains the async arena
//! types `AsyncRawFixedArena` and `AsyncRawBuddyArena`.

#[cfg(all(feature = "hazmat-raw-access", feature = "async-alloc"))]
mod async_raw;
#[cfg(feature = "hazmat-raw-access")]
mod raw_region;

#[cfg(all(feature = "hazmat-raw-access", feature = "async-alloc"))]
pub use async_raw::{AsyncRawBuddyArena, AsyncRawFixedArena};
#[cfg(feature = "hazmat-raw-access")]
pub use raw_region::{RawBuddyArena, RawFixedArena, RawFreezeError, RawRegion};
