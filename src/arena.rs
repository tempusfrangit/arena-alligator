use std::alloc::Layout;
use std::fmt;
use std::num::NonZeroUsize;

use crate::bitmap::AtomicBitmap;
use crate::buffer::Buffer;
use crate::error::{AllocError, BuildError};
use crate::metrics::{FixedArenaMetrics, MetricsState};
use crate::sync::Arc;

/// Page size used for prefaulting the arena backing allocation.
///
/// [`build()`](crate::FixedArenaBuilder::build) touches every page at
/// build time when the page size is known ([`Auto`](Self::Auto) or
/// [`Size`](Self::Size)). Use
/// [`build_unfaulted()`](crate::FixedArenaBuilder::build_unfaulted) to
/// defer faulting for explicit control (e.g. NUMA placement).
///
/// # NUMA placement
///
/// The kernel allocates physical pages on the node where the faulting
/// thread runs. Three approaches:
///
/// 1. **Pin the builder thread** and call `build()`. Pages fault on the
///    pinned node immediately.
/// 2. **`build_unfaulted()`** and call
///    [`fault_pages()`](crate::Unfaulted::fault_pages) from a thread
///    pinned to the target node.
/// 3. **`build_unfaulted().into_inner()`** and let the kernel
///    demand-fault pages as each thread touches them (first-touch policy).
///
/// # Huge pages
///
/// Transparent huge pages (THP) are handled by the kernel and work with
/// any page size here. For pre-allocated huge pages, pass the huge-page
/// size (e.g. 2 MiB) via [`Size`](Self::Size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    /// Page size is not known. No prefaulting will occur.
    Unknown,
    /// Detect page size from the OS via `sysconf(_SC_PAGESIZE)`.
    ///
    /// Only available on Unix with the `libc` feature enabled.
    #[cfg(all(unix, feature = "libc"))]
    Auto,
    /// Caller-supplied page size.
    Size(NonZeroUsize),
}

impl PageSize {
    pub(crate) fn resolve(self) -> Option<usize> {
        match self {
            PageSize::Unknown => None,
            #[cfg(all(unix, feature = "libc"))]
            PageSize::Auto => Some(os_page_size()),
            PageSize::Size(n) => Some(n.get()),
        }
    }
}

#[cfg(all(unix, feature = "libc"))]
fn os_page_size() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) is always safe and returns a positive value.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    debug_assert!(ps > 0);
    ps as usize
}

/// Touch one byte per page to force physical backing.
pub(crate) fn prefault_region(ptr: *mut u8, len: usize, page_size: usize) {
    let mut offset = 0;
    while offset < len {
        // SAFETY: ptr..ptr+len is a valid allocation. Each write is within bounds.
        unsafe { ptr.add(offset).write_volatile(0) };
        offset += page_size;
    }
}

/// Zeroize a region of memory using the `zeroize` crate.
///
/// Compiler-guaranteed not to be elided, unlike `ptr::write_bytes`.
pub(crate) fn zeroize_region(ptr: *mut u8, len: usize) {
    // SAFETY: caller guarantees ptr..ptr+len is a valid, exclusively-owned allocation.
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
    zeroize::Zeroize::zeroize(slice);
}

/// Initialization policy for arena memory.
///
/// Controls whether arena memory is zeroed. The default
/// ([`Uninit`](Self::Uninit)) leaves memory as-is for maximum throughput.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InitPolicy {
    /// Leave memory uninitialized (default).
    #[default]
    Uninit,
    /// Zero-fill memory on return to the arena and on first allocation.
    ///
    /// On return: the full slot or block is zeroed before it is marked free.
    /// On allocation: cold memory (never returned) is zeroed. Memory that
    /// has been through a return-scrub cycle is no longer cold and the
    /// alloc-path zero is skipped.
    ///
    /// Uses [`zeroize`] for compiler-guaranteed zeroing.
    Zero,
}

/// An arena whose backing pages have not yet been faulted.
///
/// Created by [`FixedArenaBuilder::build_unfaulted()`] or
/// [`BuddyArenaBuilder::build_unfaulted()`](crate::BuddyArenaBuilder::build_unfaulted).
///
/// - [`fault_pages()`](Self::fault_pages) walks every page explicitly, then
///   returns the arena. Intended for use from a NUMA-pinned thread.
/// - [`into_inner()`](Self::into_inner) skips the walk. The kernel
///   demand-faults pages on first access (first-touch policy).
/// - [`allocate()`](Unfaulted::<FixedArena>::allocate) unwraps into the arena
///   and allocates immediately.
pub struct Unfaulted<A> {
    ptr: *mut u8,
    total_size: usize,
    page_size: Option<usize>,
    inner: A,
}

