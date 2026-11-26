//! Binance USDⓈ-M futures: the partial depth stream (`<symbol>@depth<N>@100ms`) and
//! the real-time best bid/offer stream (`<symbol>@bookTicker`), which can share one
//! connection.
//!
//! A real message (recorded, trimmed to 2 levels per side):
//! ```json
//! {"e":"depthUpdate","E":1795610780582,"T":1795610780581,"s":"BTCUSDT","ps":"BTCUSDT",
//!  "U":12235913555065,"u":12235913562909,"pu":12235913554835,
//!  "b":[["58631.90","6.774"],["58631.80","0.002"]],
//!  "a":[["58632.00","3.242"],["58632.10","0.010"]],"st":1}
//! ```
//!
//! **Every message is a complete top-N snapshot**, not a list of changes: levels that
//! drop out of the top N are simply absent from the next message. So a message
//! *replaces* the book ([`DepthUpdate::apply_to`]); applying it as changes (what the Java
//! skeleton does) leaves stale levels behind.
//!
//! **Sequencing.** `pu` ("previous final update ID") equals the previous message's `u`:
//! it held for all 58 consecutive pairs in the recorded fixture. [`DepthUpdate::follows`]
//! checks it, so a gap (a dropped message) is detected even on a snapshot stream.
//!
//! **Message rules**, implemented identically by this parser and by the serde_json oracle
//! in `tests/binance_differential.rs`:
//! * One JSON object; fields in any order; unknown fields (`ps`, `st`, ...) skipped but
//!   validated as JSON.
//! * Known fields (`e E T s U u pu b a id result error`) may appear at most once, and are
//!   type-checked wherever they appear: `E T U u pu id` non-negative integers; `e s`
//!   strings; `b a` arrays of `[price, quantity]` string pairs, valid at the instrument's
//!   scale, tick and lot.
//! * With `e`: it's an event. `e` must be `"depthUpdate"`, `s` the configured symbol, and
//!   `E T U u pu b a` must all be present → [`Message::Depth`].
//! * Without `e` but with `id`: a reply to our subscribe request → [`Message::Response`]
//!   (`ok` unless an `error` field is present).
//! * Anything else → [`ErrorKind::UnknownMessage`].
//!
//! **bookTicker** (step 18): the best bid and ask, pushed on every change (~800/s on
//! BTCUSDT, against 10/s for depth). Its `u` is the same order-book update ID as depth's
//! `u` (checked on a live recording: at all 143 depth messages, the latest bookTicker
//! with `u` ≤ the depth's `u` had the identical top), so the two can be reconciled.
//! ```json
//! {"e":"bookTicker","u":12241724128962,"s":"BTCUSDT","ps":"BTCUSDT","b":"59953.90",
//!  "B":"14.704","a":"59954.00","A":"0.839","T":1795639643568,"E":1795639643569,"st":1}
//! ```
//! Note `b`/`a` are price *strings* here but level *arrays* in depth: the parser tells
//! them apart by their first character, so field order still doesn't matter.
//!
//! Rules for bookTicker (also implemented by the oracle): `e` = `"bookTicker"`; `E T s u
//! b B a A` present; `b a` price strings, `B A` quantity strings (validated wherever
//! they appear, as are `b a` in either form); `s` the configured symbol; a depth message
//! with string `b`/`a`, or a bookTicker with arrays, is rejected ([`ErrorKind::BadLevel`]).
//!
//! On error, the output buffers hold partial data and must not be used.

use ob_core::{ErrorClass, InstrumentSpec, Level, OrderBook, Side, SnapshotError};

use crate::json::{Cursor, JsonError, JsonErrorKind};
use crate::levels::{LevelBuf, LevelsError, parse_levels};

