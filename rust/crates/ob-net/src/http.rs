//! A minimal HTTPS GET, for one job: fetching a venue's REST order-book snapshot when a
//! full-depth feed (re)synchronises.
//!
//! **Cold path.** It runs once per sync, blocks, and allocates; none of that matters
//! there. What does matter is that it can't hang the feed thread or be tricked into
//! using unbounded memory, so it has:
//! * one **deadline** for the whole request (connect, TLS, send, receive);
//! * a **body size limit**, checked before reading;
//! * strict handling: only status 200 is success; `Content-Length` and `chunked` bodies
//!   are supported; a compressed body (which we never ask for) is refused.
//!
//! Why not a crate (`ureq`, `reqwest`)? Each would add a second HTTP/TLS stack and a
//! large dependency tree for one GET. This reuses our TLS setup and is ~150
//! lines that are fully tested against a local server.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Instant;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};

use crate::conn::{ConnConfig, NetError};

/// Largest response header block accepted.
const MAX_HEADERS: usize = 16 * 1024;

/// `GET https://{cfg.host}:{cfg.port}{path_and_query}`, returning the body of a 200
/// response. `cfg.connect_timeout` bounds the whole request; bodies larger than
/// `max_body` are refused.
pub fn get(cfg: &ConnConfig, path_and_query: &str, max_body: usize) -> Result<Vec<u8>, NetError> {
    let deadline = Instant::now() + cfg.connect_timeout;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(NetError::Timeout)
    };

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
    let sock = sock
        .ok_or_else(|| NetError::Io(last_err.unwrap_or_else(|| io::Error::other("no address"))))?;
    sock.set_nodelay(true)?;
    let name = ServerName::try_from(cfg.host.clone())
        .map_err(|_| NetError::Io(io::Error::other("bad host name")))?;
    let tls = ClientConnection::new(Arc::clone(&cfg.tls), name)?;
    let mut s = StreamOwned::new(tls, sock);

    let request = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ob/0.1\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        cfg.host
    );
    s.sock.set_write_timeout(Some(left()?))?;
    s.sock.set_read_timeout(Some(left()?))?;
    s.write_all(request.as_bytes())
        .map_err(|e| timed(e, deadline))?;
    s.flush().map_err(|e| timed(e, deadline))?;

    // Read until the end of the headers; whatever follows is the start of the body.
    let mut buf = Vec::with_capacity(64 * 1024);
    let header_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEADERS {
            return Err(protocol("response headers too large"));
        }
        if read_some(&mut s, &mut buf, left()?, deadline)? == 0 {
            return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
    };
    let head =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| protocol("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status = parse_status(lines.next().unwrap_or(""))?;
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| protocol("bad Content-Length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
            if !chunked {
                return Err(protocol("unsupported Transfer-Encoding"));
            }
        } else if name.eq_ignore_ascii_case("content-encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err(protocol("compressed body (not requested)"));
        }
    }
    if status != 200 {
        return Err(NetError::HttpStatus(status));
    }
    let mut body = buf.split_off(header_end);
    if chunked {
        loop {
            match dechunk(&body, max_body)? {
                Some(decoded) => return Ok(decoded),
                None => {
                    if read_some(&mut s, &mut body, left()?, deadline)? == 0 {
                        return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
                    }
                }
            }
        }
    }
    let Some(len) = length else {
        return Err(protocol("no Content-Length")); // we asked for Connection: close, but
        // read-to-EOF can't tell a complete body from a cut one; refuse it.
    };
    if len > max_body {
        return Err(protocol("body larger than the limit"));
    }
    while body.len() < len {
        if read_some(&mut s, &mut body, left()?, deadline)? == 0 {
            return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
    }
    body.truncate(len);
    Ok(body)
}

fn protocol(msg: &'static str) -> NetError {
    NetError::Io(io::Error::new(io::ErrorKind::InvalidData, msg))
}

fn timed(e: io::Error, deadline: Instant) -> NetError {
    if matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) || Instant::now() >= deadline
    {
        NetError::Timeout
    } else {
        NetError::Io(e)
    }
}

/// Appends whatever arrives (at most 64 KiB) to `buf`; returns the byte count (0 = EOF).
fn read_some(
    s: &mut StreamOwned<ClientConnection, TcpStream>,
    buf: &mut Vec<u8>,
    wait: std::time::Duration,
    deadline: Instant,
) -> Result<usize, NetError> {
    s.sock.set_read_timeout(Some(wait))?;
    let old = buf.len();
    buf.resize(old + 64 * 1024, 0);
    let n = match s.read(&mut buf[old..]) {
        Ok(n) => n,
        Err(e) => {
            buf.truncate(old);
            return Err(timed(e, deadline));
        }
    };
    buf.truncate(old + n);
    Ok(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_status(line: &str) -> Result<u16, NetError> {
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next()) {
        (Some(v), Some(code)) if v.starts_with("HTTP/1.") => {
            code.parse().map_err(|_| protocol("bad status line"))
        }
        _ => Err(protocol("bad status line")),
    }
}

/// Decodes a complete `chunked` body: `Ok(None)` if more bytes are needed.
fn dechunk(raw: &[u8], max_body: usize) -> Result<Option<Vec<u8>>, NetError> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(eol) = find(&raw[pos..], b"\r\n") else {
            return Ok(None);
        };
        let size_line =
            std::str::from_utf8(&raw[pos..pos + eol]).map_err(|_| protocol("bad chunk"))?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim(); // ignore extensions
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| protocol("bad chunk size"))?;
        pos += eol + 2;
        if size == 0 {
            return Ok(Some(out)); // trailers (if any) are ignored
        }
        if out.len() + size > max_body {
            return Err(protocol("body larger than the limit"));
        }
        if raw.len() < pos + size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        if &raw[pos + size..pos + size + 2] != b"\r\n" {
            return Err(protocol("bad chunk terminator"));
        }
        pos += size + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_bodies_decode_or_ask_for_more() {
        let full = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        assert_eq!(
            dechunk(full, 100).ok().flatten().as_deref(),
            Some(&b"hello world"[..])
        );
        for cut in 0..full.len() - 5 {
            assert!(
                matches!(dechunk(&full[..cut], 100), Ok(None)),
                "cut at {cut}"
            );
        }
        assert!(dechunk(full, 5).is_err(), "over the limit");
        assert!(dechunk(b"zz\r\n", 100).is_err());
        assert!(dechunk(b"5\r\nhelloXX0\r\n\r\n", 100).is_err());
    }

    #[test]
    fn status_lines() {
        assert_eq!(parse_status("HTTP/1.1 200 OK").ok(), Some(200));
        assert_eq!(
            parse_status("HTTP/1.1 429 Too Many Requests").ok(),
            Some(429)
        );
        assert!(parse_status("SSH-2.0").is_err());
    }
}
