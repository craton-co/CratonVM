// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.7 — anonymous pipe via `java.nio.channels.Pipe.open()`.
//!
//! Acceptance criterion: a Java program can call `Pipe.open()`, then
//! write bytes to `pipe.sink()` and read them back from `pipe.source()`.
//!
//! Backing kernel objects:
//!
//!   * **Linux/macOS/BSD**: real anonymous pipe via `libc::pipe(2)`.
//!     Returns two file descriptors (read end, write end).  We stash
//!     each as a u64 in our process-wide `pipe_table` and surface a
//!     synthetic Java `SourceChannelImpl` / `SinkChannelImpl` wrapping
//!     the id.
//!   * **Windows**: real anonymous pipe via the Win32 `CreatePipe`
//!     entry, declared with raw `extern "system"` FFI against
//!     `Kernel32` (same pattern as `native-builtins/src/servlet.rs`'s
//!     `WSAPoll`, avoids pulling in `windows-sys` as a dependency).
//!     Returns two `HANDLE`s (`HANDLE` is `*mut c_void`, stored as
//!     u64).  Same wrapping pattern; the read/write paths use
//!     `ReadFile` / `WriteFile` with overlapped=NULL for blocking
//!     semantics (matching the JDK's `SourceChannel.read` /
//!     `SinkChannel.write` blocking default).
//!
//! Because we cannot edit `lib.rs` to share a unified fd_table, this
//! module owns its own `pipe_table` keyed by a u32 id.  The Java
//! channel objects we hand back stash that id at field 0 and use it on
//! every subsequent native call.
//!
//! Thread safety: every kernel handle / fd in the table is read/written
//! from concurrent threads, but the underlying read/write syscalls are
//! thread-safe at the kernel level (the kernel serializes per-fd I/O).
//! The table itself is guarded by a `RwLock`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{OnceLock, RwLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Pipe handle representation
// ---------------------------------------------------------------------------

/// A pipe end identifier — opaque to Java, just a key in our table.
/// We wrap raw OS handles (fd or HANDLE) so the table is platform-agnostic.
#[derive(Copy, Clone)]
struct PipeEnd {
    /// Raw OS handle.  On Unix this is an `int` (the fd); on Windows
    /// it's a `HANDLE` (a pointer).  Stored as u64 so a single field
    /// covers both.
    raw: u64,
    /// `true` for the writable (sink) end, `false` for the readable
    /// (source) end.  Used to validate that read/write goes through
    /// the right side.
    is_sink: bool,
    /// Closed flag — guards against double-close from finalizers, and, since
    /// 2026-08-12, the **generation** a parked reader/writer re-asks. See the
    /// module-level "Close-awareness" section below.
    closed: bool,
    /// Threads currently between their liveness check and the return of their
    /// blocking syscall on `raw`.
    ///
    /// This is what makes closing safe at all. `PipeEnd` is `Copy` and the
    /// handle is a bare `u64` with no ownership attached, so a thread inside
    /// `ReadFile`/`read(2)` holds nothing that keeps `raw` alive — unlike every
    /// socket site in this family, which parks holding an `Arc<TcpStream>`.
    in_flight: u32,
    /// A close arrived while `in_flight > 0`; the last thread out closes `raw`.
    close_pending: bool,
}

fn pipe_table() -> &'static RwLock<HashMap<i32, PipeEnd>> {
    static T: OnceLock<RwLock<HashMap<i32, PipeEnd>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_pipe_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn register_pipe_end(end: PipeEnd) -> i32 {
    let id = next_pipe_id();
    if let Ok(mut g) = pipe_table().write() {
        g.insert(id, end);
    }
    id
}

// ---------------------------------------------------------------------------
// Close-awareness (2026-08-12, W7-53)
// ---------------------------------------------------------------------------
//
// # Why the registry re-ask the socket sites use cannot be lifted here
//
// Every other site in this family parks holding an `Arc<TcpStream>` cloned out
// of a registry. Two consequences follow from that, and BOTH of them are load-
// bearing: the OS handle cannot be closed while the thread is parked (the `Arc`
// is still alive), so `close` can only *mark* the registry — and marking the
// registry is therefore a safe, purely advisory question the parked thread can
// re-ask on any cadence it likes.
//
// Neither holds here. `PipeEnd` is `Copy`, so `pipe_end_get` handed out a
// snapshot containing a bare `u64` handle with no ownership attached, and
// `close_pipe_end` called `close_raw` on that same handle immediately. So the
// pre-2026-08-12 code was not merely unable to wake a parked reader — it CLOSED
// THE HANDLE UNDERNEATH IT, which is a use-after-close the instant the OS
// recycles the handle number and hands it to an unrelated file. (The census in
// W7-47-w2-cluster.md records these four as "removing the map entry does not
// close the handle the parked thread holds". The entry is not removed and the
// handle is closed; the defect is worse than the row says, not milder.)
//
// # The mechanism, and why it is two halves rather than one
//
// * `in_flight` / `close_pending` — an ownership discipline that gives the
//   handle the lifetime the socket sites get from their `Arc`. A thread
//   announces itself with `pipe_enter` and leaves with `pipe_leave`; `close`
//   sets `closed` immediately but defers `close_raw` to the last thread out.
//   Without this half, the generation check below would be a check the answer
//   to which arrives too late to matter.
// * `closed` re-read through `pipe_still_open` — the generation question, asked
//   between bounded readiness probes rather than once at entry. Without this
//   half, `in_flight` alone would merely make an unbounded park safe instead of
//   ending it.
//
// The readiness probe is what keeps the thread OUT of the syscall so it can ask
// at all; see `pipe_readable` / `pipe_writable`.

/// How long a parked pipe read/write waits before re-asking whether the end was
/// closed under it. Smaller than the 25 ms the socket sites use because the
/// Windows read probe is a zero-timeout query plus a sleep, so this value is a
/// genuine latency cost there rather than only a liveness bound; on Unix it is
/// a real `poll(2)` timeout and costs nothing.
const PIPE_CLOSE_POLL_MS: i32 = 5;

/// The error a parked pipe operation reports once its end has been closed from
/// another thread. `ErrorKind::Interrupted` is the carrier every close-aware
/// path in this tree uses, and it is unambiguous here because neither probe
/// reports EINTR as an error.
fn pipe_closed_err() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "channel closed")
}

/// Announce an in-flight operation on `id` and take a snapshot of the end.
///
/// `None` means the id is unknown or already closed — the caller must not touch
/// `raw`. Every `Some` MUST be paired with exactly one [`pipe_leave`].
fn pipe_enter(id: i32) -> Option<PipeEnd> {
    let mut g = pipe_table().write().ok()?;
    let end = g.get_mut(&id)?;
    if end.closed {
        return None;
    }
    end.in_flight = end.in_flight.saturating_add(1);
    Some(*end)
}

/// A snapshot of the end registered under `id`, or `None` for an unknown id.
///
/// NOT a licence to touch `end.raw`: the snapshot's handle is a bare `u64` and
/// a concurrent close can invalidate it the moment the read guard drops. Use
/// [`pipe_enter`] for anything that issues a syscall. Kept because the table's
/// own unit tests read the registration back through it, and because the
/// distinction between "look at the record" and "touch the handle" is exactly
/// what the bracket exists to draw.
fn pipe_end_get(id: i32) -> Option<PipeEnd> {
    pipe_table().read().ok()?.get(&id).copied()
}

/// Retire an in-flight operation. If a close arrived while this thread was
/// inside the syscall and this is the last thread out, the handle is closed
/// here — which is the point at which closing it is finally safe.
fn pipe_leave(id: i32) {
    let Ok(mut g) = pipe_table().write() else {
        return;
    };
    let Some(end) = g.get_mut(&id) else {
        return;
    };
    end.in_flight = end.in_flight.saturating_sub(1);
    if end.in_flight == 0 && end.close_pending {
        end.close_pending = false;
        close_raw(end.raw);
    }
}

/// The generation question: is `id` still open? Flips exactly when Java closed
/// it. A fresh read guard per call, never held across a syscall.
fn pipe_still_open(id: i32) -> bool {
    pipe_table()
        .read()
        .ok()
        .and_then(|g| g.get(&id).map(|e| !e.closed))
        .unwrap_or(false)
}

fn close_pipe_end(id: i32) -> bool {
    let mut g = match pipe_table().write() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(end) = g.get_mut(&id) {
        if !end.closed {
            end.closed = true;
            if end.in_flight == 0 {
                close_raw(end.raw);
            } else {
                // Somebody is inside a syscall on this handle. Closing it now
                // is the use-after-close described above; the last thread out
                // does it in `pipe_leave` instead. `closed` is already set, so
                // that thread's next `pipe_still_open` ends its park.
                end.close_pending = true;
            }
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Platform-specific kernel interactions
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use super::PipeEnd;

    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        let mut fds: [libc::c_int; 2] = [0, 0];
        // SAFETY: `pipe(2)` writes two ints into the array we pass.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            PipeEnd {
                raw: fds[0] as u64,
                is_sink: false,
                closed: false,
                in_flight: 0,
                close_pending: false,
            },
            PipeEnd {
                raw: fds[1] as u64,
                is_sink: true,
                closed: false,
                in_flight: 0,
                close_pending: false,
            },
        ))
    }

    pub(super) fn read_pipe(raw: u64, buf: &mut [u8]) -> std::io::Result<isize> {
        // SAFETY: `read(2)` is thread-safe; we pass our own buffer.
        let n = unsafe {
            libc::read(
                raw as libc::c_int,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as isize)
        }
    }

    pub(super) fn write_pipe(raw: u64, buf: &[u8]) -> std::io::Result<isize> {
        // SAFETY: `write(2)` is thread-safe.
        let n = unsafe {
            libc::write(
                raw as libc::c_int,
                buf.as_ptr() as *const libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as isize)
        }
    }

    pub(super) fn close_raw(raw: u64) {
        // SAFETY: best-effort close; ignore errors (matches JDK).
        unsafe {
            libc::close(raw as libc::c_int);
        }
    }

    /// Wait up to `timeout_ms` for the pipe end to be readable / writable.
    ///
    /// `Some(Ok(true))` ready (or hung up, which the following `read` then
    /// reports as EOF and the following `write` as EPIPE); `Some(Ok(false))`
    /// the slice expired; `Some(Err(_))` the probe itself failed; `None` no
    /// probe on this target.
    ///
    /// EINTR is reported as "not ready", never as an error and never as an
    /// in-place re-poll: `poll(2)` is NEVER auto-restarted by `SA_RESTART`, and
    /// this VM signals its own threads on purpose (`jit::xt_root_scan` SIGUSR2s
    /// every thread for a cross-thread root scan). Re-polling in place would
    /// restart the whole wait on every GC. Same arm, same reason, as
    /// `native-io/src/net.rs`'s `net_poll_raw`.
    pub(super) fn poll_pipe(
        raw: u64,
        want_write: bool,
        timeout_ms: i32,
    ) -> Option<std::io::Result<bool>> {
        let mut pfd = libc::pollfd {
            fd: raw as libc::c_int,
            events: if want_write {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        // SAFETY: `pfd` is a single, fully-initialised `pollfd`; `nfds == 1`
        // matches the one-element buffer.
        let rc =
            unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1 as libc::nfds_t, timeout_ms) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted || error.raw_os_error() == Some(4) {
                return Some(Ok(false));
            }
            return Some(Err(error));
        }
        // `rc > 0` rather than a `revents` mask test: POLLERR/POLLHUP/POLLNVAL
        // are delivered whether or not they were requested and must count as
        // ready, so the syscall that follows surfaces the concrete error.
        // Treating them as not-ready would park a thread forever on a pipe that
        // can never become ready, which is the failure this path exists to
        // remove.
        Some(Ok(rc > 0))
    }
}

