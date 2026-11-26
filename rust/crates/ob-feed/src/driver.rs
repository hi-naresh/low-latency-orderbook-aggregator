//! The I/O loop that runs one feed: a dedicated thread per venue.
//!
//! It owns the connection and does only I/O; every decision is the [`Feed`]'s. Per
//! iteration: read the clock, poll the socket (or connect, when allowed), hand what
//! happened to the feed, then carry out the feed's commands.
//!
//! While connected the loop never sleeps (spin mode, step 12): an idle poll costs one
//! non-blocking `read` and a timer check. While disconnected it sleeps 1 ms between
//! checks; nothing is time-critical then.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ob_net::ws::Message;
use ob_net::{ConnConfig, NetError, WsConnection};

use crate::backoff::mono_ns;
use crate::feed::{Command, Commands, Feed, FeedEvent};
use crate::probe::Probe;
use crate::venue::Venue;

/// What the driver reports (for logging).
#[derive(Debug)]
pub enum DriverEvent<'a> {
    /// A decision or state change from the feed.
    Feed(FeedEvent),
    /// A connection attempt failed.
    ConnectFailed(&'a NetError),
    /// An established connection failed.
    ConnectionLost(&'a NetError),
    /// A REST snapshot fetch failed (full depth, step 19).
    SnapshotFailed(&'a NetError),
}

/// Runs `feed` until `stop` is set. `conn_cfg` is the venue's connection settings (from
/// [`connection_config`]); `log` receives events.
pub fn run<V: Venue, P: Probe>(
    feed: &mut Feed<V, P>,
    conn_cfg: &ConnConfig,
    stop: &AtomicBool,
    log: impl FnMut(&'static str, DriverEvent<'_>),
) {
    run_with_tap(feed, conn_cfg, stop, log, |_, _| {});
}

/// What [`run_with_tap`] shows its tap.
#[derive(Clone, Copy, Debug)]
pub enum Tapped<'a> {
    /// A connection was established.
    Connected,
    /// A text message (after the feed has handled it, so recording doesn't delay it).
    Text(&'a [u8]),
    /// A REST snapshot body (full depth), after the feed has handled it.
    Snapshot(&'a [u8]),
}

/// Largest REST snapshot accepted (Binance's 1000-level snapshot is ~42 KB).
const MAX_SNAPSHOT: usize = 8 << 20;

/// [`run`], also passing every connection and text message to `tap` with its receive
/// time (monotonic ns). Used to record a session.
pub fn run_with_tap<V: Venue, P: Probe>(
    feed: &mut Feed<V, P>,
    conn_cfg: &ConnConfig,
    stop: &AtomicBool,
    log: impl FnMut(&'static str, DriverEvent<'_>),
    tap: impl FnMut(u64, Tapped<'_>),
) {
    run_full(feed, conn_cfg, None, stop, log, tap);
}

/// [`run_with_tap`], with the REST snapshot server's settings given explicitly (tests
/// point it at a local server). `None`: the venue's [`Venue::snapshot_endpoint`] host
/// with production TLS roots.
pub fn run_full<V: Venue, P: Probe>(
    feed: &mut Feed<V, P>,
    conn_cfg: &ConnConfig,
    rest_cfg: Option<&ConnConfig>,
    stop: &AtomicBool,
    mut log: impl FnMut(&'static str, DriverEvent<'_>),
    mut tap: impl FnMut(u64, Tapped<'_>),
) {
    let mut conn: Option<WsConnection> = None;
    let mut cmds = Commands::default();
    let name = feed.venue().name();
    while !stop.load(Ordering::Relaxed) {
        let now = mono_ns();
        match conn.as_mut() {
            None => {
                if feed.ready_to_connect(now) {
                    match WsConnection::connect(conn_cfg) {
                        Ok(c) => {
                            conn = Some(c);
                            let t = mono_ns();
                            feed.on_connected(t, &mut cmds);
                            tap(t, Tapped::Connected);
                        }
                        Err(e) => {
                            log(name, DriverEvent::ConnectFailed(&e));
                            feed.on_connect_failed(mono_ns(), &mut cmds);
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Some(c) => match (feed.probe().stamp(), c.poll()) {
                (read_start, Ok(Some(Message::Text(text)))) => {
                    let read_end = feed.probe().stamp();
                    feed.probe_mut().read(read_start, read_end);
                    feed.on_text(now, text, &mut cmds);
                    tap(now, Tapped::Text(text));
                    if let Some(ns) = c.last_kernel_wait_ns() {
                        feed.probe_mut().kernel_wait(ns);
                    }
                }
                (_, Ok(Some(_))) => {} // ping/pong/close: handled by the transport
                (_, Ok(None)) => {
                    feed.on_tick(now, &mut cmds);
                    std::hint::spin_loop();
                }
                (_, Err(e)) => {
                    log(name, DriverEvent::ConnectionLost(&e));
                    conn = None;
                    feed.on_disconnected(now, &mut cmds);
                }
            },
        }
        // Carry out the feed's commands (a reconnect may queue more: the loop drains all).
        while let Some(cmd) = cmds.pop() {
            match cmd {
                Command::Log(event) => log(name, DriverEvent::Feed(event)),
                Command::Send(request) => {
                    if let (Some(c), Some(bytes)) = (conn.as_mut(), feed.venue().request(request))
                        && let Err(e) = c.send_text(bytes)
                    {
                        log(name, DriverEvent::ConnectionLost(&e));
                        conn = None;
                        feed.on_disconnected(mono_ns(), &mut cmds);
                    }
                }
                Command::Reconnect => {
                    if let Some(mut c) = conn.take() {
                        let _ = c.close(1000); // best effort: a clean close if the socket still works
                    }
                    feed.on_disconnected(mono_ns(), &mut cmds);
                }
                // Cold path: a blocking GET (one per sync). Stream messages that arrive
                // meanwhile wait in the socket's kernel buffer, in order.
                Command::FetchSnapshot => {
                    let Some((host, path)) = feed.venue().snapshot_endpoint() else {
                        continue;
                    };
                    let path = path.to_string();
                    let result = match rest_cfg {
                        Some(cfg) => ob_net::http::get(cfg, &path, MAX_SNAPSHOT),
                        None => ConnConfig::new(host, "/")
                            .and_then(|cfg| ob_net::http::get(&cfg, &path, MAX_SNAPSHOT)),
                    };
                    match result {
                        Ok(body) => {
                            let t = mono_ns();
                            feed.on_snapshot(t, &body, &mut cmds);
                            tap(t, Tapped::Snapshot(&body));
                        }
                        Err(e) => {
                            log(name, DriverEvent::SnapshotFailed(&e));
                            feed.on_snapshot_failed(mono_ns(), &mut cmds);
                        }
                    }
                }
            }
        }
    }
    if let Some(mut c) = conn {
        let _ = c.close(1000);
    }
}

/// Connection settings for a venue's endpoint (production TLS roots).
pub fn connection_config<V: Venue>(venue: &V) -> Result<ConnConfig, NetError> {
    let (host, path) = venue.endpoint();
    ConnConfig::new(host, path)
}
