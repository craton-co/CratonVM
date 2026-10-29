// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Windows: the `sun.nio.ch.WEPoll` natives the JDK's own `WEPollSelectorImpl` ends in.
//!
//! HotSpot's `WEPoll.c` wraps the wepoll library (an epoll look-alike over `\Device\Afd`). This VM keeps
//! `sun.nio.ch.Net` ids virtual (`net_sockets()`, from `0x4000_0000`), so what the "epoll handle" holds
//! here is a set of `(id, events)` pairs and `wait` is one `WSAPoll` over the `SOCKET`s behind them
//! (`nio_os_fd`), the same primitive `net_poll_raw` already uses for `Net.poll`. The `data` word the
//! selector reads back keeps the Java-visible id, so `WEPollSelectorImpl.fdToKey` sees what it registered.
//!
//! The event record is `{ u32 events; u64 data }` with the data word at offset 8 (16 bytes), which is what
//! `eventSize` / `eventsOffset` / `dataOffset` say; the JDK never assumes anything else.
//!
//! A child module of `net` so it shares that file's private registry.

use super::*;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

const EPOLLIN: i32 = 1;
const EPOLLPRI: i32 = 2;
const EPOLLOUT: i32 = 4;
const EPOLLERR: i32 = 8;
const EPOLLHUP: i32 = 16;

const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_MOD: i32 = 2;
const EPOLL_CTL_DEL: i32 = 3;

const POLLERR: i16 = 0x0001;
const POLLHUP: i16 = 0x0002;
const POLLNVAL: i16 = 0x0004;
const POLLWRNORM: i16 = 0x0010;
const POLLRDNORM: i16 = 0x0100;
const POLLRDBAND: i16 = 0x0200;

/// `ERROR_INVALID_PARAMETER`, `ERROR_ALREADY_EXISTS`, `ERROR_NOT_FOUND`: what wepoll's `epoll_ctl` leaves in
/// `GetLastError()` for a bad handle, a duplicate ADD and a MOD / DEL of an unknown socket.
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_ALREADY_EXISTS: i32 = 183;
const ERROR_NOT_FOUND: i32 = 1168;

/// One epoll handle: the registered `(Java fd id -> interest)` pairs.
#[derive(Default)]
struct WepollSet {
    entries: HashMap<i64, i32>,
}