#[cfg(windows)]
mod platform {
    use super::PipeEnd;
    use std::ffi::c_void;

    // Raw Win32 FFI — same convention as `native-builtins/src/servlet.rs`'s
    // WSAPoll declaration, avoids pulling in `windows-sys` as a dep.
    type Handle = *mut c_void;
    type Bool = i32;

    #[link(name = "Kernel32")]
    extern "system" {
        fn CreatePipe(
            h_read_pipe: *mut Handle,
            h_write_pipe: *mut Handle,
            lp_pipe_attributes: *const c_void, // SECURITY_ATTRIBUTES; NULL OK
            n_size: u32,
        ) -> Bool;
        fn ReadFile(
            h_file: Handle,
            lp_buffer: *mut c_void,
            n_number_of_bytes_to_read: u32,
            lp_number_of_bytes_read: *mut u32,
            lp_overlapped: *mut c_void, // OVERLAPPED; NULL = synchronous
        ) -> Bool;
        fn WriteFile(
            h_file: Handle,
            lp_buffer: *const c_void,
            n_number_of_bytes_to_write: u32,
            lp_number_of_bytes_written: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        let mut hread: Handle = std::ptr::null_mut();
        let mut hwrite: Handle = std::ptr::null_mut();
        // SAFETY: `CreatePipe(read_handle, write_handle, NULL_attr,
        // 0_default_size)` — we pass null SECURITY_ATTRIBUTES (handles
        // not inheritable; Java pipes don't expose handles to child
        // processes anyway) and 0 for default 4 KiB buffer.
        let ok = unsafe { CreatePipe(&mut hread, &mut hwrite, std::ptr::null(), 0) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            PipeEnd {
                raw: hread as u64,
                is_sink: false,
                closed: false,
                in_flight: 0,
                close_pending: false,
            },
            PipeEnd {
                raw: hwrite as u64,
                is_sink: true,
                closed: false,
                in_flight: 0,
                close_pending: false,
            },
        ))
    }

    pub(super) fn read_pipe(raw: u64, buf: &mut [u8]) -> std::io::Result<isize> {
        let mut read: u32 = 0;
        // SAFETY: ReadFile is thread-safe per-handle.  Synchronous call
        // with NULL OVERLAPPED — blocks until the pipe has data or the
        // write end is closed (in which case it returns 0 = EOF).
        let ok = unsafe {
            ReadFile(
                raw as Handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // Treat ERROR_BROKEN_PIPE (109) as clean EOF.
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(109) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(read as isize)
    }

    pub(super) fn write_pipe(raw: u64, buf: &[u8]) -> std::io::Result<isize> {
        let mut written: u32 = 0;
        // SAFETY: `raw` is a live pipe handle owned by the registry; `buf`
        // remains readable and `written` writable for the synchronous call.
        let ok = unsafe {
            WriteFile(
                raw as Handle,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(written as isize)
    }

    pub(super) fn close_raw(raw: u64) {
        // SAFETY: best-effort close; ignore errors.
        unsafe {
            CloseHandle(raw as Handle);
        }
    }

    /// Readiness probe for a pipe end. See the Unix twin for the contract.
    ///
    /// # Read side
    ///
    /// `PeekNamedPipe` is documented to work on anonymous pipes (they are
    /// named pipes internally) and needs only the `GENERIC_READ` access
    /// `CreatePipe`'s read handle already has. It is a pure query: it consumes
    /// nothing, waits for nothing, and does not change the handle's mode — so
    /// unlike a `SetNamedPipeHandleState(PIPE_NOWAIT)` dance it cannot race a
    /// concurrent reader on the same handle into a spurious short read. Because
    /// it does not wait, the bounded wait is a `Sleep` between probes; that is
    /// the same sleep-cadence shape `net::net_accept_close_aware` uses, and it
    /// is why `PIPE_CLOSE_POLL_MS` is 5 rather than 25.
    ///
    /// `ERROR_BROKEN_PIPE` counts as READY, not as an error: the following
    /// `ReadFile` maps it to a clean 0-byte EOF, which is the answer the JDK
    /// specifies once the write end is gone.
    ///
    /// # Write side
    ///
    /// `None`, deliberately, and this is the one row of this family left open
    /// rather than closed. Windows offers no space-available query for the
    /// write end of a pipe. The two mechanisms that would give one both change
    /// how the handle behaves rather than merely observing it:
    /// `SetNamedPipeHandleState(PIPE_NOWAIT)` (documented as legacy LANMAN
    /// compatibility, and it alters every write on the handle), or creating the
    /// pipe with `CreateNamedPipe(FILE_FLAG_OVERLAPPED)` + `CreateFile` instead
    /// of `CreatePipe` and using a bounded `GetOverlappedResultEx`. The second
    /// is the correct fix and is named as the follow-up in
    /// W7-53-blocking-close-family.md; neither is a change worth making
    /// without a build to run it against, and answering `Some(Ok(false))` here
    /// would be strictly worse than `None` — it would spin a loop that could
    /// never report readiness while the caller believed it was close-aware.
    ///
    /// `None` routes the caller to a plain blocking `WriteFile`, with the
    /// generation check still applied BEFORE it and the deferred close still
    /// applied after. So a Windows sink write does not risk a use-after-close,
    /// and observes a close that has already happened — it is only a close
    /// arriving while it is inside `WriteFile` that it still cannot see.
    ///
    /// That `WriteFile` is SLICED to `PIPE_WRITE_SLICE_MAX` rather than issued
    /// once for the whole remainder, so the generation check runs between
    /// slices. Read that as a narrowing of the blind window (one slice instead
    /// of the whole payload), NOT as the wakeup: a reader that has stopped
    /// entirely still parks the first slice forever. The row stays open.
    pub(super) fn poll_pipe(
        raw: u64,
        want_write: bool,
        timeout_ms: i32,
    ) -> Option<std::io::Result<bool>> {
        if want_write {
            return None;
        }
        #[link(name = "Kernel32")]
        extern "system" {
            fn PeekNamedPipe(
                h_named_pipe: Handle,
                lp_buffer: *mut c_void,
                n_buffer_size: u32,
                lp_bytes_read: *mut u32,
                lp_total_bytes_avail: *mut u32,
                lp_bytes_left_this_message: *mut u32,
            ) -> Bool;
            fn Sleep(dw_milliseconds: u32);
        }
        let mut avail: u32 = 0;
        // SAFETY: a NULL buffer with size 0 asks for the counts alone, which is
        // the documented way to use `PeekNamedPipe` as a pure query; `avail` is
        // a live local for the duration of the call.
        let ok = unsafe {
            PeekNamedPipe(
                raw as Handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            // ERROR_BROKEN_PIPE (109): the write end is gone. Ready — the
            // `ReadFile` that follows turns it into EOF.
            if err.raw_os_error() == Some(109) {
                return Some(Ok(true));
            }
            return Some(Err(err));
        }
        if avail > 0 {
            return Some(Ok(true));
        }
        if timeout_ms > 0 {
            // SAFETY: no invariants; bounds the wait so the caller can re-ask
            // the generation.
            unsafe { Sleep(timeout_ms as u32) };
        }
        Some(Ok(false))
    }
}

// On non-unix-non-windows platforms, fail at runtime with IOException
// rather than silently noop — keeps the surface honest.
#[cfg(not(any(unix, windows)))]
mod platform {
    use super::PipeEnd;
    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.open: platform not supported",
        ))
    }
    pub(super) fn read_pipe(_: u64, _: &mut [u8]) -> std::io::Result<isize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.read: platform not supported",
        ))
    }
    pub(super) fn write_pipe(_: u64, _: &[u8]) -> std::io::Result<isize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.write: platform not supported",
        ))
    }
    pub(super) fn close_raw(_: u64) {}
    pub(super) fn poll_pipe(_: u64, _: bool, _: i32) -> Option<std::io::Result<bool>> {
        None
    }
}

use platform::{close_raw, create_anonymous_pipe, poll_pipe, read_pipe, write_pipe};

// ---------------------------------------------------------------------------
// Java-side helpers
// ---------------------------------------------------------------------------

fn io_error(message: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
        message: message.into(),
    }))
}

/// Build a real `java.nio.channels.AsynchronousCloseException`.
///
/// The concrete type is load-bearing, not decoration: `Pipe.SourceChannel` /
/// `SinkChannel` are `java.nio.channels` types, and a caller that closes a pipe
/// end from another thread catches `AsynchronousCloseException` (or its
/// `ClosedChannelException` supertype). A bare `IOException` whose message
/// merely mentions the name walks straight past that catch — the same mistake
/// `sc_read` used to make and that `socket_channel::channel_exception` was
/// written to fix. Falls back to a plain IOException if the class cannot be
/// built, which is the pre-2026-08-12 answer.
fn async_close_error(ctx: &mut dyn NativeContext, what: &str) -> MethodCallFailed {
    match ctx.new_object_initialized("java/nio/channels/AsynchronousCloseException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => {
            let pin = ctx.pin_native_root(exc);
            let exc = ctx.read_native_pin(pin, exc);
            ctx.unpin_native_roots(pin);
            MethodCallFailed::ExceptionThrown(exc)
        }
        _ => io_error(format!("{what}: channel closed")),
    }
}

