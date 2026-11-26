//! A WebSocket-over-TLS connection to one venue.
//!
//! ```text
//! connect():  DNS → TCP (timeout) → socket options → TLS handshake → WebSocket handshake
//!             (blocking, with a deadline: runs once per connection)
//! poll():     non-blocking read → decrypt → decode → Option<Message>
//!             (never blocks in spin mode: the caller's loop spins on `None`)
//! send_*():   encode (masked) → encrypt → write
//! ```
//!
//! **Buffers (step 24).** One receive buffer per connection: the socket reads ciphertext
//! into it, each TLS record is decrypted *in place* directly after the previous
//! plaintext (our own record layer, [`crate::record`]), and WebSocket messages are
//! decoded from that plaintext in place. Consuming a message moves no bytes; the unread
//! tail is moved to the front only when the free space runs low. Before step 24, rustls
//! decrypted each record into a freshly allocated `Vec`, which was then copied here.
//!
//! **Protocol duties handled here:** pings are answered with a pong carrying the same
//! payload (RFC 6455 §5.5.2), and a close from the server is echoed before the
//! connection is considered closed (§5.5.1). Both are still returned to the caller, which
//! may want to count them.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::ClientConfig;
use rustls::pki_types::ServerName;

use crate::record::{HEADER, MAX_BODY};
use crate::session::{RecvBuf, Session};
use crate::tls;
use crate::ws::{
    HandshakeError, Message, MessageReader, Opcode, ReaderConfig, WsError, encode_client_frame,
    handshake,
};

/// How to reach a venue.
#[derive(Clone, Debug)]
pub struct ConnConfig {
    /// Host name, also used for TLS certificate checking (e.g. `fstream.binance.com`).
    pub host: String,
    /// TCP port (443).
    pub port: u16,
    /// WebSocket path (e.g. `/ws`).
    pub path: String,
    /// Deadline for TCP connect + TLS + WebSocket handshakes together.
    pub connect_timeout: Duration,
    /// Kernel receive buffer to request (`SO_RCVBUF`), so a burst can't overflow it while
    /// we're busy; the kernel may round or cap it.
    pub recv_buffer: usize,
    /// WebSocket reader settings (max message size, UTF-8 checking).
    pub reader: ReaderConfig,
    /// TLS settings (production: [`tls::client_config`]).
    pub tls: Arc<ClientConfig>,
    /// OS socket tuning (step 21; all off by default, all Linux-only in effect).
    pub tuning: SocketTuning,
}

/// Opt-in kernel socket tuning (step 21). What each one did is reported by
/// [`WsConnection::tuning_report`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SocketTuning {
    /// `SO_BUSY_POLL` for this many µs (Linux).
    pub busy_poll_us: Option<u32>,
    /// `TCP_QUICKACK`, re-armed after every read (Linux).
    pub quickack: bool,
    /// Kernel receive timestamps (Linux `SO_TIMESTAMPNS`): measure how long data waited
    /// in the kernel before we read it ([`WsConnection::last_kernel_wait_ns`]).
    pub kernel_timestamps: bool,
}

impl ConnConfig {
    /// Defaults for a public `wss://host/path` endpoint.
    pub fn new(host: &str, path: &str) -> Result<Self, NetError> {
        Ok(ConnConfig {
            host: host.to_string(),
            port: 443,
            path: path.to_string(),
            connect_timeout: Duration::from_secs(10),
            recv_buffer: 4 << 20,
            reader: ReaderConfig::default(),
            tls: tls::client_config()?,
            tuning: SocketTuning::default(),
        })
    }
}

