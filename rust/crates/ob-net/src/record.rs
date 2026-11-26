//! TLS record protection after the handshake (step 24): decrypt in place, encrypt into a
//! reused buffer.
//!
//! rustls does the handshake: certificates, key exchange, and later key updates and session
//! tickets, through its [`rustls::kernel`] API, which exists for exactly this split (it
//! was built for kernel TLS). This module does only what happens to every record:
//!
//! ```text
//! TLS 1.3 record:  [type 23][03 03][len] [ ciphertext of (payload ‖ inner type ‖ zeros) ][ 16-byte tag ]
//!                   └──── header = AAD ────┘
//! nonce = iv XOR sequence number (RFC 8446 §5.3); the sequence number counts records per direction
//!
//! TLS 1.2 AES-GCM: [type][03 03][len] [ 8-byte explicit nonce ][ ciphertext ][ tag ]
//!   nonce = iv[..4] ‖ explicit;  AAD = seq ‖ type ‖ version ‖ plaintext length (RFC 5288)
//! TLS 1.2 ChaCha20-Poly1305: no explicit nonce; nonce = iv XOR seq (RFC 7905)
//! ```
//!
//! **Why:** rustls 0.23 decrypts each record and then allocates a `Vec` for the plaintext,
//! which our connection copied again into its own buffer: ~0.3 µs per record above the
//! AES-GCM cost, and an allocation in the tail (step 24's measurement). Here the record is
//! decrypted where it was received, and the plaintext lands directly after the previous
//! one, in place, ready for the WebSocket decoder. No allocation, no copy beyond the
//! decryption itself.
//!
//! The cryptography is `ring`'s AEADs, the same implementation rustls uses here. This
//! module only frames records, builds nonces and AADs, and counts sequence numbers. It is
//! tested against rustls itself (`tests/record_interop.rs`).

use ring::aead::{self, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use rustls::{AlertDescription, ConnectionTrafficSecrets, Error};

/// Record header: type, version (2), length (2).
pub const HEADER: usize = 5;
/// Largest plaintext a record may carry (RFC 8446 §5.1).
pub const MAX_PLAINTEXT: usize = 1 << 14;
/// Largest record body a peer may send (RFC 8446 §5.2: 2^14 + 256).
pub const MAX_BODY: usize = MAX_PLAINTEXT + 256;
/// Every AEAD here has a 16-byte tag.
const TAG: usize = 16;
/// TLS 1.2 AES-GCM's explicit nonce.
const EXPLICIT: usize = 8;

/// TLS record content types (RFC 8446 §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    /// 20: only during the handshake (middlebox compatibility).
    ChangeCipherSpec,
    /// 21: alerts (close_notify, fatal errors).
    Alert,
    /// 22: handshake messages (after the handshake: session tickets, key updates).
    Handshake,
    /// 23: application data.
    ApplicationData,
}

impl ContentType {
    fn from_u8(b: u8) -> Option<ContentType> {
        Some(match b {
            20 => ContentType::ChangeCipherSpec,
            21 => ContentType::Alert,
            22 => ContentType::Handshake,
            23 => ContentType::ApplicationData,
            _ => return None,
        })
    }

    fn to_u8(self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }
}

/// Which record format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// TLS 1.2 (RFC 5246, with RFC 5288 / RFC 7905 AEADs).
    Tls12,
    /// TLS 1.3 (RFC 8446).
    Tls13,
}

/// One direction's keys and sequence number.
pub struct Keys {
    key: LessSafeKey,
    iv: [u8; NONCE_LEN],
    seq: u64,
    version: Version,
    /// TLS 1.2 AES-GCM carries an explicit nonce in each record.
    explicit_nonce: bool,
}

impl Keys {
    /// Keys from rustls's extracted secrets, starting at sequence number `seq`.
    pub fn new(
        version: Version,
        secrets: &ConnectionTrafficSecrets,
        seq: u64,
    ) -> Result<Keys, Error> {
        let (alg, key, iv) = match secrets {
            ConnectionTrafficSecrets::Aes128Gcm { key, iv } => (&aead::AES_128_GCM, key, iv),
            ConnectionTrafficSecrets::Aes256Gcm { key, iv } => (&aead::AES_256_GCM, key, iv),
            ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => {
                (&aead::CHACHA20_POLY1305, key, iv)
            }
            _ => return Err(Error::General("unsupported cipher suite".into())),
        };
        let iv: [u8; NONCE_LEN] = iv
            .as_ref()
            .try_into()
            .map_err(|_| Error::General("bad traffic iv".into()))?;
        Keys::from_raw(version, alg, key.as_ref(), iv, seq)
    }