/// A blocking pipe read that observes a close of the end it is reading.
///
/// Callers must be inside a [`pipe_enter`]/[`pipe_leave`] bracket: this
/// function reads `raw` on the strength of that bracket keeping the handle
/// alive, which is the half of the mechanism the generation check below cannot
/// supply on its own. See the module's "Close-awareness" section.
///
/// # On expiry
///
/// The per-pass `PIPE_CLOSE_POLL_MS` slice expiring is not an outcome — it is
/// the point at which [`pipe_still_open`] is re-asked, and the loop continues.
/// There is no second deadline: `Pipe.SourceChannel` has no read timeout.
fn pipe_read_close_aware(id: i32, raw: u64, buf: &mut [u8]) -> std::io::Result<isize> {
    loop {
        // Asked BEFORE the first probe as well as after every one, because
        // unlike the socket sites this thread may have been handed a snapshot
        // of an end that has since been closed, and touching `raw` after that
        // is the use-after-close the bracket exists to prevent.
        if !pipe_still_open(id) {
            return Err(pipe_closed_err());
        }
        match poll_pipe(raw, false, PIPE_CLOSE_POLL_MS) {
            // No probe on this target: the pre-2026-08-12 blocking read, which
            // cannot see a close that lands while it is parked but is at least
            // no longer racing the handle out from under itself.
            None => return read_pipe(raw, buf),
            Some(Err(e)) => return Err(e),
            Some(Ok(false)) => continue,
            Some(Ok(true)) => return read_pipe(raw, buf),
        }
    }
}

/// Largest payload handed to one `write` while a blocking pipe write is sliced.
///
/// A pipe write of N bytes does not return until all N are in the buffer, and
/// the Windows `CreatePipe` default buffer is only 4 KiB, so an unsliced write
/// of a large payload parks for as long as the reader takes. 4 KiB matches that
/// buffer; the common small write is issued whole.
const PIPE_WRITE_SLICE_MAX: usize = 4 * 1024;

