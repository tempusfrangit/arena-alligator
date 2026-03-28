use std::num::NonZeroUsize;
use arena_alligator::{BuddyArena, BuddyGeometry};

fn main() {
    let geo = BuddyGeometry::exact(
        NonZeroUsize::new(4096).unwrap(),
        NonZeroUsize::new(512).unwrap(),
    ).unwrap();
    let _ = BuddyArena::builder(geo)
        .auto_spill()
        .hazmat_raw_access();
}
