//! Binary logging: the feed thread never formats text or writes to a file.
//!
//! An event becomes a fixed 32-byte record (four `u64` words) and is pushed onto the
//! venue's SPSC queue (`ob_sync::spsc`, ~6.5 ns). A logger thread pops records, decodes
//! them and does the slow part: formatting and I/O. If the logger falls behind and the
//! queue is full, the feed **drops the record and counts it**; the next record that fits
//! is preceded by a `Dropped(n)` record, so losses are visible, never silent.
//!
//! Record layout:
//!
//! | word | bits | meaning |
//! |---|---|---|
//! | 0 | 0..64 | monotonic time, ns ([`crate::mono_ns`]) |
//! | 1 | 0..8 | venue id |
//! | 1 | 8..16 | event tag |
//! | 1 | 16..32 | `a`: a sub-code (reason, error class, fault kind) |
//! | 1 | 32..64 | `b`: a detail code |
//! | 2, 3 | | payloads (sequence numbers, a delay in ns, a drop count) |

use std::fmt;
use std::io;
use std::time::Duration;

use ob_core::ErrorClass;
use ob_net::NetError;
use ob_net::ws::{HandshakeError, WsError};
use ob_sync::spsc::{Consumer, Producer, spsc};

use crate::driver::DriverEvent;
use crate::feed::{FeedEvent, ReconnectReason};
use crate::venue::ResyncReason;

/// Words per record.
pub const LOG_WORDS: usize = 4;
/// Records per venue queue (32 bytes each: 128 KiB). Events are rare (connects, gaps,
/// resyncs), so this absorbs any realistic burst, e.g. a reconnect storm.
pub const LOG_CAPACITY: usize = 4096;

/// A summary of a [`NetError`] that fits in a record (the error itself may own a string).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetFault {
    /// A socket error.
    Io(IoKind),
    /// TLS failed (details are in rustls's error, not kept here).
    Tls,
    /// The HTTP upgrade was refused; the HTTP status if there was one (403, 429, ...).
    Handshake(Option<u16>),
    /// The server broke the WebSocket protocol.
    Ws(WsError),
    /// The server closed the connection, with its close code if it sent one.
    Closed(Option<u16>),
    /// Connecting took too long.
    Timeout,
    /// A REST request got this HTTP status (e.g. 429: rate limited).
    Http(u16),
}

/// The socket errors worth telling apart in a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    /// The peer closed the connection without a TLS close_notify (or mid-message).
    UnexpectedEof,
    /// Reset by the peer (RST).
    ConnectionReset,
    /// Nothing listening.
    ConnectionRefused,
    /// Aborted locally.
    ConnectionAborted,
    /// Writing to a closed connection.
    BrokenPipe,
    /// An OS-level timeout.
    TimedOut,
    /// Name resolution failed.
    NotFound,
    /// Anything else.
    Other,
}

const IO_KINDS: [IoKind; 8] = [
    IoKind::UnexpectedEof,
    IoKind::ConnectionReset,
    IoKind::ConnectionRefused,
    IoKind::ConnectionAborted,
    IoKind::BrokenPipe,
    IoKind::TimedOut,
    IoKind::NotFound,
    IoKind::Other,
];
const CLASSES: [ErrorClass; 3] = [
    ErrorClass::Malformed,
    ErrorClass::SpecMismatch,
    ErrorClass::Invalid,
];
const RECONNECT_REASONS: [ReconnectReason; 7] = [
    ReconnectReason::Stale,
    ReconnectReason::SubscribeTimeout,
    ReconnectReason::SyncTimeout,
    ReconnectReason::PongTimeout,
    ReconnectReason::SubscribeRefused,
    ReconnectReason::TooManyResyncs,
    ReconnectReason::SnapshotFailed,
];
/// `WsError` variants without data; `UnknownOpcode(op)` is coded separately.
const WS_ERRORS: [WsError; 10] = [
    WsError::MaskedFrame,
    WsError::ReservedBits,
    WsError::FragmentedControl,
    WsError::ControlTooLong,
    WsError::NonMinimalLength,
    WsError::TooBig,
    WsError::UnexpectedContinuation,
    WsError::UnfinishedMessage,
    WsError::InvalidUtf8,
    WsError::BadClose,
];
const WS_UNKNOWN_OPCODE: u32 = 0x100;

