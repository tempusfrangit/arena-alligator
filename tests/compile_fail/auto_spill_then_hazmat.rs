use std::num::NonZeroUsize;
use arena_alligator::FixedArena;

fn main() {
    let _ = FixedArena::with_slot_capacity(
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(64).unwrap(),
    )
        .auto_spill()
        .hazmat_raw_access();
}
