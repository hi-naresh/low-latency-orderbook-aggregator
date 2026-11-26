//! `ob-simd`: SIMD kernels for the parser.
//!
//! Exchanges send each price level as `["60200.50","1.234"]`. The scalar parser walks that
//! byte by byte, and its loops end at a place that depends on each number's length: the
//! branch predictor misses about once per number (~11 ns per number, mostly that). Here
//! one call handles a **whole level** from a 32-byte window:
//!
//! ```text
//! 1. two 16-byte loads; compare all 32 bytes against '"', '.', and the digit range at once
//!    → three 32-bit masks
//! 2. the four quotes, from the quote mask (count trailing zeros, clear lowest bit)
//!    → where each number starts and ends; check '[', '","' and '"]' sit where they must
//! 3. every byte of each number must be a digit or a dot; at most one dot, not first or last
//! 4. one byte shuffle per number gathers its digits (skipping the dot) right-aligned into
//!    eight lanes: price in lanes 0–7, quantity in lanes 8–15
//! 5. three multiply-adds turn the 16 digits into two integers at once
//! ```
//!
//! No branch depends on a digit. A level that doesn't fit this exact shape (whitespace,
//! escapes, more than 8 digits, fewer than 32 bytes left, ...) returns `None`, and the
//! caller parses it the scalar way: the results are identical by construction, and tests
//! compare the two on random and adversarial input.
//!
//! **Platforms.** On x86-64 this needs SSSE3 (the byte shuffle) and SSE4.1. Both are on
//! by default for macOS targets (target CPU "penryn") and on any x86-64 CPU made since
//! 2008, so no run-time detection is needed. Where they aren't enabled at compile time,
//! [`level`] always returns `None` (scalar everywhere). An ARM NEON version is future
//! work (`docs/LINUX.md`).
//!
//! This is one of two crates allowed `unsafe` (with `ob-os`): the loads take raw
//! pointers. Each `unsafe` block reads only inside the 32-byte array it was given.
#![no_std]

/// The two numbers of one level, as digit strings converted to integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelDigits {
    /// The price's digits as an integer (the point removed): "60200.50" → 6020050.
    pub px: u32,
    /// How many of the price's digits were after the point (2 above).
    pub px_frac: u8,
    /// The quantity's digits as an integer.
    pub qty: u32,
    /// Digits after the quantity's point.
    pub qty_frac: u8,
    /// Bytes consumed: from the level's `[` through its `]`.
    pub len: u8,
}

/// True when [`level`] is the SIMD kernel; false when it always returns `None`.
pub const ACCELERATED: bool = cfg!(all(
    target_arch = "x86_64",
    target_feature = "ssse3",
    target_feature = "sse4.1"
));

/// Parses the level at the start of `w` (`["digits","digits"]`, each number with at most
/// 8 digits and at most one `.`, neither first nor last), or `None` if it isn't exactly
/// that. `w` must start at the level's `[`; bytes after its `]` are ignored.
#[inline]
pub fn level(w: &[u8; 32]) -> Option<LevelDigits> {
    imp::level(w)
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_feature = "ssse3",
    target_feature = "sse4.1"
)))]
mod imp {
    use super::LevelDigits;

    #[inline]
    pub(super) fn level(_: &[u8; 32]) -> Option<LevelDigits> {
        None
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_feature = "ssse3",
    target_feature = "sse4.1"
))]
mod imp {
    use core::arch::x86_64::{
        __m128i, _mm_add_epi8, _mm_cmpeq_epi8, _mm_cmpgt_epi8, _mm_cvtsi128_si32,
        _mm_extract_epi32, _mm_loadu_si128, _mm_madd_epi16, _mm_maddubs_epi16, _mm_min_epu8,
        _mm_movemask_epi8, _mm_or_si128, _mm_packus_epi32, _mm_set1_epi8, _mm_setr_epi8,
        _mm_setr_epi16, _mm_shuffle_epi8, _mm_sub_epi8,
    };