fn sets() -> &'static Mutex<HashMap<i64, Arc<Mutex<WepollSet>>>> {
    static T: OnceLock<Mutex<HashMap<i64, Arc<Mutex<WepollSet>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> i64 {
    static N: AtomicI64 = AtomicI64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn set_of(handle: i64) -> Option<Arc<Mutex<WepollSet>>> {
    sets().lock().get(&handle).cloned()
}

#[repr(C)]
struct WsaPollFd {
    fd: usize,
    events: i16,
    revents: i16,
}

#[link(name = "Ws2_32")]
extern "system" {
    fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
    fn getsockopt(s: usize, level: i32, optname: i32, optval: *mut u8, optlen: *mut i32) -> i32;
}

const SOL_SOCKET: i32 = 0xffff;
const SO_ERROR: i32 = 0x1007;

/// Has the non-blocking connect of socket `id` (OS handle `raw`) FAILED? `WSAPoll` does not say: a refused
/// `connect()` leaves `revents` 0 for the whole timeout (measured here: `Selector.select(5000)` after a
/// refused non-blocking connect took 5003 ms where HotSpot's wepoll answers at once), so Netty / Jetty
/// reactive clients never saw the `ConnectException` ("new connections cannot be made once the server shuts
/// down"). Two records hold the failure: the loopback refusal `connect0` already observed
/// (`net_deferred_connect_failures`, the same one `finishConnect` reports) and the socket's `SO_ERROR`.
fn connect_failed(id: i64, raw: usize) -> bool {
    if net_deferred_connect_failures().lock().contains_key(&(id as i32)) {
        return true;
    }
    let mut err: i32 = 0;
    let mut len: i32 = std::mem::size_of::<i32>() as i32;
    // SAFETY: `raw` is a live socket the caller keeps open for the call; the out pointers name writable
    // storage of the advertised length.
    let rc = unsafe { getsockopt(raw, SOL_SOCKET, SO_ERROR, (&mut err as *mut i32).cast(), &mut len) };
    rc == 0 && err != 0
}

/// epoll interest -> `WSAPoll` events. `EPOLLPRI` is out-of-band data (`POLLRDBAND`).
fn to_poll_events(events: i32) -> i16 {
    let mut e = 0i16;
    if events & EPOLLIN != 0 {
        e |= POLLRDNORM;
    }
    if events & EPOLLPRI != 0 {
        e |= POLLRDBAND;
    }
    if events & EPOLLOUT != 0 {
        e |= POLLWRNORM;
    }
    e
}

/// `WSAPoll` result -> epoll events (error and hang-up are always reported, as epoll does).
fn from_poll_events(revents: i16) -> i32 {
    let mut e = 0;
    if revents & POLLRDNORM != 0 {
        e |= EPOLLIN;
    }
    if revents & POLLRDBAND != 0 {
        e |= EPOLLPRI;
    }
    if revents & POLLWRNORM != 0 {
        e |= EPOLLOUT;
    }
    if revents & (POLLERR | POLLNVAL) != 0 {
        e |= EPOLLERR;
    }
    if revents & POLLHUP != 0 {
        e |= EPOLLHUP;
    }
    e
}

fn long_at(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

fn int_at(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

fn wepoll_event_size(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(16)))
}

fn wepoll_events_offset(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn wepoll_data_offset(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(8)))
}

fn wepoll_create(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let h = next_handle();
    sets()
        .lock()
        .insert(h, Arc::new(Mutex::new(WepollSet::default())));
    Ok(Some(Value::Long(h)))
}

fn wepoll_close(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sets().lock().remove(&long_at(args, 0));
    Ok(None)
}

/// `ctl(h, opcode, s, events) -> int`: 0, or the Windows error code (wepoll's `epoll_ctl` returns -1 and the
/// libnio wrapper hands back `GetLastError()`); `WEPollSelectorImpl` ignores the result.
fn wepoll_ctl(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = long_at(args, 0);
    let op = int_at(args, 1);
    let s = long_at(args, 2);
    let events = int_at(args, 3);
    let Some(set) = set_of(h) else {
        return Ok(Some(Value::Int(ERROR_INVALID_PARAMETER)));
    };
    let mut set = set.lock();
    let rc = match op {
        EPOLL_CTL_ADD => {
            if set.entries.contains_key(&s) {
                ERROR_ALREADY_EXISTS
            } else {
                set.entries.insert(s, events);
                0
            }
        }
        EPOLL_CTL_MOD => match set.entries.get_mut(&s) {
            Some(e) => {
                *e = events;
                0
            }
            None => ERROR_NOT_FOUND,
        },
        EPOLL_CTL_DEL => {
            if set.entries.remove(&s).is_some() {
                0
            } else {
                ERROR_NOT_FOUND
            }
        }
        _ => ERROR_INVALID_PARAMETER,
    };
    Ok(Some(Value::Int(rc)))
}

