// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JDK's own `Pipe` over real OS descriptors (JIT round 19, nio19).
//!
//! `IOUtil.makePipe` (`net_nio_real.rs`) answers two REAL OS descriptors, the
//! way libnio does, and the JDK's `PipeImpl` / `SourceChannelImpl` /
//! `SinkChannelImpl` read, write, poll, configure and close them through
//! `SocketDispatcher` / `UnixDispatcher` and `IOUtil` -- the natives that
//! otherwise serve `net_sockets()` ids (counting from `0x4000_0000`). Until
//! the VM's fabricated pipe was retired (`RETIRED_SHADOW_R19_NIO_PIPE_TRIPLES`)
//! no real `SourceChannelImpl` existed, so those natives answered a pipe end
//! with `IOException("read0: fd not a stream")`.
//!
//! This module is the pipe arm of those natives. Only descriptors
//! `IOUtil.makePipe` returned are served ([`register`]), so an fd number that
//! happens to be small is never read from or closed by accident; the row goes
//! when the descriptor is really closed.
//!
//! # Close while blocked
//!
//! libnio's `preClose` `dup2`s a half-shut socket over the fd so a thread
//! parked in `read` / `write` wakes, and the final `close` comes later. Here a
//! blocking read or write never parks in the kernel unboundedly: it `poll`s in
//! [`SLICE_MS`] slices and re-asks the row each slice. `preClose0` marks the
//! row ([`pre_close`]), which ends every waiter with EOF (a read) or `EPIPE`'s
//! `IOException` (a write) -- what the dup2'd descriptor answers on HotSpot --
//! and `close0` closes the descriptor once no thread is inside a syscall on it
//! ([`close`]: the last one out closes it), so the number cannot be reused
//! under a reader. An interruptible wait (`Thread.nioBlocker` set) also
//! re-checks the thread's interrupt flag each slice, as the socket waits do.

use super::nio_real::{errno_text, last_errno};
use super::*;
use std::collections::HashMap;

/// One OS pipe end `IOUtil.makePipe` handed out.
#[derive(Default)]
struct OsPipeFd {
    /// Threads inside a `read` / `write` / `poll` on the descriptor.
    in_flight: u32,
    /// `preClose0` or `close0` arrived: every waiter ends.
    closing: bool,
    /// `close0` arrived while `in_flight > 0`: the last thread out closes it.
    close_pending: bool,
    /// nio21 wave 19 L5: the descriptor's `O_NONBLOCK`, once known -- read by
    /// `fcntl` the first time it is asked and kept up to date by
    /// [`configure_blocking`], the one door that changes it for a pipe end
    /// (`IOUtil.configureBlocking`). `None` until then. Saves an `fcntl` per
    /// read and write.
    nonblocking: Option<bool>,
}

fn table() -> &'static Mutex<HashMap<i32, OsPipeFd>> {
    static T: std::sync::OnceLock<Mutex<HashMap<i32, OsPipeFd>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Longest single `poll` a blocking pipe wait sits in before it re-asks the row.
const SLICE_MS: i32 = 25;

/// Record a descriptor `IOUtil.makePipe` just created.
pub(super) fn register(fd: i32) {
    table().lock().insert(fd, OsPipeFd::default());
}

/// Is `fd` an open pipe end this module serves?
pub(super) fn is_pipe_fd(fd: i32) -> bool {
    (0..0x4000_0000).contains(&fd) && table().lock().contains_key(&fd)
}

/// Forget `fd` without closing it (its owner closes it itself:
/// `FileDispatcherImpl.closeIntFD`, the `EPollPort` wake-up pair).
pub(crate) fn forget(fd: i32) {
    table().lock().remove(&fd);
}

/// `preClose0`: wake every waiter; keep the descriptor (and its number).
pub(super) fn pre_close(fd: i32) -> bool {
    match table().lock().get_mut(&fd) {
        Some(row) => {
            row.closing = true;
            true
        }
        None => false,
    }
}

/// `close0`: close the descriptor now, or when the last thread inside a
/// syscall on it leaves. `false` when `fd` is not a pipe end.
pub(super) fn close(fd: i32) -> bool {
    let mut t = table().lock();
    let Some(row) = t.get_mut(&fd) else {
        return false;
    };
    row.closing = true;
    if row.in_flight > 0 {
        row.close_pending = true;
        return true;
    }
    t.remove(&fd);
    drop(t);
    // SAFETY: `fd` is a descriptor `makePipe` created and nothing else closes
    // while its row exists; no thread is inside a syscall on it.
    unsafe { libc::close(fd) };
    true
}

