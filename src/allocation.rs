use std::sync::Arc;

use crate::arena::ArenaInner;
use crate::buddy::BuddyArenaInner;

pub(crate) enum ArenaRef {
    Fixed(Arc<ArenaInner>),
    Buddy(Arc<BuddyArenaInner>),
}

impl ArenaRef {
    pub(crate) fn release(&self, allocation: AllocationKind) {
        match (self, allocation) {
            (ArenaRef::Fixed(inner), AllocationKind::Fixed { slot_idx }) => {
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