// SAFETY: the inner arena is Send, and the raw ptr is anchored by it.
unsafe impl<A: Send> Send for Unfaulted<A> {}

impl<A> Unfaulted<A> {
    pub(crate) fn new(ptr: *mut u8, total_size: usize, page_size: Option<usize>, inner: A) -> Self {
        Self {
            ptr,
            total_size,
            page_size,
            inner,
        }
    }

    /// Walk every page in the backing allocation to force physical backing,
    /// then return the unwrapped arena.
    ///
    /// Sequential faulting (low-to-high) is friendlier to TLB prefetchers
    /// and gives the kernel a better chance at physically contiguous frames.
    pub fn fault_pages(self) -> A {
        if let Some(ps) = self.page_size {
            prefault_region(self.ptr, self.total_size, ps);
        }
        self.inner
    }

    /// Unwrap the arena without faulting. Pages will be demand-faulted by
    /// the kernel on first access.
    pub fn into_inner(self) -> A {
        self.inner
    }
}

impl Unfaulted<FixedArena> {
    /// Unwrap without faulting and allocate immediately.
    ///
    /// Pages will be demand-faulted by the kernel as written.
    pub fn allocate(self) -> Result<(FixedArena, Buffer), AllocError> {
        let arena = self.into_inner();
        let buf = arena.allocate()?;
        Ok((arena, buf))
    }
}

/// Typestate marker for the default builder mode.
#[derive(Debug, Clone, Copy)]
pub struct Standard;

/// Typestate marker for auto-spill builder mode.
#[derive(Debug, Clone, Copy)]
pub struct AutoSpill;

/// Typestate marker for hazmat raw-access builder mode.
#[cfg(feature = "hazmat-raw-access")]
#[derive(Debug, Clone, Copy)]
pub struct HazmatRaw;

/// Shared builder configuration for both arena types.
pub(crate) struct BuildConfig {
    pub(crate) alignment: usize,
    pub(crate) auto_spill: bool,
    pub(crate) init_policy: InitPolicy,
    pub(crate) page_size: PageSize,
}

impl BuildConfig {
    pub(crate) fn new() -> Self {
        Self {
            alignment: 1,
            auto_spill: false,
            init_policy: InitPolicy::default(),
            #[cfg(all(unix, feature = "libc"))]
            page_size: PageSize::Auto,
            #[cfg(not(all(unix, feature = "libc")))]
            page_size: PageSize::Unknown,
        }
    }

    pub(crate) fn validate_alignment(&self) -> Result<(), BuildError> {
        if !self.alignment.is_power_of_two() {
            return Err(BuildError::InvalidAlignment);
        }
        Ok(())
    }
}

pub(crate) struct ArenaInner {
    pub(crate) ptr: *mut u8,
    pub(crate) total_size: usize,
    pub(crate) slot_capacity: usize,
    pub(crate) slot_count: usize,
    pub(crate) bitmap: AtomicBitmap,
    pub(crate) auto_spill: bool,
    pub(crate) init_policy: InitPolicy,
    pub(crate) metrics: MetricsState,
    /// Tracks which slots have been return-scrubbed. Only present when
    /// `init_policy == Zero`. Write-once: return path sets bits, alloc
    /// path only reads.
    pub(crate) zeroed_bitmap: Option<AtomicBitmap>,
    dealloc: crate::dealloc::ErasedDealloc,
    #[cfg(feature = "async-alloc")]
    pub(crate) wake_handle: Option<crate::async_alloc::WakeHandle>,
}

// SAFETY: Buffer discipline enforces exclusive access per slot:
// - Writing: one Buffer per slot index (bitmap claim enforced)
// - Frozen: immutable access through Bytes (buffer consumed by freeze)
// - No overlap between slots (each slot is at a distinct offset)
unsafe impl Send for ArenaInner {}
unsafe impl Sync for ArenaInner {}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        // SAFETY: ErasedDealloc::dealloc is called exactly once (here).
        // We take ownership via std::mem::replace to avoid double-drop.
        unsafe {
            let dealloc =
                std::mem::replace(&mut self.dealloc, crate::dealloc::ErasedDealloc::noop());
            dealloc.dealloc(self.ptr, self.total_size);
        }
    }
}