    /// Keys from raw material (tests, and [`Keys::new`]).
    pub fn from_raw(
        version: Version,
        alg: &'static aead::Algorithm,
        key: &[u8],
        iv: [u8; NONCE_LEN],
        seq: u64,
    ) -> Result<Keys, Error> {
        let gcm = alg != &aead::CHACHA20_POLY1305;
        let key =
            UnboundKey::new(alg, key).map_err(|_| Error::General("bad traffic key".into()))?;
        Ok(Keys {
            key: LessSafeKey::new(key),
            iv,
            seq,
            version,
            explicit_nonce: version == Version::Tls12 && gcm,
        })
    }

    /// The sequence number of the next record.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// iv XOR sequence number (TLS 1.3, and TLS 1.2 ChaCha20 / our GCM send nonces).
    fn xor_nonce(&self) -> [u8; NONCE_LEN] {
        let mut n = self.iv;
        for (a, b) in n[NONCE_LEN - 8..].iter_mut().zip(self.seq.to_be_bytes()) {
            *a ^= b;
        }
        n
    }

    fn next_seq(&mut self) -> Result<(), Error> {
        // 2^64 records can't happen in practice; refusing to wrap keeps nonces unique.
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| Error::General("sequence number exhausted".into()))?;
        Ok(())
    }

    /// Decrypts the complete record at `buf[rec..]` (header included) in place; a record
    /// that isn't wholly in `buf` is an error, never a panic. The plaintext is written to
    /// `buf[dst..]`, where `dst <= rec`: received records are decrypted straight into
    /// the space after the previous plaintext. Returns the content type and the
    /// plaintext's length.
    pub fn open(
        &mut self,
        buf: &mut [u8],
        dst: usize,
        rec: usize,
    ) -> Result<(ContentType, usize), Error> {
        debug_assert!(dst <= rec);
        let header: [u8; HEADER] = buf[rec..rec + HEADER]
            .try_into()
            .map_err(|_| Error::DecryptError)?;
        // After the handshake every record says version 3.3 (RFC 8446 §5.1; RFC 5246
        // §6.2.1). TLS 1.3 authenticates the header anyway; TLS 1.2's AAD uses a fixed
        // version, so without this check a changed version byte would go unnoticed.
        if header[1..3] != [3, 3] {
            return Err(Error::DecryptError);
        }
        let body = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if body > MAX_BODY {
            return Err(Error::PeerSentOversizedRecord);
        }
        let end = rec + HEADER + body;
        if end > buf.len() {
            // `Session::receive` only passes complete records; never trust that here.
            return Err(Error::DecryptError);
        }
        let (typ, len) = match self.version {
            Version::Tls13 => {
                // Every protected TLS 1.3 record says "application data" outside.
                if header[0] != ContentType::ApplicationData.to_u8() || body < TAG + 1 {
                    return Err(Error::DecryptError);
                }
                let nonce = Nonce::assume_unique_for_key(self.xor_nonce());
                let plain = self
                    .key
                    .open_within(
                        nonce,
                        Aad::from(header),
                        &mut buf[dst..end],
                        rec + HEADER - dst..,
                    )
                    .map_err(|_| Error::DecryptError)?;
                // Inner plaintext: payload ‖ type ‖ zero padding (RFC 8446 §5.4).
                let at = plain
                    .iter()
                    .rposition(|&b| b != 0)
                    .ok_or(Error::DecryptError)?;
                let typ = ContentType::from_u8(plain[at]).ok_or(Error::DecryptError)?;
                (typ, at)
            }
            Version::Tls12 => {
                let typ = ContentType::from_u8(header[0]).ok_or(Error::DecryptError)?;
                let prefix = if self.explicit_nonce { EXPLICIT } else { 0 };
                if body < prefix + TAG {
                    return Err(Error::DecryptError);
                }
                let nonce = if self.explicit_nonce {
                    let mut n = [0_u8; NONCE_LEN];
                    n[..4].copy_from_slice(&self.iv[..4]);
                    n[4..].copy_from_slice(&buf[rec + HEADER..rec + HEADER + EXPLICIT]);
                    n
                } else {
                    self.xor_nonce()
                };
                let aad = tls12_aad(self.seq, header[0], body - prefix - TAG);
                let plain = self
                    .key
                    .open_within(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad),
                        &mut buf[dst..end],
                        rec + HEADER + prefix - dst..,
                    )
                    .map_err(|_| Error::DecryptError)?;
                (typ, plain.len())
            }
        };
        if len > MAX_PLAINTEXT {
            return Err(Error::PeerSentOversizedRecord);
        }
        self.next_seq()?;
        Ok((typ, len))
    }

    /// Appends one protected record carrying `payload` (at most [`MAX_PLAINTEXT`]) to
    /// `out`. Allocates only if `out` must grow.
    pub fn seal(
        &mut self,
        typ: ContentType,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if payload.len() > MAX_PLAINTEXT {
            return Err(Error::General("record payload too large".into()));
        }
        let nonce = self.xor_nonce();
        let start = out.len();
        match self.version {
            Version::Tls13 => {
                let body = payload.len() + 1 + TAG;
                let header = record_header(ContentType::ApplicationData.to_u8(), body);
                out.extend_from_slice(&header);
                out.extend_from_slice(payload);
                out.push(typ.to_u8());
                let tag = self
                    .key
                    .seal_in_place_separate_tag(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(header),
                        &mut out[start + HEADER..],
                    )
                    .map_err(|_| Error::EncryptError)?;
                out.extend_from_slice(tag.as_ref());
            }
            Version::Tls12 => {
                let prefix = if self.explicit_nonce { EXPLICIT } else { 0 };
                let header = record_header(typ.to_u8(), prefix + payload.len() + TAG);
                out.extend_from_slice(&header);
                if self.explicit_nonce {
                    // As rustls: the explicit part of (iv XOR seq), unique per record.
                    out.extend_from_slice(&nonce[4..]);
                }
                out.extend_from_slice(payload);
                let aad = tls12_aad(self.seq, typ.to_u8(), payload.len());
                let tag = self
                    .key
                    .seal_in_place_separate_tag(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad),
                        &mut out[start + HEADER + prefix..],
                    )
                    .map_err(|_| Error::EncryptError)?;
                out.extend_from_slice(tag.as_ref());
            }
        }
        self.next_seq()
    }
}

