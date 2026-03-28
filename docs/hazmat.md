# Hazmat Raw Access

`hazmat-raw-access` is the explicit escape hatch for callers that need direct
pointer access to arena memory.

Enable the crate feature, then opt in per builder with
`.hazmat_raw_access()`. That produces `RawFixedArena` or `RawBuddyArena`
wrappers instead of the default safe write path.

## What changes

- `raw_alloc()` returns a `RawRegion` instead of a `Buffer`
- writes happen through raw pointers or `MaybeUninit<u8>` slices
- `unsafe freeze(range)` turns an initialized byte range into ordinary `Bytes`

The returned `Bytes` keeps the backing slot or buddy block alive until the last
clone or slice drops.

## Sharp edges

- `freeze(range)` is `unsafe` because the crate cannot verify that every byte in
  `range` was initialized before freezing
- `freeze(range)` validates against visible capacity, not allocator slack
- `Bytes` clones and slices retain arena backing just like the normal freeze
  path
- `auto_spill()` and `hazmat_raw_access()` are mutually exclusive at compile
  time

## Fixed arena behavior

`RawFixedArena::raw_alloc()` exposes the full slot capacity.

With `InitPolicy::Zero`, the slot is cleared before it is returned. With
`InitPolicy::Uninit`, previously used bytes may still be present and are not
safe to read until you initialize them.

## Buddy arena behavior

`RawBuddyArena::raw_alloc(len)` still allocates a power-of-two block under the
hood, but visible capacity follows the arena geometry:

- `BuddyGeometry::exact(...)` exposes the full allocated block capacity
- `BuddyGeometry::nearest(...)` caps visible capacity to the requested `len`

That keeps raw freeze semantics aligned with the existing buddy buffer API: the
caller cannot freeze allocator slack that was never part of the visible request.

## Example paths

- [`examples/hazmat_fixed_raw.rs`](../examples/hazmat_fixed_raw.rs)
- [`examples/hazmat_buddy_raw.rs`](../examples/hazmat_buddy_raw.rs)
