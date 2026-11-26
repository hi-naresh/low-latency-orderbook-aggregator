//! `ob-os`: operating-system tuning for the latency-critical threads (step 21).
//!
//! Everything here is **opt-in** and **reports what it did**: each call returns
//! [`Support`] saying whether the OS applied it, only took it as a hint, or doesn't offer
//! it, rather than silently doing nothing. That way `ob live` can print exactly which
//! tuning is in effect on the machine it runs on.
//!
//! | | Linux | macOS |
//! |---|---|---|
//! | [`pin_current_thread`] | `sched_setaffinity`: hard pinning | affinity *tag*: a hint (Intel only) |
//! | [`set_realtime_priority`] | `SCHED_FIFO` (needs `CAP_SYS_NICE`) | QoS class user-interactive: a hint |
//! | [`lock_memory`] | `mlockall`: no page faults on the hot path | unsupported |
//! | [`set_busy_poll`] | `SO_BUSY_POLL`: the kernel spins on the NIC queue | unsupported |
//! | [`set_quickack`] | `TCP_QUICKACK`: ACK at once, not after up to 40 ms | unsupported |
//! | [`enable_rx_timestamps`] + [`recv_timestamped`] | `SO_TIMESTAMPNS` (ns) | unsupported for TCP (see below) |
//!
//! macOS accepts `SO_TIMESTAMP` on a TCP socket but never delivers a timestamp with TCP
//! data (BSD kernels timestamp datagrams only); a test found this, so on macOS the call
//! reports `Unsupported` rather than pretending.
//!
//! This is the only crate in the workspace that calls the OS directly. Each `unsafe`
//! block is small and justified in a `SAFETY:` comment; the API is safe.

use std::io;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::AsRawFd;

/// What the OS did with a tuning request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Applied and enforced.
    Applied,
    /// Accepted as a hint the scheduler may ignore (macOS).
    Hint(&'static str),
    /// Not available on this OS.
    Unsupported(&'static str),
}

#[cfg(unix)]
fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn setsockopt_int(
    sock: &TcpStream,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    // SAFETY: `sock` owns a valid open socket for the duration of the call; the option
    // value is a live `c_int` whose exact size is passed alongside it.
    check(unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            level,
            name,
            (&raw const value).cast(),
            libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(4),
        )
    })
}

// ---------------------------------------------------------------------------
// Threads

/// Pins the calling thread to CPU `cpu`.
///
/// Linux: hard affinity; the scheduler won't run the thread anywhere else. Combine with
/// `isolcpus`/`nohz_full` (see `docs/TUNING.md`) so nothing else runs *there* either.
/// macOS: there is no hard affinity; threads with the same affinity tag are kept on the
/// same L2 cache where possible (Intel Macs; ignored on Apple silicon).
/// Call [`set_realtime_priority`] **first** on macOS: once a thread's policy is set this
/// way it is no longer QoS-managed, and a later QoS change fails with EPERM (tested).
pub fn pin_current_thread(cpu: usize) -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `cpu_set_t` is plain data for which all-zeroes is the empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        if cpu >= 8 * size_of::<libc::cpu_set_t>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cpu index too large",
            ));
        }
        // SAFETY: CPU_SET only writes inside `set`, and `cpu` was checked against the
        // set's capacity just above.
        unsafe { libc::CPU_SET(cpu, &mut set) };
        // SAFETY: pid 0 means the calling thread; `set` is a valid, initialised mask of
        // the size passed.
        check(unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) })?;
        Ok(Support::Applied)
    }
    #[cfg(target_os = "macos")]
    {
        let tag = libc::integer_t::try_from(cpu + 1)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cpu index too large"))?;
        let mut policy = libc::thread_affinity_policy { affinity_tag: tag };
        // SAFETY: `pthread_mach_thread_np(pthread_self())` is the calling thread's Mach
        // port, borrowed (unlike `mach_thread_self`, which creates a new reference that
        // must be freed); `policy` is a live, correctly sized THREAD_AFFINITY_POLICY
        // structure and the count is its size in `integer_t` units, as the API requires.
        let kr = unsafe {
            libc::thread_policy_set(
                libc::pthread_mach_thread_np(libc::pthread_self()),
                libc::THREAD_AFFINITY_POLICY as libc::thread_policy_flavor_t,
                (&raw mut policy).cast(),
                libc::THREAD_AFFINITY_POLICY_COUNT,
            )
        };
        if kr == libc::KERN_SUCCESS {
            Ok(Support::Hint("macOS affinity tag (no hard pinning)"))
        } else {
            Ok(Support::Unsupported(
                "macOS refused the affinity tag (Apple silicon?)",
            ))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = cpu;
        Ok(Support::Unsupported("no thread pinning on this OS"))
    }
}