/// One parsed depth message. Reuse a single instance for every message.
#[derive(Clone, Debug, Default)]
pub struct DepthUpdate {
    /// `E`: event time, ms since the epoch (when Binance sent it).
    pub event_time: u64,
    /// `T`: transaction time, ms (the matching engine time of the last change).
    pub transaction_time: u64,
    /// `U`: first update ID covered by this snapshot.
    pub first_update_id: u64,
    /// `u`: final update ID covered by this snapshot.
    pub final_update_id: u64,
    /// `pu`: the previous message's `u`.
    pub prev_final_update_id: u64,
    /// Bids, best (highest) first, as sent.
    pub bids: LevelBuf,
    /// Asks, best (lowest) first, as sent.
    pub asks: LevelBuf,
}

impl DepthUpdate {
    /// Replaces `book` with this snapshot (all-or-nothing: on error the book is unchanged).
    pub fn apply_to<const CAP: usize>(
        &self,
        book: &mut OrderBook<CAP>,
    ) -> Result<(), SnapshotError> {
        book.replace(&self.bids, &self.asks)
    }
}

/// One best bid/offer update (`bookTicker`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BookTicker {
    /// `u`: the order-book update ID this top reflects (same sequence as depth's `u`).
    pub update_id: u64,
    /// `E`: event time, ms.
    pub event_time: u64,
    /// `T`: transaction time, ms.
    pub transaction_time: u64,
    /// `b` × `B`: best bid.
    pub bid: Level,
    /// `a` × `A`: best ask.
    pub ask: Level,
}

/// What a message was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// A depth snapshot, written into the caller's [`DepthUpdate`].
    Depth,
    /// A best bid/offer update.
    BookTicker(BookTicker),
    /// A reply to a request we sent (e.g. SUBSCRIBE), matched by `id`.
    Response {
        /// The request ID we sent.
        id: u64,
        /// `false` if Binance reported an error.
        ok: bool,
    },
}

/// A field of the message, for error reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// `e`
    EventType,
    /// `E`
    EventTime,
    /// `T`
    TransactionTime,
    /// `s`
    Symbol,
    /// `U`
    FirstUpdateId,
    /// `u`
    FinalUpdateId,
    /// `pu`
    PrevFinalUpdateId,
    /// `b`
    Bids,
    /// `a`
    Asks,
    /// `B` (bookTicker: best bid quantity)
    BidQty,
    /// `lastUpdateId` (REST snapshot)
    LastUpdateId,
    /// `bids` (REST snapshot)
    SnapshotBids,
    /// `asks` (REST snapshot)
    SnapshotAsks,
    /// `A` (bookTicker: best ask quantity)
    AskQty,
    /// `id`
    Id,
    /// `result`
    Result,
    /// `error`
    Error,
}

