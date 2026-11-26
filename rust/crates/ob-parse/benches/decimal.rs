//! Decimal parser benchmarks (criterion).
//!
//! Corpus: 65,536 numbers in exchange formats, half prices and half quantities, stored
//! back to back in one buffer like the fields of a real JSON message. The corpus is too
//! large for the branch predictor to memorise (see step 4, trap 3), and every parser is
//! called through a generic closure so it is inlined (step 4, trap 1).
//!
//! Run: `cargo bench -p ob-parse --bench decimal`
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    missing_docs
)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob_core::{InstrumentSpec, ParseFixedError};

const COUNT: usize = 65_536;

struct Corpus {
    buf: Vec<u8>,
    /// (start, end, decimals) of each number in `buf`.
    fields: Vec<(usize, usize, u8)>,
}

/// Deterministic numbers in the formats Binance and Bybit send for BTCUSDT.
///
/// `mixed = true`: every price picks a format at random (Binance-style `105123.40`,
/// Bybit-style `105123.4`, or whole), a worst case for branch prediction.
/// `mixed = false`: one venue's consistent format (Binance: prices always with two
/// decimals, quantities with three), which is what a real feed looks like.
fn corpus(mixed: bool) -> Corpus {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut buf = Vec::new();
    let mut fields = Vec::with_capacity(COUNT);
    for i in 0..COUNT {
        let r = next();
        let start = buf.len();
        let decimals = if i % 2 == 0 {
            // Prices around 105,000 with a 0.1 tick.
            let ticks = 1_050_000 + r % 20_000;
            let format = if mixed { r >> 32 & 3 } else { 0 };
            let text = match format {
                0 | 1 => format!("{}.{}0", ticks / 10, ticks % 10), // Binance "105123.40"
                2 => format!("{}.{}", ticks / 10, ticks % 10),      // Bybit "105123.4"
                _ => format!("{}", ticks / 10),                     // whole number
            };
            buf.extend_from_slice(text.as_bytes());
            1
        } else {
            // Quantities with a 0.001 lot, mostly small, sometimes large.
            let lots = match r >> 40 & 7 {
                0..=4 => 1 + r % 2_000,
                5 | 6 => 1 + r % 50_000,
                _ => 1 + r % 5_000_000,
            };
            buf.extend_from_slice(format!("{}.{:03}", lots / 1000, lots % 1000).as_bytes());
            3
        };
        fields.push((start, buf.len(), decimals));
    }
    buf.extend_from_slice(&[b' '; 16]); // padding: every number has a 16-byte window
    Corpus { buf, fields }
}

/// One number to parse: its text, the 16-byte window starting at it (for
/// `swar_padded`), and its decimals. Prepared up front so the timed loop does no slicing.
type Field<'a> = (&'a [u8], &'a [u8; 16], u8);

fn fields(c: &Corpus) -> Vec<Field<'_>> {
    c.fields
        .iter()
        .map(|&(s, e, d)| {
            (
                &c.buf[s..e],
                c.buf[s..s + 16].first_chunk::<16>().unwrap(),
                d,
            )
        })
        .collect()
}

fn run<F: Fn(&[u8], &[u8; 16], u8) -> Result<i64, ParseFixedError>>(
    f: &[Field<'_>],
    parse: F,
) -> i64 {
    let mut acc = 0_i64;
    for &(text, window, d) in black_box(f) {
        acc = acc.wrapping_add(parse(text, window, d).unwrap());
    }
    acc
}

fn bench_decimal(c: &mut Criterion) {
    for (name, mixed) in [("mixed", true), ("binance", false)] {
        let corpus = corpus(mixed);
        let f = fields(&corpus);
        let reference = |t: &[u8], _: &[u8; 16], d| ob_core::parse_decimal(t, d);
        let scalar = |t: &[u8], _: &[u8; 16], d| ob_parse::decimal::scalar(t, d);
        let swar = |t: &[u8], _: &[u8; 16], d| ob_parse::decimal::swar(t, d);
        let padded = |t: &[u8], w: &[u8; 16], d| ob_parse::decimal::swar_padded(t, w, d);
        // All parsers must agree on the whole corpus before we time them.
        let want = run(&f, reference);
        assert_eq!(run(&f, scalar), want);
        assert_eq!(run(&f, swar), want);
        assert_eq!(run(&f, padded), want);

        let mut group = c.benchmark_group(format!("decimal/{name}"));
        group.throughput(Throughput::Elements(COUNT as u64));
        group.bench_function("reference", |b| b.iter(|| run(&f, reference)));
        group.bench_function("scalar", |b| b.iter(|| run(&f, scalar)));
        group.bench_function("swar", |b| b.iter(|| run(&f, swar)));
        group.bench_function("swar_padded", |b| b.iter(|| run(&f, padded)));
        // What a quick implementation often does: parse as f64 (inexact; context only).
        group.bench_function("f64_baseline", |b| {
            b.iter(|| {
                run(&f, |t, _, _| {
                    let x: f64 = std::str::from_utf8(t).unwrap().parse().unwrap();
                    Ok(x.to_bits() as i64)
                })
            });
        });
        group.finish();
    }

    let corpus = corpus(false);
    let f = fields(&corpus);
    // Full hot-path call: parse + tick/lot validation.
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let mut group = c.benchmark_group("validated");
    group.throughput(Throughput::Elements(COUNT as u64));
    group.bench_function("spec_reference", |b| {
        b.iter(|| {
            run(&f, |t, _, d| {
                if d == 1 {
                    spec.parse_px(t).map(|p| p.raw())
                } else {
                    spec.parse_qty(t).map(|q| q.raw())
                }
            })
        });
    });
    group.bench_function("ob_parse_fast", |b| {
        b.iter(|| {
            run(&f, |t, _, d| {
                if d == 1 {
                    ob_parse::parse_px(&spec, t).map(|p| p.raw())
                } else {
                    ob_parse::parse_qty(&spec, t).map(|q| q.raw())
                }
            })
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = bench_decimal
}
criterion_main!(benches);
