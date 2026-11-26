//! The read stage, taken apart (step 24).
//!
//! The wire scenario's "read" row is one number: `poll()` from the socket to a decoded
//! WebSocket message. Here each part is timed on its own, per recorded message:
//!
//! | Row | What | Where it happens in `WsConnection::fill` |
//! |---|---|---|
//! | `recv` (data waiting) | the system call that returns a record's bytes | `read_tls` → `read(2)` |
//! | `recv` (nothing yet) | the same call when no data has arrived (every idle spin) | idem |
//! | rustls: take in | copying ciphertext into rustls's buffer | `read_tls` |
//! | rustls: decrypt | `process_new_packets`: decrypt, plus a `Vec` allocated and filled per record | idem |
//! | rustls: copy out | `reader().read` into our buffer | idem |
//! | WebSocket decode | frame header, length, borrow the payload | `poll_buffered` |
//! | AES-GCM alone | `ring` decrypting the same record in place: the floor any TLS code pays | – |
//! | our record layer | `ob_net::record::Keys::open`: the same record decrypted in place (step 24) | `Session::receive` |
//!
//! The rustls rows run over an in-memory connection (a real handshake between a rustls
//! client and server, no sockets), so the crypto, buffering and allocation are exactly
//! production's and nothing else is mixed in. The `recv` rows use a real loopback socket.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use ob_net::record::{ContentType, Keys, Version};
use ob_net::ws::{MessageReader, Opcode, ReaderConfig, encode_server_frame};
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

use crate::server::localhost_tls;
use crate::stats::Dist;
use crate::stream::{Kind, Stream};

/// Distributions for each part of the read stage, ns.
#[derive(Default)]
pub struct ReadBreakdown {
    /// `recv` returning a record's bytes (loopback).
    pub recv_data: Dist,
    /// `recv` with nothing to read (`WouldBlock`): the cost of each idle spin.
    pub recv_empty: Dist,
    /// rustls `read_tls`: ciphertext into rustls's buffer.
    pub tls_take: Dist,
    /// rustls `process_new_packets`: decrypt + per-record `Vec`.
    pub tls_decrypt: Dist,
    /// rustls `reader().read`: plaintext into our buffer.
    pub tls_copy: Dist,
    /// The three rustls rows together.
    pub tls_total: Dist,
    /// WebSocket decode of the message.
    pub ws: Dist,
    /// AES-128-GCM decryption of a record of the same size, in place (`ring`).
    pub aead: Dist,
    /// Our record layer (step 24): the same TLS 1.3 record decrypted in place, ready for
    /// the WebSocket decoder: replaces the three rustls rows.
    pub ours: Dist,
    /// Mean record size, bytes.
    pub mean_bytes: u64,
}

