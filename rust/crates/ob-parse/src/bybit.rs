//! Bybit v5 public linear order book stream (`orderbook.<depth>.<symbol>`).
//!
//! Real messages (recorded, trimmed):
//! ```json
//! {"topic":"orderbook.50.BTCUSDT","type":"snapshot","ts":1795610797394,
//!  "data":{"s":"BTCUSDT","b":[["58656.20","0.835"], ...49 more],"a":[...50],
//!          "u":22042290,"seq":855165474291},"cts":1795610797392}
//! {"topic":"orderbook.50.BTCUSDT","type":"delta","ts":1795610797394,
//!  "data":{"s":"BTCUSDT","b":[],"a":[["58656.30","6.140"]],"u":22042291,"seq":855165474295},
//!  "cts":1795610797392}
//! {"success":true,"ret_msg":"","conn_id":"…","req_id":"depth","op":"subscribe"}
//! ```
//!
//! **Two kinds of book message** (unlike Binance, where every message is a snapshot):
//! * `snapshot`: the complete top N. Replaces the book. Sent first after subscribing,
//!   and again whenever Bybit decides our copy needs a reset.
//! * `delta`: only the levels that changed. Quantity `"0"` deletes a level; a new price
//!   inserts one; an existing price is overwritten. Bybit also sends deletes for levels
//!   that fall out of the top N (in the recording, replaying deltas never deleted a
//!   missing level and the book stayed at exactly 50 per side).
//! * A delta with `u == 1` means Bybit's service restarted: **treat it as a snapshot**
//!   (Bybit documentation).
//!
//! **Sequencing.** `u` rose by exactly 1 per message in all 58 recorded pairs, so any
//! other step is a gap: the book is now wrong and must be rebuilt from a new snapshot.
//! See [`BookMsg::sequence`].
//!
//! **Message rules**, implemented identically by this parser and by the serde_json oracle
//! in `tests/support/bybit_oracle.rs`:
//! * One JSON object; fields in any order; unknown fields skipped but validated. Same for
//!   the `data` object.
//! * Known top-level fields (`topic type ts cts data success ret_msg conn_id req_id op`)
//!   and known `data` fields (`s b a u seq`) may appear at most once each, and are
//!   type-checked wherever they appear: `ts cts u seq` non-negative integers; `topic type
//!   ret_msg conn_id req_id op s` strings; `success` a boolean; `b a` arrays of
//!   `[price, quantity]` string pairs valid at the instrument's scale, tick and lot.
//! * With `topic`: a book message. `topic` must be the configured topic, `type` one of
//!   `snapshot`/`delta`, `data.s` the configured symbol, and `type ts data data.s data.b
//!   data.a data.u data.seq` all present. `cts` is optional (it's only used for latency
//!   metrics; its absence shouldn't cost us book data) → [`Message::Book`].
//! * Without `topic` but with `op`: a reply to our request → [`Message::Response`]
//!   (`success` required).
//! * Anything else → [`ErrorKind::UnknownMessage`].
//!
//! On error, the output buffers hold partial data and must not be used.

use ob_core::{ErrorClass, InstrumentSpec, LevelChange, OrderBook, Side, SnapshotError};

use crate::json::{Cursor, JsonError, JsonErrorKind};
use crate::levels::{LevelBuf, LevelsError, parse_levels};

/// Snapshot or delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    /// Complete book: replaces ours.
    #[default]
    Snapshot,
    /// Changes only: applied level by level.
    Delta,
}

/// Which subscription a book message belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Channel {
    /// The depth topic (e.g. `orderbook.50.BTCUSDT`).
    #[default]
    Depth,
    /// The top-of-book topic (`orderbook.1.BTCUSDT`): one level per side, every
    /// message a snapshot, pushed every 10 ms. Its `seq` is comparable with depth's.
    Top,
}

/// One parsed book message. Reuse a single instance for every message.
#[derive(Clone, Debug, Default)]
pub struct BookMsg {
    /// Which topic it came from.
    pub channel: Channel,
    /// `type`.
    pub kind: Kind,
    /// `ts`: when Bybit's system sent it, ms since the epoch.
    pub ts: u64,
    /// `cts`: when the matching engine produced it, ms (optional field).
    pub cts: Option<u64>,
    /// `data.u`: update ID, +1 per message.
    pub update_id: u64,
    /// `data.seq`: cross-sequence number (comparable across Bybit channels).
    pub seq: u64,
    /// `data.b`: bid levels (best first in snapshots; any order in deltas).
    pub bids: LevelBuf,
    /// `data.a`: ask levels.
    pub asks: LevelBuf,
}

