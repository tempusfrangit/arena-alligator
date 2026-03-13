//! Fixed arena: allocate, write, freeze, send.

use std::num::NonZeroUsize;

use arena_alligator::FixedArena;
use bytes::BufMut;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn main() {
    let arena = FixedArena::with_slot_capacity(nz(64), nz(4096))
        .build()
        .unwrap();

    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"GET /index.html HTTP/1.1\r\n");
    buf.put_slice(b"Host: example.com\r\n\r\n");

    let bytes = buf.freeze();

    let handle = std::thread::spawn(move || {
        assert!(bytes.starts_with(b"GET"));
        println!("sent {} bytes to worker", bytes.len());
    });

    handle.join().unwrap();

    let m = arena.metrics();
    println!(
        "allocations: {}, frees: {}, bytes_live: {}",
        m.allocations_ok, m.frees, m.bytes_live
    );
}