fn record_header(typ: u8, body: usize) -> [u8; HEADER] {
    // `body` ≤ MAX_PLAINTEXT + 1 + TAG + EXPLICIT < 2^16, checked by the callers.
    let len = u16::try_from(body).unwrap_or(u16::MAX).to_be_bytes();
    [typ, 3, 3, len[0], len[1]]
}

fn tls12_aad(seq: u64, typ: u8, plain_len: usize) -> [u8; 13] {
    let mut a = [0_u8; 13];
    a[..8].copy_from_slice(&seq.to_be_bytes());
    a[8] = typ;
    a[9] = 3;
    a[10] = 3;
    let len = u16::try_from(plain_len).unwrap_or(u16::MAX).to_be_bytes();
    a[11] = len[0];
    a[12] = len[1];
    a
}

/// The error for a fatal alert from the peer.
pub(crate) fn alert_error(description: u8) -> Error {
    Error::AlertReceived(AlertDescription::from(description))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)] // test data: i % 251 fits
mod tests {
    use super::*;

    fn keys(version: Version, kind: u8) -> Keys {
        let (alg, key): (&'static aead::Algorithm, &[u8]) = match kind {
            0 => (&aead::AES_128_GCM, &[1; 16]),
            1 => (&aead::AES_256_GCM, &[2; 32]),
            _ => (&aead::CHACHA20_POLY1305, &[3; 32]),
        };
        Keys::from_raw(version, alg, key, [9; NONCE_LEN], 0).unwrap()
    }

    fn pair(version: Version, kind: u8) -> (Keys, Keys) {
        (keys(version, kind), keys(version, kind))
    }

    fn all() -> impl Iterator<Item = (Version, u8)> {
        [Version::Tls12, Version::Tls13]
            .into_iter()
            .flat_map(|v| (0..3).map(move |k| (v, k)))
    }