/// How a book message relates to the last one we applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sequence {
    /// A snapshot, or a delta with `u == 1` (service restart): apply as a fresh book.
    Reset,
    /// The next delta (`u == last + 1`): apply it.
    Next,
    /// One or more messages were missed: drop the book and resubscribe for a snapshot.
    Gap {
        /// The `u` we expected.
        expected: u64,
    },
    /// Older than or equal to what we have (a duplicate or replay): ignore it.
    Stale,
    /// A delta before any snapshot: nothing to apply it to; wait for a snapshot.
    NoBaseline,
}

/// What applying a message did, for metrics and desync detection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Applied {
    /// Deletes for prices not in our book. Zero in a healthy stream; non-zero suggests
    /// our book has drifted from Bybit's.
    pub removed_absent: u16,
    /// Inserts that didn't fit (worse than every level of a full side).
    pub out_of_depth: u16,
}

impl BookMsg {
    /// `true` if this message replaces the book (a snapshot, or `u == 1`).
    pub const fn is_reset(&self) -> bool {
        matches!(self.kind, Kind::Snapshot) || self.update_id == 1
    }

    /// Classifies this message against the last applied update ID (`None` = no book yet).
    pub const fn sequence(&self, last_update_id: Option<u64>) -> Sequence {
        if self.is_reset() {
            return Sequence::Reset;
        }
        match last_update_id {
            None => Sequence::NoBaseline,
            Some(last) if self.update_id <= last => Sequence::Stale,
            Some(last) if self.update_id == last + 1 => Sequence::Next,
            Some(last) => Sequence::Gap { expected: last + 1 },
        }
    }

    /// Applies this message to `book`: a reset replaces it (validated, all-or-nothing),
    /// a delta updates it level by level.
    ///
    /// Check [`BookMsg::sequence`] first; this method doesn't. After a delta, the caller
    /// should also check `book.is_crossed()`: a crossed single-venue book means our copy
    /// is wrong and needs a snapshot.
    pub fn apply_to<const CAP: usize>(
        &self,
        book: &mut OrderBook<CAP>,
    ) -> Result<Applied, SnapshotError> {
        if self.is_reset() {
            book.replace(&self.bids, &self.asks)?;
            return Ok(Applied::default());
        }
        let mut applied = Applied::default();
        for (side, levels) in [(Side::Bid, &self.bids), (Side::Ask, &self.asks)] {
            for level in levels.iter() {
                match book.apply(side, *level) {
                    LevelChange::RemovedAbsent => applied.removed_absent += 1,
                    LevelChange::OutOfDepth => applied.out_of_depth += 1,
                    _ => {}
                }
            }
        }
        Ok(applied)
    }
}

/// A reply's operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Reply to `subscribe`.
    Subscribe,
    /// Reply to `unsubscribe`.
    Unsubscribe,
    /// Reply to our `{"op":"ping"}` heartbeat (Bybit answers with op `ping` or `pong`).
    Pong,
    /// Any other operation.
    Other,
}

/// What a message was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// A book message, written into the caller's [`BookMsg`].
    Book,
    /// A reply to a request we sent.
    Response {
        /// Which request.
        op: Op,
        /// `false` if Bybit rejected it.
        success: bool,
    },
}

/// A field of the message, for error reports (top level, then inside `data`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// `topic`
    Topic,
    /// `type`
    Type,
    /// `ts`
    Ts,
    /// `cts`
    Cts,
    /// `data`
    Data,
    /// `success`
    Success,
    /// `ret_msg`
    RetMsg,
    /// `conn_id`
    ConnId,
    /// `req_id`
    ReqId,
    /// `op`
    Op,
    /// `data.s`
    Symbol,
    /// `data.b`
    Bids,
    /// `data.a`
    Asks,
    /// `data.u`
    UpdateId,
    /// `data.seq`
    Seq,
}

impl Field {
    fn top(key: &[u8]) -> Option<Field> {
        Some(match key {
            b"topic" => Field::Topic,
            b"type" => Field::Type,
            b"ts" => Field::Ts,
            b"cts" => Field::Cts,
            b"data" => Field::Data,
            b"success" => Field::Success,
            b"ret_msg" => Field::RetMsg,
            b"conn_id" => Field::ConnId,
            b"req_id" => Field::ReqId,
            b"op" => Field::Op,
            _ => return None,
        })
    }

