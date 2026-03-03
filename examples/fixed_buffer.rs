//! Fixed arena: allocate, write, freeze, send.

use std::num::NonZeroUsize;

use arena_alligator::FixedArena;
use bytes::BufMut;

fn main() {
    let arena = FixedArena::builder(
        NonZeroUsize::new(64).unwrap(),
        NonZeroUsize::new(4096).unwrap(),
    )
    .build()
    .unwrap();

    // Buffer implements BufMut.
    let mut buf = arena.allocate().unwrap();
    buf.put_slice(b"GET /index.html HTTP/1.1\r\n");
    buf.put_slice(b"Host: example.com\r\n\r\n");

    // Freeze without copying on the common path.
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
