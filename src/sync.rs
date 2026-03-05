#[cfg(loom)]
pub(crate) use loom::sync::Arc;
#[cfg(not(loom))]
pub(crate) use std::sync::Arc;

#[cfg(loom)]
pub(crate) mod atomic {
    #[cfg(feature = "async-alloc")]
    pub(crate) use loom::sync::atomic::{AtomicBool, AtomicPtr};
    pub(crate) use loom::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
pub(crate) mod atomic {
    #[cfg(feature = "async-alloc")]
    pub(crate) use std::sync::atomic::{AtomicBool, AtomicPtr};
    pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};
}
