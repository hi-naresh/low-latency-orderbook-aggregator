//! Two real threads, millions of records: every record arrives whole, in order, exactly
//! once; drops happen only when the queue is full, and are fully accounted for.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::thread;

use ob_sync::spsc::spsc;

#[test]
fn millions_of_records_cross_threads_intact() {
    let n: u64 = if cfg!(miri) { 2_000 } else { 2_000_000 };
    let (mut p, mut c) = spsc::<4, 1024>();
    let producer = thread::spawn(move || {
        let mut dropped = 0_u64;
        for i in 0..n {
            // A record is checkable on its own: words derived from its index.
            let r = [i, !i, i.wrapping_mul(0x9E37_79B9_7F4A_7C15), i ^ 0xAAAA];
            while !p.try_push(&r) {
                dropped += 1; // full: count it, then retry (this test wants every record)
                std::hint::spin_loop();
            }
        }
        dropped
    });
    let mut next = 0_u64;
    while next < n {
        if let Some(r) = c.pop() {
            assert_eq!(
                r,
                [
                    next,
                    !next,
                    next.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    next ^ 0xAAAA
                ],
                "record {next} torn or out of order"
            );
            next += 1;
        }
    }
    let full_events = producer.join().unwrap();
    assert_eq!(c.pop(), None, "nothing extra");
    println!("producer found the queue full {full_events} times");
}