    use super::LevelDigits;

    #[inline]
    pub(super) fn level(w: &[u8; 32]) -> Option<LevelDigits> {
        // SAFETY: this module is only compiled when SSSE3 and SSE4.1 are enabled for the
        // whole build (the `cfg` above), so every CPU this binary runs on has them.
        unsafe { kernel(w) }
    }

    /// 16 bytes of `w` from `at` (`at` ≤ 16).
    #[inline]
    #[target_feature(enable = "ssse3,sse4.1")]
    fn load(w: &[u8; 32], at: usize) -> __m128i {
        let at = at.min(16);
        // SAFETY: `at + 16 <= 32`, so the 16 bytes read lie inside `w`; `_mm_loadu_si128`
        // has no alignment requirement.
        unsafe { _mm_loadu_si128(w.as_ptr().add(at).cast()) }
    }

    /// One bit per byte of the 32-byte window: the high bit of each byte of `lo`, `hi`.
    #[inline]
    #[target_feature(enable = "ssse3,sse4.1")]
    fn bits(lo: __m128i, hi: __m128i) -> u32 {
        let l = _mm_movemask_epi8(lo).cast_unsigned() & 0xFFFF;
        let h = _mm_movemask_epi8(hi).cast_unsigned() & 0xFFFF;
        l | h << 16
    }

    /// One number of the level: `len` bytes from `start`, with the window's digit and
    /// dot masks. Returns (digit count, digits after the point, dot position relative to
    /// the start, or 16 if none).
    #[inline]
    fn shape(start: usize, len: usize, digits: u32, dots: u32) -> Option<(usize, usize, i8)> {
        if !(1..=9).contains(&len) {
            return None;
        }
        let range = ((1_u32 << len) - 1) << start;
        if range & !(digits | dots) != 0 {
            return None; // something other than a digit or a dot
        }
        let dot = dots & range;
        if dot == 0 {
            return (len <= 8).then_some((len, 0, 16));
        }
        if dot & (dot - 1) != 0 || dot == 1 << start || dot == 1 << (start + len - 1) {
            return None; // two dots, or ".5" / "5."
        }
        let rel = dot.trailing_zeros() as usize - start; // < 9: fits an i8
        Some((len - 1, len - 1 - rel, i8::try_from(rel).unwrap_or(16)))
    }

    /// The shuffle that gathers `n` digits (skipping a dot at `dot`) right-aligned into
    /// the eight output lanes ending at lane `last`. Lanes that must be zero get an index
    /// with the high bit set, which `pshufb` turns into 0.
    #[inline]
    #[target_feature(enable = "ssse3,sse4.1")]
    fn gather(n: usize, dot: i8, last: i8, other_half: __m128i) -> __m128i {
        let iota = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        // Lane k takes digit j = k - (last + 1 - n); a negative j → a zero lane.
        let shift = i8::try_from(n).unwrap_or(0) - (last + 1);
        let idx = _mm_add_epi8(iota, _mm_set1_epi8(shift));
        // Digits after the dot sit one byte further on (cmpgt gives -1 there).
        let idx = _mm_sub_epi8(idx, _mm_cmpgt_epi8(idx, _mm_set1_epi8(dot - 1)));
        _mm_or_si128(idx, other_half)
    }

