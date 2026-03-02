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
                #[cfg(feature = "async-alloc")]
                if let Some(wake_handle) = &inner.wake_handle {
                    wake_handle.wake();
                }
            }
            (ArenaRef::Buddy(inner), AllocationKind::Buddy { order, block_idx }) => {
                inner.release_block(order, block_idx);
            }
            _ => unreachable!("allocation kind must match owning arena"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AllocationKind {
    Fixed { slot_idx: usize },
    Buddy { order: usize, block_idx: usize },
}