/// 1-based position of `x` in `table` (0 is never a valid code, so a zeroed or corrupt
/// record decodes to `None` rather than to the first variant).
fn code_of<T: PartialEq>(table: &[T], x: &T) -> u32 {
    let i = table.iter().position(|t| t == x).unwrap_or(table.len() - 1);
    u32::try_from(i + 1).unwrap_or(0)
}
fn from_code<T: Copy>(table: &[T], code: u64) -> Option<T> {
    let i = usize::try_from(code.checked_sub(1)?).ok()?;
    table.get(i).copied()
}
/// `Option<u16>` in 32 bits: bit 16 says "present".
fn opt_u16(x: Option<u16>) -> u32 {
    x.map_or(0, |v| 0x1_0000 | u32::from(v))
}
fn from_opt_u16(b: u64) -> Option<u16> {
    (b & 0x1_0000 != 0).then(|| u16::try_from(b & 0xFFFF).unwrap_or(0))
}

impl NetFault {
    /// Summarises an error.
    pub fn of(e: &NetError) -> Self {
        match e {
            NetError::Io(e) => NetFault::Io(match e.kind() {
                io::ErrorKind::UnexpectedEof => IoKind::UnexpectedEof,
                io::ErrorKind::ConnectionReset => IoKind::ConnectionReset,
                io::ErrorKind::ConnectionRefused => IoKind::ConnectionRefused,
                io::ErrorKind::ConnectionAborted => IoKind::ConnectionAborted,
                io::ErrorKind::BrokenPipe => IoKind::BrokenPipe,
                io::ErrorKind::TimedOut => IoKind::TimedOut,
                io::ErrorKind::NotFound => IoKind::NotFound,
                _ => IoKind::Other,
            }),
            NetError::Tls(_) => NetFault::Tls,
            NetError::Handshake(HandshakeError::Status(s)) => NetFault::Handshake(Some(*s)),
            NetError::Handshake(_) => NetFault::Handshake(None),
            NetError::Ws(w) => NetFault::Ws(*w),
            NetError::Closed(c) => NetFault::Closed(*c),
            NetError::Timeout => NetFault::Timeout,
            NetError::HttpStatus(s) => NetFault::Http(*s),
        }
    }

    fn encode(self) -> (u16, u32) {
        match self {
            NetFault::Io(k) => (1, code_of(&IO_KINDS, &k)),
            NetFault::Tls => (2, 0),
            NetFault::Handshake(s) => (3, opt_u16(s)),
            NetFault::Ws(WsError::UnknownOpcode(op)) => (4, WS_UNKNOWN_OPCODE | u32::from(op)),
            NetFault::Ws(w) => (4, code_of(&WS_ERRORS, &w)),
            NetFault::Closed(c) => (5, opt_u16(c)),
            NetFault::Timeout => (6, 0),
            NetFault::Http(s) => (7, u32::from(s)),
        }
    }

    fn decode(a: u64, b: u64) -> Option<Self> {
        Some(match a {
            1 => NetFault::Io(from_code(&IO_KINDS, b)?),
            2 => NetFault::Tls,
            3 => NetFault::Handshake(from_opt_u16(b)),
            4 if b & u64::from(WS_UNKNOWN_OPCODE) != 0 => {
                NetFault::Ws(WsError::UnknownOpcode(u8::try_from(b & 0xFF).ok()?))
            }
            4 => NetFault::Ws(from_code(&WS_ERRORS, b)?),
            5 => NetFault::Closed(from_opt_u16(b)),
            6 => NetFault::Timeout,
            7 => NetFault::Http(u16::try_from(b).ok()?),
            _ => return None,
        })
    }
}

