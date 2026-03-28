//! Low-level zero-copy access to arena memory.
//!
//! Available with the `hazmat-raw-access` feature. Types here expose
//! raw pointers and unsafe freeze semantics.
//!
//! Freezing a raw region returns ordinary [`bytes::Bytes`]. Clones,
//! slices, and downstream `Bytes` helpers keep the arena backing
//! allocation pinned until the final reference drops.

#[cfg(feature = "hazmat-raw-access")]
mod raw_region;

#[cfg(feature = "hazmat-raw-access")]
pub use raw_region::{RawBuddyArena, RawFixedArena, RawFreezeError, RawRegion};