/// One `WSAPoll` pass over the set's current members. Returns the ready `(id, epoll events)` pairs.
fn poll_once(
    set: &Arc<Mutex<WepollSet>>,
    max: usize,
    slice_ms: i32,
) -> std::io::Result<Vec<(i64, i32)>> {
    let snapshot: Vec<(i64, i32)> = set.lock().entries.iter().map(|(k, v)| (*k, *v)).collect();
    let mut keeps = Vec::with_capacity(snapshot.len());
    let mut ids = Vec::with_capacity(snapshot.len());
    let mut fds: Vec<WsaPollFd> = Vec::with_capacity(snapshot.len());
    for (id, events) in snapshot {
        // An id whose socket is gone (closed, never opened) is skipped: epoll drops a closed descriptor.
        if let Some(os) = crate::net::nio_os_fd(id as i32) {
            fds.push(WsaPollFd {
                fd: os.raw,
                events: to_poll_events(events),
                revents: 0,
            });
            ids.push(id);
            keeps.push(os);
        }
    }
    if fds.is_empty() {
        if slice_ms > 0 {
            std::thread::sleep(Duration::from_millis(slice_ms as u64));
        }
        return Ok(Vec::new());
    }
    // SAFETY: `fds` is a live, writable array of `fds.len()` entries; `keeps` holds every socket open for
    // the duration of the call.
    let rc = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, slice_ms) };
    drop(keeps);
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // A socket that asked for writability (OP_CONNECT / OP_WRITE) and shows nothing may be a refused connect.
    for i in 0..fds.len() {
        if fds[i].revents == 0 && fds[i].events & POLLWRNORM != 0 && connect_failed(ids[i], fds[i].fd) {
            fds[i].revents = POLLERR | POLLHUP | POLLWRNORM;
        }
    }
    let mut ready = Vec::new();
    for (pfd, id) in fds.iter().zip(ids) {
        if pfd.revents != 0 {
            let events = from_poll_events(pfd.revents);
            if events != 0 {
                ready.push((id, events));
                if ready.len() >= max {
                    break;
                }
            }
        }
    }
    Ok(ready)
}

/// `wait(h, pollAddress, numfds, timeout) -> int`: the number of events written at `pollAddress`.
///
/// `WSAPoll` blocks in slices (`SLICE_MS`) so an interrupt of a thread that armed `nioBlocker`
/// (`AbstractSelector.begin`) and a concurrent `close` of the handle are noticed: this VM's
/// `Thread.interrupt()` only sets the flag, so the blocker has to be run from here (see `net.rs`).
fn wepoll_wait(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const SLICE_MS: i32 = 50;
    let h = long_at(args, 0);
    let address = long_at(args, 1);
    let numfds = int_at(args, 2).clamp(1, 65_536) as usize;
    let timeout = int_at(args, 3);
    let Some(set) = set_of(h) else {
        return Err(ioex("WEPoll.wait: bad handle"));
    };
    let interruptible = timeout != 0 && nio_blocker_armed(ctx);
    let deadline =
        (timeout > 0).then(|| std::time::Instant::now() + Duration::from_millis(timeout as u64));
    let ready = loop {
        let slice = if timeout == 0 {
            0
        } else {
            match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    (left.as_millis() as i64).min(i64::from(SLICE_MS)) as i32
                }
                None => SLICE_MS,
            }
        };
        if timeout != 0 {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        }
        let r = poll_once(&set, numfds, slice);
        let interrupted = interruptible && ctx.is_interrupted(false);
        if timeout != 0 {
            ctx.end_blocking_region();
        }
        let ready = match r {
            Ok(v) => v,
            Err(e) => {
                return Err(crate::libnio_error::io_exception_with_last_error(
                    &e,
                    "WSAPoll failed",
                ))
            }
        };
        if !ready.is_empty() || timeout == 0 {
            break ready;
        }
        if sets().lock().get(&h).is_none() {
            break ready;
        }
        if interrupted {
            // The blocker is `wakeup()`: it writes the selector's pipe, and one non-blocking pass collects
            // that event for `processEvents`.
            nio_deliver_interrupt(ctx);
            break poll_once(&set, numfds, 0).unwrap_or_default();
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break ready;
        }
    };
    if !ready.is_empty() {
        let mut bytes = vec![0u8; ready.len() * 16];
        for (i, (id, events)) in ready.iter().enumerate() {
            bytes[i * 16..i * 16 + 4].copy_from_slice(&(*events as u32).to_ne_bytes());
            bytes[i * 16 + 8..i * 16 + 16].copy_from_slice(&(*id as u64).to_ne_bytes());
        }
        if !ctx.copy_to_native_memory(address, &bytes) {
            return Err(ioex(format!(
                "WEPoll.wait: invalid poll array address {address:#x}"
            )));
        }
    }
    Ok(Some(Value::Int(ready.len() as i32)))
}