/// A blocking pipe write that observes a close of the end it is writing — the
/// write twin of [`pipe_read_close_aware`], with the same bracket requirement.
///
/// Returns the bytes transferred. A close after a partial transfer answers the
/// partial count; a close with nothing out returns [`pipe_closed_err`].
fn pipe_write_close_aware(id: i32, raw: u64, data: &[u8]) -> std::io::Result<isize> {
    let mut written: usize = 0;
    loop {
        if written == data.len() {
            return Ok(written as isize);
        }
        if !pipe_still_open(id) {
            if written > 0 {
                return Ok(written as isize);
            }
            return Err(pipe_closed_err());
        }
        match poll_pipe(raw, true, PIPE_CLOSE_POLL_MS) {
            // No write-readiness probe on this target — Windows, today. There
            // is no wakeup to be had here and none is invented: a close that
            // lands while this thread is inside `WriteFile` is still invisible
            // to it, and W7-53-blocking-close-family.md keeps that row OPEN.
            //
            // What IS done is bound how long "inside `WriteFile`" lasts. The
            // remainder used to be handed to ONE unsliced write, so a close
            // arriving during a 1 MiB payload was unobservable for the whole
            // payload — the generation check above ran once and then the thread
            // was gone for the duration. Slicing it to `PIPE_WRITE_SLICE_MAX`
            // makes that check run between slices, which is the identical
            // argument `net.rs`'s `NET_WRITE_SLICE_MAX` makes for a blocking
            // `send` ("a single unsliced send parks for an unbounded time and
            // observes no close"), and it needs no new Win32 binding.
            //
            // The bound this buys is honest and worth stating precisely: the
            // window is now one slice, i.e. as long as the reader takes to
            // drain 4 KiB, rather than as long as it takes to drain everything.
            // On a reader that has stopped entirely the FIRST slice still parks
            // forever, which is exactly the residual the record names and is
            // why this is a narrowing rather than a fix.
            None => {
                let end = (written + PIPE_WRITE_SLICE_MAX).min(data.len());
                match write_pipe(raw, &data[written..end]) {
                    Ok(n) if n > 0 => written += n as usize,
                    // Accepted nothing and reported no error. Unlike the
                    // `Some(Ok(true))` arm there is no probe here to bound a
                    // retry, so looping would be a busy spin; answer the
                    // partial count instead, which is what a caller of a
                    // gathering write can already receive.
                    Ok(_) => {
                        return if written > 0 {
                            Ok(written as isize)
                        } else {
                            Err(pipe_closed_err())
                        };
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Some(Err(e)) => return Err(e),
            Some(Ok(false)) => continue,
            Some(Ok(true)) => {
                let end = (written + PIPE_WRITE_SLICE_MAX).min(data.len());
                match write_pipe(raw, &data[written..end]) {
                    Ok(n) if n > 0 => written += n as usize,
                    // Writable, then not: park again rather than report a short
                    // write the caller did not ask for.
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// This crate's **private** slot map for a pipe channel, expressed RELATIVE to
/// the base [`channel_private_base`] resolves — never as absolute slot 0..3.
///
/// ```text
///   base + 0: id          (Int)  — index into pipe_table
///   base + 1: open_flag   (Int)  — 1 = open, 0 = closed
///   base + 2: is_sink     (Int)  — 1 = sink, 0 = source
///   base + 3: blocking    (Int)  — 1 = blocking (write-only bookkeeping)
/// ```
///
/// # Why it is not indexed from 0 any more (G46-1, 2026-08-17)
///
/// Until this fix the four writes went to absolute slots 0..3, which is a
/// STATEMENT ABOUT THE LAYOUT and is true only of a fabricated stub. Against
/// the real image the receiver's class is `sun.nio.ch.SourceChannelImpl` /
/// `SinkChannelImpl`, and `javap -p` over Adoptium 25.0.3+9 gives the real
/// transitive layout (superclass first, statics excluded):
///
/// ```text
///   0 closeLock          : Ljava/lang/Object;                 AbstractInterruptibleChannel
///   1 closed             : Z
///   2 interruptor        : Lsun/nio/ch/Interruptible;
///   3 interruptedTarget  : Ljava/lang/Object;
///   4 provider           : Ljava/nio/channels/spi/SelectorProvider;   AbstractSelectableChannel
///   5 keys               : [Ljava/nio/channels/SelectionKey;
///   6 keyCount           : I
///   7 keyLock            : Ljava/lang/Object;
///   8 regLock            : Ljava/lang/Object;
///   9 nonBlocking        : Z
///  10 sc                 : L…SocketChannel;                   S{ource,ink}ChannelImpl
/// ```
///
/// So three of the four writes landed on REFERENCE slots and
/// `heap::coerce_field_value_by_descriptor` nulled every one of them
/// (`G30-1-the-silent-reference-slot-coercion-20260817.md`). MEASURED on
/// `target-rel3` (`9ae371468`), `--jdk-only`, `CRATONVM_DBG_COERCION=1`, one
/// `Pipe.open()`: six `primitive-into-reference` events, `descriptor=L`, at
/// the three lines below — and the *pipe id* was one of them, so the first
/// `sink.write(…)` died with `java.io.IOException: SinkChannel.write: missing
/// pipe id`. The open flag, meanwhile, aliased `closed : boolean` with the
/// polarity INVERTED: a fresh channel was `closed = true` and `close()` set
/// `closed = false`, i.e. exactly the `java.net.ServerSocket.bound` shape
/// G16-1/G38-1 record ("close() made the socket become bound").
///
/// The remedy is the one `native-io/src/lib.rs`'s `MBB_PRIVATE_*` map already
/// uses (W7-68) and that `cratonvm_native_api::appended_slots` exists for:
/// start the private map ABOVE every field the real class declares, and
/// collapse the base to 0 exactly when the class is a fabricated stub, where
/// the private map IS the layout.
///
/// # Sound as a per-class base
///
/// `appended_slots` cannot make a native safe against a receiver it did not
/// allocate (W7-49 §8). Every receiver that reaches these slots is one
/// [`alloc_channel`] produced: MEASURED with `--dump-native-registry` on the
/// same binary, `native-io/src/pipe.rs` OWNS every `Pipe.open`/`source`/`sink`
/// row and every `read`/`write`/`isOpen`/`close` row on both the
/// `sun/nio/ch/*ChannelImpl` classes and the abstract `Pipe$*Channel` ones;
/// the twin registrations in `native-builtins/src/phases_late/net_channels.rs`
/// all report `owns_slot=false` except the two `configureBlocking` rows, and
/// those are an identity that writes no field. [`channel_private_base`]'s
/// width guard covers the remainder.
const PIPE_FIELD_ID: usize = 0;
const PIPE_FIELD_OPEN: usize = 1;
const PIPE_FIELD_KIND: usize = 2;
const PIPE_FIELD_BLOCKING: usize = 3;
/// How many private slots [`alloc_channel`] appends above the real layout.
const PIPE_CHANNEL_PRIVATE_SLOTS: usize = 4;

/// Pipe wrapper layout (2 fields):
///   slot 0: source  (Object) — SourceChannelImpl
///   slot 1: sink    (Object) — SinkChannelImpl
///
/// These two stay at absolute 0/1 ON PURPOSE, and the reason is measured, not
/// assumed: `javap -p java.nio.channels.Pipe` on Adoptium 25.0.3+9 declares
/// ZERO instance fields, so `appended_slots::base_for_class` would answer 0
/// and applying it here would move nothing. The pair also holds only
/// `Value::Object` references, which the `b'L'` coercion arm passes through
/// untouched — and indeed the `Pipe.open()` measurement above attributes none
/// of its six events to the wrapper writes. Leaving them put also keeps this
/// model byte-identical to the twin at `net_channels.rs:2242`, which indexes
/// the same wrapper from 0.
const PIPE_WRAPPER_FIELD_SOURCE: usize = 0;
const PIPE_WRAPPER_FIELD_SINK: usize = 1;

/// Where this crate's private slot map starts on `class_name`.
///
/// One function, called the same way by the allocator and by every accessor,
/// is what keeps the two from ever disagreeing — the reason
/// `cratonvm_native_api::appended_slots` deliberately exposes only one of
/// these.
fn channel_base_for_class(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    cratonvm_native_api::appended_slots::base_for_class(ctx, class_name)
}

/// [`channel_base_for_class`] for an accessor, which holds the receiver rather
/// than the class name.
///
/// The width guard is load-bearing in both directions:
///
///   * a receiver this crate did NOT allocate (a real `SourceChannelImpl`
///     built by JDK bytecode, or a stub-mode object) is too narrow for
///     `base + 4`, so the base collapses to 0 and the accessor reads the same
///     slots it read before this fix — the pre-existing answer, never a new
///     refusal and never an out-of-range access;
///   * [`alloc_channel`]'s class-resolution-FAILED arm allocates against
///     `ClassId::new(0)`, which `NativeContextImpl::alloc_object` substitutes
///     with `cratonvm/synthetic/AnonymousObject$4`. That substitute declares
///     four fields, so a later `base_for_class` on the receiver would answer
///     4 and disagree with the 0 the allocator used. The width check
///     (`4 < 4 + 4`) sends it back to 0, which is the base that was used.
fn channel_private_base(ctx: &mut dyn NativeContext, this: ObjectRef) -> usize {
    let class_id = ctx.class_id_of_object(this);
    // Bound to a local before the match: the arm needs `ctx` mutably, and a
    // `match ctx.class_name_of_id(..)` keeps the scrutinee's shared reborrow
    // alive for the whole match.
    let class_name = ctx.class_name_of_id(class_id);
    let base = match class_name {
        Some(name) => channel_base_for_class(ctx, &name),
        None => 0,
    };
    if ctx.object_num_fields(this) >= base + PIPE_CHANNEL_PRIVATE_SLOTS {
        base
    } else {
        0
    }
}

fn alloc_channel(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    is_sink: bool,
    id: i32,
) -> ObjectRef {
    // Resolved BEFORE the allocation, so no GC can run between deciding the
    // base and writing through it.
    let base = channel_base_for_class(ctx, class_name);
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or_else(|_| ClassId::new(0));
    // `base + 4`, not a flat 4: on the real class the four private slots must
    // sit above `closeLock`/`closed`/`interruptor`/`interruptedTarget`.
    // `alloc_object` clamps the count UP to the class's declared width, so on
    // a stub (`base == 0`) this is the historic four-slot object unchanged.
    let obj = ctx.alloc_object(cid, base + PIPE_CHANNEL_PRIVATE_SLOTS);
    ctx.set_field(obj, base + PIPE_FIELD_ID, Value::Int(id));
    ctx.set_field(obj, base + PIPE_FIELD_OPEN, Value::Int(1));
    ctx.set_field(
        obj,
        base + PIPE_FIELD_KIND,
        Value::Int(if is_sink { 1 } else { 0 }),
    );
    ctx.set_field(obj, base + PIPE_FIELD_BLOCKING, Value::Int(1));
    // …and the REAL field, by name, with the polarity the JDK declares.
    // `AbstractInterruptibleChannel.isOpen()` is `return !closed`, so a fresh
    // channel is `closed = false`. This is the write the indexed open flag was
    // accidentally making with the sign reversed; it is a correct value in the
    // slot the class actually declares for it, and it is a no-op on any layout
    // that does not declare the name.
    //
    // `closeLock`, `interruptor`, `provider` and `keyLock`/`regLock` are
    // deliberately NOT written: this crate has no correct value for any of
    // them (a fresh `java.lang.Object` for `closeLock` would need an
    // allocation between the writes above, i.e. a GC point over a receiver
    // held in a bare local), and they read back null either way. NOMINATED
    // rather than guessed — see G46-1 §5.
    ctx.set_field_by_name(obj, "closed", Value::Int(0));
    obj
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

/// Decoded view of a heap-backed `java.nio.ByteBuffer`.
///
/// `base` is the buffer's `offset` field — the index inside `arr` that the
/// buffer's *own* index 0 maps to. It is 0 for `ByteBuffer.allocate(n)` and
/// for `ByteBuffer.wrap(a)`/`wrap(a, off, len)` (those encode the window in
/// `position`/`limit`), but NON-ZERO for every buffer produced by
/// `slice()` — `HeapByteBuffer.slice()` builds
/// `new HeapByteBuffer(hb, -1, 0, rem, rem, pos + offset)`, i.e. it resets
/// `position` to 0 and folds the parent position into `offset`.
struct HeapBufferView {
    arr: ObjectRef,
    /// `ByteBuffer.offset` — see the struct doc.
    base: i32,
    position: i32,
    limit: i32,
}

impl HeapBufferView {
    /// Absolute index into `arr` of the buffer's current `position`.
    fn index(&self) -> usize {
        self.base.saturating_add(self.position).max(0) as usize
    }

    /// `limit - position`, never negative.
    fn remaining(&self) -> i32 {
        self.limit.saturating_sub(self.position).max(0)
    }
}

/// Extract the underlying byte-array + offset/position/limit from a Java
/// `ByteBuffer`-shaped object.  Tolerates several layouts:
///   * Heap ByteBuffer: field "hb" or slot 5 holds a `byte[]`.
///   * Position is at field "position" or slot 0.
///   * Limit is at field "limit" or slot 1.
///   * Offset is at field "offset" (real JDK only; absent/0 in the
///     synthetic layout).
///
/// Returns None if no recognizable layout (in particular for a
/// `DirectByteBuffer`, whose `hb` is null — those have no backing array and
/// must be handled by the caller).
///
/// AUDIT 2026-07-26 (native-io-audit): this used to drop `offset` entirely
/// and index `arr` by the raw `position`. Every sliced buffer therefore
/// read/wrote the WRONG region of the backing array — silently, with no
/// exception — which is the same failure shape as the historical
/// `DirectByteBuffer.put` and ByteBuffer mark/reset aliasing defects.
/// `socket_channel::buffer_access` already handled `offset` correctly; this
/// module was the divergent copy.
fn buffer_view(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<HeapBufferView> {
    let arr = match ctx.get_field_by_name(buf, "hb") {
        Value::Object(Some(a)) => a,
        _ => match ctx.get_field(buf, 5) {
            Value::Object(Some(a)) => a,
            _ => return None,
        },
    };
    let position = match ctx.get_field_by_name(buf, "position") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    let limit = match ctx.get_field_by_name(buf, "limit") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 1) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    // `offset` only exists on the real-JDK `ByteBuffer`; the synthetic
    // layout has no such field, so a non-Int / negative read means 0.
    let base = match ctx.get_field_by_name(buf, "offset") {
        Value::Int(v) if v >= 0 => v,
        _ => 0,
    };
    Some(HeapBufferView {
        arr,
        base,
        position,
        limit,
    })
}

fn buffer_set_position(ctx: &mut dyn NativeContext, buf: ObjectRef, new_pos: i32) {
    ctx.set_field_by_name(buf, "position", Value::Int(new_pos));
    ctx.set_field(buf, 0, Value::Int(new_pos));
}

// ---------------------------------------------------------------------------
// Native handlers
// ---------------------------------------------------------------------------

/// `java.nio.channels.Pipe.open()` — allocate a new (source, sink)
/// pair backed by a real kernel pipe.  Returns the wrapper.
fn pipe_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let (read_end, write_end) =
        create_anonymous_pipe().map_err(|e| io_error(format!("Pipe.open: {e}")))?;
    let read_id = register_pipe_end(read_end);
    let write_id = register_pipe_end(write_end);
    let source = alloc_channel(ctx, "sun/nio/ch/SourceChannelImpl", false, read_id);
    let sink = alloc_channel(ctx, "sun/nio/ch/SinkChannelImpl", true, write_id);

    // The wrapper is minted AS `sun/nio/ch/PipeImpl`, the class HotSpot
    // 25.0.3+9 constructs here, and NOT as the abstract `java/nio/channels/
    // Pipe` this line used to name.
    //
    // **The defect.** `ensure_class_initialized("java/nio/channels/Pipe")`
    // resolves to the real, ABSTRACT JDK class, so `alloc_object` then minted
    // an object whose runtime class is abstract — a receiver `new` cannot
    // produce (JVMS §6.5 makes it an `InstantiationError`). MEASURED
    // (`H21-1` §2): `Pipe.open().getClass()` answered
    // `java.nio.channels.Pipe`, `Modifier.isAbstract == true`, in BOTH modes,
    // against `sun.nio.ch.PipeImpl` on the oracle. The two CHANNELS were
    // already right (`SourceChannelImpl` / `SinkChannelImpl`, above); only the
    // wrapper was not.
    //
    // **Why the slot indices survive the change, measured rather than hoped.**
    // `javap -p sun.nio.ch.PipeImpl` on 25.0.3+9 declares exactly two instance
    // fields, in this order:
    //
    //     private final sun.nio.ch.SourceChannelImpl source;   // slot 0
    //     private final sun.nio.ch.SinkChannelImpl   sink;     // slot 1
    //
    // — the same two slots, in the same order, that `PIPE_WRAPPER_FIELD_SOURCE`
    // / `_SINK` already name, holding values of exactly the declared types. So
    // the writes below land on the REAL fields instead of on private slots
    // above a zero-field layout, and `PipeImpl.source()`'s own bytecode would
    // return the right object even if it ran. It does not run: `source`/`sink`
    // are registered on `sun/nio/ch/PipeImpl` as well as on the abstract class
    // (see `register_pipe_real`), and dispatch keys on the receiver
    // (`H11-1`), so these natives keep answering.
    //
    // `Pipe.open()` itself is STATIC — no receiver — so its registration stays
    // on `java/nio/channels/Pipe`, where the constant-pool class is the key.
    // `[route discrim]`.
    // Written as a `match` rather than `.or_else(|_| ctx…)` on purpose: the
    // closure form needs a second mutable borrow of `ctx` and this lane was
    // forbidden to build, so it may not lean on a borrow-checker judgement it
    // cannot check.
    let pipe_cid = match ctx.ensure_class_initialized("sun/nio/ch/PipeImpl") {
        Ok(cid) => cid,
        Err(_) => ctx
            .ensure_class_initialized("java/nio/channels/Pipe")
            .unwrap_or_else(|_| ClassId::new(0)),
    };
    let wrapper = ctx.alloc_object(pipe_cid, 2);
    ctx.set_field(
        wrapper,
        PIPE_WRAPPER_FIELD_SOURCE,
        Value::Object(Some(source)),
    );
    ctx.set_field(wrapper, PIPE_WRAPPER_FIELD_SINK, Value::Object(Some(sink)));
    Ok(Some(Value::Object(Some(wrapper))))
}

/// `java.nio.channels.Pipe.source()` — return the cached source channel.
fn pipe_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    if ctx.object_num_fields(this) >= 1 {
        return Ok(Some(ctx.get_field(this, PIPE_WRAPPER_FIELD_SOURCE)));
    }
    Ok(Some(Value::Object(None)))
}

/// `java.nio.channels.Pipe.sink()` — return the cached sink channel.
fn pipe_sink(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    if ctx.object_num_fields(this) >= 2 {
        return Ok(Some(ctx.get_field(this, PIPE_WRAPPER_FIELD_SINK)));
    }
    Ok(Some(Value::Object(None)))
}

/// `sun.nio.ch.SinkChannelImpl.write(ByteBuffer)` — write the buffer's
/// `[position, limit)` slice to the pipe.  Returns the number of bytes
/// actually written and advances `position` accordingly.
fn sink_write_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SinkChannel.write: null this"));
    };
    let base = channel_private_base(ctx, this);
    let id = match ctx.get_field(this, base + PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SinkChannel.write: missing pipe id")),
    };
    // `pipe_enter` both answers "is this end still open?" and pins the raw
    // handle against a concurrent close for the duration; every exit below must
    // reach exactly one `pipe_leave`.
    let end = pipe_enter(id).ok_or_else(|| io_error("SinkChannel.write: closed or unknown"))?;
    if !end.is_sink {
        pipe_leave(id);
        return Err(io_error(
            "SinkChannel.write: wrong end (source registered as sink)",
        ));
    }
    let Some(buf) = arg_obj(args, 1) else {
        pipe_leave(id);
        return Err(io_error("SinkChannel.write: null buffer"));
    };
    let Some(view) = buffer_view(ctx, buf) else {
        pipe_leave(id);
        return Err(io_error(
            "SinkChannel.write: unrecognised ByteBuffer layout",
        ));
    };
    let remaining = view.remaining();
    if remaining <= 0 {
        pipe_leave(id);
        return Ok(Some(Value::Int(0)));
    }
    // Clamp to what actually exists behind `offset + position`; a bogus
    // `limit` must not turn into an out-of-range array read.
    let start = view.index();
    let avail = ctx.array_length(view.arr).saturating_sub(start);
    let to_write = (remaining as usize).min(avail);
    if to_write == 0 {
        pipe_leave(id);
        return Ok(Some(Value::Int(0)));
    }
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    let mut bytes = vec![0u8; to_write];
    ctx.read_byte_array_into(view.arr, start, &mut bytes);
    let position = view.position;
    // A sink write blocks once the kernel pipe buffer is full (the Windows
    // `CreatePipe` default is only 4 KiB, and `WriteFile` with a NULL
    // OVERLAPPED is synchronous). Publish this thread as GC-safe first, or a
    // concurrent stop-the-world pause waits on a mutator parked in the
    // kernel — the documented "STW blocking-region missing native I/O
    // family" hang shape. `buf` is used after the region, so re-sync it.
    let mut held = [Value::Object(Some(buf))];
    ctx.begin_blocking_region();
    let written = pipe_write_close_aware(id, end.raw, &bytes);
    ctx.end_blocking_region_refs(&mut held);
    pipe_leave(id);
    let buf = match held[0] {
        Value::Object(Some(o)) => o,
        _ => buf,
    };
    let n = match written {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            return Err(async_close_error(ctx, "SinkChannel.write"));
        }
        Err(e) => return Err(io_error(format!("write: {e}"))),
    };
    if n > 0 {
        buffer_set_position(ctx, buf, position + n as i32);
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `sun.nio.ch.SourceChannelImpl.read(ByteBuffer)` — read up to
/// `limit - position` bytes from the pipe into the buffer.  Returns
/// the number of bytes read or -1 on EOF.
fn source_read_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SourceChannel.read: null this"));
    };
    let base = channel_private_base(ctx, this);
    let id = match ctx.get_field(this, base + PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SourceChannel.read: missing pipe id")),
    };
    // See `sink_write_buffer` — `pipe_enter` answers the liveness question AND
    // pins the raw handle; every exit must reach exactly one `pipe_leave`.
    let end = pipe_enter(id).ok_or_else(|| io_error("SourceChannel.read: closed or unknown"))?;
    if end.is_sink {
        pipe_leave(id);
        return Err(io_error(
            "SourceChannel.read: wrong end (sink registered as source)",
        ));
    }
    let Some(buf) = arg_obj(args, 1) else {
        pipe_leave(id);
        return Err(io_error("SourceChannel.read: null buffer"));
    };
    let Some(view) = buffer_view(ctx, buf) else {
        pipe_leave(id);
        return Err(io_error(
            "SourceChannel.read: unrecognised ByteBuffer layout",
        ));
    };
    // Clamp the read to what is actually addressable behind
    // `offset + position`, so a buffer whose `limit` overshoots its backing
    // array cannot make us read more than we can store.
    let start = view.index();
    let avail = ctx.array_length(view.arr).saturating_sub(start);
    let space = (view.remaining() as usize).min(avail);
    if space == 0 {
        pipe_leave(id);
        return Ok(Some(Value::Int(0)));
    }
    let position = view.position;
    let mut bytes = vec![0u8; space];
    // `read(2)` / `ReadFile` on an empty pipe blocks until the peer writes or
    // closes. Without this bracket a thread parked here never reaches a
    // safepoint, so a concurrent stop-the-world pause hangs the whole VM —
    // the "STW blocking-region missing native I/O family" shape. `arr` and
    // `buf` are both used after the region, so re-sync them through
    // `end_blocking_region_refs`.
    let mut held = [Value::Object(Some(view.arr)), Value::Object(Some(buf))];
    ctx.begin_blocking_region();
    let read = pipe_read_close_aware(id, end.raw, &mut bytes);
    ctx.end_blocking_region_refs(&mut held);
    pipe_leave(id);
    let arr = match held[0] {
        Value::Object(Some(o)) => o,
        _ => view.arr,
    };
    let buf = match held[1] {
        Value::Object(Some(o)) => o,
        _ => buf,
    };
    let n = match read {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            return Err(async_close_error(ctx, "SourceChannel.read"));
        }
        Err(e) => return Err(io_error(format!("read: {e}"))),
    };
    if n == 0 {
        // EOF — JDK signals -1.
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    let copy_len = (n as usize).min(bytes.len());
    ctx.write_byte_array_from(arr, start, &bytes[..copy_len]);
    buffer_set_position(ctx, buf, position + copy_len as i32);
    Ok(Some(Value::Int(copy_len as i32)))
}

/// `sun.nio.ch.SinkChannelImpl.write(byte[], int off, int len)` — direct
/// byte-array variant used by some JDK internal paths and helpful for
/// tests that don't want to allocate a real ByteBuffer.
fn sink_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SinkChannel.write[bytes]: null this"));
    };
    let base = channel_private_base(ctx, this);
    let id = match ctx.get_field(this, base + PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SinkChannel.write[bytes]: missing pipe id")),
    };
    let end = pipe_enter(id).ok_or_else(|| io_error("SinkChannel.write[bytes]: closed"))?;
    if !end.is_sink {
        pipe_leave(id);
        return Err(io_error("SinkChannel.write[bytes]: not a sink"));
    }
    let Some(arr) = arg_obj(args, 1) else {
        pipe_leave(id);
        return Err(io_error("SinkChannel.write[bytes]: null array"));
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let alen = ctx.array_length(arr);
    if off.saturating_add(len) > alen {
        pipe_leave(id);
        return Err(io_error(format!(
            "SinkChannel.write[bytes]: out of bounds off={off} len={len} alen={alen}"
        )));
    }
    let mut bytes = vec![0u8; len];
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    ctx.read_byte_array_into(arr, off, &mut bytes);
    // Same GC-cooperation contract as `sink_write_buffer` — a full kernel
    // pipe buffer parks this thread in `write(2)`/`WriteFile`. Nothing here
    // uses a Java ref after the region, so the plain `end_blocking_region`
    // is sufficient.
    ctx.begin_blocking_region();
    let written = pipe_write_close_aware(id, end.raw, &bytes);
    ctx.end_blocking_region();
    pipe_leave(id);
    let n = match written {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            return Err(async_close_error(ctx, "SinkChannel.write"));
        }
        Err(e) => return Err(io_error(format!("write: {e}"))),
    };
    Ok(Some(Value::Int(n as i32)))
}

/// `sun.nio.ch.SourceChannelImpl.read(byte[], int off, int len)`.
fn source_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SourceChannel.read[bytes]: null this"));
    };
    let base = channel_private_base(ctx, this);
    let id = match ctx.get_field(this, base + PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SourceChannel.read[bytes]: missing pipe id")),
    };
    let end = pipe_enter(id).ok_or_else(|| io_error("SourceChannel.read[bytes]: closed"))?;
    if end.is_sink {
        pipe_leave(id);
        return Err(io_error("SourceChannel.read[bytes]: not a source"));
    }
    let Some(arr) = arg_obj(args, 1) else {
        pipe_leave(id);
        return Err(io_error("SourceChannel.read[bytes]: null array"));
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let alen = ctx.array_length(arr);
    if off.saturating_add(len) > alen {
        pipe_leave(id);
        return Err(io_error(format!(
            "SourceChannel.read[bytes]: out of bounds off={off} len={len} alen={alen}"
        )));
    }
    let mut bytes = vec![0u8; len];
    // Blocking kernel read — same GC-cooperation contract as
    // `source_read_buffer`. `arr` is written after the region, so re-sync it.
    let mut held = [Value::Object(Some(arr))];
    ctx.begin_blocking_region();
    let read = pipe_read_close_aware(id, end.raw, &mut bytes);
    ctx.end_blocking_region_refs(&mut held);
    pipe_leave(id);
    let arr = match held[0] {
        Value::Object(Some(o)) => o,
        _ => arr,
    };
    let n = match read {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            return Err(async_close_error(ctx, "SourceChannel.read"));
        }
        Err(e) => return Err(io_error(format!("read: {e}"))),
    };
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    let copy_len = (n as usize).min(bytes.len());
    ctx.write_byte_array_from(arr, off, &bytes[..copy_len]);
    Ok(Some(Value::Int(copy_len as i32)))
}

