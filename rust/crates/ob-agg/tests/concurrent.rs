//! Two feed threads publish continuously while an aggregator thread refreshes and
//! queries. Every view must be internally consistent per venue (each venue's book is a
//! whole publish), and the cross-venue results must be correctly ordered.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::print_stdout
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use ob_agg::{Aggregator, Config, VenueConfig};
use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, VenueStatus, book_channel};

/// Publish `k` of venue `v`: 20 levels per side, every quantity = k, prices shifted by k
/// (venue 1 one tick higher), so a torn copy is detectable.
fn book_for(v: i64, k: i64) -> OrderBook {
    let q = Qty::from_raw(k).unwrap();
    let bids: Vec<Level> = (0..20)
        .map(|i| Level::new(Px::from_raw(100_000 + v + k % 1000 - i), q))
        .collect();
    let asks: Vec<Level> = (0..20)
        .map(|i| Level::new(Px::from_raw(101_000 + v + k % 1000 + i), q))
        .collect();
    let mut b: OrderBook = OrderBook::new();
    b.replace(&bids, &asks).unwrap();
    b
}

#[test]
fn views_stay_consistent_while_two_feeds_publish() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    let mut writers = Vec::new();
    for v in 0..2_i64 {
        let (mut p, r) = book_channel();
        agg.add_venue(
            VenueConfig {
                name: "v",
                depth: 20,
                taker_fee_ppm: 0,
            },
            r,
        )
        .unwrap();
        let stop = Arc::clone(&stop);
        writers.push(thread::spawn(move || {
            let mut k = 0_i64;
            while !stop.load(Ordering::Relaxed) {
                k += 1;
                p.publish(
                    &book_for(v, k),
                    BookMeta {
                        update_id: k as u64,
                        exchange_ts: 0,
                        recv_ns: 0,
                        status: VenueStatus::Live,
                    },
                );
            }
            k
        }));
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(if cfg!(miri) { 0 } else { 2 });
    let (mut refreshes, mut copies) = (0_u64, 0_u64);
    loop {
        copies += agg.refresh() as u64;
        refreshes += 1;
        for (i, id) in agg.venue_ids().enumerate() {
            let snap = agg.snapshot(id).unwrap();
            let k = snap.meta.update_id as i64;
            if k == 0 {
                continue;
            }
            let expected = book_for(i as i64, k);
            assert_eq!(
                snap.bids(),
                expected.bids().iter().collect::<Vec<_>>(),
                "venue {i} torn"
            );
            assert_eq!(
                snap.asks(),
                expected.asks().iter().collect::<Vec<_>>(),
                "venue {i} torn"
            );
        }
        let bids = agg.best_bids(0);
        assert!(bids.windows(2).all(|w| w[0].px >= w[1].px));
        let vol = agg.bid_volume_at_or_above(Px::from_raw(0), 0);
        assert_eq!(vol.iter().map(|v| v.levels).sum::<usize>(), bids.len() * 20);
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let publishes: i64 = writers.into_iter().map(|w| w.join().unwrap()).sum();
    println!("publishes: {publishes}, refreshes: {refreshes}, venue copies: {copies}");
    assert!(refreshes > 0);
}