/// Enter a syscall on `fd`; `None` once the end is closing, else the cached
/// `O_NONBLOCK` (`None` when not known yet).
fn enter(fd: i32) -> Option<Option<bool>> {
    let mut t = table().lock();
    let row = t.get_mut(&fd)?;
    if row.closing {
        return None;
    }
    row.in_flight += 1;
    Some(row.nonblocking)
}

/// What [`enter_any`] found for a descriptor.
enum Entry {
    /// Not a pipe end this module serves (the caller's socket path takes it).
    NotPipe,
    /// A pipe end that is closing.
    Closing,
    /// Entered (counted in `in_flight`); the cached `O_NONBLOCK`.
    Open(Option<bool>),
}

/// nio21 wave 20 L5: [`is_pipe_fd`] and [`enter`] in ONE table lock -- the
/// dispatcher natives asked the first and then took the second, three locks
/// of the table per pipe syscall with [`exit`].
fn enter_any(fd: i32) -> Entry {
    if !(0..0x4000_0000).contains(&fd) {
        return Entry::NotPipe;
    }
    let mut t = table().lock();
    match t.get_mut(&fd) {
        None => Entry::NotPipe,
        Some(row) if row.closing => Entry::Closing,
        Some(row) => {
            row.in_flight += 1;
            Entry::Open(row.nonblocking)
        }
    }
}

/// How a one-shot `RWF_NOWAIT` syscall ended.
enum NoWait {
    /// The syscall's count (`0`: end of file for a read).
    Done(usize),
    /// The end is not ready (`EAGAIN`).
    Again,
    /// Anything else (unsupported, `EINTR`, an error): the caller takes its
    /// ordinary path, which reproduces the answer.
    Fallback,
}

/// `RWF_NOWAIT` support of this process's pipes: 0 unknown, 1 off (switch
/// off, or the kernel refused it), 2 on. An OS capability, not compatibility
/// state.
static NOWAIT_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// nio21 wave 20 L5 (`CRATONVM_NIO_PIPE_RWF_NOWAIT`, default on, `=0` off;
/// Linux only): may a pipe read / write try ONE `preadv2` / `pwritev2` with
/// `RWF_NOWAIT` first? That answers "done" or `EAGAIN` in one syscall where
/// the ready-first path spends two (`poll`, then the syscall), and it cannot
/// park whatever another thread does in between. A kernel whose pipes do not
/// take the flag answers `EOPNOTSUPP` once and the process stops asking.
fn nowait_enabled() -> bool {
    use std::sync::atomic::Ordering;
    match NOWAIT_STATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = cfg!(target_os = "linux")
                && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_NIO_PIPE_RWF_NOWAIT");
            NOWAIT_STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// One `preadv2` (`write == false`) or `pwritev2` with `RWF_NOWAIT` at the
/// current position of `fd`, over `len` bytes at `ptr`.
#[cfg(target_os = "linux")]
fn rw_nowait(fd: i32, ptr: *mut u8, len: usize, write: bool) -> NoWait {
    let iov = libc::iovec {
        iov_base: ptr.cast(),
        iov_len: len,
    };
    // SAFETY: `iov` names `len` bytes the caller owns (readable for a write,
    // writable for a read); the descriptor is open while this thread is
    // counted in `in_flight`; offset -1 is the current position, as `read` /
    // `write` use.
    let n = unsafe {
        if write {
            libc::pwritev2(fd, &iov, 1, -1, libc::RWF_NOWAIT)
        } else {
            libc::preadv2(fd, &iov, 1, -1, libc::RWF_NOWAIT)
        }
    };
    if n >= 0 {
        return NoWait::Done(n as usize);
    }
    match last_errno() {
        e if e == libc::EAGAIN => NoWait::Again,
        libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS => {
            NOWAIT_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
            NoWait::Fallback
        }
        _ => NoWait::Fallback,
    }
}

#[cfg(not(target_os = "linux"))]
fn rw_nowait(_fd: i32, _ptr: *mut u8, _len: usize, _write: bool) -> NoWait {
    NoWait::Fallback
}

/// Linux `PIPE_BUF`: a pipe write of at most this many bytes is atomic, so a
/// non-waiting write of it either writes everything or nothing (`EAGAIN`) --
/// what a blocking write that found room does. A longer blocking write could
/// come back short where the blocking one writes on, so it keeps the old path.
const PIPE_BUF_ATOMIC: usize = 4096;

thread_local! {
    /// nio21 wave 20 L5: the read path's staging buffer, reused per thread
    /// instead of a fresh `vec![0; len]` per read. Taken out for the call and
    /// put back after it, so a nested read (the interrupt delivery runs Java)
    /// simply gets a fresh one.
    static READ_BUF: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Keep at most this much staging capacity per thread.
const READ_BUF_KEEP: usize = 1 << 20;

/// [`is_nonblocking`] through the row's cache: `known` is what [`enter`]
/// answered; an unknown mode is read once and recorded.
fn nonblocking_mode(fd: i32, known: Option<bool>) -> bool {
    if let Some(nb) = known {
        return nb;
    }
    let nb = is_nonblocking(fd);
    if let Some(row) = table().lock().get_mut(&fd) {
        row.nonblocking = Some(nb);
    }
    nb
}

/// nio21 wave 19 L5 (`CRATONVM_NIO_PIPE_READY_FIRST`, default on; `=0` is the
/// kill switch): may a blocking read / write ask the descriptor with a
/// zero-timeout `poll` first, and skip the GC-blocking region when it is
/// already ready? Read once per process (an OS-level fast path, not
/// compatibility state).
fn ready_first_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_NIO_PIPE_READY_FIRST")
    })
}

