use core::alloc::Layout;

/// Strategy for deallocating arena backing memory.
///
/// # Safety
///
/// Implementations must correctly free the memory region at `ptr` with
/// length `len`. The pointer and length will match what was originally
/// provided to `from_raw` or allocated internally.
pub unsafe trait Dealloc: Send + Sync + 'static {
    /// Free the backing memory.
    ///
    /// # Safety
    ///
    /// `ptr` must be the same pointer originally provided to the arena,
    /// and `len` must match the original length.
    unsafe fn dealloc(self, ptr: *mut u8, len: usize);
}

/// Frees memory via [`std::alloc::dealloc`] with the stored [`Layout`].
///
/// This is the default dealloc strategy for arenas that allocate their
/// own backing memory.
pub struct HeapDealloc {
    layout: Layout,
}

impl HeapDealloc {
    /// Wrap a [`Layout`] for deallocation via [`std::alloc::dealloc`].
    ///
    /// The layout must match the one used to allocate the memory.
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }
}

// SAFETY: dealloc matches the std::alloc::alloc that produced the memory.
unsafe impl Dealloc for HeapDealloc {
    unsafe fn dealloc(self, ptr: *mut u8, _len: usize) {
        // SAFETY: caller guarantees ptr was allocated with this layout.
        unsafe { std::alloc::dealloc(ptr, self.layout) }
    }
}

/// No-op deallocator for caller-managed memory.
///
/// Use when the caller retains responsibility for freeing the backing
/// memory after the arena drops (e.g. static buffers, linker-placed
/// memory in embedded/no_std).
pub struct NoDealloc;

// SAFETY: no-op is always safe.
unsafe impl Dealloc for NoDealloc {
    unsafe fn dealloc(self, _ptr: *mut u8, _len: usize) {}
}

/// Type-erased deallocator stored in arena inner structs.
///
/// Erases `D: Dealloc` once at arena construction time. The arena's
/// `Drop` impl calls `drop_fn` exactly once.
pub(crate) struct ErasedDealloc {
    data: *mut (),
    drop_fn: unsafe fn(*mut (), *mut u8, usize),
    free_fn: unsafe fn(*mut ()),
}

// SAFETY: The contained D: Dealloc is Send + Sync + 'static,
// and we only access it through the type-matched drop_fn.
unsafe impl Send for ErasedDealloc {}
unsafe impl Sync for ErasedDealloc {}

impl ErasedDealloc {
    /// Erase a concrete `D: Dealloc` into a function pointer + data pair.
    ///
    /// Always boxes `D`. For zero-sized types like `NoDealloc` the
    /// box is a no-op allocation; for sized types like `HeapDealloc`
    /// it stores the `Layout` on the heap once at construction time.
    pub(crate) fn new<D: Dealloc>(dealloc: D) -> Self {
        unsafe fn drop_fn<D: Dealloc>(data: *mut (), ptr: *mut u8, len: usize) {
            debug_assert!(!data.is_null());
            let dealloc = unsafe { *Box::from_raw(data as *mut D) };
            unsafe { dealloc.dealloc(ptr, len) };
        }

        unsafe fn free_fn<D>(data: *mut ()) {
            if !data.is_null() {
                unsafe { drop(Box::from_raw(data as *mut D)) };
            }
        }

        let data = Box::into_raw(Box::new(dealloc)) as *mut ();
        Self {
            data,
            drop_fn: drop_fn::<D>,
            free_fn: free_fn::<D>,
        }
    }

    /// A no-op sentinel used as a replacement value in Drop impls
    /// after the real deallocator has been taken.
    pub(crate) fn noop() -> Self {
        unsafe fn noop_fn(_data: *mut (), _ptr: *mut u8, _len: usize) {}
        unsafe fn noop_free(_data: *mut ()) {}
        Self {
            data: core::ptr::null_mut(),
            drop_fn: noop_fn,
            free_fn: noop_free,
        }
    }

    /// Call the erased deallocator. Consumes the payload.
    ///
    /// # Safety
    ///
    /// Must be called exactly once. `ptr` and `len` must match the
    /// original arena backing memory.
    pub(crate) unsafe fn dealloc(mut self, ptr: *mut u8, len: usize) {
        let data = self.data;
        // Prevent Drop from double-freeing: null the data pointer
        // so free_fn in Drop is a no-op.
        self.data = core::ptr::null_mut();
        unsafe { (self.drop_fn)(data, ptr, len) };
    }
}

impl Drop for ErasedDealloc {
    fn drop(&mut self) {
        // Free the boxed D without calling D::dealloc on the backing memory.
        // This runs when the noop sentinel is dropped after replacement in
        // ArenaInner::drop, or if an ErasedDealloc is dropped without being
        // consumed (e.g. build failure).
        unsafe { (self.free_fn)(self.data) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_dealloc_frees_memory() {
        let layout = Layout::from_size_align(4096, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        let dealloc = HeapDealloc::new(layout);
        unsafe { dealloc.dealloc(ptr, 4096) };
    }

    #[test]
    fn no_dealloc_is_noop() {
        let ptr = core::ptr::NonNull::<u8>::dangling().as_ptr();
        unsafe { NoDealloc.dealloc(ptr, 0) };
    }

    #[test]
    fn erased_dealloc_heap() {
        let layout = Layout::from_size_align(4096, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        let erased = ErasedDealloc::new(HeapDealloc::new(layout));
        unsafe { erased.dealloc(ptr, 4096) };
    }

    #[test]
    fn erased_dealloc_noop() {
        let ptr = core::ptr::NonNull::<u8>::dangling().as_ptr();
        let erased = ErasedDealloc::new(NoDealloc);
        unsafe { erased.dealloc(ptr, 0) };
    }
}