fn channel_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    let base = channel_private_base(ctx, this);
    if ctx.object_num_fields(this) > base + PIPE_FIELD_OPEN {
        return Ok(Some(ctx.get_field(this, base + PIPE_FIELD_OPEN)));
    }
    Ok(Some(Value::Int(1)))
}

fn channel_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let base = channel_private_base(ctx, this);
    if ctx.object_num_fields(this) > base + PIPE_FIELD_OPEN {
        ctx.set_field(this, base + PIPE_FIELD_OPEN, Value::Int(0));
    }
    // The real field, by name and with the JDK's polarity —
    // `AbstractInterruptibleChannel.close()` is `if (!closed) { closed = true;
    // … }` and `isOpen()` is `return !closed`. Before G46-1 the indexed write
    // above aliased this very field and set it to FALSE here, so a closed pipe
    // channel told real bytecode it was open: the `ServerSocket.bound` shape.
    ctx.set_field_by_name(this, "closed", Value::Int(1));
    if let Value::Int(id) = ctx.get_field(this, base + PIPE_FIELD_ID) {
        close_pipe_end(id);
    }
    Ok(None)
}

fn channel_configure_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let blocking = arg_int(args, 1);
    let base = channel_private_base(ctx, this);
    if ctx.object_num_fields(this) > base + PIPE_FIELD_BLOCKING {
        ctx.set_field(this, base + PIPE_FIELD_BLOCKING, Value::Int(blocking));
    }
    // `AbstractSelectableChannel.isBlocking()` is `return !nonBlocking`, so the
    // real field is the NEGATION of the argument. Same by-name rule as
    // `closed`: a correct value in the slot the class declares, a no-op on a
    // layout that does not declare the name. Before G46-1 the indexed write
    // above landed on `interruptedTarget : Object` and was nulled.
    ctx.set_field_by_name(
        this,
        "nonBlocking",
        Value::Int(if blocking == 0 { 1 } else { 0 }),
    );
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: unknown — needs census. A pipe is an OS object, so the
// intent is bridge-shaped, but the registrations do not land where the JDK puts
// the boundary: none of the 19 resolvable triples is ACC_NATIVE in JDK 25, 12
// name methods absent from the real class, 3 shadow concrete bytecode and 2 are
// abstract. `java.nio.channels.Pipe` is an abstract factory whose real work
// happens in `sun.nio.ch.PipeImpl` over `Net`/`IOUtil` natives that this crate
// already bridges elsewhere. Likely disposition is "move down a layer or
// delete", but that needs `invocations` to confirm nothing depends on the
// current placement.
//
// CENSUS TAKEN 2026-08-20 (H11). The `invocations` this comment asked for, from
// `--dump-native-registry` on `fe59bf9d9` under `--jdk-only`, driving a real
// `Pipe.open()` + `write` + `read` + `close` through `Pipe.SinkChannel`- and
// `Pipe.SourceChannel`-TYPED locals (so the constant-pool class at every call
// site is the ABSTRACT nested class):
//
//     sun/nio/ch/SourceChannelImpl.read(Ljava/nio/ByteBuffer;)I   invocations: 1
//     sun/nio/ch/SinkChannelImpl.write(Ljava/nio/ByteBuffer;)I    invocations: 1
//     sun/nio/ch/{Source,Sink}ChannelImpl.close()V                invocations: 1
//     java/nio/channels/Pipe$SourceChannel.read(…)I               invocations: 0
//     java/nio/channels/Pipe$SinkChannel.write(…)I                invocations: 0
//     java/nio/channels/Pipe$SourceChannel.close()V               invocations: 0
//     java/nio/channels/Pipe$SinkChannel.close()V                 invocations: 0
//
// and `pipe.source().getClass().getName()` answers `sun.nio.ch.SourceChannelImpl`
// on CratonVM, the same string HotSpot 25.0.3+9 gives. So `pipe_open`'s choice
// to allocate the two `Impl` classes (below, ~line 1024) is the right one and
// this family is ALREADY "in the right place" — it is not an instance of the
// P1 row's "bridges on the abstract public API".
//
// The six abstract rows are consequently DEAD WEIGHT for every receiver this VM
// mints. **Do not delete them from this file on that basis alone.**
// `native-builtins/src/phases_late/net_channels.rs` (~2281–2400) registers the
// SAME six triples with a DIFFERENT implementation — its callbacks read the fd
// from field slot 1 and the open flag from slot 0, a layout unrelated to this
// module's. Today this file wins the slot (`owns_slot: true` on all six).
// Deleting these lines does not remove the registrations; it hands them to an
// incompatible body. `[2 producers, 1 slot]` / `[dup nati]`. Retiring them is a
// single commit spanning both crates, with a build.
//
// The one population that CAN still reach these rows is an application subclass
// of `Pipe.SourceChannel` / `Pipe.SinkChannel` (both constructors are
// `protected`): its receiver carries the user's class name, declares none of
// these methods, and `try_stackless_invoke`'s superclass walk then finds the
// abstract row and hijacks it. That is a hazard, not a service.
/// Register the WP3.7 Pipe natives.  Idempotent.
pub fn register_pipe_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pipe = "java/nio/channels/Pipe";
    // `open()` is STATIC. There is no receiver, so the key is the constant-pool
    // class and this row must stay on the abstract public class — the one place
    // in this function where the abstract spelling is the CORRECT one, not a
    // leftover. `H11-1` §2.1.
    r.register(pipe, "open", "()Ljava/nio/channels/Pipe;", pipe_open);
    // `source()` / `sink()` are INSTANCE methods, so the key is the receiver's
    // runtime class, and since 2026-08-21 `pipe_open` mints
    // `sun/nio/ch/PipeImpl` (see the comment at its allocation). Both spellings
    // are registered:
    //
    //   * `sun/nio/ch/PipeImpl` — the receiver this VM now builds, and the one
    //     that actually dispatches. Without this row the real `PipeImpl.source()`
    //     bytecode would run instead: harmless here only because the slot map
    //     happens to coincide, and this crate does not get to rely on that.
    //   * `java/nio/channels/Pipe` — kept, NOT retired. `native-builtins/src/
    //     phases_late/net_channels.rs` registers the same triples with an
    //     incompatible field layout and loses the slot to this file today;
    //     deleting these two lines would not remove a native, it would hand the
    //     slot to that body. `[2 producers, 1 slot]` / `H11-2` §4. It is also
    //     still the fallback class when `sun.nio.ch.PipeImpl` is absent from
    //     the image.
    for p in [pipe, "sun/nio/ch/PipeImpl"] {
        r.register(
            p,
            "source",
            "()Ljava/nio/channels/Pipe$SourceChannel;",
            pipe_source,
        );
        r.register(
            p,
            "sink",
            "()Ljava/nio/channels/Pipe$SinkChannel;",
            pipe_sink,
        );
    }

    // SourceChannelImpl
    let source = "sun/nio/ch/SourceChannelImpl";
    r.register(
        source,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        source_read_buffer,
    );
    r.register(source, "isOpen", "()Z", channel_is_open);
    r.register(source, "close", "()V", channel_close);
    r.register(
        source,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        channel_configure_blocking,
    );

    // Abstract SourceChannel — same implementations, different declared class.
    //
    // CORRECTION 2026-08-20 (H11): this comment used to end "so Java-side
    // dispatch lands here either way". It does not. Dispatch keys on the
    // RECEIVER's runtime class, which for every pipe this VM opens is
    // `sun/nio/ch/SourceChannelImpl` (see `pipe_open`), and the rows above
    // answer. MEASURED at `invocations: 0` for all three of these while the
    // `Impl` rows took the calls — the census block at the head of this
    // function has the numbers. Kept only because `native-builtins` registers
    // the same triples with an incompatible body; see that block before
    // touching a line here.
    let abstract_source = "java/nio/channels/Pipe$SourceChannel";
    r.register(
        abstract_source,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        source_read_buffer,
    );
    r.register(abstract_source, "isOpen", "()Z", channel_is_open);
    r.register(abstract_source, "close", "()V", channel_close);

    // SinkChannelImpl
    let sink = "sun/nio/ch/SinkChannelImpl";
    r.register(sink, "write", "(Ljava/nio/ByteBuffer;)I", sink_write_buffer);
    r.register(sink, "isOpen", "()Z", channel_is_open);
    r.register(sink, "close", "()V", channel_close);
    r.register(
        sink,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        channel_configure_blocking,
    );

    let abstract_sink = "java/nio/channels/Pipe$SinkChannel";
    r.register(
        abstract_sink,
        "write",
        "(Ljava/nio/ByteBuffer;)I",
        sink_write_buffer,
    );
    r.register(abstract_sink, "isOpen", "()Z", channel_is_open);
    r.register(abstract_sink, "close", "()V", channel_close);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn wp37_pipe_create_close_kernel_level() {
        // Direct kernel-level round-trip — bypass the Java plumbing.
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        assert!(!read_end.is_sink);
        assert!(write_end.is_sink);
        let payload: &[u8] = b"hello pipe";
        let n = write_pipe(write_end.raw, payload).expect("write");
        assert_eq!(n as usize, payload.len());
        let mut got = vec![0u8; payload.len()];
        let r = read_pipe(read_end.raw, &mut got).expect("read");
        assert_eq!(r as usize, payload.len());
        assert_eq!(&got, payload);
        close_raw(read_end.raw);
        close_raw(write_end.raw);
    }

    #[test]
    fn wp37_pipe_table_register_and_close() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let r_id = register_pipe_end(read_end);
        let w_id = register_pipe_end(write_end);
        assert!(r_id > 0 && w_id > 0 && r_id != w_id);
        let r = pipe_end_get(r_id).expect("read end registered");
        assert!(!r.is_sink);
        let w = pipe_end_get(w_id).expect("write end registered");
        assert!(w.is_sink);
        assert!(close_pipe_end(r_id));
        assert!(close_pipe_end(w_id));
        // Idempotent — second close returns false (already closed).
        assert!(!close_pipe_end(r_id));
    }

    #[test]
    fn wp37_pipe_table_unknown_id_returns_none() {
        assert!(pipe_end_get(i32::MAX - 1).is_none());
        assert!(!close_pipe_end(i32::MAX - 1));
    }

    // --- W7-53 (2026-08-12): close-awareness ---

    /// The row this whole family is about: a thread parked in a blocking pipe
    /// read must come back when another thread closes the end.
    ///
    /// RED-by-construction against the pre-2026-08-12 tree: `read_pipe` there
    /// parked in `ReadFile`/`read(2)` with `lpOverlapped = NULL`, and
    /// `close_pipe_end` neither woke it nor could have — it closed the handle
    /// out from under it instead.
    ///
    /// The read is proved to be genuinely parked before the close, not merely
    /// racing it: the reader publishes `started` and the closing thread waits
    /// for that AND for a further 100 ms, which is 20 poll slices, before it
    /// closes anything. A close issued ahead of the read would prove nothing,
    /// because the read would then return for an unrelated reason.
    #[test]
    fn a_close_wakes_a_reader_parked_on_a_pipe() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let r_id = register_pipe_end(read_end);
        let started = Arc::new(AtomicBool::new(false));
        let reader_started = Arc::clone(&started);
        let reader = std::thread::spawn(move || {
            let end = pipe_enter(r_id).expect("open at entry");
            reader_started.store(true, Ordering::SeqCst);
            let mut buf = [0u8; 16];
            let result = pipe_read_close_aware(r_id, end.raw, &mut buf);
            pipe_leave(r_id);
            result.map(|n| n as i64).map_err(|e| e.kind())
        });
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // Nobody writes to `write_end`, so the reader is parked on an empty
        // pipe for the whole of this sleep.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!reader.is_finished(), "the read did not park at all");
        close_pipe_end(r_id);
        let outcome = reader.join().expect("reader thread");
        assert_eq!(
            outcome,
            Err(std::io::ErrorKind::Interrupted),
            "a parked pipe read must observe the close"
        );
        close_raw(write_end.raw);
    }

    /// The close-aware read is still a read: the wakeup must not cost payload.
    #[test]
    fn a_close_aware_pipe_read_still_delivers_bytes() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let r_id = register_pipe_end(read_end);
        write_pipe(write_end.raw, b"payload").expect("write");
        let end = pipe_enter(r_id).expect("open");
        let mut buf = [0u8; 16];
        let n = pipe_read_close_aware(r_id, end.raw, &mut buf).expect("read");
        pipe_leave(r_id);
        assert_eq!(&buf[..n as usize], b"payload");
        close_pipe_end(r_id);
        close_raw(write_end.raw);
    }

    /// The other half of the mechanism: a close arriving while an operation is
    /// in flight must NOT close the raw handle underneath it.
    ///
    /// Before 2026-08-12 `close_pipe_end` called `close_raw` unconditionally,
    /// so the handle a parked thread was mid-syscall on was freed and its
    /// number could be recycled onto an unrelated file. That is why the
    /// generation check alone is not the fix here and is the fix on every
    /// socket site in this family: those park holding an `Arc<TcpStream>`,
    /// which keeps the handle alive for them.
    #[test]
    fn a_close_during_an_in_flight_op_defers_the_raw_close() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let r_id = register_pipe_end(read_end);
        let entered = pipe_enter(r_id).expect("open");
        assert!(close_pipe_end(r_id), "first close reports it did the work");
        let mid = pipe_end_get(r_id).expect("entry survives the close");
        assert!(mid.closed, "the generation flipped immediately");
        assert!(
            mid.close_pending,
            "the raw close is deferred while an op is in flight"
        );
        assert!(
            !pipe_still_open(r_id),
            "a parked op re-asking now must see the close"
        );
        // The handle is still valid here, which is the whole point: a thread
        // mid-syscall on `entered.raw` has not had it freed under it.
        assert_eq!(entered.raw, mid.raw);
        pipe_leave(r_id);
        let after = pipe_end_get(r_id).expect("entry still present");
        assert!(
            !after.close_pending,
            "the last op out performs the deferred close"
        );
        close_raw(write_end.raw);
    }

    // --- AUDIT 2026-07-26 (native-io-audit) regressions ---

    use crate::test_support::MockNativeContext;

    /// Build a real-JDK-shaped `HeapByteBuffer` over a fresh byte[] of
    /// `cap` bytes, with the given `offset`/`position`/`limit`.
    fn heap_buffer(
        ctx: &mut MockNativeContext,
        cap: usize,
        offset: i32,
        position: i32,
        limit: i32,
    ) -> (ObjectRef, ObjectRef) {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, cap);
        let bb = ctx.alloc_object(8);
        ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
        ctx.set_field_by_name(bb, "offset", Value::Int(offset));
        ctx.set_field_by_name(bb, "position", Value::Int(position));
        ctx.set_field_by_name(bb, "limit", Value::Int(limit));
        (bb, arr)
    }

    /// `buffer_view` used to drop the ByteBuffer `offset` field entirely and
    /// index the backing array by the raw `position`. Every buffer produced
    /// by `slice()` carries a non-zero `offset` (HeapByteBuffer.slice() resets
    /// position to 0 and folds the parent position into offset), so a pipe
    /// read/write against a sliced buffer silently touched the WRONG region
    /// of the backing array — no exception, wrong bytes.
    #[test]
    fn audit_buffer_view_honours_slice_offset() {
        let mut ctx = MockNativeContext::new();
        // A slice of a 16-byte array starting at index 6, 4 bytes long:
        // offset=6, position=0, limit=4.
        let (bb, _arr) = heap_buffer(&mut ctx, 16, 6, 0, 4);
        let view = buffer_view(&ctx, bb).expect("heap layout recognised");
        assert_eq!(view.base, 6, "offset must be read");
        assert_eq!(view.index(), 6, "array index is offset + position");
        assert_eq!(view.remaining(), 4);

        // With a non-zero position too: offset=6, position=1 -> index 7.
        ctx.set_field_by_name(bb, "position", Value::Int(1));
        let view = buffer_view(&ctx, bb).expect("heap layout recognised");
        assert_eq!(view.index(), 7);
        assert_eq!(view.remaining(), 3);
    }

    /// A plain `ByteBuffer.allocate()` (offset absent / 0) must be unchanged
    /// by the fix.
    #[test]
    fn audit_buffer_view_plain_allocate_unchanged() {
        let mut ctx = MockNativeContext::new();
        let (bb, _arr) = heap_buffer(&mut ctx, 8, 0, 2, 6);
        let view = buffer_view(&ctx, bb).expect("heap layout recognised");
        assert_eq!(view.base, 0);
        assert_eq!(view.index(), 2);
        assert_eq!(view.remaining(), 4);
    }

    /// The synthetic layout has no `offset` field at all; a missing (or
    /// non-Int) read must be treated as 0, not as a bogus base.
    #[test]
    fn audit_buffer_view_missing_offset_field_is_zero() {
        let mut ctx = MockNativeContext::new();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        let bb = ctx.alloc_object(8);
        ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
        ctx.set_field_by_name(bb, "position", Value::Int(1));
        ctx.set_field_by_name(bb, "limit", Value::Int(3));
        // No "offset" named field set -> get_field_by_name yields Object(None).
        let view = buffer_view(&ctx, bb).expect("heap layout recognised");
        assert_eq!(view.base, 0);
        assert_eq!(view.index(), 1);
    }

    /// A sliced destination buffer must receive the pipe bytes at
    /// `offset + position`, and the blocking read must be bracketed in a
    /// GC-cooperative blocking region.
    #[test]
    fn audit_source_read_writes_at_slice_offset_and_brackets_gc() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let read_id = register_pipe_end(read_end);
        let payload = b"XYZ";
        assert_eq!(
            write_pipe(write_end.raw, payload).expect("write") as usize,
            payload.len()
        );
        close_raw(write_end.raw);

        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(4);
        ctx.set_field(this, PIPE_FIELD_ID, Value::Int(read_id));
        // 16-byte array, slice starting at 6 with room for 3 bytes.
        let (bb, arr) = heap_buffer(&mut ctx, 16, 6, 0, 3);

        let before = ctx.blocking_region_counts();
        let r = source_read_buffer(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(bb))],
        );
        assert!(matches!(r, Ok(Some(Value::Int(3)))), "read 3 bytes");

        // Bytes must land at arr[6..9], NOT arr[0..3].
        let mut got = vec![0u8; 16];
        ctx.read_byte_array_into(arr, 0, &mut got);
        assert_eq!(
            &got[6..9],
            &payload[..],
            "pipe bytes must be written at offset + position"
        );
        assert_eq!(
            &got[0..3],
            &[0u8, 0, 0][..],
            "nothing may be written at the raw position"
        );
        // position is buffer-relative, so it advances 0 -> 3.
        assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(3));

        let after = ctx.blocking_region_counts();
        assert_eq!(
            (after.0 - before.0, after.1 - before.1),
            (1, 1),
            "the blocking kernel read must be bracketed exactly once"
        );
        close_pipe_end(read_id);
    }

    /// The sink write must read from `offset + position` too, and bracket the
    /// (potentially blocking) kernel write.
    #[test]
    fn audit_sink_write_reads_from_slice_offset_and_brackets_gc() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let write_id = register_pipe_end(write_end);

        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(4);
        ctx.set_field(this, PIPE_FIELD_ID, Value::Int(write_id));
        let (bb, arr) = heap_buffer(&mut ctx, 16, 6, 0, 3);
        // Put a decoy at the raw position and the real payload at the slice.
        ctx.write_byte_array_from(arr, 0, b"BAD");
        ctx.write_byte_array_from(arr, 6, b"OK!");

        let before = ctx.blocking_region_counts();
        let r = sink_write_buffer(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(bb))],
        );
        assert!(matches!(r, Ok(Some(Value::Int(3)))));
        let after = ctx.blocking_region_counts();
        assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));

        let mut got = [0u8; 3];
        let n = read_pipe(read_end.raw, &mut got).expect("read back");
        assert_eq!(n, 3);
        assert_eq!(
            &got[..],
            &b"OK!"[..],
            "the bytes on the wire must come from offset + position"
        );
        assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(3));
        close_pipe_end(write_id);
        close_raw(read_end.raw);
    }

    /// A `limit` that overshoots the backing array must clamp rather than
    /// read/write out of range.
    #[test]
    fn audit_buffer_view_clamps_limit_past_array_end() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let read_id = register_pipe_end(read_end);
        write_pipe(write_end.raw, b"abcdefgh").expect("write");
        close_raw(write_end.raw);

        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(4);
        ctx.set_field(this, PIPE_FIELD_ID, Value::Int(read_id));
        // 4-byte array, offset 2 -> only 2 bytes addressable, but limit says 8.
        let (bb, arr) = heap_buffer(&mut ctx, 4, 2, 0, 8);
        let r = source_read_buffer(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(bb))],
        );
        assert!(
            matches!(r, Ok(Some(Value::Int(n))) if n <= 2),
            "must not claim more bytes than the array can hold, got {r:?}"
        );
        let mut got = vec![0u8; 4];
        ctx.read_byte_array_into(arr, 0, &mut got);
        assert_eq!(&got[2..4], &b"ab"[..]);
        close_pipe_end(read_id);
    }

    #[test]
    fn wp37_eof_after_writer_close() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        // Close write end immediately — read should observe EOF.
        close_raw(write_end.raw);
        let mut buf = [0u8; 32];
        let r = read_pipe(read_end.raw, &mut buf).expect("read at EOF");
        assert_eq!(r, 0, "expected EOF");
        close_raw(read_end.raw);
    }

    // -----------------------------------------------------------------------
    // G46-1 (2026-08-17) — the private slot map, and the two real fields it
    // used to alias.
    //
    // MEASURED BEFORE, `target-rel3` (`9ae371468`), `--jdk-only`,
    // `CRATONVM_DBG_COERCION=1`, one `Pipe.open()`: six
    // `primitive-into-reference` events with `descriptor=L` at the three
    // `alloc_channel` lines, and `G46PipeProbe` died at the first write with
    // `java.io.IOException: SinkChannel.write: missing pipe id`.
    // -----------------------------------------------------------------------

    /// The real JDK 25 transitive instance layout of
    /// `sun.nio.ch.SourceChannelImpl` / `SinkChannelImpl`, from `javap -p`
    /// against Adoptium 25.0.3+9 — superclass first, statics excluded.
    ///
    /// This is the table the pre-G46 slot map collided with, written down so
    /// the collision is a property the test can state rather than a fact in a
    /// comment.
    const REAL_CHANNEL_LAYOUT: [(&str, &str); 11] = [
        ("closeLock", "Ljava/lang/Object;"),
        ("closed", "Z"),
        ("interruptor", "Lsun/nio/ch/Interruptible;"),
        ("interruptedTarget", "Ljava/lang/Object;"),
        ("provider", "Ljava/nio/channels/spi/SelectorProvider;"),
        ("keys", "[Ljava/nio/channels/SelectionKey;"),
        ("keyCount", "I"),
        ("keyLock", "Ljava/lang/Object;"),
        ("regLock", "Ljava/lang/Object;"),
        ("nonBlocking", "Z"),
        ("sc", "Ljava/nio/channels/SocketChannel;"),
    ];

    /// The arithmetic half: on the real class the private map starts above
    /// every declared field, so no private slot can land on a reference the
    /// class declares — which is the entire defect.
    ///
    /// RED against the pre-G46 tree, where the map was the absolute 0..3 this
    /// test's first assertion now forbids.
    #[test]
    fn the_private_slot_map_clears_every_declared_reference_slot() {
        let base = REAL_CHANNEL_LAYOUT.len();
        let private = [
            PIPE_FIELD_ID,
            PIPE_FIELD_OPEN,
            PIPE_FIELD_KIND,
            PIPE_FIELD_BLOCKING,
        ];
        assert_eq!(
            private.len(),
            PIPE_CHANNEL_PRIVATE_SLOTS,
            "the width must cover every private slot the allocator writes"
        );
        for p in private {
            let absolute = base + p;
            assert!(
                REAL_CHANNEL_LAYOUT.get(absolute).is_none(),
                "private slot {p} lands on {:?}, which the class declares",
                REAL_CHANNEL_LAYOUT.get(absolute)
            );
        }
        // And the three that the coercion actually destroyed are reference
        // slots — the reason a primitive there became null rather than merely
        // being in the wrong place.
        for slot in [PIPE_FIELD_ID, PIPE_FIELD_KIND, PIPE_FIELD_BLOCKING] {
            let (name, desc) = REAL_CHANNEL_LAYOUT[slot];
            assert!(
                desc.starts_with('L') || desc.starts_with('['),
                "{name} was expected to be a reference slot, got {desc}"
            );
        }
        // …and the fourth aliased `closed`, which is the same WIDTH but the
        // opposite MEANING: `isOpen()` is `!closed`.
        assert_eq!(REAL_CHANNEL_LAYOUT[PIPE_FIELD_OPEN], ("closed", "Z"));
    }

    /// A fresh channel must read back `closed = false` on the field the class
    /// declares, not `true`.
    ///
    /// RED against the pre-G46 tree: nothing wrote `closed` by name there, and
    /// the indexed open flag put `Int(1)` on that very slot — so a brand-new
    /// pipe channel told real `AbstractInterruptibleChannel.isOpen()` bytecode
    /// that it was already closed.
    #[test]
    fn a_fresh_channel_is_not_closed_on_the_field_the_class_declares() {
        let mut ctx = MockNativeContext::new();
        let obj = alloc_channel(&mut ctx, "sun/nio/ch/SourceChannelImpl", false, 41);
        assert_eq!(
            ctx.get_field_by_name(obj, "closed"),
            Value::Int(0),
            "a fresh channel is open, i.e. closed == false"
        );
        let base = channel_private_base(&mut ctx, obj);
        assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_ID), Value::Int(41));
        assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_OPEN), Value::Int(1));
        assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_KIND), Value::Int(0));
        assert_eq!(
            ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
            Value::Int(1)
        );
    }

    /// The other end of the same inversion, and the `ServerSocket.bound` shape
    /// this record is about: `close()` must set `closed = true`.
    ///
    /// RED against the pre-G46 tree, where `channel_close` wrote `Int(0)` at
    /// the slot the real class declares as `closed` — so closing the channel
    /// told the JDK it had just become OPEN.
    #[test]
    fn closing_a_channel_sets_the_real_closed_field_true() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let read_id = register_pipe_end(read_end);
        let mut ctx = MockNativeContext::new();
        let obj = alloc_channel(&mut ctx, "sun/nio/ch/SourceChannelImpl", false, read_id);
        assert_eq!(
            channel_is_open(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
            Some(Value::Int(1))
        );
        channel_close(&mut ctx, &[Value::Object(Some(obj))]).expect("close");
        assert_eq!(
            ctx.get_field_by_name(obj, "closed"),
            Value::Int(1),
            "close() must set closed = true, not false"
        );
        assert_eq!(
            channel_is_open(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
            Some(Value::Int(0)),
            "isOpen() must answer false after close()"
        );
        close_raw(write_end.raw);
    }

    /// `AbstractSelectableChannel.isBlocking()` is `return !nonBlocking`, so
    /// the real field is the NEGATION of `configureBlocking`'s argument.
    ///
    /// RED against the pre-G46 tree, which wrote the argument verbatim at
    /// absolute slot 3 — `interruptedTarget : Object` — where the coercion
    /// nulled it, and never touched `nonBlocking` at all.
    #[test]
    fn configure_blocking_records_the_negation_on_the_real_field() {
        let mut ctx = MockNativeContext::new();
        let obj = alloc_channel(&mut ctx, "sun/nio/ch/SinkChannelImpl", true, 5);
        channel_configure_blocking(&mut ctx, &[Value::Object(Some(obj)), Value::Int(0)])
            .expect("configureBlocking(false)");
        assert_eq!(ctx.get_field_by_name(obj, "nonBlocking"), Value::Int(1));
        let base = channel_private_base(&mut ctx, obj);
        assert_eq!(
            ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
            Value::Int(0)
        );

        channel_configure_blocking(&mut ctx, &[Value::Object(Some(obj)), Value::Int(1)])
            .expect("configureBlocking(true)");
        assert_eq!(ctx.get_field_by_name(obj, "nonBlocking"), Value::Int(0));
        assert_eq!(
            ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
            Value::Int(1)
        );
    }

    /// The width guard. A receiver this crate did not allocate — anything too
    /// narrow for `base + PIPE_CHANNEL_PRIVATE_SLOTS` — must fall back to the
    /// legacy base rather than index past the object.
    ///
    /// This is what keeps the fix from turning a foreign receiver into an
    /// out-of-range access, and it is also what reconciles the
    /// `AnonymousObject$4` substitution the allocator's failure arm produces.
    #[test]
    fn a_receiver_too_narrow_for_the_private_map_uses_the_legacy_base() {
        let mut ctx = MockNativeContext::new();
        let narrow = ctx.alloc_object_with_class(1, "sun/nio/ch/SourceChannelImpl");
        assert_eq!(channel_private_base(&mut ctx, narrow), 0);
        // And every accessor must still answer rather than panic or refuse in
        // a new way: no pipe id at slot 0 is the pre-existing IOException.
        let r = source_read_buffer(&mut ctx, &[Value::Object(Some(narrow))]);
        assert!(
            r.is_err(),
            "a receiver with no pipe id still raises IOException"
        );
    }

    /// Source tripwire: every private access must go through the resolved
    /// base. Scans only what is ABOVE this test module, because the needles
    /// appear verbatim in the assertions here.
    #[test]
    fn every_private_pipe_slot_access_is_base_relative() {
        let src = include_str!("pipe.rs")
            .split("mod tests {")
            .next()
            .expect("split always yields a first element");
        for needle in [
            "ctx.get_field(this, PIPE_FIELD_ID)",
            "ctx.set_field(obj, PIPE_FIELD_ID",
            "ctx.get_field(this, PIPE_FIELD_OPEN)",
            "ctx.set_field(this, PIPE_FIELD_OPEN",
        ] {
            assert!(
                !src.contains(needle),
                "`{needle}` indexes the private map from 0, which on the real \
                 sun.nio.ch.*ChannelImpl layout is closeLock/closed (G46-1)"
            );
        }
        assert!(
            src.contains("let base = channel_private_base(ctx, this);"),
            "the accessors must resolve the private base from the receiver"
        );
    }
}