/// Is `fd` ready for `events` right now (a zero-timeout `poll`, outside any
/// blocking region)? A hang-up or error counts as ready, as in [`wait`]: the
/// syscall that follows answers it at once. `false` on a poll error (the
/// caller then takes its blocking path, which reports it).
fn ready_now(fd: i32, events: libc::c_short) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`; the descriptor stays open while this thread
    // is counted in `in_flight`; a zero timeout never sleeps.
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
}

/// Leave a syscall; closes the descriptor if a `close0` was waiting for it.
fn exit(fd: i32) {
    let mut t = table().lock();
    let close_now = match t.get_mut(&fd) {
        Some(row) => {
            row.in_flight = row.in_flight.saturating_sub(1);
            row.close_pending && row.in_flight == 0
        }
        None => false,
    };
    if close_now {
        t.remove(&fd);
        drop(t);
        // SAFETY: as in `close`; this thread was the last one inside.
        unsafe { libc::close(fd) };
    }
}

fn closing(fd: i32) -> bool {
    table().lock().get(&fd).map_or(true, |row| row.closing)
}

fn is_nonblocking(fd: i32) -> bool {
    // SAFETY: `fcntl(F_GETFL)` reads the descriptor's flags; no memory.
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    fl >= 0 && fl & libc::O_NONBLOCK != 0
}

/// `IOUtil.configureBlocking` on a pipe end: the descriptor's `O_NONBLOCK`.
pub(super) fn configure_blocking(fd: i32, blocking: bool) -> std::io::Result<()> {
    // SAFETY: flag reads and writes on a descriptor this module serves.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let want = if blocking {
            fl & !libc::O_NONBLOCK
        } else {
            fl | libc::O_NONBLOCK
        };
        if want != fl && libc::fcntl(fd, libc::F_SETFL, want) < 0 {
            if let Some(row) = table().lock().get_mut(&fd) {
                row.nonblocking = None;
            }
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(row) = table().lock().get_mut(&fd) {
        row.nonblocking = Some(!blocking);
    }
    Ok(())
}

/// How a slice-wise wait ended.
enum Wait {
    Ready,
    Closed,
    Interrupted,
    TimedOut,
}

/// Wait for `events` on `fd` for up to `timeout_ms` (`-1` = forever) in
/// [`SLICE_MS`] slices, re-asking the row and, when `interruptible`, the
/// thread's interrupt flag between slices. The caller holds `enter`.
fn wait(
    ctx: &mut dyn NativeContext,
    fd: i32,
    events: libc::c_short,
    timeout_ms: i64,
    interruptible: bool,
) -> std::io::Result<Wait> {
    let deadline = (timeout_ms >= 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64));
    loop {
        if closing(fd) {
            return Ok(Wait::Closed);
        }
        if interruptible && ctx.is_interrupted(false) {
            return Ok(Wait::Interrupted);
        }
        let slice = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() && timeout_ms != 0 {
                    return Ok(Wait::TimedOut);
                }
                (left.as_millis() as i32).min(SLICE_MS)
            }
            None => SLICE_MS,
        };
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`; the descriptor stays open while this
        // thread is counted in `in_flight`.
        let rc = unsafe { libc::poll(&mut pfd, 1, slice) };
        if rc > 0 {
            return Ok(Wait::Ready);
        }
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
        if timeout_ms == 0 {
            return Ok(Wait::TimedOut);
        }
    }
}