/// Fixed-size slot arena allocator.
///
/// All slots have identical capacity. Allocation is lock-free via atomic
/// bitmap. Produces `bytes::Bytes` through [`Buffer::freeze()`].
///
/// Cheap to clone — clones share the same backing memory via `Arc`.
#[derive(Clone)]
pub struct FixedArena {
    pub(crate) inner: Arc<ArenaInner>,
}

impl fmt::Debug for FixedArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixedArena")
            .field("slot_count", &self.inner.slot_count)
            .field("slot_capacity", &self.inner.slot_capacity)
            .finish()
    }
}

impl FixedArena {
    /// Create a builder with per-slot capacity in bytes.
    pub fn with_slot_capacity(
        slot_count: NonZeroUsize,
        slot_capacity: NonZeroUsize,
    ) -> FixedArenaBuilder {
        FixedArenaBuilder {
            slot_count,
            slot_capacity,
            config: BuildConfig::new(),
            _mode: std::marker::PhantomData,
        }
    }

    /// Create a builder with total arena capacity in bytes.
    ///
    /// Per-slot capacity is derived as `ceil(total / slot_count)`, then
    /// rounded up to alignment at build time.
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use arena_alligator::FixedArena;
    ///
    /// let arena = FixedArena::with_arena_capacity(
    ///     NonZeroUsize::new(4).unwrap(),
    ///     NonZeroUsize::new(1000).unwrap(),
    /// ).build().unwrap();
    /// assert_eq!(arena.slot_count(), 4);
    /// assert_eq!(arena.slot_capacity(), 250); // ceil(1000 / 4)
    /// ```
    pub fn with_arena_capacity(
        slot_count: NonZeroUsize,
        arena_capacity: NonZeroUsize,
    ) -> FixedArenaBuilder {
        let per_slot = arena_capacity.get().div_ceil(slot_count.get());
        FixedArenaBuilder {
            slot_count,
            // per_slot >= 1 because arena_capacity >= 1 and slot_count >= 1
            slot_capacity: NonZeroUsize::new(per_slot).unwrap(),
            config: BuildConfig::new(),
            _mode: std::marker::PhantomData,
        }
    }

    /// Create an arena from user-provided memory.
    ///
    /// This is the preallocated-memory path: the arena uses the caller's
    /// backing region instead of allocating its own. Ownership transfers to
    /// the returned builder and then to the arena produced by
    /// [`build()`](RawBackedFixedArenaBuilder::build). When the last arena
    /// reference drops, the backing region is released through `dealloc`.
    ///
    /// Use [`NoDealloc`](crate::NoDealloc) for caller-managed memory such as
    /// static buffers or externally-owned mappings. For `&'static mut [u8]`,
    /// prefer the safe [`from_static()`](Self::from_static) wrapper.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to a valid, exclusively-owned allocation of at
    ///   least `len` bytes.
    /// - The region must not be accessed through any other mutable or shared
    ///   alias for the lifetime of the arena and any frozen [`Bytes`](bytes::Bytes)
    ///   derived from it.
    /// - The memory must remain valid until `D::dealloc` is called, which
    ///   happens when the last arena reference and last frozen `Bytes`
    ///   derived from it drop.
    /// - `dealloc` must correctly release the original region, or be
    ///   [`NoDealloc`](crate::NoDealloc) if the caller retains
    ///   responsibility for the backing memory.
    ///
    /// If `build()` returns `Err`, the caller retains ownership of the
    /// memory and remains responsible for releasing it.
    pub unsafe fn from_raw<D: crate::dealloc::Dealloc>(
        ptr: *mut u8,
        len: usize,
        spec: crate::spec::SlotSpec,
        dealloc: D,
    ) -> RawBackedFixedArenaBuilder<D> {
        RawBackedFixedArenaBuilder {
            ptr,
            len,
            spec,
            dealloc,
            config: BuildConfig::new(),
        }
    }

    /// Build a fixed arena from a `&'static mut` buffer with [`NoDealloc`](crate::NoDealloc).
    ///
    /// This is a safe convenience wrapper over [`from_raw()`](Self::from_raw)
    /// for static buffers (e.g. linker-placed memory in embedded/no_std).
    /// The static lifetime guarantees the memory outlives the arena, and
    /// [`NoDealloc`](crate::NoDealloc) matches the fact that static memory
    /// must not be freed.
    pub fn from_static(
        buf: &'static mut [u8],
        spec: crate::spec::SlotSpec,
    ) -> RawBackedFixedArenaBuilder<crate::dealloc::NoDealloc> {
        // SAFETY: static lifetime guarantees the memory outlives the arena
        // and all derived Bytes. Exclusive &mut ensures no aliasing.
        // NoDealloc is correct because static memory must not be freed.
        unsafe { Self::from_raw(buf.as_mut_ptr(), buf.len(), spec, crate::dealloc::NoDealloc) }
    }