    fn data(key: &[u8]) -> Option<Field> {
        Some(match key {
            b"s" => Field::Symbol,
            b"b" => Field::Bids,
            b"a" => Field::Asks,
            b"u" => Field::UpdateId,
            b"seq" => Field::Seq,
            _ => return None,
        })
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// Why a message was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    /// Not valid JSON, or a value of the wrong type (in `field`, if known).
    Json(JsonErrorKind, Option<Field>),
    /// A known field appeared twice.
    DuplicateField(Field),
    /// A required field is missing.
    MissingField(Field),
    /// `topic` is not the one we subscribed to.
    WrongTopic,
    /// `type` is neither `snapshot` nor `delta`.
    WrongType,
    /// `data.s` is not our symbol.
    WrongSymbol,
    /// A level that isn't a `[price, quantity]` pair of strings.
    BadLevel(Field),
    /// More than [`crate::levels::MAX_LEVELS`] levels on one side.
    TooManyLevels(Field),
    /// Neither a book message nor a response.
    UnknownMessage,
}

/// A rejected message: what, and where (byte offset).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    /// What went wrong.
    pub kind: ErrorKind,
    /// Byte offset in the message.
    pub offset: usize,
}

impl Error {
    /// Which policy applies. Whatever the class, the message is dropped whole,
    /// and because Bybit sends deltas, the book must then be rebuilt from a snapshot.
    pub const fn class(&self) -> ErrorClass {
        match self.kind {
            ErrorKind::Json(JsonErrorKind::Decimal(e), _) => e.class(),
            _ => ErrorClass::Malformed,
        }
    }
}

/// Parses Bybit order book messages for one instrument and topic.
#[derive(Clone, Copy, Debug)]
pub struct BybitBookParser<'a> {
    spec: InstrumentSpec,
    topic: &'a [u8],
    top_topic: Option<&'a [u8]>,
    symbol: &'a [u8],
}

const REQUIRED_TOP: [Field; 3] = [Field::Type, Field::Ts, Field::Data];
const REQUIRED_DATA: [Field; 5] = [
    Field::Symbol,
    Field::Bids,
    Field::Asks,
    Field::UpdateId,
    Field::Seq,
];

impl<'a> BybitBookParser<'a> {
    /// A parser for `topic` (e.g. `b"orderbook.50.BTCUSDT"`) and `symbol` (`b"BTCUSDT"`),
    /// with the instrument's scale, tick and lot.
    pub const fn new(spec: InstrumentSpec, topic: &'a [u8], symbol: &'a [u8]) -> Self {
        BybitBookParser {
            spec,
            topic,
            top_topic: None,
            symbol,
        }
    }

    /// Also accepts `top_topic` (e.g. `b"orderbook.1.BTCUSDT"`), reported as
    /// [`Channel::Top`].
    #[must_use]
    pub const fn with_top_topic(mut self, top_topic: &'a [u8]) -> Self {
        self.top_topic = Some(top_topic);
        self
    }

    /// Parses one WebSocket text frame. For [`Message::Book`], the result is in `out`.
    pub fn parse(&self, msg: &[u8], out: &mut BookMsg) -> Result<Message, Error> {
        let mut c = Cursor::new(msg);
        let mut seen: u32 = 0;
        let mut channel: Option<Channel> = None;
        let mut kind: Option<Kind> = None;
        let mut symbol_ok = false;
        let mut success = false;
        let mut op = Op::Other;
        out.bids.clear();
        out.asks.clear();
        out.cts = None;

        c.expect(b'{').map_err(|e| json(e, None))?;
        if c.has_first(b'}').map_err(|e| json(e, None))? {
            loop {
                let key_at = c.pos();
                let key = c.key_colon().map_err(|e| json(e, None))?;
                match Field::top(key) {
                    None => c.skip_value().map_err(|e| json(e, None))?,
                    Some(field) => {
                        see(&mut seen, field, key_at)?;
                        let f = Some(field);
                        match field {
                            Field::Topic => {
                                let t = c.string().map_err(|e| json(e, f))?;
                                channel = if t == self.topic {
                                    Some(Channel::Depth)
                                } else if self.top_topic == Some(t) {
                                    Some(Channel::Top)
                                } else {
                                    None
                                };
                            }
                            Field::Type => {
                                kind = match c.string().map_err(|e| json(e, f))? {
                                    b"snapshot" => Some(Kind::Snapshot),
                                    b"delta" => Some(Kind::Delta),
                                    _ => None,
                                };
                            }
                            Field::Ts => out.ts = c.u64().map_err(|e| json(e, f))?,
                            Field::Cts => out.cts = Some(c.u64().map_err(|e| json(e, f))?),
                            Field::Data => self.data(&mut c, out, &mut seen, &mut symbol_ok)?,
                            Field::Success => success = c.boolean().map_err(|e| json(e, f))?,
                            Field::Op => {
                                op = match c.string().map_err(|e| json(e, f))? {
                                    b"subscribe" => Op::Subscribe,
                                    b"unsubscribe" => Op::Unsubscribe,
                                    b"ping" | b"pong" => Op::Pong,
                                    _ => Op::Other,
                                };
                            }
                            Field::RetMsg | Field::ConnId | Field::ReqId => {
                                c.string().map_err(|e| json(e, f))?;
                            }
                            // Data fields only appear inside `data`.
                            Field::Symbol
                            | Field::Bids
                            | Field::Asks
                            | Field::UpdateId
                            | Field::Seq => {}
                        }
                    }
                }
                if !c.next_or_close(b'}').map_err(|e| json(e, None))? {
                    break;
                }
            }
        }
        c.end().map_err(|e| json(e, None))?;

        let end = c.pos();
        let missing = |fields: &[Field]| fields.iter().copied().find(|f| seen & f.bit() == 0);
        if seen & Field::Topic.bit() != 0 {
            // A missing `data` is reported as `Data`, before any of its fields.
            if let Some(field) = missing(&REQUIRED_TOP).or_else(|| missing(&REQUIRED_DATA)) {
                return Err(Error {
                    kind: ErrorKind::MissingField(field),
                    offset: end,
                });
            }
            let Some(channel) = channel else {
                return Err(Error {
                    kind: ErrorKind::WrongTopic,
                    offset: end,
                });
            };
            let Some(kind) = kind else {
                return Err(Error {
                    kind: ErrorKind::WrongType,
                    offset: end,
                });
            };
            if !symbol_ok {
                return Err(Error {
                    kind: ErrorKind::WrongSymbol,
                    offset: end,
                });
            }
            out.kind = kind;
            out.channel = channel;
            Ok(Message::Book)
        } else if seen & Field::Op.bit() != 0 {
            if seen & Field::Success.bit() == 0 {
                return Err(Error {
                    kind: ErrorKind::MissingField(Field::Success),
                    offset: end,
                });
            }
            Ok(Message::Response { op, success })
        } else {
            Err(Error {
                kind: ErrorKind::UnknownMessage,
                offset: end,
            })
        }
    }

