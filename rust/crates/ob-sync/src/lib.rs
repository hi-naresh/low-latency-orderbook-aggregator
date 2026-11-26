//! `ob-sync`: lock-free publication of order books from one writer thread to many
//! reader threads.
//!
//! * [`seqlock`]: the primitive, a single-writer sequence lock over 64-bit words, written
//!   to be correct under the Rust/C++ memory model (atomics + fences), not just on x86.
//! * [`book`]: a feed publishes its book after every message, with its health; readers
//!   get the top of book (one cache line) or the full depth, always as of a whole message.
//! * [`spsc`]: a bounded single-producer single-consumer queue of fixed-size records, for
//!   handing log events from a feed thread to a logger thread without waiting.
//!
//! The crate contains no `unsafe` code, and the compiler enforces that.
#![forbid(unsafe_code)]

pub mod book;
pub mod seqlock;
pub mod spsc;

pub use book::{
    BookMeta, BookPublisher, BookReader, DEPTH, Snapshot, Top, TopOverlay, VenueStatus,
    book_channel,
};