    /// Number of slots in this arena.
    pub fn slot_count(&self) -> usize {
        self.inner.slot_count
    }

    /// Capacity of each slot in bytes (aligned).
    pub fn slot_capacity(&self) -> usize {
        self.inner.slot_capacity
    }

    /// Snapshot current allocator metrics.
    pub fn metrics(&self) -> FixedArenaMetrics {
        self.inner.metrics.fixed_snapshot()
    }

    /// Allocate a buffer. Returns `Err(AllocError::ArenaFull)` if all slots are in use.
    pub fn allocate(&self) -> Result<Buffer, AllocError> {
        let Some(slot_idx) = self.inner.bitmap.try_alloc() else {
            self.inner.metrics.record_alloc_failure();
            return Err(AllocError::ArenaFull);
        };

        let offset = slot_idx * self.inner.slot_capacity;

        match self.inner.init_policy {
            InitPolicy::Zero => {
                if let Some(ref zeroed_bm) = self.inner.zeroed_bitmap
                    && !zeroed_bm.all_set_in_range(slot_idx, slot_idx + 1)
                {
                    // SAFETY: ptr+offset..ptr+offset+slot_capacity is within the arena
                    // allocation and exclusively owned by this slot (bitmap claim above).
                    unsafe { zeroize_region(self.inner.ptr.add(offset), self.inner.slot_capacity) };
                }
            }
            InitPolicy::Uninit => {}
        }

        self.inner
            .metrics
            .record_alloc_success(self.inner.slot_capacity);

        Ok(Buffer::new_fixed(
            crate::allocation::ArenaRef::Fixed(self.inner.clone()),
            self.inner.ptr,
            self.inner.auto_spill,
            slot_idx,
            offset,
            self.inner.slot_capacity,
        ))
    }
}

/// Builder for a fixed arena backed by user-provided memory.
///
/// Created via [`FixedArena::from_raw()`].
///
/// This mirrors the ordinary fixed-arena builder for post-construction
/// policy knobs, but it does not own the backing allocation itself. The
/// caller's `D` is erased when [`build()`](Self::build) succeeds.
pub struct RawBackedFixedArenaBuilder<D: crate::dealloc::Dealloc> {
    ptr: *mut u8,
    len: usize,
    spec: crate::spec::SlotSpec,
    dealloc: D,
    config: BuildConfig,
}

impl<D: crate::dealloc::Dealloc> RawBackedFixedArenaBuilder<D> {
    /// Set the initialization policy for allocated buffers.
    ///
    /// [`InitPolicy::Zero`] zeroes visible slot capacity on first allocation
    /// and on return, just like the self-allocated builder path.
    pub fn init_policy(mut self, policy: InitPolicy) -> Self {
        self.config.init_policy = policy;
        self
    }

    /// Set the page size used for prefaulting.
    ///
    /// This only affects [`build()`](Self::build) prefault behavior. It does
    /// not change the caller-provided backing region or its alignment.
    pub fn page_size(mut self, policy: PageSize) -> Self {
        self.config.page_size = policy;
        self
    }

    /// Set the minimum alignment for each slot.
    ///
    /// Slot sizes are padded **down** to this alignment. Must be a power
    /// of two. The caller is responsible for ensuring the backing pointer
    /// itself satisfies this alignment.
    pub fn alignment(mut self, align: usize) -> Self {
        self.config.alignment = align;
        self
    }

    /// Build the arena from user-provided memory.
    ///
    /// `SlotSpec` resolves the visible slot geometry from the supplied
    /// region. Tail bytes that do not fit a whole aligned slot are left
    /// unused.
    pub fn build(self) -> Result<FixedArena, BuildError> {
        if self.ptr.is_null() {
            return Err(BuildError::NullPointer);
        }
        self.config.validate_alignment()?;

        let page_size = self.config.page_size.resolve();
        let (slot_count, slot_capacity) = self.spec.resolve(self.len, self.config.alignment)?;

        let zeroed_bitmap = match self.config.init_policy {
            InitPolicy::Zero => Some(AtomicBitmap::new_empty(slot_count)),
            InitPolicy::Uninit => None,
        };

        let inner = ArenaInner {
            ptr: self.ptr,
            total_size: self.len,
            slot_capacity,
            slot_count,
            bitmap: AtomicBitmap::new(slot_count),
            auto_spill: false,
            init_policy: self.config.init_policy,
            metrics: MetricsState::new(slot_count * slot_capacity),
            zeroed_bitmap,
            dealloc: crate::dealloc::ErasedDealloc::new(self.dealloc),
            #[cfg(feature = "async-alloc")]
            wake_handle: None,
        };

        let arena = FixedArena {
            inner: Arc::new(inner),
        };

        if let Some(ps) = page_size {
            prefault_region(
                arena.inner.ptr,
                arena.inner.slot_count * arena.inner.slot_capacity,
                ps,
            );
        }

        Ok(arena)
    }
}