/// Gives the calling thread real-time priority.
///
/// Linux: `SCHED_FIFO` at `priority` (1–99): it runs until it blocks or yields, ahead of
/// every normal thread. A spinning FIFO thread never yields, so **only use it on an
/// isolated core**, or it can starve the rest of the system. Needs `CAP_SYS_NICE`.
/// macOS: no user real-time class for this; the user-interactive QoS class asks for the
/// fastest cores and the lowest scheduling latency.
pub fn set_realtime_priority(priority: u8) -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        let param = libc::sched_param {
            sched_priority: libc::c_int::from(priority.clamp(1, 99)),
        };
        // SAFETY: pid 0 is the calling thread; `param` is a live, initialised struct.
        check(unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) })?;
        Ok(Support::Applied)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = priority;
        // SAFETY: changes only the calling thread's QoS class; no pointers involved.
        let ret = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        check(ret)?;
        Ok(Support::Hint("macOS QoS class user-interactive"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = priority;
        Ok(Support::Unsupported("no real-time priority on this OS"))
    }
}

/// Locks all current and future memory of the process into RAM: no page faults (and no
/// swapping) on the hot path. Linux `mlockall`; needs `CAP_IPC_LOCK` or a high enough
/// `RLIMIT_MEMLOCK`.
pub fn lock_memory() -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: flags only; affects this process's memory residency, nothing else.
        check(unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) })?;
        Ok(Support::Applied)
    }
    #[cfg(not(target_os = "linux"))]
    Ok(Support::Unsupported("mlockall is Linux-only here"))
}

// ---------------------------------------------------------------------------
// Sockets

/// Linux `SO_BUSY_POLL`: on a blocking read, the kernel polls the network card's receive
/// queue for up to `usec` µs before sleeping, cutting interrupt and wake-up latency.
/// (Complements our user-space spinning: it removes the kernel's own wait.) Needs
/// `CAP_NET_ADMIN` to raise above `net.core.busy_read`.
pub fn set_busy_poll(sock: &TcpStream, usec: u32) -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        // From the kernel's include/uapi/asm-generic/socket.h (stable ABI); not exported
        // by this libc version for linux-gnu.
        const SO_BUSY_POLL: libc::c_int = 46;
        let v = libc::c_int::try_from(usec).unwrap_or(libc::c_int::MAX);
        setsockopt_int(sock, libc::SOL_SOCKET, SO_BUSY_POLL, v)?;
        Ok(Support::Applied)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sock, usec);
        Ok(Support::Unsupported("SO_BUSY_POLL is Linux-only"))
    }
}

/// Linux `TCP_QUICKACK`: acknowledge received data immediately instead of delaying the
/// ACK (up to 40 ms), so the venue's TCP sender isn't held back by our ACKs. The kernel
/// clears it again in some situations, so it is re-armed after each read where used.
pub fn set_quickack(sock: &TcpStream) -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        setsockopt_int(sock, libc::IPPROTO_TCP, libc::TCP_QUICKACK, 1)?;
        Ok(Support::Applied)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
        Ok(Support::Unsupported("TCP_QUICKACK is Linux-only"))
    }
}

#[cfg(target_os = "linux")]
const SO_TIMESTAMPNS: libc::c_int = 35; // asm-generic/socket.h: SO_TIMESTAMPNS_OLD (64-bit)

/// Asks the kernel to timestamp received data, for [`recv_timestamped`].
/// Linux `SO_TIMESTAMPNS` (ns). macOS: unsupported for TCP (it accepts the option but
/// delivers no timestamps for stream sockets).
pub fn enable_rx_timestamps(sock: &TcpStream) -> io::Result<Support> {
    #[cfg(target_os = "linux")]
    {
        setsockopt_int(sock, libc::SOL_SOCKET, SO_TIMESTAMPNS, 1)?;
        Ok(Support::Applied)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
        Ok(Support::Unsupported(
            "no kernel receive timestamps for TCP on this OS",
        ))
    }
}