    /// Seal then open, for every version and cipher, at sizes 0 to the maximum, several
    /// records in a row (sequence numbers advance in step).
    #[test]
    fn round_trip_in_place_every_suite_and_size() {
        for (v, k) in all() {
            let (mut tx, mut rx) = pair(v, k);
            for len in [0, 1, 15, 16, 17, 1000, MAX_PLAINTEXT] {
                let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let mut wire = vec![0xEE_u8; 7]; // something before: dst < rec
                tx.seal(ContentType::ApplicationData, &payload, &mut wire)
                    .unwrap();
                let (typ, n) = rx.open(&mut wire, 3, 7).unwrap();
                assert_eq!((typ, n), (ContentType::ApplicationData, len), "{v:?} {k}");
                assert_eq!(&wire[3..3 + n], &payload[..]);
                assert_eq!(&wire[..3], &[0xEE; 3], "nothing before dst is touched");
            }
            assert_eq!((tx.seq(), rx.seq()), (7, 7));
        }
    }

    #[test]
    fn content_types_survive() {
        for (v, k) in all() {
            let (mut tx, mut rx) = pair(v, k);
            for typ in [ContentType::Alert, ContentType::Handshake] {
                let mut wire = Vec::new();
                tx.seal(typ, &[1, 2], &mut wire).unwrap();
                assert_eq!(rx.open(&mut wire, 0, 0).unwrap(), (typ, 2));
            }
        }
    }

    /// Any changed byte (header, nonce, ciphertext or tag) is rejected, and so is a record
    /// replayed or taken out of order (the sequence number is part of the nonce/AAD).
    #[test]
    fn tampering_and_reordering_are_rejected() {
        for (v, k) in all() {
            let (mut tx, mut rx) = pair(v, k);
            let mut wire = Vec::new();
            tx.seal(ContentType::ApplicationData, b"hello", &mut wire)
                .unwrap();
            for i in 0..wire.len() {
                // A fresh receiver each time: a failed open must not depend on state.
                let (_, mut rx2) = pair(v, k);
                let mut bad = wire.clone();
                bad[i] ^= 0x01;
                assert!(rx2.open(&mut bad, 0, 0).is_err(), "{v:?} {k} byte {i}");
            }
            let mut second = Vec::new();
            tx.seal(ContentType::ApplicationData, b"world", &mut second)
                .unwrap();
            let mut out_of_order = second.clone();
            assert!(
                rx.open(&mut out_of_order, 0, 0).is_err(),
                "record 1 as record 0"
            );
            let mut first = wire.clone();
            rx.open(&mut first, 0, 0).unwrap();
            let mut replay = wire.clone();
            assert!(rx.open(&mut replay, 0, 0).is_err(), "record 0 again");
        }
    }

    #[test]
    fn oversized_records_are_rejected() {
        let (_, mut rx) = pair(Version::Tls13, 0);
        let mut big = vec![23, 3, 3, 0, 0];
        let len = u16::try_from(MAX_BODY + 1).unwrap().to_be_bytes();
        big[3..5].copy_from_slice(&len);
        big.resize(HEADER + MAX_BODY + 1, 0);
        assert!(matches!(
            rx.open(&mut big, 0, 0),
            Err(Error::PeerSentOversizedRecord)
        ));
        let (mut tx, _) = pair(Version::Tls13, 0);
        assert!(
            tx.seal(
                ContentType::ApplicationData,
                &vec![0; MAX_PLAINTEXT + 1],
                &mut Vec::new()
            )
            .is_err()
        );
    }

    /// TLS 1.3 padding: zeros after the inner type are removed; all zeros is an error.
    #[test]
    fn tls13_padding_is_stripped() {
        let (tx, mut rx) = pair(Version::Tls13, 0);
        // Build a padded record by hand: payload ‖ type ‖ 0 0 0.
        let mut inner = b"abc".to_vec();
        inner.extend_from_slice(&[23, 0, 0, 0]);
        let header = record_header(23, inner.len() + TAG);
        let mut wire = header.to_vec();
        wire.extend_from_slice(&inner);
        let tag = tx
            .key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(tx.xor_nonce()),
                Aad::from(header),
                &mut wire[HEADER..],
            )
            .unwrap();
        wire.extend_from_slice(tag.as_ref());
        assert_eq!(
            rx.open(&mut wire, 0, 0).unwrap(),
            (ContentType::ApplicationData, 3)
        );
        assert_eq!(&wire[..3], b"abc");
    }
}
