//! The opening handshake (RFC 6455 §4): an HTTP/1.1 upgrade request from us, and a check
//! of the server's `101 Switching Protocols` answer.
//!
//! Runs once per connection, so clarity matters more than speed here. What it checks:
//! * status `101`;
//! * `Upgrade: websocket` and `Connection: upgrade` (header names and these values are
//!   case-insensitive; `Connection` may list several tokens);
//! * `Sec-WebSocket-Accept` = base64(SHA-1(our key + the RFC's fixed GUID)). This proves
//!   the server really speaks WebSocket and isn't, say, a cache replaying a response;
//! * **no** `Sec-WebSocket-Extensions` or `Sec-WebSocket-Protocol`: we asked for neither,
//!   so a server that answers with one is broken (§4.1). In particular no
//!   `permessage-deflate` compression, which would cost CPU on every message.

/// The fixed GUID from RFC 6455 §1.3.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The largest handshake response we'll buffer before giving up.
pub const MAX_RESPONSE: usize = 16 * 1024;

/// Standard base64 (RFC 4648 §4), with padding.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> shift & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `Sec-WebSocket-Key` header value for 16 random bytes (the caller supplies them
/// from a random source; the codec itself does no I/O).
pub fn client_key(random: [u8; 16]) -> String {
    base64(&random)
}

/// The `Sec-WebSocket-Accept` value a correct server returns for `key`.
pub fn expected_accept(key: &str) -> String {
    let mut sha = sha1_smol::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64(&sha.digest().bytes())
}

/// The HTTP request that opens the connection.
pub fn request(host: &str, path: &str, key: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
}

/// Why the server's answer was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// Not a valid HTTP response.
    Malformed,
    /// A status other than 101 (e.g. 403 from a geo-block, 429 rate limit).
    Status(u16),
    /// Missing or wrong `Upgrade` / `Connection` header.
    NotAnUpgrade,
    /// Missing or wrong `Sec-WebSocket-Accept`.
    BadAccept,
    /// The server added an extension or subprotocol we didn't ask for.
    Unrequested(String),
    /// More than [`MAX_RESPONSE`] bytes without the end of the headers.
    TooLarge,
}

/// Checks the server's response. `buf` holds the bytes received so far.
///
/// * `Ok(Some(n))`: valid; the response is `buf[..n]`. **Bytes after `n` are already
///   WebSocket frames** (the server may send data in the same packet), so keep them.
/// * `Ok(None)`: the headers aren't complete yet; read more.
pub fn check_response(buf: &[u8], key: &str) -> Result<Option<usize>, HandshakeError> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_RESPONSE {
            Err(HandshakeError::TooLarge)
        } else {
            Ok(None)
        };
    };
    let head = core::str::from_utf8(&buf[..end]).map_err(|_| HandshakeError::Malformed)?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(HandshakeError::Malformed)?;
    let mut parts = status_line.splitn(3, ' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(HandshakeError::Malformed);
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(HandshakeError::Malformed)?;
    if status != 101 {
        return Err(HandshakeError::Status(status));
    }
    let (mut upgrade, mut connection, mut accept) = (false, false, None);
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HandshakeError::Malformed)?;
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade = value.eq_ignore_ascii_case("websocket"),
            "connection" => {
                connection = value
                    .split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            }
            "sec-websocket-accept" => accept = Some(value),
            "sec-websocket-extensions" | "sec-websocket-protocol" => {
                return Err(HandshakeError::Unrequested(line.to_string()));
            }
            _ => {}
        }
    }
    if !upgrade || !connection {
        return Err(HandshakeError::NotAnUpgrade);
    }
    if accept != Some(expected_accept(key).as_str()) {
        return Err(HandshakeError::BadAccept);
    }
    Ok(Some(end + 4))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc_4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn accept_key_rfc_6455_example() {
        // RFC 6455 §1.3: the key "dGhlIHNhbXBsZSBub25jZQ==" must give this accept value.
        assert_eq!(
            expected_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        // §4.1: the key is 16 random bytes, base64-encoded (24 characters).
        assert_eq!(client_key(*b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    fn ok_response(extra: &str) -> String {
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n{extra}\r\n"
        )
    }

    #[test]
    fn request_has_every_required_header() {
        let r = request("fstream.binance.com", "/ws", KEY);
        assert!(r.starts_with("GET /ws HTTP/1.1\r\n"));
        for h in [
            "Host: fstream.binance.com",
            "Upgrade: websocket",
            "Connection: Upgrade",
            "Sec-WebSocket-Version: 13",
        ] {
            assert!(r.contains(h), "{h}");
        }
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_valid_response_and_frames_in_the_same_read() {
        let mut buf = ok_response("").into_bytes();
        let n = buf.len();
        buf.extend_from_slice(&[0x81, 0x02, b'h', b'i']); // a frame right behind the headers
        assert_eq!(check_response(&buf, KEY), Ok(Some(n)));
        assert_eq!(&buf[n..], &[0x81, 0x02, b'h', b'i']);
    }

    #[test]
    fn incomplete_then_complete() {
        let full = ok_response("");
        for cut in 0..full.len() {
            assert_eq!(
                check_response(&full.as_bytes()[..cut], KEY),
                Ok(None),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn header_names_and_tokens_are_case_insensitive() {
        let r = "HTTP/1.1 101 Switching Protocols\r\nupgrade: WebSocket\r\nCONNECTION: keep-alive, Upgrade\r\n\
                 sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        assert!(check_response(r.as_bytes(), KEY).unwrap().is_some());
    }

    #[test]
    fn refusals() {
        let geo = "HTTP/1.1 451 Unavailable For Legal Reasons\r\n\r\n";
        assert_eq!(
            check_response(geo.as_bytes(), KEY),
            Err(HandshakeError::Status(451))
        );
        let wrong = ok_response("").replace(
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        assert_eq!(
            check_response(wrong.as_bytes(), KEY),
            Err(HandshakeError::BadAccept)
        );
        let no_upgrade = ok_response("").replace("Upgrade: websocket\r\n", "");
        assert_eq!(
            check_response(no_upgrade.as_bytes(), KEY),
            Err(HandshakeError::NotAnUpgrade)
        );
        let deflate = ok_response("Sec-WebSocket-Extensions: permessage-deflate\r\n");
        assert!(matches!(
            check_response(deflate.as_bytes(), KEY),
            Err(HandshakeError::Unrequested(_))
        ));
        assert_eq!(
            check_response(b"garbage\r\n\r\n", KEY),
            Err(HandshakeError::Malformed)
        );
        assert_eq!(
            check_response(&vec![b'x'; MAX_RESPONSE + 1], KEY),
            Err(HandshakeError::TooLarge)
        );
    }
}
