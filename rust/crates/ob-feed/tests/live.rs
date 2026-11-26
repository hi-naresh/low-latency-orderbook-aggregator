//! Both feeds live (needs the internet; ignored by default): each venue on its own
//! thread running the real driver, an aggregator on the test thread reading the
//! published books, as the finished system will.
//!
//! Run: `cargo test -p ob-feed --release --test live -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ob_agg::{Action, Aggregator, Config, VenueConfig};
use ob_core::InstrumentSpec;
use ob_feed::driver::{self, DriverEvent};
use ob_feed::{Binance, Bybit, Feed, FeedConfig, FeedStats, Venue, mono_ns};
use ob_sync::book_channel;

fn spawn<V: Venue + Send + 'static>(
    venue: V,
    stop: Arc<AtomicBool>,
) -> (ob_sync::BookReader, thread::JoinHandle<FeedStats>) {
    let (publisher, reader) = book_channel();
    let handle = thread::spawn(move || {
        let cfg = driver::connection_config(&venue).unwrap();
        let mut feed = Feed::new(venue, publisher, FeedConfig::default(), mono_ns());
        driver::run(&mut feed, &cfg, &stop, |name, event| match event {
            DriverEvent::Feed(e) => println!("[{name}] {e:?}"),
            DriverEvent::ConnectFailed(e)
            | DriverEvent::ConnectionLost(e)
            | DriverEvent::SnapshotFailed(e) => {
                println!("[{name}] {e}")
            }
        });
        feed.stats()
    });
    (reader, handle)
}

#[test]
#[ignore = "needs the internet"]
fn both_feeds_live_with_the_aggregator() {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (bn_reader, bn) = spawn(Binance::new(spec, "BTCUSDT"), Arc::clone(&stop));
    let (bb_reader, bb) = spawn(Bybit::new(spec, "BTCUSDT"), Arc::clone(&stop));

    let mut agg = Aggregator::new(Config::default());
    agg.add_venue(
        VenueConfig {
            name: "binance",
            depth: 20,
            taker_fee_ppm: 500,
        },
        bn_reader,
    )
    .unwrap();
    agg.add_venue(
        VenueConfig {
            name: "bybit",
            depth: 50,
            taker_fee_ppm: 550,
        },
        bb_reader,
    )
    .unwrap();

    let start = Instant::now();
    let mut both_live_seen = false;
    while start.elapsed() < Duration::from_secs(20) {
        thread::sleep(Duration::from_secs(2));
        agg.refresh();
        let now = mono_ns();
        let states: Vec<String> = agg
            .venue_ids()
            .map(|id| {
                format!(
                    "{}={:?}",
                    agg.venue(id).unwrap().name,
                    agg.state(id, now).unwrap()
                )
            })
            .collect();
        let (bids, asks) = (agg.best_bids(now), agg.best_asks(now));
        both_live_seen |= bids.len() == 2;
        let quote = |q: &ob_agg::Quote| {
            format!(
                "{} {} @{}",
                spec.fmt_qty(q.qty),
                spec.fmt_px(q.px),
                agg.venue(q.venue).unwrap().name
            )
        };
        let plan = agg.optimal_execution(Action::Buy, spec.parse_qty("1").unwrap(), now);
        println!(
            "t={:>2}s {} | best bid {} | best ask {} | buy 1 BTC: vwap {} incl. fees {} via {:?} | arb: {:?}",
            start.elapsed().as_secs(),
            states.join(" "),
            bids.first().map_or("-".into(), quote),
            asks.first().map_or("-".into(), quote),
            plan.vwap()
                .map_or("-".into(), |p| spec.fmt_px(p).to_string()),
            plan.vwap_including_fees()
                .map_or("-".into(), |p| spec.fmt_px(p).to_string()),
            plan.children()
                .iter()
                .map(|c| (
                    agg.venue(c.venue).unwrap().name,
                    spec.fmt_qty(c.qty).to_string()
                ))
                .collect::<Vec<_>>(),
            agg.scan_arbitrage(now)
                .best
                .map(|a| spec.fmt_notional(a.net_profit).to_string()),
        );
    }
    stop.store(true, Ordering::Relaxed);
    let (bn, bb) = (bn.join().unwrap(), bb.join().unwrap());
    println!("binance {bn:?}\nbybit   {bb:?}");
    assert!(both_live_seen, "both venues should have been live together");
    assert!(bn.applied > 100 && bb.applied > 100);
}