/// Builder for [`FixedArena`].
///
/// Created via [`FixedArena::with_slot_capacity()`] or
/// [`FixedArena::with_arena_capacity()`].
///
/// The `Mode` parameter controls which build targets are available:
///
/// - [`Standard`] (default): builds [`FixedArena`]. Can transition to
///   [`AutoSpill`] or [`HazmatRaw`].
/// - [`AutoSpill`]: builds [`FixedArena`] with heap overflow fallback.
/// - [`HazmatRaw`]: builds [`RawFixedArena`](crate::hazmat::RawFixedArena)
///   with raw pointer access. Requires `hazmat-raw-access` feature.
pub struct FixedArenaBuilder<Mode = Standard> {
    slot_count: NonZeroUsize,
    slot_capacity: NonZeroUsize,
    config: BuildConfig,
    _mode: std::marker::PhantomData<Mode>,
}

impl<Mode> FixedArenaBuilder<Mode> {
    /// Alignment for arena backing, slot boundaries, and slot capacities.
    ///
    /// Must be a power of 2. Default: 1 (no alignment constraint).
    /// Use 4096 for O_DIRECT / DMA compatibility.
    pub fn alignment(mut self, n: usize) -> Self {
        self.config.alignment = n;
        self
    }

    /// Set the initialization policy for allocated buffers.
    ///
    /// Default: [`InitPolicy::Uninit`]. When set to [`InitPolicy::Zero`],
    /// every allocation writes zeroes across the slot before returning.
    pub fn init_policy(mut self, policy: InitPolicy) -> Self {
        self.config.init_policy = policy;
        self
    }

    /// Set the page size used for prefaulting.
    ///
    /// Default: [`PageSize::Auto`] on Unix with the `libc` feature,
    /// [`PageSize::Unknown`] otherwise.
    ///
    /// When set to [`PageSize::Auto`] or [`PageSize::Size`], [`build()`](Self::build)
    /// touches every page at build time. Use [`build_unfaulted()`](Self::build_unfaulted)
    /// to defer the walk (e.g. for NUMA placement).
    pub fn page_size(mut self, policy: PageSize) -> Self {
        self.config.page_size = policy;
        self
    }

    fn build_inner(
        self,
        #[cfg(feature = "async-alloc")] wake_handle: Option<crate::async_alloc::WakeHandle>,
    ) -> Result<FixedArena, BuildError> {
        self.config.validate_alignment()?;

        let slot_count = self.slot_count.get();
        let slot_capacity = self.slot_capacity.get();

        let aligned_capacity =
            align_up(slot_capacity, self.config.alignment).ok_or(BuildError::SizeOverflow)?;

        let total_size = slot_count
            .checked_mul(aligned_capacity)
            .ok_or(BuildError::SizeOverflow)?;

        let layout = Layout::from_size_align(total_size, self.config.alignment)
            .map_err(|_| BuildError::SizeOverflow)?;

        // SAFETY: layout has non-zero size (slot_count > 0, aligned_capacity > 0).
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        let zeroed_bitmap = match self.config.init_policy {
            InitPolicy::Zero => Some(AtomicBitmap::new_empty(slot_count)),
            InitPolicy::Uninit => None,
        };

        let inner = ArenaInner {
            ptr,
            total_size,
            slot_capacity: aligned_capacity,
            slot_count,
            bitmap: AtomicBitmap::new(slot_count),
            auto_spill: self.config.auto_spill,
            init_policy: self.config.init_policy,
            metrics: MetricsState::new(total_size),
            zeroed_bitmap,
            dealloc: crate::dealloc::ErasedDealloc::new(crate::dealloc::HeapDealloc::new(layout)),
            #[cfg(feature = "async-alloc")]
            wake_handle,
        };

        Ok(FixedArena {
            inner: Arc::new(inner),
        })
    }

    fn build_fixed(self) -> Result<FixedArena, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_inner(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        if let Some(ps) = page_size {
            prefault_region(
                arena.inner.ptr,
                arena.inner.slot_count * arena.inner.slot_capacity,
                ps,
            );
        }
        Ok(arena)
    }