/// Anything that ends or prevents a connection.
#[derive(Debug)]
pub enum NetError {
    /// Socket error (including EOF as `UnexpectedEof`).
    Io(io::Error),
    /// TLS error (bad certificate, protocol violation, ...).
    Tls(rustls::Error),
    /// The server refused the WebSocket upgrade.
    Handshake(HandshakeError),
    /// WebSocket protocol violation by the server.
    Ws(WsError),
    /// The connection is closed (by either side); the code the server sent, if any.
    Closed(Option<u16>),
    /// The connect deadline passed.
    Timeout,
    /// An HTTP request (REST snapshot, step 19) got a status other than 200.
    HttpStatus(u16),
}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> Self {
        NetError::Io(e)
    }
}
impl From<rustls::Error> for NetError {
    fn from(e: rustls::Error) -> Self {
        NetError::Tls(e)
    }
}
impl From<WsError> for NetError {
    fn from(e: WsError) -> Self {
        NetError::Ws(e)
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for NetError {}

/// How `poll` waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitMode {
    /// Non-blocking socket: `poll` returns `Ok(None)` immediately when nothing has
    /// arrived, and the caller spins. Lowest latency, one core at 100%.
    Spin,
    /// Blocking socket: `poll` sleeps in the kernel until data arrives or `timeout`
    /// passes. Frees the core; pays the wake-up latency (measured in the step 12 notes).
    Block {
        /// Longest single wait.
        timeout: Duration,
    },
}

/// One live WebSocket connection over TLS.
pub struct WsConnection {
    sock: TcpStream,
    tls: Session,
    /// Received bytes: plaintext not yet consumed, then ciphertext not yet decrypted.
    rb: RecvBuf,
    /// Bytes of the message returned by the previous `poll`, consumed on the next call.
    pending_consume: usize,
    reader: MessageReader,
    /// Scratch for outgoing frames before encryption (reused).
    frame: Vec<u8>,
    /// Encrypted bytes waiting to be written (reused), and how many are already sent.
    out: Vec<u8>,
    out_sent: usize,
    /// We've sent a close frame.
    close_sent: bool,
    /// The server's close code, once received.
    closed_by_peer: Option<Option<u16>>,
    tuning: SocketTuning,
    /// What the OS did with each tuning request (for logging).
    tuning_report: [(&'static str, ob_os::Support); 3],
    /// Receive timestamps are active (requested and supported).
    stamping: bool,
    /// For the last read: how long its data waited in the kernel (ns), if stamped.
    last_kernel_wait_ns: Option<u64>,
}

/// One maximum-size TLS record, header included (RFC 8446 §5.2).
const TLS_RECORD: usize = HEADER + MAX_BODY;

impl WsConnection {
    /// Connects, completes TLS and the WebSocket handshake, and leaves the socket in
    /// [`WaitMode::Spin`]. Blocks for at most `cfg.connect_timeout`.
    pub fn connect(cfg: &ConnConfig) -> Result<Self, NetError> {
        let deadline = Instant::now() + cfg.connect_timeout;
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or(NetError::Timeout)
        };

        // TCP, trying each resolved address in turn.
        let mut last_err = None;
        let mut sock = None;
        for addr in (cfg.host.as_str(), cfg.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, left()?) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let sock = sock.ok_or_else(|| {
            NetError::Io(last_err.unwrap_or_else(|| io::Error::other("no address")))
        })?;
        // Nagle's algorithm would hold our small frames (pings, subscriptions) back
        // waiting for more data; market data is latency-sensitive.
        sock.set_nodelay(true)?;
        socket2::SockRef::from(&sock).set_recv_buffer_size(cfg.recv_buffer)?;
        // A refused option (e.g. raising SO_BUSY_POLL without CAP_NET_ADMIN on Linux) is
        // reported, never fatal: the connection works without it.
        let off = ob_os::Support::Unsupported("not requested");
        let refused = ob_os::Support::Unsupported("refused by the OS (permissions?)");
        let t = cfg.tuning;
        let busy = t
            .busy_poll_us
            .map_or(off, |us| ob_os::set_busy_poll(&sock, us).unwrap_or(refused));
        let quick = if t.quickack {
            ob_os::set_quickack(&sock).unwrap_or(refused)
        } else {
            off
        };
        let stamps = if t.kernel_timestamps {
            ob_os::enable_rx_timestamps(&sock).unwrap_or(refused)
        } else {
            off
        };

        let name = ServerName::try_from(cfg.host.clone())
            .map_err(|_| NetError::Io(io::Error::other("bad host name")))?;
        // Room for the largest message, one record arriving behind it, and one being read.
        let mut rb = RecvBuf::new(cfg.reader.max_message + 2 * TLS_RECORD);
        // TLS handshake (blocking, bounded by the deadline), then our record layer.
        let mut s = &sock;
        let tls = Session::handshake(Arc::clone(&cfg.tls), name, &mut s, &mut rb, |s| {
            s.set_read_timeout(Some(left()?))?;
            s.set_write_timeout(Some(left()?))?;
            Ok(())
        })
        .map_err(|e| match e {
            NetError::Io(io) => timeout_or(io, deadline), // a socket timeout is the deadline
            other => other,
        })?;
        let mut conn = WsConnection {
            sock,
            tls,
            rb,
            pending_consume: 0,
            reader: MessageReader::new(cfg.reader),
            frame: Vec::with_capacity(1024),
            out: Vec::with_capacity(4096),
            out_sent: 0,
            close_sent: false,
            closed_by_peer: None,
            tuning: t,
            tuning_report: [
                ("busy_poll", busy),
                ("quickack", quick),
                ("kernel_timestamps", stamps),
            ],
            stamping: stamps == ob_os::Support::Applied,
            last_kernel_wait_ns: None,
        };
        // Records that arrived with the end of the handshake (e.g. session tickets).
        conn.tls
            .receive(&mut conn.rb, &mut conn.out)
            .map_err(NetError::Tls)?;

        // WebSocket handshake.
        let mut random = [0_u8; 16];
        tls::random(&mut random)?;
        let key = handshake::client_key(random);
        conn.tls.seal_app(
            handshake::request(&cfg.host, &cfg.path, &key).as_bytes(),
            &mut conn.out,
        )?;
        conn.flush_blocking(deadline)?;
        loop {
            match handshake::check_response(conn.unread(), &key).map_err(NetError::Handshake)? {
                Some(n) => {
                    conn.rb.start += n; // anything after the response is already frames
                    break;
                }
                None => {
                    conn.sock.set_read_timeout(Some(left()?))?;
                    if conn.fill().map_err(|e| timeout_or(e, deadline))? == 0 {
                        return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
                    }
                }
            }
        }
        conn.set_wait(WaitMode::Spin)?;
        Ok(conn)
    }

    /// Switches between spinning and blocking reads.
    pub fn set_wait(&mut self, mode: WaitMode) -> io::Result<()> {
        match mode {
            WaitMode::Spin => self.sock.set_nonblocking(true),
            WaitMode::Block { timeout } => {
                self.sock.set_nonblocking(false)?;
                self.sock
                    .set_read_timeout(Some(timeout.max(Duration::from_micros(1))))
            }
        }
    }

    /// The negotiated TLS cipher suite (for logs and tests).
    pub fn cipher_suite(&self) -> Option<rustls::CipherSuite> {
        Some(self.tls.cipher_suite())
    }

    /// The negotiated TLS version (for logs and tests).
    pub fn protocol_version(&self) -> rustls::ProtocolVersion {
        self.tls.protocol_version()
    }

    /// The next message, if one has arrived.
    ///
    /// * `Ok(Some(msg))`: handle it before calling `poll` again (it borrows our buffer).
    /// * `Ok(None)`: nothing complete yet (spin mode: returns at once; block mode: after
    ///   the timeout).
    /// * `Err(NetError::Closed(_))` once closed; any other error ends the connection.
    pub fn poll(&mut self) -> Result<Option<Message<'_>>, NetError> {
        self.rb.start += core::mem::take(&mut self.pending_consume);
        if let Some(code) = self.closed_by_peer {
            return Err(NetError::Closed(code));
        }
        self.flush_nonblocking()?;
        // Only if no whole frame is buffered, read the socket (once; never blocks in spin
        // mode).
        if self.next_opcode()?.is_none() {
            match self.fill() {
                Ok(0) => return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into())),
                Ok(_) => {}
                Err(e) if is_would_block(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        self.poll_buffered()
    }

    /// Decodes the frame at the front of the buffer, if a whole one is there.
    fn poll_buffered(&mut self) -> Result<Option<Message<'_>>, NetError> {
        let Some(opcode) = self.next_opcode()? else {
            return Ok(None);
        };
        let (start, end) = (self.rb.start, self.rb.end);
        if matches!(opcode, Opcode::Ping | Opcode::Close) {
            // Rare: protocol duties first (RFC 6455 §5.5): answer a ping with the same
            // payload, echo a close once. The decode below validates the frame (e.g. the
            // close code); what's needed is copied out so `self` is free to send.
            let decoded = self
                .reader
                .decode(&self.rb.buf[start..end])?
                .ok_or(WsError::TooBig)?;
            let consumed = decoded.consumed;
            let duty = match decoded.message {
                Some(Message::Ping(p)) => Duty::Pong(Control::copy(p)),
                Some(Message::Close(c)) => Duty::EchoClose(c.map(|c| c.code)),
                _ => Duty::None,
            };
            match duty {
                Duty::Pong(payload) => self.send_frame(Opcode::Pong, payload.bytes())?,
                Duty::EchoClose(code) => {
                    self.closed_by_peer = Some(code);
                    if !self.close_sent {
                        let echo = code.map(u16::to_be_bytes);
                        self.send_frame(Opcode::Close, echo.as_ref().map_or(&[][..], |b| &b[..]))?;
                        self.close_sent = true;
                    }
                }
                Duty::None => {}
            }
            self.pending_consume = consumed;
            // Control frames don't touch the reader's fragment state: decoding again gives
            // the same message, now borrowed for the caller.
            return Ok(self
                .reader
                .decode(&self.rb.buf[start..end])?
                .and_then(|d| d.message));
        }
        // The hot path: data (zero-copy for single-frame messages), decoded once.
        let decoded = self
            .reader
            .decode(&self.rb.buf[start..end])?
            .ok_or(WsError::TooBig)?;
        match decoded.message {
            None => {
                self.rb.start += decoded.consumed; // a fragment was absorbed
                Ok(None)
            }
            Some(message) => {
                self.pending_consume = decoded.consumed; // consumed on the next call
                Ok(Some(message))
            }
        }
    }