/// `SocketDispatcher.read0` / `readv0` on a pipe end, libnio's
/// `convertReturnVal(.., reading = true)`: the count, `-1` at EOF,
/// `IOS_UNAVAILABLE` for an empty non-blocking end, `IOS_INTERRUPTED` on
/// `EINTR`, otherwise an `IOException` with the errno text.
///
/// `None` when `fd` is not a pipe end (the caller's socket path takes it);
/// the pipe-end check and the syscall entry are one table lock
/// ([`enter_any`]).
pub(super) fn read_if_pipe(
    ctx: &mut dyn NativeContext,
    fd: i32,
    len: usize,
    sink: &mut dyn FnMut(&mut dyn NativeContext, &[u8]) -> Result<(), MethodCallFailed>,
) -> Option<MethodCallResult> {
    match enter_any(fd) {
        Entry::NotPipe => None,
        Entry::Closing => Some(Ok(Some(Value::Int(-1)))),
        Entry::Open(known) => Some(read_entered(ctx, fd, known, len, sink)),
    }
}

/// The body of [`read_if_pipe`] for an entered descriptor; leaves it ([`exit`]).
fn read_entered(
    ctx: &mut dyn NativeContext,
    fd: i32,
    known: Option<bool>,
    len: usize,
    sink: &mut dyn FnMut(&mut dyn NativeContext, &[u8]) -> Result<(), MethodCallFailed>,
) -> MethodCallResult {
    let nonblocking = nonblocking_mode(fd, known);
    let mut buf = READ_BUF.with(|c| c.take());
    buf.clear();
    buf.resize(len, 0);
    let result = (|| {
        // nio21 wave 19/20 L5: an interrupted thread keeps the old path (the
        // `nioBlocker` protocol); otherwise one `RWF_NOWAIT` read first, then
        // the zero-timeout readiness probe.
        let try_first = ready_first_enabled() && !ctx.is_interrupted(false);
        let mut not_ready = false;
        if try_first && nowait_enabled() {
            match rw_nowait(fd, buf.as_mut_ptr(), len, false) {
                NoWait::Done(0) => return Ok(Some(Value::Int(-1))),
                NoWait::Done(n) => {
                    sink(ctx, &buf[..n])?;
                    return Ok(Some(Value::Int(n as i32)));
                }
                NoWait::Again if nonblocking => return Ok(Some(Value::Int(-2))),
                NoWait::Again => not_ready = true,
                NoWait::Fallback => {}
            }
        }
        if !nonblocking && (not_ready || !(try_first && ready_now(fd, libc::POLLIN))) {
            let interruptible = nio_blocker_armed(ctx);
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let w = wait(ctx, fd, libc::POLLIN, -1, interruptible);
            ctx.end_blocking_region();
            match w {
                Ok(Wait::Ready) | Ok(Wait::TimedOut) => {}
                Ok(Wait::Closed) => return Ok(Some(Value::Int(-1))),
                Ok(Wait::Interrupted) => {
                    nio_deliver_interrupt(ctx);
                    return Ok(Some(Value::Int(-1)));
                }
                Err(e) => return Err(ioex(e.to_string())),
            }
        }
        // SAFETY: `buf` is `len` writable bytes; the descriptor is open.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), len) };
        if n > 0 {
            sink(ctx, &buf[..n as usize])?;
            return Ok(Some(Value::Int(n as i32)));
        }
        if n == 0 {
            return Ok(Some(Value::Int(-1)));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(e) if e == libc::EAGAIN => Ok(Some(Value::Int(-2))),
            Some(libc::EINTR) => Ok(Some(Value::Int(-3))),
            _ => Err(ioex(errno_text(last_errno()))),
        }
    })();
    exit(fd);
    if buf.capacity() <= READ_BUF_KEEP {
        READ_BUF.with(|c| {
            let held = c.take();
            c.set(if held.capacity() < buf.capacity() {
                buf
            } else {
                held
            });
        });
    }
    result
}