    fn build_fixed_unfaulted(self) -> Result<Unfaulted<FixedArena>, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_inner(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        let total_size = arena.inner.slot_count * arena.inner.slot_capacity;
        Ok(Unfaulted::new(
            arena.inner.ptr,
            total_size,
            page_size,
            arena,
        ))
    }

    #[cfg(feature = "async-alloc")]
    fn build_fixed_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncFixedArena<W>, BuildError>
    where
        W: crate::async_alloc::Waiter,
    {
        let page_size = self.config.page_size.resolve();
        let waiters = std::sync::Arc::new(waiters);
        let arena = self.build_inner(Some(crate::async_alloc::WakeHandle::new(
            std::sync::Arc::clone(&waiters),
        )))?;

        if let Some(ps) = page_size {
            prefault_region(
                arena.inner.ptr,
                arena.inner.slot_count * arena.inner.slot_capacity,
                ps,
            );
        }

        Ok(crate::async_alloc::AsyncFixedArena::new(arena, waiters))
    }
}

impl FixedArenaBuilder<Standard> {
    /// Transition to [`AutoSpill`] mode. Overflow writes copy to heap,
    /// freeing the arena slot.
    ///
    /// Mutually exclusive with
    /// [`hazmat_raw_access()`](Self::hazmat_raw_access) at compile time.
    pub fn auto_spill(self) -> FixedArenaBuilder<AutoSpill> {
        FixedArenaBuilder {
            slot_count: self.slot_count,
            slot_capacity: self.slot_capacity,
            config: BuildConfig {
                auto_spill: true,
                ..self.config
            },
            _mode: std::marker::PhantomData,
        }
    }

    /// Build the arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<FixedArena, BuildError> {
        self.build_fixed()
    }

    /// Build the arena without prefaulting. Returns an [`Unfaulted`] wrapper.
    ///
    /// See [`Unfaulted`] for the three consumption paths: explicit fault,
    /// demand-fault, or direct allocate.
    pub fn build_unfaulted(self) -> Result<Unfaulted<FixedArena>, BuildError> {
        self.build_fixed_unfaulted()
    }
}

impl FixedArenaBuilder<AutoSpill> {
    /// Build the arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<FixedArena, BuildError> {
        self.build_fixed()
    }

    /// Build the arena without prefaulting. Returns an [`Unfaulted`] wrapper.
    pub fn build_unfaulted(self) -> Result<Unfaulted<FixedArena>, BuildError> {
        self.build_fixed_unfaulted()
    }
}

#[cfg(feature = "hazmat-raw-access")]
impl FixedArenaBuilder<Standard> {
    /// Transition to [`HazmatRaw`] mode.
    ///
    /// Mutually exclusive with [`auto_spill()`](Self::auto_spill) at compile
    /// time.
    pub fn hazmat_raw_access(self) -> FixedArenaBuilder<HazmatRaw> {
        FixedArenaBuilder {
            slot_count: self.slot_count,
            slot_capacity: self.slot_capacity,
            config: self.config,
            _mode: std::marker::PhantomData,
        }
    }
}

#[cfg(feature = "hazmat-raw-access")]
impl FixedArenaBuilder<HazmatRaw> {
    /// Build the arena, prefaulting pages if a page size is configured.
    pub fn build(self) -> Result<crate::hazmat::RawFixedArena, BuildError> {
        self.build_fixed().map(crate::hazmat::RawFixedArena)
    }

    /// Build the arena without prefaulting.
    pub fn build_unfaulted(self) -> Result<Unfaulted<crate::hazmat::RawFixedArena>, BuildError> {
        let page_size = self.config.page_size.resolve();
        let arena = self.build_inner(
            #[cfg(feature = "async-alloc")]
            None,
        )?;
        let total_size = arena.inner.slot_count * arena.inner.slot_capacity;
        Ok(Unfaulted::new(
            arena.inner.ptr,
            total_size,
            page_size,
            crate::hazmat::RawFixedArena(arena),
        ))
    }
}

#[cfg(feature = "async-alloc")]
impl FixedArenaBuilder<Standard> {
    /// Build an async-capable arena using the default notify-based waiter.
    pub fn build_async(self) -> Result<crate::async_alloc::AsyncFixedArena, BuildError> {
        self.build_fixed_async_with(crate::async_alloc::NotifyWaiters::new(1))
    }