/// Times each part of the read stage over `n` messages of `kind`.
pub fn read_breakdown(kind: Kind, n: usize, clock: &quanta::Clock) -> ReadBreakdown {
    let mut r = ReadBreakdown::default();
    let (mut client, mut server) = connected_pair();
    let mut stream = Stream::new(kind);
    let mut frame = Vec::with_capacity(64 * 1024);
    let mut cipher = Vec::with_capacity(64 * 1024);
    let mut plain = vec![0_u8; 1 << 20];
    let mut reader = MessageReader::new(ReaderConfig::default());
    let key = LessSafeKey::new(
        #[allow(clippy::unwrap_used)] // a 16-byte key is valid
        UnboundKey::new(&AES_128_GCM, &[7; 16]).unwrap(),
    );
    let mut sealed = Vec::with_capacity(64 * 1024);
    let raw_keys = |seq| {
        #[allow(clippy::unwrap_used)] // valid constant key material
        Keys::from_raw(Version::Tls13, &AES_128_GCM, &[7; 16], [1; 12], seq).unwrap()
    };
    let (mut our_tx, mut our_rx) = (raw_keys(0), raw_keys(0));
    let mut record = Vec::with_capacity(64 * 1024);
    let mut bytes_total = 0_u64;
    let (mut tx, mut rx) = socket_pair();
    let mut sock_buf = vec![0_u8; 1 << 20];

    for i in 0..n {
        frame.clear();
        encode_server_frame(&mut frame, true, Opcode::Text, stream.next_message());
        // One TLS record from the server (untimed).
        cipher.clear();
        #[allow(clippy::unwrap_used)] // in-memory I/O can't fail
        {
            server.writer().write_all(&frame).unwrap();
            while server.wants_write() {
                server.write_tls(&mut cipher).unwrap();
            }
        }
        bytes_total += cipher.len() as u64;

        // rustls, as `WsConnection::fill` uses it.
        let t0 = clock.raw();
        let mut src = &cipher[..];
        while !src.is_empty() {
            #[allow(clippy::unwrap_used)]
            client.read_tls(&mut src).unwrap();
        }
        let t1 = clock.raw();
        #[allow(clippy::unwrap_used)]
        let state = client.process_new_packets().unwrap();
        let t2 = clock.raw();
        let mut want = state.plaintext_bytes_to_read();
        let mut got = 0;
        while want > 0 {
            #[allow(clippy::unwrap_used)]
            let k = client.reader().read(&mut plain[got..]).unwrap();
            got += k;
            want -= k;
        }
        let t3 = clock.raw();
        let decoded = reader.decode(&plain[..got]);
        let t4 = clock.raw();
        debug_assert!(matches!(decoded, Ok(Some(_))));
        std::hint::black_box(&decoded);
        r.tls_take.record(clock.delta_as_nanos(t0, t1));
        r.tls_decrypt.record(clock.delta_as_nanos(t1, t2));
        r.tls_copy.record(clock.delta_as_nanos(t2, t3));
        r.tls_total.record(clock.delta_as_nanos(t0, t3));
        r.ws.record(clock.delta_as_nanos(t3, t4));

        // The floor: AES-128-GCM in place on the same number of bytes.
        sealed.clear();
        sealed.extend_from_slice(&frame);
        let nonce = |i: usize| {
            let mut n = [0_u8; 12];
            n[4..].copy_from_slice(&(i as u64).to_be_bytes());
            Nonce::assume_unique_for_key(n)
        };
        #[allow(clippy::unwrap_used)]
        key.seal_in_place_append_tag(nonce(i), Aad::empty(), &mut sealed)
            .unwrap();
        let t5 = clock.raw();
        let opened = key.open_in_place(nonce(i), Aad::empty(), &mut sealed);
        let t6 = clock.raw();
        debug_assert!(opened.is_ok());
        std::hint::black_box(&opened);
        r.aead.record(clock.delta_as_nanos(t5, t6));

        // Our record layer: the same frame as one TLS 1.3 record, decrypted where it lies
        // (as `Session::receive` does for the first record after the plaintext).
        record.clear();
        #[allow(clippy::unwrap_used)]
        our_tx
            .seal(ContentType::ApplicationData, &frame, &mut record)
            .unwrap();
        let t9 = clock.raw();
        let opened = our_rx.open(&mut record, 0, 0);
        let t10 = clock.raw();
        debug_assert!(matches!(opened, Ok((ContentType::ApplicationData, n)) if n == frame.len()));
        std::hint::black_box(&opened);
        r.ours.record(clock.delta_as_nanos(t9, t10));

        // The system call, on a real loopback socket: same bytes.
        #[allow(clippy::unwrap_used)]
        tx.write_all(&cipher).unwrap();
        let mut left = cipher.len();
        while left > 0 {
            let t7 = clock.raw();
            let res = rx.read(&mut sock_buf);
            let t8 = clock.raw();
            match res {
                Ok(k) => {
                    r.recv_data.record(clock.delta_as_nanos(t7, t8));
                    left -= k;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    r.recv_empty.record(clock.delta_as_nanos(t7, t8));
                }
                #[allow(clippy::panic)]
                Err(e) => panic!("loopback read: {e}"),
            }
        }
    }
    r.mean_bytes = bytes_total / n.max(1) as u64;
    r
}

/// A rustls client and server with the handshake completed in memory.
#[allow(clippy::unwrap_used)] // set-up
fn connected_pair() -> (rustls::ClientConnection, rustls::ServerConnection) {
    let (server_cfg, client_cfg) = localhost_tls();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut client = rustls::ClientConnection::new(client_cfg, name).unwrap();
    let mut server = rustls::ServerConnection::new(server_cfg).unwrap();
    let mut buf = Vec::new();
    for _ in 0..10 {
        buf.clear();
        while client.wants_write() {
            client.write_tls(&mut buf).unwrap();
        }
        let mut s = &buf[..];
        while !s.is_empty() {
            server.read_tls(&mut s).unwrap();
        }
        server.process_new_packets().unwrap();
        buf.clear();
        while server.wants_write() {
            server.write_tls(&mut buf).unwrap();
        }
        let mut s = &buf[..];
        while !s.is_empty() {
            client.read_tls(&mut s).unwrap();
        }
        client.process_new_packets().unwrap();
    }
    assert!(!client.is_handshaking() && !server.is_handshaking());
    (client, server)
}

/// A connected loopback TCP pair; the reading end is non-blocking, as in production.
#[allow(clippy::unwrap_used)] // set-up
fn socket_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let tx = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (rx, _) = l.accept().unwrap();
    tx.set_nodelay(true).unwrap();
    rx.set_nonblocking(true).unwrap();
    (tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_part_is_measured_once_per_message() {
        let clock = quanta::Clock::new();
        let r = read_breakdown(Kind::BinanceTop, 200, &clock);
        for d in [
            &r.tls_take,
            &r.tls_decrypt,
            &r.tls_copy,
            &r.ws,
            &r.aead,
            &r.ours,
        ] {
            assert_eq!(d.len(), 200);
        }
        assert!(r.recv_data.len() >= 200);
        assert!(r.mean_bytes > 100);
    }
}