/// `read`, also returning the kernel's receive timestamp for the data (Unix time in ns),
/// if the socket has [`enable_rx_timestamps`] and the OS supplied one.
///
/// The timestamp is when the kernel received the data, before our thread was told. So
/// `now − timestamp` at the moment we start processing is the time the data waited in
/// the kernel plus our wake-up: the part of the latency the user-space probes can't see.
pub fn recv_timestamped(sock: &TcpStream, buf: &mut [u8]) -> io::Result<(usize, Option<u64>)> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Control buffer for one timestamp message; u64s keep it aligned for cmsghdr.
        let mut control = [0_u64; 8];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: `msghdr` is plain data; all-zeroes is a valid empty header.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        // `usize` on Linux, `socklen_t` on macOS: the conversion is a no-op on one of them.
        #[allow(clippy::useless_conversion)]
        {
            msg.msg_controllen = size_of_val(&control).try_into().unwrap_or_default();
        }
        // SAFETY: `msg` points at `iov` (which points into `buf`, writable for its full
        // length) and at `control` (writable for `msg_controllen` bytes); all live for
        // the call. The kernel writes at most those lengths.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, 0) };
        let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
        let mut stamp = None;
        // SAFETY: walking the control messages the kernel just wrote into `control`,
        // using the libc macros that respect `msg_controllen`; each data pointer is read
        // only when the header says it holds the expected timestamp type, with an
        // unaligned read because cmsg data alignment isn't guaranteed for the struct.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            while !cmsg.is_null() {
                let (level, kind) = ((*cmsg).cmsg_level, (*cmsg).cmsg_type);
                #[cfg(target_os = "linux")]
                if level == libc::SOL_SOCKET && kind == SO_TIMESTAMPNS {
                    let ts: libc::timespec = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                    // `tv_nsec` is `i64` on 64-bit Linux, `i32` on some 32-bit targets.
                    #[allow(clippy::useless_conversion)]
                    let nsec = i64::from(ts.tv_nsec);
                    stamp = to_ns(ts.tv_sec, nsec);
                }
                #[cfg(target_os = "macos")]
                if level == libc::SOL_SOCKET && kind == libc::SCM_TIMESTAMP {
                    let tv: libc::timeval = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                    stamp = to_ns(tv.tv_sec, i64::from(tv.tv_usec) * 1000);
                }
                cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
            }
        }
        Ok((n, stamp))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        use std::io::Read;
        Ok(((&*sock).read(buf)?, None))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn to_ns(sec: libc::time_t, nsec: i64) -> Option<u64> {
    let sec = u64::try_from(sec).ok()?;
    let nsec = u64::try_from(nsec).ok()?;
    sec.checked_mul(1_000_000_000)?.checked_add(nsec)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::print_stdout)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn thread_tuning_reports_what_it_did() {
        // Must not fail on any supported OS; what it reports depends on the OS.
        let pin = std::thread::spawn(|| pin_current_thread(0))
            .join()
            .unwrap()
            .unwrap();
        let rt = std::thread::spawn(|| set_realtime_priority(10))
            .join()
            .unwrap();
        println!("pin: {pin:?}, realtime: {rt:?}, lock: {:?}", lock_memory());
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(pin, Support::Hint(_) | Support::Unsupported(_)));
            assert!(matches!(rt.unwrap(), Support::Hint(_)));
            assert!(matches!(lock_memory().unwrap(), Support::Unsupported(_)));
        }
    }

    #[test]
    fn kernel_receive_timestamps_arrive_with_the_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tx = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (rx, _) = listener.accept().unwrap();
        let support = enable_rx_timestamps(&rx).unwrap();
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        tx.write_all(b"hello").unwrap();
        let mut buf = [0_u8; 16];
        let (n, stamp) = recv_timestamped(&rx, &mut buf).unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        assert_eq!(&buf[..n], b"hello");
        println!(
            "kernel timestamp: {stamp:?}; read {} ns after sending",
            after - before
        );
        if support != Support::Applied {
            assert_eq!(stamp, None, "unsupported must mean no timestamps");
            return; // macOS: nothing more to check
        }
        let stamp = u128::from(stamp.expect("the kernel should timestamp TCP data"));
        // Between sending and our read (µs resolution on macOS: allow 1 µs either way).
        assert!(
            stamp + 1_000 >= before && stamp <= after + 1_000,
            "{before} ≤ {stamp} ≤ {after}"
        );
    }

    #[test]
    fn socket_options_apply_or_say_why_not() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let s = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let busy = set_busy_poll(&s, 50);
        let quick = set_quickack(&s);
        println!("busy poll: {busy:?}, quickack: {quick:?}");
        #[cfg(target_os = "macos")]
        assert!(matches!(busy.unwrap(), Support::Unsupported(_)));
        #[cfg(target_os = "linux")]
        assert_eq!(quick.unwrap(), Support::Applied);
    }
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::unwrap_used, clippy::print_stdout)]
mod macos_order {
    use super::*;

    /// macOS: once a thread's policy was changed with `thread_policy_set` (the affinity
    /// tag), it is no longer QoS-managed, and setting a QoS class fails with EPERM. So
    /// set the QoS class first, then the tag (which `ob live` does).
    ///
    /// Apple Silicon refuses the affinity tag altogether (`Unsupported`), so the thread's
    /// policy never changes and the QoS class is accepted either way (step 29: found by
    /// reviewing the code for the M-series benchmark run).
    #[test]
    fn qos_must_come_before_the_affinity_tag() {
        let (tag, tag_then_qos) = std::thread::spawn(|| {
            let tag = pin_current_thread(0).unwrap();
            (tag, set_realtime_priority(10).map_err(|e| e.kind()))
        })
        .join()
        .unwrap();
        let qos_then_tag = std::thread::spawn(|| {
            let q = set_realtime_priority(10).map_err(|e| e.kind());
            (q, pin_current_thread(0).unwrap())
        })
        .join()
        .unwrap();
        println!("tag {tag:?} → QoS: {tag_then_qos:?};  QoS → tag: {qos_then_tag:?}");
        match tag {
            // Intel: the tag was applied, so the thread left QoS management.
            Support::Hint(_) => {
                assert_eq!(tag_then_qos, Err(io::ErrorKind::PermissionDenied));
            }
            // Apple Silicon: no tag, so nothing changed and QoS still works.
            _ => assert!(matches!(tag_then_qos, Ok(Support::Hint(_)))),
        }
        assert!(matches!(qos_then_tag.0, Ok(Support::Hint(_))));
    }
}