/// Everything a feed thread logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogEvent {
    /// A state change or decision of the feed.
    Feed(FeedEvent),
    /// A connection attempt failed.
    ConnectFailed(NetFault),
    /// An established connection failed.
    ConnectionLost(NetFault),
    /// This many records were dropped because the queue was full.
    Dropped(u64),
    /// A REST snapshot fetch failed (full depth).
    SnapshotFailed(NetFault),
}

impl From<DriverEvent<'_>> for LogEvent {
    fn from(e: DriverEvent<'_>) -> Self {
        match e {
            DriverEvent::Feed(f) => LogEvent::Feed(f),
            DriverEvent::ConnectFailed(e) => LogEvent::ConnectFailed(NetFault::of(e)),
            DriverEvent::ConnectionLost(e) => LogEvent::ConnectionLost(NetFault::of(e)),
            DriverEvent::SnapshotFailed(e) => LogEvent::SnapshotFailed(NetFault::of(e)),
        }
    }
}

/// One log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// When ([`crate::mono_ns`]).
    pub t_ns: u64,
    /// Which venue (an id the application assigns).
    pub venue: u8,
    /// What happened.
    pub event: LogEvent,
}

impl LogRecord {
    /// The record as four words.
    pub fn encode(&self) -> [u64; LOG_WORDS] {
        let (tag, a, b, w2, w3): (u8, u16, u32, u64, u64) = match self.event {
            LogEvent::Feed(f) => match f {
                FeedEvent::Connected => (1, 0, 0, 0, 0),
                FeedEvent::Live => (2, 0, 0, 0, 0),
                FeedEvent::Gap => (3, 0, 0, 0, 0),
                FeedEvent::Rejected(c) => (4, 0, code_of(&CLASSES, &c), 0, 0),
                FeedEvent::Resync(r) => match r {
                    ResyncReason::Gap { expected, got } => (5, 1, 0, expected, got),
                    ResyncReason::Crossed => (5, 2, 0, 0, 0),
                    ResyncReason::RemovedAbsent => (5, 3, 0, 0, 0),
                    ResyncReason::BadSnapshot => (5, 4, 0, 0, 0),
                    ResyncReason::Unparseable(c) => (5, 5, code_of(&CLASSES, &c), 0, 0),
                    ResyncReason::Depleted => (5, 6, 0, 0, 0),
                },
                FeedEvent::Reconnecting(r) => (6, 0, code_of(&RECONNECT_REASONS, &r), 0, 0),
                FeedEvent::Disconnected { retry_in } => {
                    let ns = u64::try_from(retry_in.as_nanos()).unwrap_or(u64::MAX);
                    (7, 0, 0, ns, 0)
                }
                FeedEvent::FetchingSnapshot => (11, 0, 0, 0, 0),
            },
            LogEvent::ConnectFailed(f) => {
                let (a, b) = f.encode();
                (8, a, b, 0, 0)
            }
            LogEvent::ConnectionLost(f) => {
                let (a, b) = f.encode();
                (9, a, b, 0, 0)
            }
            LogEvent::Dropped(n) => (10, 0, 0, n, 0),
            LogEvent::SnapshotFailed(f) => {
                let (a, b) = f.encode();
                (12, a, b, 0, 0)
            }
        };
        let w1 =
            u64::from(self.venue) | u64::from(tag) << 8 | u64::from(a) << 16 | u64::from(b) << 32;
        [self.t_ns, w1, w2, w3]
    }

