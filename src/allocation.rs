use crate::arena::ArenaInner;
use crate::buddy::BuddyArenaInner;
use crate::sync::Arc;

pub(crate) enum ArenaRef {
    Fixed(Arc<ArenaInner>),
    Buddy(Arc<BuddyArenaInner>),
}

impl Clone for ArenaRef {
    fn clone(&self) -> Self {
        match self {
            ArenaRef::Fixed(arc) => ArenaRef::Fixed(arc.clone()),
            ArenaRef::Buddy(arc) => ArenaRef::Buddy(arc.clone()),
        }
    }
}

impl ArenaRef {
    pub(crate) fn release(&self, allocation: AllocationKind) {
        match (self, allocation) {
            (ArenaRef::Fixed(inner), AllocationKind::Fixed { slot_idx }) => {
                if inner.init_policy == crate::arena::InitPolicy::Zero {
                    let offset = slot_idx * inner.slot_capacity;
                    // SAFETY: slot is exclusively owned (not yet freed). ptr+offset is valid.
                    unsafe {
                        crate::arena::zeroize_region(inner.ptr.add(offset), inner.slot_capacity);
                    }
                    if let Some(ref zeroed_bm) = inner.zeroed_bitmap {
                        zeroed_bm.set_range(slot_idx, slot_idx + 1);
                    }
                }
                inner.bitmap.free(slot_idx);
                inner.metrics.record_free(inner.slot_capacity);
                #[cfg(feature = "async-alloc")]
                if let Some(wake_handle) = &inner.wake_handle {
                    wake_handle.wake();
                }
            }
            (ArenaRef::Buddy(inner), AllocationKind::Buddy { order, block_idx }) => {
                inner.metrics.record_free(inner.block_size(order));
                inner.release_block(order, block_idx);
            }
            _ => unreachable!("allocation kind must match owning arena"),
        }
    }

    pub(crate) fn record_frozen(&self) {
        match self {
            ArenaRef::Fixed(inner) => inner.metrics.record_frozen(),
            ArenaRef::Buddy(inner) => inner.metrics.record_frozen(),
        }
    }

    pub(crate) fn record_spill(&self) {
        match self {
            ArenaRef::Fixed(inner) => inner.metrics.record_spill(),
            ArenaRef::Buddy(inner) => inner.metrics.record_spill(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AllocationKind {
    Fixed { slot_idx: usize },
    Buddy { order: usize, block_idx: usize },
}