    /// The opcode of the frame at the front of the buffer if it's complete; errors early
    /// on a protocol violation. Header parse only: cheap, and borrows nothing.
    fn next_opcode(&self) -> Result<Option<Opcode>, NetError> {
        Ok(crate::ws::decode_frame(self.unread(), usize::MAX)?.map(|(f, _)| f.opcode))
    }

    /// Sends a text message (subscriptions, Bybit's JSON ping).
    pub fn send_text(&mut self, text: &[u8]) -> Result<(), NetError> {
        self.send_frame(Opcode::Text, text)
    }

    /// Starts a clean close with `code` (1000 = normal).
    /// Then ends our side of the TLS session (close_notify), so the server sees a clean
    /// shutdown rather than a truncated stream. Reading continues to work (e.g. for the
    /// server's echoed close frame).
    pub fn close(&mut self, code: u16) -> Result<(), NetError> {
        if !self.close_sent {
            self.send_frame(Opcode::Close, &code.to_be_bytes())?;
            self.close_sent = true;
            self.tls.seal_close_notify(&mut self.out)?;
            self.flush_nonblocking()?;
        }
        Ok(())
    }

    fn send_frame(&mut self, opcode: Opcode, payload: &[u8]) -> Result<(), NetError> {
        let mut mask = [0_u8; 4];
        tls::random(&mut mask)?; // RFC 6455 §5.3: a fresh unpredictable key per frame
        self.frame.clear();
        encode_client_frame(&mut self.frame, true, opcode, payload, mask);
        self.tls.seal_app(&self.frame, &mut self.out)?;
        self.flush_nonblocking()
    }