    /// The `data` object.
    fn data(
        &self,
        c: &mut Cursor<'_>,
        out: &mut BookMsg,
        seen: &mut u32,
        symbol_ok: &mut bool,
    ) -> Result<(), Error> {
        let d = Some(Field::Data);
        c.expect(b'{').map_err(|e| json(e, d))?;
        if !c.has_first(b'}').map_err(|e| json(e, d))? {
            return Ok(());
        }
        loop {
            let key_at = c.pos();
            let key = c.key_colon().map_err(|e| json(e, d))?;
            match Field::data(key) {
                None => c.skip_value().map_err(|e| json(e, d))?,
                Some(field) => {
                    see(seen, field, key_at)?;
                    let f = Some(field);
                    match field {
                        Field::Symbol => {
                            *symbol_ok = c.string().map_err(|e| json(e, f))? == self.symbol
                        }
                        Field::Bids => self.levels(c, &mut out.bids, field)?,
                        Field::Asks => self.levels(c, &mut out.asks, field)?,
                        Field::UpdateId => out.update_id = c.u64().map_err(|e| json(e, f))?,
                        Field::Seq => out.seq = c.u64().map_err(|e| json(e, f))?,
                        _ => {}
                    }
                }
            }
            if !c.next_or_close(b'}').map_err(|e| json(e, d))? {
                return Ok(());
            }
        }
    }

    /// `[["price","qty"], ...]` into `out`.
    fn levels(&self, c: &mut Cursor<'_>, out: &mut LevelBuf, field: Field) -> Result<(), Error> {
        parse_levels(c, &self.spec, |l| out.push(l)).map_err(|e| levels_error(e, field))
    }
}

fn see(seen: &mut u32, field: Field, offset: usize) -> Result<(), Error> {
    if *seen & field.bit() != 0 {
        return Err(Error {
            kind: ErrorKind::DuplicateField(field),
            offset,
        });
    }
    *seen |= field.bit();
    Ok(())
}

fn levels_error(e: LevelsError, field: Field) -> Error {
    let (kind, offset) = match e {
        LevelsError::Json(e) => return json(e, Some(field)),
        LevelsError::Spec(e, at) => return decimal(e, Some(field), at),
        LevelsError::BadLevel(at) => (ErrorKind::BadLevel(field), at),
        LevelsError::TooMany(at) => (ErrorKind::TooManyLevels(field), at),
    };
    Error { kind, offset }
}

fn json(e: JsonError, field: Option<Field>) -> Error {
    Error {
        kind: ErrorKind::Json(e.kind, field),
        offset: e.offset,
    }
}

fn decimal(e: ob_core::ParseFixedError, field: Option<Field>, offset: usize) -> Error {
    Error {
        kind: ErrorKind::Json(JsonErrorKind::Decimal(e), field),
        offset,
    }
}