    /// Decodes a record; `None` if it isn't one this version wrote.
    pub fn decode(w: [u64; LOG_WORDS]) -> Option<Self> {
        let [t_ns, w1, w2, w3] = w;
        let venue = u8::try_from(w1 & 0xFF).ok()?;
        let (tag, a, b) = ((w1 >> 8) & 0xFF, (w1 >> 16) & 0xFFFF, w1 >> 32);
        let feed = |f| Some(LogEvent::Feed(f));
        let event = match tag {
            1 => feed(FeedEvent::Connected),
            2 => feed(FeedEvent::Live),
            3 => feed(FeedEvent::Gap),
            4 => feed(FeedEvent::Rejected(from_code(&CLASSES, b)?)),
            5 => feed(FeedEvent::Resync(match a {
                1 => ResyncReason::Gap {
                    expected: w2,
                    got: w3,
                },
                2 => ResyncReason::Crossed,
                3 => ResyncReason::RemovedAbsent,
                4 => ResyncReason::BadSnapshot,
                5 => ResyncReason::Unparseable(from_code(&CLASSES, b)?),
                6 => ResyncReason::Depleted,
                _ => return None,
            })),
            6 => feed(FeedEvent::Reconnecting(from_code(&RECONNECT_REASONS, b)?)),
            7 => feed(FeedEvent::Disconnected {
                retry_in: Duration::from_nanos(w2),
            }),
            8 => Some(LogEvent::ConnectFailed(NetFault::decode(a, b)?)),
            9 => Some(LogEvent::ConnectionLost(NetFault::decode(a, b)?)),
            10 => Some(LogEvent::Dropped(w2)),
            11 => feed(FeedEvent::FetchingSnapshot),
            12 => Some(LogEvent::SnapshotFailed(NetFault::decode(a, b)?)),
            _ => None,
        }?;
        Some(LogRecord { t_ns, venue, event })
    }
}

impl fmt::Display for LogEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogEvent::Feed(FeedEvent::Disconnected { retry_in }) => {
                write!(f, "disconnected; retrying in {} ms", retry_in.as_millis())
            }
            LogEvent::Feed(e) => write!(f, "{e:?}"),
            LogEvent::ConnectFailed(e) => write!(f, "connect failed: {e:?}"),
            LogEvent::ConnectionLost(e) => write!(f, "connection lost: {e:?}"),
            LogEvent::Dropped(n) => write!(f, "{n} log records dropped (logger too slow)"),
            LogEvent::SnapshotFailed(e) => write!(f, "snapshot fetch failed: {e:?}"),
        }
    }
}

/// The feed thread's end: never blocks, never allocates.
pub struct LogWriter {
    queue: Producer<LOG_WORDS, LOG_CAPACITY>,
    venue: u8,
    dropped: u64,
}

/// The logger thread's end.
pub struct LogReader {
    queue: Consumer<LOG_WORDS, LOG_CAPACITY>,
}

/// A log queue for one venue.
pub fn log_channel(venue: u8) -> (LogWriter, LogReader) {
    let (queue, consumer) = spsc();
    (
        LogWriter {
            queue,
            venue,
            dropped: 0,
        },
        LogReader { queue: consumer },
    )
}

impl LogWriter {
    /// Records `event` at time `t_ns`. If the queue is full the record is dropped and
    /// counted; the count is logged as soon as there is room again.
    #[inline]
    pub fn log(&mut self, t_ns: u64, event: LogEvent) {
        if self.dropped > 0 {
            let note = LogRecord {
                t_ns,
                venue: self.venue,
                event: LogEvent::Dropped(self.dropped),
            };
            if !self.queue.try_push(&note.encode()) {
                self.dropped += 1;
                return;
            }
            self.dropped = 0;
        }
        let record = LogRecord {
            t_ns,
            venue: self.venue,
            event,
        };
        if !self.queue.try_push(&record.encode()) {
            self.dropped += 1;
        }
    }

    /// Records dropped and not yet reported.
    pub fn pending_drops(&self) -> u64 {
        self.dropped
    }
}

