//! Record and replay: capture exactly what each venue sent, then run it again through
//! the same feed code, as fast as possible and deterministically.
//!
//! **Why.** Live data never repeats, so a live run can't be compared with another one.
//! A recording can: replay it to measure the parse → apply → publish path without
//! network noise, to compare two builds on identical input, or to reproduce a bug from
//! production byte for byte.
//!
//! **File format** (little-endian):
//!
//! ```text
//! header:  b"OBREC" 0 0 1                                  8 bytes (format version 1)
//! record:  wall_ns u64 | mono_ns u64 | venue u8 | kind u8 | 0 0 | len u32 | len bytes
//! ```
//! * `wall_ns`: Unix time when received (for the exchange → us delay on replay).
//! * `mono_ns`: [`crate::mono_ns`] when received (intervals, and the feed's `now`).
//! * `kind`: 0 = a text message, 1 = a (re)connection, so replay sees reconnects too,
//!   2 = a REST snapshot body (full depth), so replay rebuilds books exactly.

use std::io::{self, Read, Write};

use crate::feed::{Command, Commands, Feed, FeedConfig, FeedStats};
use crate::probe::{StageProbe, WallClock};
use crate::venue::Venue;
use ob_sync::BookPublisher;

const MAGIC: [u8; 8] = *b"OBREC\0\0\x01";
const HEADER: usize = 24;
/// Refuse records larger than this (a corrupt length would otherwise allocate GBs).
pub const MAX_RECORD: usize = 16 << 20;

/// What a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A WebSocket text message.
    Text,
    /// The connection was (re)established.
    Connected,
    /// A REST snapshot body.
    Snapshot,
}

/// One recorded event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// Unix ns when received.
    pub wall_ns: u64,
    /// Monotonic ns when received.
    pub mono_ns: u64,
    /// Venue id (assigned by the application).
    pub venue: u8,
    /// What it is.
    pub kind: Kind,
    /// The message (empty for `Connected`).
    pub text: &'a [u8],
}

/// Writes a recording.
pub struct RecordWriter<W: Write> {
    out: W,
    records: u64,
}

impl<W: Write> RecordWriter<W> {
    /// Starts a recording (writes the header).
    pub fn new(mut out: W) -> io::Result<Self> {
        out.write_all(&MAGIC)?;
        Ok(RecordWriter { out, records: 0 })
    }

    /// Appends one record.
    pub fn write(&mut self, frame: &Frame<'_>) -> io::Result<()> {
        let len = u32::try_from(frame.text.len())
            .ok()
            .filter(|&l| l as usize <= MAX_RECORD)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "record too large"))?;
        let mut h = [0_u8; HEADER];
        h[0..8].copy_from_slice(&frame.wall_ns.to_le_bytes());
        h[8..16].copy_from_slice(&frame.mono_ns.to_le_bytes());
        h[16] = frame.venue;
        h[17] = match frame.kind {
            Kind::Text => 0,
            Kind::Connected => 1,
            Kind::Snapshot => 2,
        };
        h[20..24].copy_from_slice(&len.to_le_bytes());
        self.out.write_all(&h)?;
        self.out.write_all(frame.text)?;
        self.records += 1;
        Ok(())
    }

    /// Records written so far.
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Flushes and returns the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Reads a recording, one record at a time, into a reused buffer.
pub struct RecordReader<R: Read> {
    input: R,
    buf: Vec<u8>,
}

impl<R: Read> RecordReader<R> {
    /// Checks the header.
    pub fn new(mut input: R) -> io::Result<Self> {
        let mut magic = [0_u8; 8];
        input.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an ob recording (or a different format version)",
            ));
        }
        Ok(RecordReader {
            input,
            buf: Vec::with_capacity(64 * 1024),
        })
    }

    /// The next record; `Ok(None)` at a clean end of file. (Not an `Iterator`: the frame
    /// borrows the reader's buffer, which the next call reuses.) A file cut off mid-record
    /// (the recorder was killed) gives `UnexpectedEof`.
    pub fn read_frame(&mut self) -> io::Result<Option<Frame<'_>>> {
        let mut h = [0_u8; HEADER];
        // Distinguish a clean end (0 bytes) from a truncated header.
        let mut got = 0;
        while got < HEADER {
            match self.input.read(&mut h[got..])? {
                0 if got == 0 => return Ok(None),
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => got += n,
            }
        }
        let word = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap_or([0; 8]));
        let len = u32::from_le_bytes(h[20..24].try_into().unwrap_or([0; 4])) as usize;
        if len > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "record too large",
            ));
        }
        let kind = match h[17] {
            0 => Kind::Text,
            1 => Kind::Connected,
            2 => Kind::Snapshot,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown record kind",
                ));
            }
        };
        self.buf.resize(len, 0);
        self.input.read_exact(&mut self.buf)?;
        Ok(Some(Frame {
            wall_ns: word(0),
            mono_ns: word(8),
            venue: h[16],
            kind,
            text: &self.buf,
        }))
    }
}

/// Anything that can be fed recorded frames (one feed per venue, behind `dyn`).
pub trait Replay {
    /// Feeds one recorded frame.
    fn replay(&mut self, frame: &Frame<'_>);
    /// The feed's counters.
    fn stats(&self) -> FeedStats;
    /// The stage timings.
    fn probe(&self) -> &StageProbe;
    /// The venue's name.
    fn name(&self) -> &'static str;
}

/// A feed driven by a recording instead of a socket: the same state machine, parser,
/// book and publisher as live; only the source of bytes and of time differ.
pub struct ReplayFeed<V: Venue> {
    feed: Feed<V, StageProbe>,
    cmds: Commands,
}

impl<V: Venue> ReplayFeed<V> {
    /// A feed for `venue`, publishing to `publisher`.
    pub fn new(venue: V, publisher: BookPublisher) -> Self {
        ReplayFeed {
            feed: Feed::with_probe(
                venue,
                publisher,
                FeedConfig::default(),
                0,
                StageProbe::new(WallClock::Fixed(0)),
            ),
            cmds: Commands::default(),
        }
    }
}

impl<V: Venue> Replay for ReplayFeed<V> {
    fn replay(&mut self, frame: &Frame<'_>) {
        match frame.kind {
            Kind::Connected => {
                // A reconnect live is a disconnect followed by a connect.
                self.feed.on_disconnected(frame.mono_ns, &mut self.cmds);
                self.feed.on_connected(frame.mono_ns, &mut self.cmds);
            }
            Kind::Text => {
                self.feed.probe_mut().wall = WallClock::Fixed(frame.wall_ns);
                self.feed.on_text(frame.mono_ns, frame.text, &mut self.cmds);
            }
            // The snapshot the live feed fetched, at the point it arrived.
            Kind::Snapshot => self
                .feed
                .on_snapshot(frame.mono_ns, frame.text, &mut self.cmds),
        }
        // What the driver would do; requests (subscribe, ping) have nowhere to go, and a
        // snapshot fetch is answered by the recorded snapshot that follows.
        while let Some(cmd) = self.cmds.pop() {
            if cmd == Command::Reconnect {
                self.feed.on_disconnected(frame.mono_ns, &mut self.cmds);
            }
        }
    }
    fn stats(&self) -> FeedStats {
        self.feed.stats()
    }
    fn probe(&self) -> &StageProbe {
        self.feed.probe()
    }
    fn name(&self) -> &'static str {
        self.feed.venue().name()
    }
}
