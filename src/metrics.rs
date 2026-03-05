use crate::sync::atomic::{AtomicUsize, Ordering};

/// Snapshot of fixed arena metrics.
#[non_exhaustive]
pub struct FixedArenaMetrics {
    /// Successful arena-backed allocations.
    pub allocations_ok: u64,
    /// Failed allocation attempts that returned `ArenaFull`.
    pub allocations_failed: u64,
    /// Releases back to allocator control.
    pub frees: u64,
    /// Successful arena-backed freezes.
    pub frozen: u64,
    /// Spill transitions from arena-backed to heap-backed storage.
    pub spills: u64,
    /// Total bytes reserved by the arena instance.
    pub bytes_reserved: usize,
    /// Total retained arena-backed capacity currently live.
    pub bytes_live: usize,
}

/// Snapshot of buddy arena metrics.
#[non_exhaustive]
pub struct BuddyArenaMetrics {
    /// Successful arena-backed allocations.
    pub allocations_ok: u64,
    /// Failed allocation attempts that returned `ArenaFull`.
    pub allocations_failed: u64,
    /// Releases back to allocator control.
    pub frees: u64,
    /// Successful arena-backed freezes.
    pub frozen: u64,
    /// Spill transitions from arena-backed to heap-backed storage.
    pub spills: u64,
    /// Total bytes reserved by the arena instance.
    pub bytes_reserved: usize,
    /// Total retained arena-backed capacity currently live.
    pub bytes_live: usize,
    /// Number of buddy split steps performed.
    pub splits: u64,
    /// Number of buddy coalesce steps performed.
    pub coalesces: u64,
    /// Largest currently allocatable buddy block in bytes.
    pub largest_free_block: usize,
}

pub(crate) struct MetricsState {
    allocations_ok: AtomicUsize,
    allocations_failed: AtomicUsize,
    frees: AtomicUsize,
    frozen: AtomicUsize,
    spills: AtomicUsize,
    splits: AtomicUsize,
    coalesces: AtomicUsize,
    bytes_reserved: usize,
    bytes_live: AtomicUsize,
}

impl MetricsState {
    pub(crate) fn new(bytes_reserved: usize) -> Self {
        Self {
            allocations_ok: AtomicUsize::new(0),
            allocations_failed: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            frozen: AtomicUsize::new(0),
            spills: AtomicUsize::new(0),
            splits: AtomicUsize::new(0),
            coalesces: AtomicUsize::new(0),
            bytes_reserved,
            bytes_live: AtomicUsize::new(0),
        }
    }

    pub(crate) fn record_alloc_success(&self, capacity: usize) {
        self.allocations_ok.fetch_add(1, Ordering::Relaxed);
        self.bytes_live.fetch_add(capacity, Ordering::Relaxed);
    }

    pub(crate) fn record_alloc_failure(&self) {
        self.allocations_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_free(&self, capacity: usize) {
        self.frees.fetch_add(1, Ordering::Relaxed);
        self.bytes_live.fetch_sub(capacity, Ordering::Relaxed);
    }

    pub(crate) fn record_frozen(&self) {
        self.frozen.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_spill(&self) {
        self.spills.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_splits(&self, count: u64) {
        self.splits.fetch_add(count as usize, Ordering::Relaxed);
    }

    pub(crate) fn record_coalesce(&self) {
        self.coalesces.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn fixed_snapshot(&self) -> FixedArenaMetrics {
        FixedArenaMetrics {
            allocations_ok: self.allocations_ok.load(Ordering::Relaxed) as u64,
            allocations_failed: self.allocations_failed.load(Ordering::Relaxed) as u64,
            frees: self.frees.load(Ordering::Relaxed) as u64,
            frozen: self.frozen.load(Ordering::Relaxed) as u64,
            spills: self.spills.load(Ordering::Relaxed) as u64,
            bytes_reserved: self.bytes_reserved,
            bytes_live: self.bytes_live.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn buddy_snapshot(&self, largest_free_block: usize) -> BuddyArenaMetrics {
        BuddyArenaMetrics {
            allocations_ok: self.allocations_ok.load(Ordering::Relaxed) as u64,
            allocations_failed: self.allocations_failed.load(Ordering::Relaxed) as u64,
            frees: self.frees.load(Ordering::Relaxed) as u64,
            frozen: self.frozen.load(Ordering::Relaxed) as u64,
            spills: self.spills.load(Ordering::Relaxed) as u64,
            bytes_reserved: self.bytes_reserved,
            bytes_live: self.bytes_live.load(Ordering::Relaxed),
            splits: self.splits.load(Ordering::Relaxed) as u64,
            coalesces: self.coalesces.load(Ordering::Relaxed) as u64,
            largest_free_block,
        }
    }
}