/// `SocketDispatcher.write0` / `writev0` on a pipe end, libnio's
/// `convertReturnVal(.., reading = false)`: the count (a blocking end writes
/// what fits after waiting for room), `IOS_UNAVAILABLE` for a full
/// non-blocking end, `IOS_INTERRUPTED` on `EINTR`, otherwise an
/// `IOException` (`EPIPE`: "Broken pipe").
///
/// `None` when `fd` is not a pipe end; one table lock as in
/// [`read_if_pipe`].
pub(super) fn write_if_pipe(
    ctx: &mut dyn NativeContext,
    fd: i32,
    data: &[u8],
) -> Option<MethodCallResult> {
    match enter_any(fd) {
        Entry::NotPipe => None,
        Entry::Closing => Some(Err(ioex(errno_text(libc::EPIPE)))),
        Entry::Open(known) => Some(write_entered(ctx, fd, known, data)),
    }
}

/// The body of [`write_if_pipe`] for an entered descriptor; leaves it ([`exit`]).
fn write_entered(
    ctx: &mut dyn NativeContext,
    fd: i32,
    known: Option<bool>,
    data: &[u8],
) -> MethodCallResult {
    let nonblocking = nonblocking_mode(fd, known);
    let result = (|| {
        // As in `read_entered`. A blocking end tries the non-waiting write
        // only for an atomic length (`PIPE_BUF_ATOMIC`).
        let try_first = ready_first_enabled() && !ctx.is_interrupted(false);
        let mut not_ready = false;
        if try_first && (nonblocking || data.len() <= PIPE_BUF_ATOMIC) && nowait_enabled() {
            match rw_nowait(fd, data.as_ptr().cast_mut(), data.len(), true) {
                NoWait::Done(n) => return Ok(Some(Value::Int(n as i32))),
                NoWait::Again if nonblocking => return Ok(Some(Value::Int(-2))),
                NoWait::Again => not_ready = true,
                NoWait::Fallback => {}
            }
        }
        if !nonblocking && (not_ready || !(try_first && ready_now(fd, libc::POLLOUT))) {
            let interruptible = nio_blocker_armed(ctx);
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let w = wait(ctx, fd, libc::POLLOUT, -1, interruptible);
            ctx.end_blocking_region();
            match w {
                Ok(Wait::Ready) | Ok(Wait::TimedOut) => {}
                Ok(Wait::Closed) => return Err(ioex(errno_text(libc::EPIPE))),
                Ok(Wait::Interrupted) => {
                    // The callers stage `data` in the per-thread I/O scratch:
                    // the interrupt runs Java, so it is delivered once the
                    // scratch is released (`deliver_pending_nio_interrupt`).
                    NIO_INTERRUPT_PENDING.with(|p| p.set(true));
                    return Ok(Some(Value::Int(0)));
                }
                Err(e) => return Err(ioex(e.to_string())),
            }
        }
        // SAFETY: `data` is readable; the descriptor is open.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n >= 0 {
            return Ok(Some(Value::Int(n as i32)));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(e) if e == libc::EAGAIN => Ok(Some(Value::Int(-2))),
            Some(libc::EINTR) => Ok(Some(Value::Int(-3))),
            _ => Err(ioex(errno_text(last_errno()))),
        }
    })();
    exit(fd);
    result
}