    /// Writes pending encrypted output without blocking; what doesn't fit stays queued
    /// and is retried on the next `poll`.
    fn flush_nonblocking(&mut self) -> Result<(), NetError> {
        while self.out_sent < self.out.len() {
            match (&self.sock).write(&self.out[self.out_sent..]) {
                Ok(0) => return Err(NetError::Io(io::ErrorKind::WriteZero.into())),
                Ok(n) => self.out_sent += n,
                Err(e) if is_would_block(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        self.out.clear();
        self.out_sent = 0;
        Ok(())
    }

    fn flush_blocking(&mut self, deadline: Instant) -> Result<(), NetError> {
        while self.out_sent < self.out.len() {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or(NetError::Timeout)?;
            self.sock
                .set_write_timeout(Some(left.max(Duration::from_micros(1))))?;
            match (&self.sock).write(&self.out[self.out_sent..]) {
                Ok(0) => return Err(NetError::Io(io::ErrorKind::WriteZero.into())),
                Ok(n) => self.out_sent += n,
                Err(e) => return Err(timeout_or(e, deadline)),
            }
        }
        self.out.clear();
        self.out_sent = 0;
        Ok(())
    }

    /// Reads from the socket once and decrypts every complete record in place. Returns
    /// the bytes read from the socket (0 = EOF, or the peer's TLS close_notify).
    fn fill(&mut self) -> io::Result<usize> {
        if self.tls.peer_closed() {
            return Ok(0);
        }
        let space = self.rb.space(TLS_RECORD)?;
        let n = if self.stamping {
            // recvmsg instead of read, to collect the kernel's receive timestamp.
            let (n, stamp) = ob_os::recv_timestamped(&self.sock, space)?;
            if let Some(kernel_ns) = stamp {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
                self.last_kernel_wait_ns = Some(now.saturating_sub(kernel_ns));
            }
            n
        } else {
            (&self.sock).read(space)?
        };
        if self.tuning.quickack && n > 0 {
            // Linux clears quick-ACK mode on its own; re-arm it after each read.
            let _ = ob_os::set_quickack(&self.sock);
        }
        if n == 0 {
            return Ok(0); // EOF without a TLS close_notify
        }
        self.rb.raw_end += n;
        self.tls
            .receive(&mut self.rb, &mut self.out)
            .map_err(io::Error::other)?;
        if !self.out.is_empty() {
            // A key update to answer: send it now (never blocks).
            self.flush_nonblocking().map_err(io::Error::other)?;
        }
        Ok(n)
    }

    /// What the OS did with each requested socket tuning option.
    pub fn tuning_report(&self) -> &[(&'static str, ob_os::Support)] {
        &self.tuning_report
    }

    /// How long the data of the most recent read waited in the kernel before we read it
    /// (ns): network-card interrupt → socket buffer → our `recvmsg`. Linux with
    /// [`SocketTuning::kernel_timestamps`] only; `None` otherwise.
    pub fn last_kernel_wait_ns(&self) -> Option<u64> {
        self.last_kernel_wait_ns
    }

    fn unread(&self) -> &[u8] {
        self.rb.unread()
    }
}

/// What `poll` must do before returning a control frame.
enum Duty {
    Pong(Control),
    EchoClose(Option<u16>),
    None,
}

/// A control frame's payload (at most 125 bytes), copied onto the stack.
struct Control {
    bytes: [u8; 125],
    len: usize,
}

impl Control {
    fn copy(payload: &[u8]) -> Control {
        let len = payload.len().min(125); // the decoder already enforces ≤ 125
        let mut bytes = [0_u8; 125];
        bytes[..len].copy_from_slice(&payload[..len]);
        Control { bytes, len }
    }
    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

fn is_would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn timeout_or(e: io::Error, deadline: Instant) -> NetError {
    if is_would_block(&e) || Instant::now() >= deadline {
        NetError::Timeout
    } else {
        NetError::Io(e)
    }
}