    /// Build an async-capable arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncFixedArena<W>, BuildError>
    where
        W: crate::async_alloc::Waiter,
    {
        self.build_fixed_async_with(waiters)
    }
}

#[cfg(feature = "async-alloc")]
impl FixedArenaBuilder<AutoSpill> {
    /// Build an async-capable arena using the default notify-based waiter.
    pub fn build_async(self) -> Result<crate::async_alloc::AsyncFixedArena, BuildError> {
        self.build_fixed_async_with(crate::async_alloc::NotifyWaiters::new(1))
    }

    /// Build an async-capable arena with a custom waiter policy.
    pub fn build_async_with<W>(
        self,
        waiters: W,
    ) -> Result<crate::async_alloc::AsyncFixedArena<W>, BuildError>
    where
        W: crate::async_alloc::Waiter,
    {
        self.build_fixed_async_with(waiters)
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    let rounded = value.checked_add(alignment - 1)?;
    Some(rounded & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn build_basic_arena() {
        let arena = FixedArena::with_slot_capacity(nz(4), nz(64))
            .build()
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[test]
    fn build_invalid_alignment_fails() {
        let err = FixedArena::with_slot_capacity(nz(4), nz(64))
            .alignment(3)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn build_zero_alignment_fails() {
        let err = FixedArena::with_slot_capacity(nz(4), nz(64))
            .alignment(0)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::InvalidAlignment);
    }

    #[test]
    fn metrics_track_allocate_free_and_failure() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .build()
            .unwrap();

        let initial = arena.metrics();
        assert_eq!(initial.bytes_reserved, 64);
        assert_eq!(initial.bytes_live, 0);

        let buf = arena.allocate().unwrap();
        let after_alloc = arena.metrics();
        assert_eq!(after_alloc.allocations_ok, 1);
        assert_eq!(after_alloc.allocations_failed, 0);
        assert_eq!(after_alloc.bytes_live, 64);

        assert_eq!(arena.allocate().unwrap_err(), AllocError::ArenaFull);
        let after_fail = arena.metrics();
        assert_eq!(after_fail.allocations_failed, 1);
        assert_eq!(after_fail.bytes_live, 64);

        drop(buf);
        let after_free = arena.metrics();
        assert_eq!(after_free.frees, 1);
        assert_eq!(after_free.bytes_live, 0);
    }

    #[test]
    fn build_size_overflow_fails() {
        let err = FixedArena::with_slot_capacity(nz(usize::MAX), nz(2))
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::SizeOverflow);
    }

    #[test]
    fn alignment_rounding_overflow_fails() {
        let err = FixedArena::with_slot_capacity(nz(1), nz(usize::MAX))
            .alignment(2)
            .build()
            .unwrap_err();
        assert_eq!(err, BuildError::SizeOverflow);
    }

    #[test]
    fn alignment_rounds_capacity_up() {
        let arena = FixedArena::with_slot_capacity(nz(2), nz(100))
            .alignment(64)
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 128);
    }

