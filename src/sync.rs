#[cfg(not(loom))]
pub(crate) use alloc::sync::Arc;
#[cfg(loom)]
pub(crate) use loom::sync::Arc;

#[cfg(loom)]
pub(crate) mod atomic {
    #[cfg(feature = "async-alloc")]
    pub(crate) use loom::sync::atomic::AtomicU8;
    pub(crate) use loom::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
pub(crate) mod atomic {
    #[cfg(feature = "async-alloc")]
    pub(crate) use core::sync::atomic::AtomicU8;
    pub(crate) use core::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(all(loom, feature = "async-alloc"))]
pub(crate) mod cell {
    pub(crate) use loom::cell::UnsafeCell;
}

#[cfg(all(not(loom), feature = "async-alloc"))]
pub(crate) mod cell {
    /// `core::cell::UnsafeCell` behind the closure-based API of loom, so the
    /// same code compiles against the checked cell of loom under `cfg(loom)`.
    pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(crate) const fn new(data: T) -> Self {
            Self(core::cell::UnsafeCell::new(data))
        }

        #[inline(always)]
        pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}