/// `Net.poll(fd, events, timeout)` on a pipe end: `1` when ready, `0` on a
/// timeout. A closing end reports ready, so the caller's retry finds the
/// channel closed (as for a socket).
pub(super) fn poll(
    ctx: &mut dyn NativeContext,
    fd: i32,
    events: i32,
    timeout_ms: i64,
) -> MethodCallResult {
    if enter(fd).is_none() {
        return Ok(Some(Value::Int(1)));
    }
    let mut ev: libc::c_short = 0;
    if events & i32::from(libc::POLLIN) != 0 {
        ev |= libc::POLLIN;
    }
    if events & i32::from(libc::POLLOUT) != 0 {
        ev |= libc::POLLOUT;
    }
    let interruptible = timeout_ms != 0 && nio_blocker_armed(ctx);
    if timeout_ms != 0 {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    let w = wait(ctx, fd, ev, timeout_ms, interruptible);
    if timeout_ms != 0 {
        ctx.end_blocking_region();
    }
    exit(fd);
    match w {
        Ok(Wait::Ready) | Ok(Wait::Closed) => Ok(Some(Value::Int(1))),
        Ok(Wait::Interrupted) => {
            nio_deliver_interrupt(ctx);
            Ok(Some(Value::Int(1)))
        }
        Ok(Wait::TimedOut) => Ok(Some(Value::Int(0))),
        Err(e) => Err(ioex(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipe table is process-wide and keyed by fd number: a test that
    /// closes an fd and then asserts "not a pipe end" races another test whose
    /// fresh `pipe()` reuses that number and registers it. The tests that
    /// register rows run one at a time.
    fn serial() -> parking_lot::MutexGuard<'static, ()> {
        static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        SERIAL.lock()
    }

    /// A pipe end is served only while its row exists, `close` defers to the
    /// last thread inside, and `preClose` ends a blocked reader.
    #[test]
    fn r19_os_pipe_rows_close_and_pre_close() {
        let _serial = serial();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: two-element out array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        assert!(!is_pipe_fd(r));
        register(r);
        register(w);
        assert!(is_pipe_fd(r) && is_pipe_fd(w));
        configure_blocking(r, false).unwrap();
        assert!(is_nonblocking(r));
        configure_blocking(r, true).unwrap();
        assert!(!is_nonblocking(r));
        // A thread inside defers the close.
        enter(w).unwrap();
        assert!(close(w));
        assert!(is_pipe_fd(w), "still in flight: the row stays");
        assert!(enter(w).is_none(), "closing: no new syscall");
        exit(w);
        assert!(!is_pipe_fd(w), "last one out closed it");
        // preClose: the reader's wait ends Closed; the row survives until close0.
        enter(r).unwrap();
        assert!(pre_close(r));
        assert!(closing(r));
        exit(r);
        assert!(is_pipe_fd(r));
        assert!(close(r));
        assert!(!is_pipe_fd(r));
        assert!(!close(r), "a second close is not this module's");
    }

    /// nio21 wave 20 L5: one lock answers "not a pipe end", "closing" or
    /// entered; the one-shot `RWF_NOWAIT` syscalls answer done / `EAGAIN`, or
    /// fall back where the kernel's pipes refuse the flag.
    #[test]
    fn w20_l5_enter_any_and_nowait() {
        let _serial = serial();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: two-element out array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        assert!(matches!(enter_any(r), Entry::NotPipe));
        assert!(matches!(enter_any(-1), Entry::NotPipe));
        register(r);
        register(w);
        assert!(matches!(enter_any(r), Entry::Open(None)));
        let mut one = [0u8; 8];
        match rw_nowait(r, one.as_mut_ptr(), one.len(), false) {
            NoWait::Again | NoWait::Fallback => {}
            NoWait::Done(n) => panic!("an empty pipe read {n} bytes"),
        }
        let msg = *b"abc";
        match rw_nowait(w, msg.as_ptr().cast_mut(), msg.len(), true) {
            NoWait::Done(3) => match rw_nowait(r, one.as_mut_ptr(), one.len(), false) {
                NoWait::Done(3) => assert_eq!(&one[..3], b"abc"),
                _ => panic!("the written bytes are readable"),
            },
            NoWait::Fallback => {}
            _ => panic!("an empty pipe takes 3 bytes"),
        }
        exit(r);
        assert!(pre_close(r));
        assert!(matches!(enter_any(r), Entry::Closing));
        assert!(close(r));
        assert!(close(w));
    }

    /// nio21 wave 19 L5: the row caches the end's blocking mode (read once,
    /// kept by `configure_blocking`), and the zero-timeout readiness probe
    /// answers what a read / write would find.
    #[test]
    fn w19_l5_mode_cache_and_readiness() {
        let _serial = serial();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: two-element out array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        register(r);
        register(w);
        let known = enter(r).unwrap();
        assert_eq!(known, None, "unknown until asked");
        assert!(!nonblocking_mode(r, known));
        exit(r);
        assert_eq!(enter(r).unwrap(), Some(false), "recorded");
        exit(r);
        configure_blocking(r, false).unwrap();
        assert_eq!(enter(r).unwrap(), Some(true));
        exit(r);
        configure_blocking(r, true).unwrap();
        assert_eq!(enter(r).unwrap(), Some(false));
        exit(r);
        assert!(!ready_now(r, libc::POLLIN), "empty: a read would block");
        assert!(ready_now(w, libc::POLLOUT), "room: a write would not");
        let b = 7u8;
        // SAFETY: one readable byte into a pipe this test owns.
        assert_eq!(unsafe { libc::write(w, (&b as *const u8).cast(), 1) }, 1);
        assert!(ready_now(r, libc::POLLIN), "a byte: a read returns at once");
        assert!(close(r));
        assert!(close(w));
    }
}
