#[cfg(not(loom))]
pub(crate) use alloc::sync::Arc;
#[cfg(loom)]
pub(crate) use loom::sync::Arc;

#[cfg(loom)]
pub(crate) mod atomic {
    pub(crate) use loom::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
pub(crate) mod atomic {
    pub(crate) use core::sync::atomic::{AtomicUsize, Ordering};
}
