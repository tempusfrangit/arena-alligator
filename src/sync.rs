#[cfg(loom)]
pub(crate) use loom::sync::Arc;
#[cfg(not(loom))]
pub(crate) use std::sync::Arc;

#[cfg(loom)]
pub(crate) mod atomic {
    pub(crate) use loom::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
pub(crate) mod atomic {
    pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};
}