impl Field {
    fn from_key(key: &[u8]) -> Option<Field> {
        Some(match key {
            b"e" => Field::EventType,
            b"E" => Field::EventTime,
            b"T" => Field::TransactionTime,
            b"s" => Field::Symbol,
            b"U" => Field::FirstUpdateId,
            b"u" => Field::FinalUpdateId,
            b"pu" => Field::PrevFinalUpdateId,
            b"b" => Field::Bids,
            b"a" => Field::Asks,
            b"B" => Field::BidQty,
            b"lastUpdateId" => Field::LastUpdateId,
            b"bids" => Field::SnapshotBids,
            b"asks" => Field::SnapshotAsks,
            b"A" => Field::AskQty,
            b"id" => Field::Id,
            b"result" => Field::Result,
            b"error" => Field::Error,
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
    /// `e` is neither `"depthUpdate"` nor `"bookTicker"`.
    WrongEvent,
    /// `s` is not the symbol we subscribed to.
    WrongSymbol,
    /// A level that isn't a `[price, quantity]` pair of strings.
    BadLevel(Field),
    /// More than [`crate::levels::MAX_LEVELS`] levels on one side.
    TooManyLevels(Field),
    /// Neither an event nor a response.
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
    /// Which policy applies (step 2): spec mismatches mean the venue changed its tick or
    /// lot (reload instrument info); negative quantities are invalid data; everything
    /// else is malformed. In every case the message is dropped whole and the venue resyncs.
    pub const fn class(&self) -> ErrorClass {
        match self.kind {
            ErrorKind::Json(JsonErrorKind::Decimal(e), _) => e.class(),
            _ => ErrorClass::Malformed,
        }
    }
}

/// Parses Binance partial depth messages for one instrument.
#[derive(Clone, Copy, Debug)]
pub struct BinanceDepthParser<'a> {
    spec: InstrumentSpec,
    symbol: &'a [u8],
}

const REQUIRED_FOR_TICKER: [Field; 9] = [
    Field::EventType,
    Field::EventTime,
    Field::TransactionTime,
    Field::Symbol,
    Field::FinalUpdateId,
    Field::Bids,
    Field::BidQty,
    Field::Asks,
    Field::AskQty,
];

const REQUIRED_FOR_DEPTH: [Field; 9] = [
    Field::EventType,
    Field::EventTime,
    Field::TransactionTime,
    Field::Symbol,
    Field::FirstUpdateId,
    Field::FinalUpdateId,
    Field::PrevFinalUpdateId,
    Field::Bids,
    Field::Asks,
];

impl<'a> BinanceDepthParser<'a> {
    /// A parser for `symbol` (as Binance writes it, e.g. `b"BTCUSDT"`) with the
    /// instrument's scale, tick and lot.
    pub const fn new(spec: InstrumentSpec, symbol: &'a [u8]) -> Self {
        BinanceDepthParser { spec, symbol }
    }

    /// Parses one WebSocket text frame. For [`Message::Depth`], the result is in `out`;
    /// a [`Message::BookTicker`] carries its own (small) result.
    pub fn parse(&self, msg: &[u8], out: &mut DepthUpdate) -> Result<Message, Error> {
        #[derive(PartialEq)]
        enum Event {
            Depth,
            Ticker,
            Other,
        }
        let mut c = Cursor::new(msg);
        let mut seen: u32 = 0;
        let mut event = Event::Other;
        let mut symbol_ok = false;
        let mut id = 0;
        // bookTicker's `b a` (price strings) and `B A` (quantities); `*_str` records which
        // form `b`/`a` took.
        let (mut bid_px, mut ask_px, mut bid_qty, mut ask_qty) = (0, 0, 0, 0);
        let (mut bids_str, mut asks_str) = (false, false);
        out.bids.clear();
        out.asks.clear();

        c.expect(b'{').map_err(|e| json(e, None))?;
        if c.has_first(b'}').map_err(|e| json(e, None))? {
            loop {
                let key_at = c.pos();
                let key = c.key_colon().map_err(|e| json(e, None))?;
                match Field::from_key(key) {
                    None => c.skip_value().map_err(|e| json(e, None))?,
                    Some(field) => {
                        if seen & field.bit() != 0 {
                            return Err(Error {
                                kind: ErrorKind::DuplicateField(field),
                                offset: key_at,
                            });
                        }
                        seen |= field.bit();
                        let f = Some(field);
                        match field {
                            Field::EventType => {
                                event = match c.string().map_err(|e| json(e, f))? {
                                    b"depthUpdate" => Event::Depth,
                                    b"bookTicker" => Event::Ticker,
                                    _ => Event::Other,
                                };
                            }
                            Field::Symbol => {
                                symbol_ok = c.string().map_err(|e| json(e, f))? == self.symbol;
                            }
                            Field::EventTime => out.event_time = c.u64().map_err(|e| json(e, f))?,
                            Field::TransactionTime => {
                                out.transaction_time = c.u64().map_err(|e| json(e, f))?;
                            }
                            Field::FirstUpdateId => {
                                out.first_update_id = c.u64().map_err(|e| json(e, f))?
                            }
                            Field::FinalUpdateId => {
                                out.final_update_id = c.u64().map_err(|e| json(e, f))?
                            }
                            Field::PrevFinalUpdateId => {
                                out.prev_final_update_id = c.u64().map_err(|e| json(e, f))?;
                            }
                            Field::Bids if c.peek() == Some(b'"') => {
                                bids_str = true;
                                bid_px = self.price(&mut c, field)?;
                            }
                            Field::Asks if c.peek() == Some(b'"') => {
                                asks_str = true;
                                ask_px = self.price(&mut c, field)?;
                            }
                            Field::Bids => self.levels(&mut c, &mut out.bids, field)?,
                            Field::Asks => self.levels(&mut c, &mut out.asks, field)?,
                            Field::BidQty => bid_qty = self.qty(&mut c, field)?,
                            // REST snapshot fields: not part of stream messages, skipped
                            // like any unknown field.
                            Field::LastUpdateId | Field::SnapshotBids | Field::SnapshotAsks => {
                                seen &= !field.bit(); // and, like unknown fields, may repeat
                                c.skip_value().map_err(|e| json(e, f))?
                            }
                            Field::AskQty => ask_qty = self.qty(&mut c, field)?,
                            Field::Id => id = c.u64().map_err(|e| json(e, f))?,
                            Field::Result | Field::Error => {
                                c.skip_value().map_err(|e| json(e, f))?
                            }
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
        let err = |kind| Err(Error { kind, offset: end });
        if seen & Field::EventType.bit() != 0 {
            let required: &[Field] = match event {
                Event::Ticker => &REQUIRED_FOR_TICKER,
                _ => &REQUIRED_FOR_DEPTH,
            };
            for &field in required {
                if seen & field.bit() == 0 {
                    return err(ErrorKind::MissingField(field));
                }
            }
            if event == Event::Other {
                return err(ErrorKind::WrongEvent);
            }
            // `b`/`a` must have the event's form: arrays for depth, strings for tickers.
            let want_str = event == Event::Ticker;
            if bids_str != want_str {
                return err(ErrorKind::BadLevel(Field::Bids));
            }
            if asks_str != want_str {
                return err(ErrorKind::BadLevel(Field::Asks));
            }
            if !symbol_ok {
                return err(ErrorKind::WrongSymbol);
            }
            if event == Event::Ticker {
                let level = |px, qty| -> Result<Level, Error> {
                    let px = self
                        .spec
                        .px_from_raw(px)
                        .map_err(|e| decimal(e, None, end))?;
                    let qty = self
                        .spec
                        .qty_from_raw(qty)
                        .map_err(|e| decimal(e, None, end))?;
                    Ok(Level::new(px, qty))
                };
                return Ok(Message::BookTicker(BookTicker {
                    update_id: out.final_update_id,
                    event_time: out.event_time,
                    transaction_time: out.transaction_time,
                    bid: level(bid_px, bid_qty)?,
                    ask: level(ask_px, ask_qty)?,
                }));
            }
            Ok(Message::Depth)
        } else if seen & Field::Id.bit() != 0 {
            Ok(Message::Response {
                id,
                ok: seen & Field::Error.bit() == 0,
            })
        } else {
            Err(Error {
                kind: ErrorKind::UnknownMessage,
                offset: end,
            })
        }
    }

    /// A `"price"` string (raw, validated against tick).
    fn price(&self, c: &mut Cursor<'_>, field: Field) -> Result<i64, Error> {
        let at = c.pos();
        let raw = c
            .decimal(self.spec.px_decimals())
            .map_err(|e| json(e, Some(field)))?;
        self.spec
            .px_from_raw(raw)
            .map(|p| p.raw())
            .map_err(|e| decimal(e, Some(field), at))
    }

    /// A `"quantity"` string (raw, validated against lot and sign).
    fn qty(&self, c: &mut Cursor<'_>, field: Field) -> Result<i64, Error> {
        let at = c.pos();
        let raw = c
            .decimal(self.spec.qty_decimals())
            .map_err(|e| json(e, Some(field)))?;
        self.spec
            .qty_from_raw(raw)
            .map(|q| q.raw())
            .map_err(|e| decimal(e, Some(field), at))
    }

    /// `[["price","qty"], ...]` into `out`.
    fn levels(&self, c: &mut Cursor<'_>, out: &mut LevelBuf, field: Field) -> Result<(), Error> {
        self.levels_with(c, field, |l| out.push(l))
    }

    /// `[["price","qty"], ...]`, each level handed to `push`.
    fn levels_with(
        &self,
        c: &mut Cursor<'_>,
        field: Field,
        push: impl FnMut(Level) -> bool,
    ) -> Result<(), Error> {
        parse_levels(c, &self.spec, push).map_err(|e| levels_error(e, field))
    }
}

/// The header of a REST order-book snapshot (`GET /fapi/v1/depth`, step 19); the
/// levels go to the caller's callback (this crate doesn't allocate).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// `lastUpdateId`: the book as of this update ID.
    pub last_update_id: u64,
    /// `E` (0 if absent).
    pub event_time: u64,
    /// `T` (0 if absent).
    pub transaction_time: u64,
}

impl BinanceDepthParser<'_> {
    /// Parses a REST depth snapshot, handing each level to `level(side, level)` in the
    /// order sent (best first); `level` returns `false` to refuse more (→
    /// [`ErrorKind::TooManyLevels`]).
    ///
    /// Rules (also implemented by the oracle in `tests/binance_rest.rs`): one object;
    /// `lastUpdateId bids asks` required, `E T` optional; known fields at most once and
    /// type-checked; unknown fields skipped; levels valid at the instrument's scale, tick
    /// and lot. On error, levels already handed over must be discarded.
    pub fn parse_rest_snapshot(
        &self,
        msg: &[u8],
        mut level: impl FnMut(Side, Level) -> bool,
    ) -> Result<SnapshotHeader, Error> {
        let mut c = Cursor::new(msg);
        let mut out = SnapshotHeader::default();
        let mut seen: u32 = 0;
        c.expect(b'{').map_err(|e| json(e, None))?;
        if c.has_first(b'}').map_err(|e| json(e, None))? {
            loop {
                let key_at = c.pos();
                let key = c.key_colon().map_err(|e| json(e, None))?;
                let field = match Field::from_key(key) {
                    Some(
                        f @ (Field::LastUpdateId
                        | Field::EventTime
                        | Field::TransactionTime
                        | Field::SnapshotBids
                        | Field::SnapshotAsks),
                    ) => f,
                    _ => {
                        c.skip_value().map_err(|e| json(e, None))?;
                        if !c.next_or_close(b'}').map_err(|e| json(e, None))? {
                            break;
                        }
                        continue;
                    }
                };
                if seen & field.bit() != 0 {
                    return Err(Error {
                        kind: ErrorKind::DuplicateField(field),
                        offset: key_at,
                    });
                }
                seen |= field.bit();
                let f = Some(field);
                match field {
                    Field::LastUpdateId => out.last_update_id = c.u64().map_err(|e| json(e, f))?,
                    Field::EventTime => out.event_time = c.u64().map_err(|e| json(e, f))?,
                    Field::TransactionTime => {
                        out.transaction_time = c.u64().map_err(|e| json(e, f))?;
                    }
                    Field::SnapshotBids => {
                        self.levels_with(&mut c, field, |l| level(Side::Bid, l))?;
                    }
                    _ => self.levels_with(&mut c, field, |l| level(Side::Ask, l))?,
                }
                if !c.next_or_close(b'}').map_err(|e| json(e, None))? {
                    break;
                }
            }
        }
        c.end().map_err(|e| json(e, None))?;
        for field in [
            Field::LastUpdateId,
            Field::SnapshotBids,
            Field::SnapshotAsks,
        ] {
            if seen & field.bit() == 0 {
                return Err(Error {
                    kind: ErrorKind::MissingField(field),
                    offset: c.pos(),
                });
            }
        }
        Ok(out)
    }
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