    #[test]
    fn alignment_4096_rounds_up() {
        let arena = FixedArena::with_slot_capacity(nz(4), nz(100))
            .alignment(4096)
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 4096);
    }

    #[test]
    fn prefault_disabled_builds() {
        let arena = FixedArena::with_slot_capacity(nz(4), nz(64))
            .page_size(PageSize::Unknown)
            .build()
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
    }

    #[test]
    fn prefault_explicit_page_size_builds() {
        let arena = FixedArena::with_slot_capacity(nz(4), nz(4096))
            .page_size(PageSize::Size(nz(4096)))
            .build()
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
    }

    #[cfg(all(unix, feature = "libc"))]
    #[test]
    fn prefault_auto_builds() {
        let arena = FixedArena::with_slot_capacity(nz(4), nz(4096))
            .page_size(PageSize::Auto)
            .build()
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
    }

    #[test]
    fn build_unfaulted_then_fault_pages() {
        let faultable = FixedArena::with_slot_capacity(nz(4), nz(4096))
            .page_size(PageSize::Size(nz(4096)))
            .build_unfaulted()
            .unwrap();
        let arena = faultable.fault_pages();
        assert_eq!(arena.slot_count(), 4);
        let _buf = arena.allocate().unwrap();
    }

    #[test]
    fn build_unfaulted_into_inner_skips_fault() {
        let faultable = FixedArena::with_slot_capacity(nz(4), nz(64))
            .page_size(PageSize::Unknown)
            .build_unfaulted()
            .unwrap();
        let arena = faultable.into_inner();
        assert_eq!(arena.slot_count(), 4);
        let _buf = arena.allocate().unwrap();
    }

    #[test]
    fn clone_shares_inner() {
        let arena = FixedArena::with_slot_capacity(nz(2), nz(64))
            .build()
            .unwrap();
        let arena2 = arena.clone();
        assert_eq!(arena.slot_count(), arena2.slot_count());
        assert_eq!(arena.slot_capacity(), arena2.slot_capacity());
    }

    #[test]
    fn allocate_and_drop() {
        let arena = FixedArena::with_slot_capacity(nz(2), nz(64))
            .build()
            .unwrap();

        let buf1 = arena.allocate().unwrap();
        let buf2 = arena.allocate().unwrap();
        assert!(arena.allocate().is_err(), "arena should be full");

        drop(buf1);
        let _buf3 = arena.allocate().unwrap();
        drop(buf2);
    }

    #[test]
    fn allocate_full_returns_arena_full() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(32))
            .build()
            .unwrap();

        let _buf = arena.allocate().unwrap();
        let err = arena.allocate().unwrap_err();
        assert_eq!(err, crate::AllocError::ArenaFull);
    }

    #[test]
    fn drop_returns_slot() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(32))
            .build()
            .unwrap();

        let buf = arena.allocate().unwrap();
        drop(buf);
        assert!(
            arena.allocate().is_ok(),
            "slot should be available after drop"
        );
    }

    #[test]
    fn init_policy_zero_fills_slot() {
        use bytes::BufMut;

        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .init_policy(InitPolicy::Zero)
            .page_size(PageSize::Unknown)
            .build()
            .unwrap();

        // Write non-zero data, freeze, drop to return the slot.
        let mut buf = arena.allocate().unwrap();
        buf.put_slice(&[0xAB; 64]);
        let bytes = buf.freeze();
        drop(bytes);

        // Re-allocate the same slot; zero policy should have cleared it.
        let buf = arena.allocate().unwrap();
        let slot = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 64) };
        assert!(slot.iter().all(|&b| b == 0), "slot should be zeroed");
    }

    #[test]
    fn init_policy_default_is_uninit() {
        assert_eq!(InitPolicy::default(), InitPolicy::Uninit);
    }

    #[test]
    fn builder_with_arena_capacity() {
        let arena = FixedArena::with_arena_capacity(nz(4), nz(256))
            .build()
            .unwrap();
        assert_eq!(arena.slot_count(), 4);
        assert_eq!(arena.slot_capacity(), 64);
    }

    #[test]
    fn builder_arena_capacity_rounds_up() {
        let arena = FixedArena::with_arena_capacity(nz(3), nz(1000))
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 334);
    }

    #[test]
    fn builder_arena_capacity_with_alignment() {
        let arena = FixedArena::with_arena_capacity(nz(3), nz(1000))
            .alignment(64)
            .build()
            .unwrap();
        assert_eq!(arena.slot_capacity(), 384);
    }

    #[test]
    fn auto_spill_builder_produces_fixed_arena() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .auto_spill()
            .build()
            .unwrap();
        let _buf = arena.allocate().unwrap();
    }

    #[cfg(feature = "hazmat-raw-access")]
    #[test]
    fn hazmat_builder_produces_raw_fixed_arena() {
        let raw_arena = FixedArena::with_slot_capacity(nz(4), nz(64))
            .hazmat_raw_access()
            .build()
            .unwrap();
        let _buf = raw_arena.allocate().unwrap();
    }

    #[test]
    fn zero_policy_zeroes_on_return() {
        use bytes::BufMut;

        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .init_policy(InitPolicy::Zero)
            .page_size(PageSize::Unknown)
            .build()
            .unwrap();

        let mut buf = arena.allocate().unwrap();
        buf.put_slice(&[0xAB; 64]);
        let bytes = buf.freeze();
        drop(bytes);

        let buf = arena.allocate().unwrap();
        let slot = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 64) };
        assert!(
            slot.iter().all(|&b| b == 0),
            "slot should be zeroed from return path"
        );
    }

    #[test]
    fn zero_policy_first_alloc_zeroes_cold_memory() {
        let arena = FixedArena::with_slot_capacity(nz(1), nz(64))
            .init_policy(InitPolicy::Zero)
            .page_size(PageSize::Unknown)
            .build()
            .unwrap();

        let buf = arena.allocate().unwrap();
        let slot = unsafe { std::slice::from_raw_parts(buf.ptr.add(buf.offset), 64) };
        assert!(slot.iter().all(|&b| b == 0), "first alloc should be zeroed");
    }
}