    #[inline]
    #[target_feature(enable = "ssse3,sse4.1")]
    fn kernel(w: &[u8; 32]) -> Option<LevelDigits> {
        if w[0] != b'[' || w[1] != b'"' {
            return None;
        }
        let (lo, hi) = (load(w, 0), load(w, 16));
        let quote = _mm_set1_epi8(b'"'.cast_signed());
        let point = _mm_set1_epi8(b'.'.cast_signed());
        let zero = _mm_set1_epi8(b'0'.cast_signed());
        let nine = _mm_set1_epi8(9);
        let quotes = bits(_mm_cmpeq_epi8(lo, quote), _mm_cmpeq_epi8(hi, quote));
        let dots = bits(_mm_cmpeq_epi8(lo, point), _mm_cmpeq_epi8(hi, point));
        let (dl, dh) = (_mm_sub_epi8(lo, zero), _mm_sub_epi8(hi, zero));
        let digits = bits(
            _mm_cmpeq_epi8(_mm_min_epu8(dl, nine), dl),
            _mm_cmpeq_epi8(_mm_min_epu8(dh, nine), dh),
        );
        // Quotes: q0 = 1 (checked above), then q1, q2, q3.
        let rest = quotes & quotes.wrapping_sub(1);
        let q1 = rest.trailing_zeros() as usize;
        let rest = rest & rest.wrapping_sub(1);
        let q2 = rest.trailing_zeros() as usize;
        let rest = rest & rest.wrapping_sub(1);
        let q3 = rest.trailing_zeros() as usize;
        // Shape ["A","B"]: q2 == q1 + 2 with ',' between, and ']' right after q3.
        if q3 >= 31 || q2 != q1 + 2 || w[q1 + 1] != b',' || w[q3 + 1] != b']' {
            return None;
        }
        let (a, b) = (2, q2 + 1);
        let (na, fa, da) = shape(a, q1 - a, digits, dots)?;
        let (nb, fb, db) = shape(b, q3 - b, digits, dots)?;
        // A has at most 9 bytes, so q1 ≤ 11 and b ≤ 14: both 16-byte loads fit.
        let va = _mm_sub_epi8(load(w, a), zero);
        let vb = _mm_sub_epi8(load(w, b), zero);
        let high_half = _mm_setr_epi8(
            0, 0, 0, 0, 0, 0, 0, 0, -128, -128, -128, -128, -128, -128, -128, -128,
        );
        let ia = gather(na, da, 7, high_half);
        let ib = gather(nb, db, 15, _mm_set1_epi8(0));
        let v = _mm_or_si128(_mm_shuffle_epi8(va, ia), _mm_shuffle_epi8(vb, ib));
        // 16 digits → 8 two-digit → 4 four-digit → 2 eight-digit numbers.
        let v = _mm_maddubs_epi16(
            v,
            _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1),
        );
        let v = _mm_madd_epi16(v, _mm_setr_epi16(100, 1, 100, 1, 100, 1, 100, 1));
        let v = _mm_packus_epi32(v, v);
        let v = _mm_madd_epi16(v, _mm_setr_epi16(10000, 1, 10000, 1, 10000, 1, 10000, 1));
        Some(LevelDigits {
            px: _mm_cvtsi128_si32(v).cast_unsigned(),
            px_frac: u8::try_from(fa).unwrap_or(0),
            qty: _mm_extract_epi32::<1>(v).cast_unsigned(),
            qty_frac: u8::try_from(fb).unwrap_or(0),
            len: u8::try_from(q3 + 2).unwrap_or(0),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    use proptest::prelude::*;

    use super::*;

    /// The reference: what `level` must return, computed the obvious way.
    fn model(w: &[u8; 32]) -> Option<LevelDigits> {
        let s = w;
        if s[0] != b'[' || s[1] != b'"' {
            return None;
        }
        let q1 = 2 + s[2..].iter().position(|&c| c == b'"')?;
        if s.get(q1 + 1) != Some(&b',') || s.get(q1 + 2) != Some(&b'"') {
            return None;
        }
        let b = q1 + 3;
        let q3 = b + s[b..].iter().position(|&c| c == b'"')?;
        if q3 >= 31 || s[q3 + 1] != b']' {
            return None;
        }
        let num = |t: &[u8]| -> Option<(u32, u8)> {
            if t.is_empty() || t.len() > 9 {
                return None;
            }
            let dots = t.iter().filter(|&&c| c == b'.').count();
            if dots > 1 || t[0] == b'.' || t[t.len() - 1] == b'.' {
                return None;
            }
            if !t.iter().all(|&c| c.is_ascii_digit() || c == b'.') {
                return None;
            }
            let digits: Vec<u8> = t.iter().copied().filter(u8::is_ascii_digit).collect();
            if digits.len() > 8 {
                return None;
            }
            let v = digits
                .iter()
                .fold(0_u32, |v, &d| v * 10 + u32::from(d - b'0'));
            let frac = t
                .iter()
                .position(|&c| c == b'.')
                .map_or(0, |p| t.len() - 1 - p);
            Some((v, u8::try_from(frac).unwrap()))
        };
        let (px, px_frac) = num(&s[2..q1])?;
        let (qty, qty_frac) = num(&s[b..q3])?;
        Some(LevelDigits {
            px,
            px_frac,
            qty,
            qty_frac,
            len: u8::try_from(q3 + 2).unwrap(),
        })
    }

    fn window(text: &str, fill: u8) -> [u8; 32] {
        let mut w = [fill; 32];
        let n = text.len().min(32);
        w[..n].copy_from_slice(&text.as_bytes()[..n]);
        w
    }

    #[test]
    fn real_levels() {
        if !ACCELERATED {
            return;
        }
        let w = window(r#"["60200.50","1.234"],["60200.40","0.010"]"#, b' ');
        assert_eq!(
            level(&w),
            Some(LevelDigits {
                px: 6_020_050,
                px_frac: 2,
                qty: 1234,
                qty_frac: 3,
                len: 20
            })
        );
        let w = window(r#"["9825.9","0"]]"#, b'x');
        assert_eq!(
            level(&w),
            Some(LevelDigits {
                px: 98259,
                px_frac: 1,
                qty: 0,
                qty_frac: 0,
                len: 14
            })
        );
        for bad in [
            r#"[ "1.0","2"]"#,      // whitespace
            r#"["1.","2"]"#,        // trailing dot
            r#"[".5","2"]"#,        // leading dot
            r#"["1.2.3","2"]"#,     // two dots
            r#"["123456789","2"]"#, // 9 digits
            r#"["1","2" ]"#,        // space before ]
            r#"["1";"2"]"#,         // wrong separator
            r#"["1","-2"]"#,        // sign
            r#"["1","2\""]"#,       // escape
            r#"["","2"]"#,          // empty
        ] {
            assert_eq!(level(&window(bad, b']')), None, "{bad}");
            assert_eq!(model(&window(bad, b']')), None, "{bad}");
        }
    }

    fn arb_number() -> impl Strategy<Value = String> {
        prop_oneof![
            // Realistic: digits, maybe a point.
            ("[0-9]{1,6}", proptest::option::of("[0-9]{0,4}")).prop_map(|(i, f)| match f {
                Some(f) => std::format!("{i}.{f}"),
                None => i,
            }),
            // Anything short from a hostile alphabet.
            "[0-9.\"\\\\,\\] e+-]{0,10}",
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]
        /// The kernel agrees with the model on realistic and hostile levels, whatever
        /// follows the level in the window.
        #[test]
        fn kernel_matches_model(a in arb_number(), b in arb_number(), fill in any::<u8>(),
                                tail in "[\\],\\[\" 0-9]{0,6}") {
            let text = std::format!(r#"["{a}","{b}"]{tail}"#);
            let w = window(&text, fill);
            if ACCELERATED {
                prop_assert_eq!(level(&w), model(&w), "{}", text);
            } else {
                prop_assert_eq!(level(&w), None);
            }
        }

        /// Any 32 bytes at all: never a panic, and agreement with the model.
        #[test]
        fn arbitrary_bytes(w in proptest::array::uniform32(any::<u8>())) {
            let got = level(&w);
            if ACCELERATED {
                prop_assert_eq!(got, model(&w));
            }
        }
    }
}