pub(super) fn register(r: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let c = "sun/nio/ch/WEPoll";
    let b = NativeKind::Bridge;
    r.register_with_kind(c, "eventSize", "()I", wepoll_event_size, b);
    r.register_with_kind(c, "eventsOffset", "()I", wepoll_events_offset, b);
    r.register_with_kind(c, "dataOffset", "()I", wepoll_data_offset, b);
    r.register_with_kind(c, "create", "()J", wepoll_create, b);
    r.register_with_kind(c, "ctl", "(JIJI)I", wepoll_ctl, b);
    r.register_with_kind(c, "wait", "(JJII)I", wepoll_wait, b);
    r.register_with_kind(c, "close", "(J)V", wepoll_close, b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_translation_round_trips_the_bits_the_selector_uses() {
        assert_eq!(
            from_poll_events(to_poll_events(EPOLLIN | EPOLLPRI | EPOLLOUT)),
            EPOLLIN | EPOLLPRI | EPOLLOUT
        );
        assert_eq!(from_poll_events(POLLHUP), EPOLLHUP);
        assert_eq!(from_poll_events(POLLERR), EPOLLERR);
        assert_eq!(from_poll_events(POLLNVAL), EPOLLERR);
        assert_eq!(to_poll_events(0), 0);
    }

    #[test]
    fn a_set_answers_add_mod_del_like_epoll_ctl() {
        let h = next_handle();
        sets()
            .lock()
            .insert(h, Arc::new(Mutex::new(WepollSet::default())));
        let mut ctx = crate::test_support::MockNativeContext::new();
        let ctl = |op: i32, s: i64, ev: i32, ctx: &mut dyn NativeContext| match wepoll_ctl(
            ctx,
            &[
                Value::Long(h),
                Value::Int(op),
                Value::Long(s),
                Value::Int(ev),
            ],
        ) {
            Ok(Some(Value::Int(rc))) => rc,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(ctl(EPOLL_CTL_ADD, 7, EPOLLIN, &mut ctx), 0);
        assert_eq!(
            ctl(EPOLL_CTL_ADD, 7, EPOLLIN, &mut ctx),
            ERROR_ALREADY_EXISTS
        );
        assert_eq!(ctl(EPOLL_CTL_MOD, 7, EPOLLOUT, &mut ctx), 0);
        assert_eq!(ctl(EPOLL_CTL_MOD, 8, EPOLLOUT, &mut ctx), ERROR_NOT_FOUND);
        assert_eq!(ctl(EPOLL_CTL_DEL, 7, 0, &mut ctx), 0);
        assert_eq!(ctl(EPOLL_CTL_DEL, 7, 0, &mut ctx), ERROR_NOT_FOUND);
        sets().lock().remove(&h);
    }

    #[test]
    fn wait_reports_a_readable_loopback_socket_by_its_java_id() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut a, _) = l.accept().unwrap();
        let id = register_handle(NetSocketHandle::Stream(Arc::new(c)));
        let set = Arc::new(Mutex::new(WepollSet::default()));
        set.lock().entries.insert(i64::from(id), EPOLLIN);
        assert!(poll_once(&set, 8, 0).unwrap().is_empty());
        use std::io::Write;
        a.write_all(b"x").unwrap();
        let mut ready = Vec::new();
        for _ in 0..50 {
            ready = poll_once(&set, 8, 20).unwrap();
            if !ready.is_empty() {
                break;
            }
        }
        assert_eq!(ready, vec![(i64::from(id), EPOLLIN)]);
        close_net_fd(id);
    }
}