impl LogReader {
    /// The next record, if any. A record that doesn't decode (impossible unless the
    /// format changes) is skipped.
    pub fn pop(&mut self) -> Option<LogRecord> {
        loop {
            let words = self.queue.pop()?;
            if let Some(r) = LogRecord::decode(words) {
                return Some(r);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn every_event() -> Vec<LogEvent> {
        let mut faults: Vec<NetFault> = IO_KINDS.iter().map(|k| NetFault::Io(*k)).collect();
        faults.extend([
            NetFault::Tls,
            NetFault::Handshake(None),
            NetFault::Handshake(Some(403)),
            NetFault::Closed(None),
            NetFault::Closed(Some(1000)),
            NetFault::Timeout,
            NetFault::Http(429),
            NetFault::Http(u16::MAX),
            NetFault::Ws(WsError::UnknownOpcode(0)),
            NetFault::Ws(WsError::UnknownOpcode(0xF)),
        ]);
        faults.extend(WS_ERRORS.iter().map(|w| NetFault::Ws(*w)));
        let mut feed = vec![
            FeedEvent::Connected,
            FeedEvent::Live,
            FeedEvent::Gap,
            FeedEvent::FetchingSnapshot,
            FeedEvent::Disconnected {
                retry_in: Duration::from_millis(1234),
            },
        ];
        for c in CLASSES {
            feed.push(FeedEvent::Rejected(c));
            feed.push(FeedEvent::Resync(ResyncReason::Unparseable(c)));
        }
        feed.extend(RECONNECT_REASONS.map(FeedEvent::Reconnecting));
        feed.extend(
            [
                ResyncReason::Gap {
                    expected: u64::MAX,
                    got: 7,
                },
                ResyncReason::Crossed,
                ResyncReason::RemovedAbsent,
                ResyncReason::BadSnapshot,
                ResyncReason::Depleted,
            ]
            .map(FeedEvent::Resync),
        );
        let mut all: Vec<LogEvent> = feed.into_iter().map(LogEvent::Feed).collect();
        for f in faults {
            all.push(LogEvent::ConnectFailed(f));
            all.push(LogEvent::ConnectionLost(f));
            all.push(LogEvent::SnapshotFailed(f));
        }
        all.push(LogEvent::Dropped(u64::MAX));
        all
    }

    #[test]
    fn every_event_round_trips() {
        let events = every_event();
        assert!(events.len() > 60);
        for (i, event) in events.into_iter().enumerate() {
            let r = LogRecord {
                t_ns: u64::MAX - i as u64,
                venue: 255,
                event,
            };
            assert_eq!(LogRecord::decode(r.encode()), Some(r), "{event:?}");
        }
    }

    #[test]
    fn zeroed_or_unknown_records_are_rejected() {
        assert_eq!(LogRecord::decode([0; 4]), None);
        assert_eq!(LogRecord::decode([1, 99 << 8, 0, 0]), None); // unknown tag
        assert_eq!(LogRecord::decode([1, 4 << 8, 0, 0]), None); // Rejected, class 0
    }

    #[test]
    fn a_full_queue_drops_and_then_reports_the_count() {
        let (mut w, mut r) = log_channel(3);
        for t in 0..LOG_CAPACITY as u64 + 5 {
            w.log(t, LogEvent::Feed(FeedEvent::Gap));
        }
        assert_eq!(w.pending_drops(), 5);
        // The logger catches up a little; the next log reports the loss first.
        for _ in 0..10 {
            r.pop().unwrap();
        }
        w.log(9999, LogEvent::Feed(FeedEvent::Live));
        let rest: Vec<LogRecord> = std::iter::from_fn(|| r.pop()).collect();
        assert_eq!(rest.len(), LOG_CAPACITY - 10 + 2);
        let n = rest.len();
        assert_eq!(rest[n - 2].event, LogEvent::Dropped(5));
        assert_eq!(rest[n - 1].event, LogEvent::Feed(FeedEvent::Live));
        assert!(rest.iter().all(|r| r.venue == 3));
        assert_eq!(w.pending_drops(), 0);
    }
}
