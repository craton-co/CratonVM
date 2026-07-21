// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.1 — real platform-backed `sun.nio.ch.Selector`, `SelectionKey`,
//! `SelectableChannel` natives.
//!
//! Provides the JDK NIO multiplexer primitives that XNIO / Undertow / Vert.x
//! / Netty all sit on top of. Multiplexes over real OS sockets handed in by
//! the caller at `register()` time, using a kernel wait primitive:
//!
//!   * **Linux**: raw `epoll_create1` / `epoll_ctl` / `epoll_wait` via `libc`.
//!     A self-pipe is registered with the epoll set so `wakeup()` can yank
//!     a blocked thread out of `epoll_wait`.
//!   * **Windows**: `WSAPoll` via raw `#[link(name = "Ws2_32")]` FFI (same
//!     pattern as `native-builtins/src/servlet.rs`). JDK's
//!     `WindowsSelectorImpl` uses the same kernel primitive (it predates
//!     IOCP support for synchronous channels). Wakeup is implemented via a
//!     UDP loopback socket pair so it can participate in `WSAPoll`'s
//!     socket-only fd_set. IOCP-backed `WindowsAsyncSelector` for
//!     `AsynchronousSocketChannel` is a follow-up WP and not required for
//!     `Selector` semantics.
//!   * **Other Unix (macOS / BSD)**: falls back to a `poll(2)` loop via
//!     `libc::poll` — same shape as `WSAPoll`, no kqueue gymnastics needed
//!     for the test surface; this WP target is Linux + Windows.
//!
//! ## Design
//!
//! A process-wide `SelectorState` map keyed by selector id holds the real
//! state (registered keys, wakeup socket pair / pipe, raw OS handles).
//! The Java-side `SelectorImpl` object carries only scaffolding fields:
//!
//!   field 0: id               (Int)            — lookup key into selectors()
//!   field 1: registered_map   (Object, unused) — Java-side Set<SelectionKey>
//!   field 2: selected_set     (Object, unused) — Java-side Set<SelectionKey>
//!   field 3: keys_set         (Object, unused) — Java-side Set<SelectionKey>
//!   field 4: open_flag        (Int)            — 1 = open, 0 = closed
//!
//! `SelectionKeyImpl` carries:
//!
//!   field 0: selector         (Object)  — parent Selector
//!   field 1: channel          (Object)  — the SelectableChannel
//!   field 2: interestOps      (Int)     — current interest bitmask
//!   field 3: readyOps         (Int)     — last-computed ready bitmask
//!   field 4: attachment       (Object)  — user attachment slot
//!
//! ## Readiness
//!
//! For sockets handed to the selector we extract the OS-level handle (raw
//! fd on Unix, `SOCKET` on Windows) and submit it to the kernel wait. The
//! interest bitmask is translated to platform events:
//!
//!   * `OP_READ`    -> `EPOLLIN`  / `POLLRDNORM`
//!   * `OP_ACCEPT`  -> `EPOLLIN`  / `POLLRDNORM`  (pending connection)
//!   * `OP_WRITE`   -> `EPOLLOUT` / `POLLWRNORM`
//!   * `OP_CONNECT` -> `EPOLLOUT` / `POLLWRNORM`
//!
//! When `OP_ACCEPT` fires we additionally drain one pending TCP connection
//! out of the listener and stash it in `pending_accepted` so a subsequent
//! `take_pending_accepted` call returns it without a second `accept()`.
//!
//! The selector does NOT own any socket lifecycle beyond its own wakeup
//! pair — caller-provided handles are stored as `try_clone()`d clones so
//! dropping the selector state drops only its clones, not the originals.
//! This avoids double-close issues.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
#[allow(unused_imports)]
use std::time::{Duration, Instant};
// C27 (Round-11 GC-safety fix): the sk_table side-table is now keyed by
// a SelectionKey's GC-stable identity hash code (i32). FxHashMap matches
// the rest of the crate's small-int side-tables and avoids SipHash on
// the hot select() / interest-op update paths.
use rustc_hash::FxHashMap;

// Gated selector tracing (CRATONVM_DBG_SELECTOR=1): logs the kernel-wait
// enter/exit and wakeup() with the carrier OS thread name (which carries the
// Java thread name, set at Thread.start). Used to diagnose the intermittent
// reactor worker leak at shutdown — a thread that logs ENTER but never EXIT is
// parked in the kernel wait; the timeout_c value tells us whether it's an
// infinite (-1) or timed wait, and whether a matching wakeup fired.
fn sel_dbg_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("CRATONVM_DBG_SELECTOR")
            .map(|v| {
                let t = v.trim();
                !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false)
    })
}

/// Opt-out for the Windows selector's active connect-completion probe (Phase 1b
/// in `kernel_select_windows`). Default OFF (probe ENABLED) — set
/// `CRATONVM_NO_SELECTOR_CONNECT_PROBE=1` to fall back to pure WSAPoll readiness
/// for A/B debugging.
fn connect_probe_disabled() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| {
        std::env::var("CRATONVM_NO_SELECTOR_CONNECT_PROBE")
            .map(|v| {
                let t = v.trim();
                !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false)
    })
}

/// Coarse cap (ms) applied to an otherwise-INDEFINITE `Selector.select()` so a
/// missed wakeup self-heals (see the call site). Default 50 ms — low enough
/// that reactor request/response tests do not burn their whole verifier budget
/// across a handful of missed wakeups, while still avoiding a tight idle spin.
/// Overridable via CRATONVM_SELECT_MAX_BLOCK_MS.
fn select_infinite_cap_ms() -> i32 {
    static CAP: OnceLock<i32> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("CRATONVM_SELECT_MAX_BLOCK_MS")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(50)
    })
}

fn sel_dbg(msg: impl AsRef<str>) {
    let tname = std::thread::current()
        .name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".to_string());
    eprintln!("[SEL] {} thread={}", msg.as_ref(), tname);
}

// ---------------------------------------------------------------------------
// OP_* constants (JDK SelectionKey.OP_*)
// ---------------------------------------------------------------------------

pub const OP_READ: i32 = 1;
pub const OP_WRITE: i32 = 4;
pub const OP_CONNECT: i32 = 8;
pub const OP_ACCEPT: i32 = 16;

/// Max time (ms) the Windows selector blocks in WSAPoll while a non-blocking
/// connect is still in progress, before returning to re-probe the original fd
/// for completion (see `kernel_select_windows` Phase 1b). Small enough that a
/// completed connect is surfaced near-instantly even if WSAPoll never reports
/// the cloned handle's POLLOUT edge; large enough to avoid a busy spin.
const CONNECT_REPOLL_MS: i32 = 50;

// ---------------------------------------------------------------------------
// Field indices
// ---------------------------------------------------------------------------

const SI_ID: usize = 0;
const SI_OPEN_FLAG: usize = 4;

const SK_SELECTOR: usize = 0;
const SK_CHANNEL: usize = 1;
const SK_INTEREST_OPS: usize = 2;
const SK_READY_OPS: usize = 3;
const SK_ATTACHMENT: usize = 4;

// ---------------------------------------------------------------------------
// Selectable handle (selector-owned clone of the real socket)
// ---------------------------------------------------------------------------

/// A cloned handle registered with a selector. The selector uses only
/// non-blocking probe operations on these clones. The originals stay with
/// the caller (e.g. `net.rs` `NetSocketHandle::Listener`) and close on
/// their own drop.
enum SelectableHandle {
    Listener(TcpListener),
    Stream(TcpStream),
    Udp(UdpSocket),
    /// Registered without a live handle — `kernel_select` skips it; useful
    /// for lifecycle tests that want a key in the map without a real socket.
    Dummy,
}

/// Public enum used by external callers to hand a live socket to the
/// selector. Shaped after `net.rs::NetSocketHandle` but decoupled to honor
/// the "don't touch net.rs" seal.
pub enum SelectableKind {
    Listener(TcpListener),
    Stream(TcpStream),
    Udp(UdpSocket),
}

impl SelectableHandle {
    /// Return the OS-level handle as a platform integer (RawFd on Unix,
    /// RawSocket on Windows). Returns `None` for `Dummy`.
    #[cfg(unix)]
    fn os_handle(&self) -> Option<i64> {
        use std::os::unix::io::AsRawFd;
        match self {
            SelectableHandle::Listener(l) => Some(l.as_raw_fd() as i64),
            SelectableHandle::Stream(s) => Some(s.as_raw_fd() as i64),
            SelectableHandle::Udp(s) => Some(s.as_raw_fd() as i64),
            SelectableHandle::Dummy => None,
        }
    }

    #[cfg(windows)]
    fn os_handle(&self) -> Option<i64> {
        use std::os::windows::io::AsRawSocket;
        match self {
            SelectableHandle::Listener(l) => Some(l.as_raw_socket() as i64),
            SelectableHandle::Stream(s) => Some(s.as_raw_socket() as i64),
            SelectableHandle::Udp(s) => Some(s.as_raw_socket() as i64),
            SelectableHandle::Dummy => None,
        }
    }

    /// Stub for any other platform — the kernel wait module will fall back
    /// to a probe loop, so OS handles aren't needed.
    #[cfg(not(any(unix, windows)))]
    fn os_handle(&self) -> Option<i64> {
        None
    }

    fn is_listener(&self) -> bool {
        matches!(self, SelectableHandle::Listener(_))
    }
}

// ---------------------------------------------------------------------------
// Registered-key state (native side)
// ---------------------------------------------------------------------------

struct KeyState {
    /// Net fd (caller-provided identifier; used for take_pending_accepted).
    /// Read on Windows + macOS poll paths; on Linux we use the HashMap
    /// key directly.
    #[allow(dead_code)]
    net_fd: i32,
    /// Current interest bitmask.
    interest_ops: i32,
    /// Last-computed ready bitmask.
    ready_ops: i32,
    /// Java-side SelectionKeyImpl. `None` for synthetic / test
    /// registrations that don't carry a real key.
    ///
    /// C27 (Round-11 GC-safety fix): previously this was a raw
    /// `usize = obj.as_ptr() as usize`, resurrected via
    /// `unsafe { ObjectRef::from_raw(raw as *mut u8) }` whenever
    /// `selector_selected_keys` / `selector_keys` needed the Java key
    /// back. Under a moving GC the raw pointer is stale after
    /// compaction; the previous code returned wrong-object or wild
    /// dereferences. Store the `ObjectRef` directly so a future
    /// `update_after_gc` hook can remap it; pair it with `key_hash`
    /// (GC-stable identity hash code) for cross-table identity
    /// comparisons that mustn't depend on the post-GC pointer.
    key_obj: Option<ObjectRef>,
    /// GC-stable identity hash code of the SelectionKey (0 for synthetic
    /// / test registrations that don't carry a real key). Used to look
    /// up the matching `SkState` row in `sk_table` and to compare key
    /// identity across selectors without dereferencing `key_obj`.
    key_hash: i32,
    /// True if cancel() has been called; pruned at the start of the next
    /// select cycle.
    cancelled: bool,
    /// Selector-owned clone of the underlying socket.
    handle: SelectableHandle,
}

struct SelectorState {
    open: bool,
    /// Map from net_fd -> key state.
    keys: HashMap<i32, KeyState>,
    /// UDP socket pair for wakeup (used on Windows; also used as fallback
    /// on platforms without epoll). `wakeup_peer` is the address of the
    /// receiver socket so the sender knows where to deliver.
    #[allow(dead_code)]
    wakeup_sender: Option<UdpSocket>,
    #[allow(dead_code)]
    wakeup_receiver: Option<UdpSocket>,
    #[allow(dead_code)]
    wakeup_peer: Option<SocketAddr>,

    /// Linux-only: epoll fd plus self-pipe pair for kernel-backed wait.
    #[cfg(target_os = "linux")]
    epoll_fd: Option<libc::c_int>,
    #[cfg(target_os = "linux")]
    wakeup_pipe_read: Option<libc::c_int>,
    #[cfg(target_os = "linux")]
    wakeup_pipe_write: Option<libc::c_int>,

    /// Buffered newly-accepted TCP streams (one per listener fd).
    pending_accepted: VecDeque<(i32 /*listener fd*/, TcpStream)>,
    /// Woken flag — set by `wakeup()`, cleared on next select entry.
    woken: bool,
    /// Kernel selects that have released this state lock and may still be
    /// blocked in epoll_wait. Close keeps its wakeup descriptors alive until
    /// the last such select has observed the close wakeup.
    in_flight_selects: usize,
}

impl SelectorState {
    fn new() -> Self {
        Self {
            open: true,
            keys: HashMap::new(),
            wakeup_sender: None,
            wakeup_receiver: None,
            wakeup_peer: None,
            #[cfg(target_os = "linux")]
            epoll_fd: None,
            #[cfg(target_os = "linux")]
            wakeup_pipe_read: None,
            #[cfg(target_os = "linux")]
            wakeup_pipe_write: None,
            pending_accepted: VecDeque::new(),
            woken: false,
            in_flight_selects: 0,
        }
    }

    /// Initialize the wakeup UDP socket pair. Called lazily on first
    /// blocking select on Windows / non-Linux fallback. UDP loopback is
    /// chosen because it works inside `WSAPoll`'s socket-only wait set.
    #[allow(dead_code)] // unused on Linux (epoll uses self-pipe instead)
    fn init_wakeup_udp(&mut self) -> std::io::Result<()> {
        if self.wakeup_receiver.is_some() {
            return Ok(());
        }
        let recv = UdpSocket::bind("127.0.0.1:0")?;
        recv.set_nonblocking(true)?;
        let recv_addr = recv.local_addr()?;
        let sender = UdpSocket::bind("127.0.0.1:0")?;
        sender.set_nonblocking(true)?;
        self.wakeup_sender = Some(sender);
        self.wakeup_receiver = Some(recv);
        self.wakeup_peer = Some(recv_addr);
        Ok(())
    }

    /// Linux-only: lazy-init epoll fd + self-pipe.
    #[cfg(target_os = "linux")]
    fn init_epoll(&mut self) -> std::io::Result<()> {
        if self.epoll_fd.is_some() {
            return Ok(());
        }
        // SAFETY: epoll_create1 is a syscall; rc < 0 indicates failure.
        let efd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if efd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut pipefd: [libc::c_int; 2] = [0; 2];
        // SAFETY: pipe2 fills pipefd with two valid fds (read, write).
        let rc = unsafe { libc::pipe2(pipefd.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: efd was a valid fd from epoll_create1.
            unsafe { libc::close(efd) };
            return Err(e);
        }
        // Register the read end of the pipe for EPOLLIN.
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            // Use u64::MAX as a sentinel "this is the wakeup pipe" key.
            u64: u64::MAX,
        };
        // SAFETY: efd, pipefd[0], &mut ev are all valid for this call.
        let rc = unsafe { libc::epoll_ctl(efd, libc::EPOLL_CTL_ADD, pipefd[0], &mut ev) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: all fds are valid.
            unsafe {
                libc::close(efd);
                libc::close(pipefd[0]);
                libc::close(pipefd[1]);
            }
            return Err(e);
        }
        self.epoll_fd = Some(efd);
        self.wakeup_pipe_read = Some(pipefd[0]);
        self.wakeup_pipe_write = Some(pipefd[1]);
        Ok(())
    }

    /// Drain wakeup datagrams (UDP path).
    #[allow(dead_code)] // unused on Linux (epoll uses self-pipe instead)
    fn drain_wakeup_udp(&mut self) {
        if let Some(r) = &self.wakeup_receiver {
            let mut buf = [0u8; 64];
            loop {
                match r.recv(&mut buf) {
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    /// Windows/non-Linux: interrupt an in-progress blocked `WSAPoll` so it
    /// re-evaluates the (just-changed) key set immediately, instead of
    /// waiting out its full timeout.
    ///
    /// Unlike Linux epoll — where adding an fd to a live epoll set is picked
    /// up by an already-blocked `epoll_wait` automatically, and `MOD`
    /// additionally gets an explicit self-pipe nudge in
    /// `selector_set_interest` — `WSAPoll` takes a fixed fd array as a
    /// direct call argument. A thread already blocked inside `WSAPoll` has
    /// no way to observe a NEW key (`selector_register`) or a changed
    /// `interest_ops` on an existing key (`selector_set_interest`) until
    /// that call naturally returns. `select_infinite_cap_ms()` only bounds
    /// truly INDEFINITE waits (Java `select()` with no timeout); an
    /// explicit, finite timeout (e.g. Jetty's `select(30000)` idle-poll) is
    /// honored as-is and is NOT capped — so a missed registration/interest
    /// change here does not self-heal within any bounded window, it stalls
    /// for the caller's full requested timeout. Found while investigating
    /// `JettyClientHttpConnectorBuilderTests`'s 100%-reproducible hang/crash
    /// (`docs/known-issues/springboot/http-client-connector-teardown-hang-crash.md`):
    /// a real, confirmed gap (a registration lost this exact way, verified
    /// via `CRATONVM_DBG_SELECTOR=1` tracing) — but NOT, on its own,
    /// sufficient to fix that specific hang; see the doc for the remaining
    /// open gap further down Jetty's connect/handshake call chain.
    ///
    /// Deliberately does NOT set the sticky public `woken` flag (matches
    /// `selector_set_interest`'s Linux self-pipe nudge): this is an internal
    /// "please re-check readiness now" prod, not a public
    /// `Selector.wakeup()` request that should make the *next* `select()`
    /// call return immediately too.
    #[cfg(not(target_os = "linux"))]
    fn nudge_blocked_poll(&mut self) {
        if sel_dbg_enabled() {
            sel_dbg("NUDGE".to_string());
        }
        if self.wakeup_sender.is_none() {
            let _ = self.init_wakeup_udp();
        }
        if let (Some(sender), Some(peer)) = (self.wakeup_sender.as_ref(), self.wakeup_peer) {
            let _ = sender.send_to(b"N", peer);
        }
    }

    /// Drain wakeup pipe (Linux self-pipe).
    #[cfg(target_os = "linux")]
    fn drain_wakeup_pipe(&mut self) {
        if let Some(rfd) = self.wakeup_pipe_read {
            let mut buf = [0u8; 64];
            // SAFETY: rfd is a valid non-blocking fd; read returns -1/EAGAIN
            // when empty.
            loop {
                let n =
                    unsafe { libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
            }
        }
    }
}

fn release_closed_selector_handles(st: &mut SelectorState) {
    debug_assert!(!st.open);
    if st.in_flight_selects != 0 {
        return;
    }
    st.wakeup_sender.take();
    st.wakeup_receiver.take();
    st.wakeup_peer.take();
    #[cfg(target_os = "linux")]
    {
        if let Some(efd) = st.epoll_fd.take() {
            // SAFETY: efd was a valid fd owned by this selector.
            unsafe { libc::close(efd) };
        }
        if let Some(rfd) = st.wakeup_pipe_read.take() {
            unsafe { libc::close(rfd) };
        }
        if let Some(wfd) = st.wakeup_pipe_write.take() {
            unsafe { libc::close(wfd) };
        }
    }
}

impl Drop for SelectorState {
    fn drop(&mut self) {
        // The registry intentionally retains closed selectors so concurrent
        // callers can finish safely. Drop is only reached at process teardown.
        self.open = false;
        self.in_flight_selects = 0;
        release_closed_selector_handles(self);
    }
}

/// Process-wide selector registry. Keyed by an integer id we allocate at
/// `Selector.open()`.
fn selectors() -> &'static RwLock<HashMap<i32, Mutex<SelectorState>>> {
    static REG: OnceLock<RwLock<HashMap<i32, Mutex<SelectorState>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_selector_id() -> i32 {
    // Start > 0 so "0" reliably signals "no selector".
    static NEXT: AtomicI32 = AtomicI32::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException {
        message: msg.into(),
    }
    .into()
}

fn illegal_arg(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: msg.into(),
    }
    .into()
}

fn closed_selector() -> MethodCallFailed {
    RuntimeError::IOException {
        message: "ClosedSelectorException".to_string(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Public API — lifecycle
// ---------------------------------------------------------------------------

/// Allocate a fresh native SelectorState + return its id. Exposed for unit
/// tests and for `sun.nio.ch.SelectorProvider.openSelector0()` callers.
pub fn selector_open() -> i32 {
    let id = next_selector_id();
    selectors()
        .write()
        .insert(id, Mutex::new(SelectorState::new()));
    id
}

/// Close a selector. Invalidates all registered keys, drops the wakeup
/// sockets / epoll fd. Idempotent.
///
/// We can't move the `Mutex<SelectorState>` out of the registry because
/// other threads may hold MutexGuards derived from `regs.get(&id)`'s
/// reference. Instead we mark the state closed in-place and explicitly
/// release every owned OS handle here. The Mutex stays in the HashMap.
pub fn selector_close(id: i32) {
    let regs = selectors().read();
    if let Some(s) = regs.get(&id) {
        let mut st = s.lock();
        if !st.open {
            return;
        }
        st.open = false;
        // An in-flight epoll_wait must be woken before its self-pipe and
        // epoll fd can be released. In particular, closing an epoll fd from a
        // different thread is not a portable wakeup primitive. Keep those
        // descriptors alive until the final in-flight select returns.
        st.woken = true;
        #[cfg(target_os = "linux")]
        if let Some(wfd) = st.wakeup_pipe_write {
            let byte: u8 = b'W';
            // SAFETY: wfd remains owned by this selector until the last
            // in-flight select calls release_closed_selector_handles().
            let _ = unsafe { libc::write(wfd, &byte as *const u8 as *const libc::c_void, 1) };
        }
        #[cfg(not(target_os = "linux"))]
        if let (Some(sender), Some(peer)) = (st.wakeup_sender.as_ref(), st.wakeup_peer) {
            let _ = sender.send_to(b"W", peer);
        }
        st.keys.clear();
        st.pending_accepted.clear();
        release_closed_selector_handles(&mut st);
    }
}

/// Wakeup a concurrently-blocked select.
pub fn selector_wakeup(id: i32) -> Result<(), MethodCallFailed> {
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        return Err(closed_selector());
    }
    st.woken = true;
    if sel_dbg_enabled() {
        sel_dbg(format!("WAKEUP id={id}"));
    }

    // Linux: write a byte to the self-pipe to wake epoll_wait.
    #[cfg(target_os = "linux")]
    {
        if st.wakeup_pipe_write.is_none() {
            // Lazy-init so a wakeup() before any select() still arms the
            // pipe for the next blocking call.
            let _ = st.init_epoll();
        }
        if let Some(wfd) = st.wakeup_pipe_write {
            let byte: u8 = b'W';
            // SAFETY: wfd is a valid non-blocking fd; partial writes ok.
            let _ = unsafe { libc::write(wfd, &byte as *const u8 as *const libc::c_void, 1) };
        }
    }

    // Windows / non-Linux: send a UDP datagram to the wakeup receiver.
    #[cfg(not(target_os = "linux"))]
    {
        if st.wakeup_sender.is_none() {
            let _ = st.init_wakeup_udp();
        }
        if let (Some(sender), Some(peer)) = (st.wakeup_sender.as_ref(), st.wakeup_peer) {
            // Best-effort single-byte datagram. If it fails the `woken`
            // flag still gets observed at top of select().
            let _ = sender.send_to(b"W", peer);
        }
    }

    Ok(())
}

/// Register a selectable handle with a selector. Idempotent on re-register
/// of the same net_fd (updates interestOps + replaces the handle clone).
///
/// `key_obj` is the SelectionKey ObjectRef the selector should hand back
/// from `selectedKeys()` / `keys()`; pass `None` for synthetic
/// registrations that don't have a Java-side key. `key_hash` is the
/// SelectionKey's GC-stable identity hash code (or 0 for synthetic);
/// it's used to cross-match against `sk_table` rows in cancel /
/// interest-op updates without dereferencing `key_obj` (which may have
/// been relocated by the GC between registration and the next lookup).
pub fn selector_register(
    id: i32,
    net_fd: i32,
    interest_ops: i32,
    key_obj: Option<ObjectRef>,
    key_hash: i32,
    kind: Option<SelectableKind>,
) -> Result<(), MethodCallFailed> {
    let handle = match kind {
        Some(SelectableKind::Listener(l)) => {
            // Try to set non-blocking up front. Failure is non-fatal —
            // accept() during ready-detection just won't return WouldBlock.
            let _ = l.set_nonblocking(true);
            SelectableHandle::Listener(l)
        }
        Some(SelectableKind::Stream(s)) => {
            let _ = s.set_nonblocking(true);
            SelectableHandle::Stream(s)
        }
        Some(SelectableKind::Udp(s)) => {
            let _ = s.set_nonblocking(true);
            SelectableHandle::Udp(s)
        }
        None => SelectableHandle::Dummy,
    };
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        return Err(closed_selector());
    }
    if sel_dbg_enabled() {
        sel_dbg(format!(
            "REGISTER id={id} net_fd={net_fd} interest_ops={interest_ops}"
        ));
    }
    let _prev = st.keys.insert(
        net_fd,
        KeyState {
            net_fd,
            interest_ops,
            ready_ops: 0,
            key_obj,
            key_hash,
            cancelled: false,
            handle,
        },
    );

    // Linux: keep the epoll set in sync with the live registration.
    #[cfg(target_os = "linux")]
    {
        let _ = st.init_epoll();
        if let Some(efd) = st.epoll_fd {
            let new_handle = &st.keys.get(&net_fd).unwrap().handle;
            if let Some(os) = new_handle.os_handle() {
                let mut ev = libc::epoll_event {
                    events: linux_events_for(interest_ops, new_handle.is_listener()) as u32,
                    u64: net_fd as u64,
                };
                // If a previous registration existed, MOD; otherwise ADD.
                let op = if _prev.is_some() {
                    libc::EPOLL_CTL_MOD
                } else {
                    libc::EPOLL_CTL_ADD
                };
                // SAFETY: efd, os, &mut ev all valid for this call. ENOENT
                // on a stale MOD just means the prev handle was already
                // dropped — fall back to ADD.
                let rc = unsafe { libc::epoll_ctl(efd, op, os as libc::c_int, &mut ev) };
                if rc < 0 && op == libc::EPOLL_CTL_MOD {
                    let _ = unsafe {
                        libc::epoll_ctl(efd, libc::EPOLL_CTL_ADD, os as libc::c_int, &mut ev)
                    };
                }
            }
        }
    }
    // Windows/non-Linux: unlike epoll (where an already-blocked epoll_wait
    // observes a live ADD to its own epoll set automatically), WSAPoll's fd
    // array is a fixed call argument — a thread already parked in WSAPoll
    // cannot see this new key until its full timeout elapses. Nudge it so it
    // re-polls with the now-current key set on the very next loop iteration.
    // See `SelectorState::nudge_blocked_poll`'s doc comment for the full
    // story.
    #[cfg(not(target_os = "linux"))]
    {
        st.nudge_blocked_poll();
    }
    Ok(())
}

/// Update interestOps on an already-registered key.
pub fn selector_set_interest(id: i32, net_fd: i32, ops: i32) -> Result<(), MethodCallFailed> {
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        return Err(closed_selector());
    }
    if sel_dbg_enabled() {
        sel_dbg(format!("SET_INTEREST id={id} net_fd={net_fd} ops={ops}"));
    }
    // Snapshot the bits we need from `k` so we can drop the mutable borrow
    // before touching `st.epoll_fd` again.
    #[cfg(target_os = "linux")]
    let (is_listener, os_h) = {
        let Some(k) = st.keys.get_mut(&net_fd) else {
            return Err(ioex("CancelledKeyException: key not registered"));
        };
        k.interest_ops = ops;
        (k.handle.is_listener(), k.handle.os_handle())
    };
    #[cfg(not(target_os = "linux"))]
    {
        let Some(k) = st.keys.get_mut(&net_fd) else {
            return Err(ioex("CancelledKeyException: key not registered"));
        };
        k.interest_ops = ops;
    }

    #[cfg(target_os = "linux")]
    {
        if let (Some(efd), Some(os)) = (st.epoll_fd, os_h) {
            let mut ev = libc::epoll_event {
                events: linux_events_for(ops, is_listener) as u32,
                u64: net_fd as u64,
            };
            // SAFETY: efd, os, &mut ev valid.
            let _ =
                unsafe { libc::epoll_ctl(efd, libc::EPOLL_CTL_MOD, os as libc::c_int, &mut ev) };
        }
        // epoll_ctl(MOD) does not reliably interrupt an already-blocked
        // epoll_wait. In particular, Tomcat arms OP_WRITE after a partial
        // gathering write; without a nudge, the last HTTP/2 frame can remain
        // queued until shutdown and the peer sees a truncated GOAWAY frame.
        // Do not set the sticky `woken` bit: this is a readiness re-check, not
        // a public Selector.wakeup() request.
        if let Some(wfd) = st.wakeup_pipe_write {
            let byte: u8 = b'I';
            let _ = unsafe {
                libc::write(wfd, &byte as *const u8 as *const libc::c_void, 1)
            };
        }
    }
    // Windows/non-Linux equivalent of the epoll self-pipe nudge above: a
    // thread already blocked in WSAPoll cannot observe this interest_ops
    // change until its full timeout elapses otherwise. See
    // `SelectorState::nudge_blocked_poll`'s doc comment for the full story.
    #[cfg(not(target_os = "linux"))]
    {
        st.nudge_blocked_poll();
    }
    Ok(())
}

/// Mark a key cancelled. It is removed on the next `select()`.
pub fn selector_cancel(id: i32, net_fd: i32) {
    let regs = selectors().read();
    if let Some(s) = regs.get(&id) {
        let mut st = s.lock();
        if let Some(k) = st.keys.get_mut(&net_fd) {
            k.cancelled = true;
        }
    }
}

/// Drop every selector's registration for `net_fd`, releasing the cloned socket
/// handle each holds. Called when a channel is closed.
///
/// The selector stores caller-provided handles as `try_clone()`d duplicates
/// (see `selector_register`). On Windows a duplicated socket keeps the
/// underlying OS socket alive until *every* duplicate is closed — so dropping
/// only the channel's original handle on close would NOT shut the connection,
/// and the peer's blocking read never sees EOF (it hangs forever). The real JDK
/// avoids this because `AbstractSelectableChannel.implCloseChannel()` cancels
/// and removes the channel's keys as part of close; CratonVM's synthetic
/// channel close must do the same. Removing the `KeyState` drops its cloned
/// `TcpStream`/`TcpListener`, so once the original is also dropped the kernel
/// fully closes the socket and emits FIN.
///
/// Takes only the selector locks (never `tcp_registry`), so it composes with the
/// select path's `selectors → tcp_registry` lock order without inversion.
pub fn deregister_fd_everywhere(net_fd: i32) {
    let regs = selectors().read();
    for (_sel_id, sel) in regs.iter() {
        let mut st = sel.lock();
        st.keys.remove(&net_fd);
    }
}

/// Translate JDK interestOps to Linux epoll event mask.
#[cfg(target_os = "linux")]
fn linux_events_for(interest: i32, _is_listener: bool) -> i32 {
    let mut e: i32 = 0;
    if interest & (OP_READ | OP_ACCEPT) != 0 {
        e |= libc::EPOLLIN;
    }
    if interest & (OP_WRITE | OP_CONNECT) != 0 {
        e |= libc::EPOLLOUT;
    }
    // EPOLLRDHUP is delivered as part of EPOLLIN by JDK. Add it so peer
    // shutdowns wake the selector promptly.
    e |= libc::EPOLLRDHUP;
    e
}

/// Translate Linux epoll event mask back to JDK readyOps, masked by the
/// caller-supplied interest set.
#[cfg(target_os = "linux")]
fn linux_ready_for(events: i32, interest: i32, is_listener: bool) -> i32 {
    let mut r = 0;
    let in_ready = events & (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP) != 0;
    let out_ready = events & libc::EPOLLOUT != 0;
    if is_listener {
        if in_ready && (interest & OP_ACCEPT != 0) {
            r |= OP_ACCEPT;
        }
    } else {
        if in_ready && (interest & OP_READ != 0) {
            r |= OP_READ;
        }
        if out_ready && (interest & OP_WRITE != 0) {
            r |= OP_WRITE;
        }
        if out_ready && (interest & OP_CONNECT != 0) {
            r |= OP_CONNECT;
        }
    }
    // EPOLLERR: JDK signals it as readable+writable subject to interest.
    if events & libc::EPOLLERR != 0 {
        if interest & OP_READ != 0 {
            r |= OP_READ;
        }
        if interest & OP_WRITE != 0 {
            r |= OP_WRITE;
        }
        if !is_listener && interest & OP_CONNECT != 0 {
            r |= OP_CONNECT;
        }
    }
    r
}

#[cfg(target_os = "linux")]
fn finish_in_flight_linux_select_locked(st: &mut SelectorState) {
    debug_assert!(st.in_flight_selects > 0);
    st.in_flight_selects -= 1;
    if !st.open {
        release_closed_selector_handles(st);
    }
}

#[cfg(target_os = "linux")]
fn finish_in_flight_linux_select(id: i32) {
    let regs = selectors().read();
    if let Some(s) = regs.get(&id) {
        let mut st = s.lock();
        finish_in_flight_linux_select_locked(&mut st);
    }
}

// ---------------------------------------------------------------------------
// Kernel-backed select — Linux (epoll)
// ---------------------------------------------------------------------------

/// Compute readyOps for every registered key by waiting on the epoll fd.
/// Returns the count of keys with non-zero readyOps (after applying the
/// interest mask). Drains the wakeup pipe before returning.
#[cfg(target_os = "linux")]
fn kernel_select_linux(id: i32, timeout_ms: i32) -> Result<i32, MethodCallFailed> {
    // Phase 1: snapshot prerequisites under the lock — fd and connect
    // candidates. Interest bits intentionally remain live for phase 3. Then
    // release the lock so wakeup() can hit it during the actual epoll_wait.
    let (efd, connect_candidates) = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return Err(closed_selector());
        };
        let mut st = s.lock();
        if !st.open {
            return Err(closed_selector());
        }
        // Drop cancelled keys + remove their epoll registrations up front.
        let cancelled: Vec<i32> = st
            .keys
            .iter()
            .filter(|(_, v)| v.cancelled)
            .map(|(k, _)| *k)
            .collect();
        for net_fd in cancelled {
            if let Some(k) = st.keys.remove(&net_fd) {
                if let (Some(efd), Some(os)) = (st.epoll_fd, k.handle.os_handle()) {
                    // SAFETY: efd valid; os may already be closed by caller
                    // (in which case kernel returns ENOENT — non-fatal).
                    unsafe {
                        libc::epoll_ctl(
                            efd,
                            libc::EPOLL_CTL_DEL,
                            os as libc::c_int,
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
        }
        let _ = st.init_epoll();
        // Sticky wakeup (see the Windows path / selector_wakeup contract): a
        // wakeup() that landed before this epoll_wait — or whose self-pipe write
        // was lost — must still return immediately so a missed reactor-shutdown
        // wakeup can't leave the worker blocked in epoll forever.
        if st.woken {
            st.woken = false;
            st.drain_wakeup_pipe();
            return Ok(0);
        }
        let efd = match st.epoll_fd {
            Some(v) => v,
            None => return Err(ioex("Selector.select: epoll init failed")),
        };
        // net_fds of keys with OP_CONNECT interest backed by a non-blocking
        // connect. epoll usually reports connect-completion as EPOLLOUT, but
        // the selector polls a cloned fd while finishConnect() operates on the
        // original tcp_registry fd. Probe the original too so an already-finished
        // loopback connect cannot be missed and strand Netty's event loop.
        let mut connect_candidates: Vec<i32> = Vec::new();
        for (net_fd, k) in st.keys.iter() {
            if !k.handle.is_listener() && k.interest_ops & OP_CONNECT != 0 && k.net_fd > 0 {
                connect_candidates.push(k.net_fd);
            }
        }
        // From this point until phase 3, selector_close() must retain the
        // wakeup handles because epoll_wait may be asleep without this lock.
        st.in_flight_selects += 1;
        (efd, connect_candidates)
    };

    // Phase 1b: actively probe each OP_CONNECT candidate's original socket for
    // connect completion. This mirrors the Windows selector path: relying only
    // on the cloned fd's writable edge can miss a connect that completed before
    // registration or before this epoll_wait arms, which leaves reactors parked
    // forever waiting for finishConnect() to run. A completed or failed connect
    // is delivered as OP_CONNECT-ready this cycle; a still-pending connect caps
    // the blocking wait so we re-probe promptly.
    let mut timeout_ms = timeout_ms;
    let mut conn_ready: Vec<i32> = Vec::new();
    if !connect_probe_disabled() {
        let mut had_pending = false;
        let mut not_connecting = 0usize;
        let ncand = connect_candidates.len();
        for net_fd in connect_candidates {
            match crate::socket_channel::probe_connect_status(net_fd) {
                crate::socket_channel::SelectorConnectProbe::Ready => conn_ready.push(net_fd),
                crate::socket_channel::SelectorConnectProbe::Pending => had_pending = true,
                crate::socket_channel::SelectorConnectProbe::NotConnecting => not_connecting += 1,
            }
        }
        if sel_dbg_enabled() && ncand > 0 {
            sel_dbg(format!(
                "linux connect-probe cand={ncand} ready={} pending={had_pending} notconn={not_connecting}",
                conn_ready.len()
            ));
        }
        if !conn_ready.is_empty() {
            timeout_ms = 0;
        } else if had_pending && (timeout_ms < 0 || timeout_ms > CONNECT_REPOLL_MS) {
            timeout_ms = CONNECT_REPOLL_MS;
        }
    }

    // Phase 2: epoll_wait without any selector lock held.
    let mut events: [libc::epoll_event; 64] = unsafe { std::mem::zeroed() };
    // SAFETY: efd is valid; events buffer is sized correctly.
    let n = unsafe {
        libc::epoll_wait(
            efd,
            events.as_mut_ptr(),
            events.len() as libc::c_int,
            timeout_ms,
        )
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted || err.raw_os_error() == Some(libc::EINTR) {
            finish_in_flight_linux_select(id);
            return Ok(0);
        }
        // A close can race the wait. It is a wakeup, not an I/O error.
        if selectors()
            .read()
            .get(&id)
            .map(|s| !s.lock().open)
            .unwrap_or(true)
        {
            finish_in_flight_linux_select(id);
            return Ok(0);
        }
        finish_in_flight_linux_select(id);
        return Err(ioex(format!("epoll_wait: {err}")));
    }

    // Phase 3: re-acquire lock, apply ready bits, drain wakeup pipe,
    // accept any pending listener connections.
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        // The close raced a select already executing in epoll_wait. The JDK
        // wakes that existing operation; it must not surface as a teardown
        // failure. A later select still fails in the entry check above.
        finish_in_flight_linux_select_locked(&mut st);
        return Ok(0);
    }

    // Reset all readyOps before re-application.
    for k in st.keys.values_mut() {
        k.ready_ops = 0;
    }

    let mut count = 0;
    let mut woken = false;
    let mut accepted_streams: Vec<(i32, TcpStream)> = Vec::new();
    for ev in &events[..n as usize] {
        if ev.u64 == u64::MAX {
            woken = true;
            continue;
        }
        let net_fd = ev.u64 as i32;
        // `interest_ops` can change while epoll_wait is blocked (Tomcat arms
        // OP_WRITE after a partial gathering write). Applying this event with
        // the phase-1 snapshot can report a stale OP_READ bit, which makes the
        // phase-3 safety probe skip this key and strands the newly armed write.
        // Re-read the live key under this lock so the readiness mask and the
        // later probe observe the same selector state.
        let Some(k) = st.keys.get_mut(&net_fd) else {
            continue; // Race: key was removed/cancelled.
        };
        let is_listener = k.handle.is_listener();
        let ready = linux_ready_for(ev.events as i32, k.interest_ops, is_listener);
        if ready != 0 {
            k.ready_ops = ready;
            count += 1;
            if is_listener && ready & OP_ACCEPT != 0 {
                if let SelectableHandle::Listener(listener) = &k.handle {
                    match listener.accept() {
                        Ok((stream, _)) => accepted_streams.push((net_fd, stream)),
                        Err(_) => {}
                    }
                }
            }
        }
    }
    for (fd, stream) in accepted_streams {
        st.pending_accepted.push_back((fd, stream));
    }

    // Deliver OP_CONNECT readiness detected by the original-fd probe. This is
    // idempotent with epoll-derived readiness: only count the key once.
    for net_fd in conn_ready {
        if let Some(k) = st.keys.get_mut(&net_fd) {
            if k.interest_ops & OP_CONNECT != 0 {
                let was_zero = k.ready_ops == 0;
                k.ready_ops |= OP_CONNECT;
                if was_zero {
                    count += 1;
                }
            }
        }
    }

    // Safety net for readiness edges missed around register/interest changes.
    // Netty can register/bind/connect and immediately enqueue work on another
    // event-loop thread; if the epoll edge lands before the fd is fully armed,
    // the selector may otherwise sleep until a later wakeup and reactor tests
    // observe request timeouts. The nonblocking probe is the same conservative
    // readiness check used by the generic selector fallback, applied only to
    // interest bits that epoll/connect-probe have not already marked ready this cycle.
    // A read event that raced an OP_WRITE arm must not suppress the write probe.
    let mut probed_accepts: Vec<(i32, TcpStream)> = Vec::new();
    for (fd, k) in st.keys.iter_mut() {
        if k.cancelled || k.interest_ops == 0 {
            continue;
        }
        let missing_interest = k.interest_ops & !k.ready_ops;
        if missing_interest == 0 {
            continue;
        }
        let (ready, accepted) = probe_handle(&k.handle, missing_interest);
        if ready != 0 {
            let was_zero = k.ready_ops == 0;
            k.ready_ops |= ready;
            if was_zero {
                count += 1;
            }
        }
        if let Some(stream) = accepted {
            probed_accepts.push((*fd, stream));
        }
    }
    for (fd, stream) in probed_accepts {
        st.pending_accepted.push_back((fd, stream));
    }

    if woken || st.woken {
        st.woken = false;
        st.drain_wakeup_pipe();
    }
    finish_in_flight_linux_select_locked(&mut st);
    Ok(count)
}

// ---------------------------------------------------------------------------
// Kernel-backed select — Windows (WSAPoll, raw FFI)
//
// Matches the existing pattern in `native-builtins/src/servlet.rs`: declare
// `WSAPoll` directly via `#[link(name = "Ws2_32")]` so we don't pull in the
// `windows-sys` crate just for one symbol. WSAPOLLFD is a `(SOCKET, i16, i16)`
// triple where `SOCKET = UINT_PTR = usize`.
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[repr(C)]
struct Wsapollfd {
    fd: usize, // SOCKET (UINT_PTR), usize on both 32/64-bit Win
    events: i16,
    revents: i16,
}

#[cfg(windows)]
const WSAPOLLRDNORM: i16 = 0x0100;
#[cfg(windows)]
const WSAPOLLWRNORM: i16 = 0x0010;
#[cfg(windows)]
const WSAPOLLERR: i16 = 0x0001;
#[cfg(windows)]
const WSAPOLLHUP: i16 = 0x0002;

#[cfg(windows)]
#[link(name = "Ws2_32")]
extern "system" {
    fn WSAPoll(fd_array: *mut Wsapollfd, fds: u32, timeout: i32) -> i32;
}

#[cfg(windows)]
fn kernel_select_windows(id: i32, timeout_ms: i32) -> Result<i32, MethodCallFailed> {
    // Phase 1: snapshot under lock — pollfd entries plus the wakeup
    // receiver socket + interest/listener metadata, then drop the lock.
    let (mut pollfds, key_index, wakeup_idx, connect_candidates) = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return Err(closed_selector());
        };
        let mut st = s.lock();
        if !st.open {
            return Err(closed_selector());
        }
        // Prune cancelled keys.
        st.keys.retain(|_, v| !v.cancelled);

        // Lazy-init wakeup pair (UDP loopback).
        let _ = st.init_wakeup_udp();

        // Sticky wakeup: a wakeup() that arrived BEFORE we entered the kernel
        // wait (or whose UDP nudge was dropped) must still make this select()
        // return immediately — the contract documented on selector_wakeup()
        // ("the woken flag still gets observed at top of select()"). Without
        // this, a reactor's shutdown wakeup() could be missed, leaving the I/O
        // worker blocked in WSAPoll forever: an uninterruptible RUNNABLE thread
        // leaked at client teardown (ES RestClient ThreadLeakError).
        if st.woken {
            st.woken = false;
            st.drain_wakeup_udp();
            return Ok(0);
        }

        let mut pollfds: Vec<Wsapollfd> = Vec::with_capacity(st.keys.len() + 1);
        let mut key_index: Vec<(i32, i32, bool)> = Vec::with_capacity(st.keys.len());
        // net_fds of keys with OP_CONNECT interest backed by a non-blocking
        // connect — probed for completion after the lock is dropped (see below).
        let mut connect_candidates: Vec<i32> = Vec::new();
        for k in st.keys.values_mut() {
            // Reset readyOps prior to wait.
            k.ready_ops = 0;
            // Collect OP_CONNECT candidates for the original-fd completion probe
            // BEFORE the os_handle() gate: the selector's cloned handle may be
            // absent/Dummy (clone failed at registration), but the connecting
            // socket still lives in `tcp_registry` under net_fd and must be
            // probed there. (This is exactly the case that parked the selector.)
            if !k.handle.is_listener() && k.interest_ops & OP_CONNECT != 0 && k.net_fd > 0 {
                connect_candidates.push(k.net_fd);
            }
            let Some(os) = k.handle.os_handle() else {
                continue;
            };
            let interest = k.interest_ops;
            let is_listener = k.handle.is_listener();
            let mut events: i16 = 0;
            if interest & (OP_READ | OP_ACCEPT) != 0 {
                events |= WSAPOLLRDNORM;
            }
            if interest & (OP_WRITE | OP_CONNECT) != 0 {
                events |= WSAPOLLWRNORM;
            }
            if events == 0 {
                continue;
            }
            pollfds.push(Wsapollfd {
                fd: os as usize,
                events,
                revents: 0,
            });
            key_index.push((k.net_fd, interest, is_listener));
        }

        // Append the wakeup receiver as the last entry so we can detect
        // wakeup() and bail out.
        let wakeup_idx = if let Some(recv) = &st.wakeup_receiver {
            use std::os::windows::io::AsRawSocket;
            let s = recv.as_raw_socket() as usize;
            pollfds.push(Wsapollfd {
                fd: s,
                events: WSAPOLLRDNORM,
                revents: 0,
            });
            Some(pollfds.len() - 1)
        } else {
            None
        };

        (pollfds, key_index, wakeup_idx, connect_candidates)
    };

    // Phase 1b: actively probe each OP_CONNECT candidate's **original** socket
    // for connect completion (off the live `tcp_registry` fd, not the selector's
    // cloned handle). On Windows, `WSAPoll(POLLOUT)` of the *cloned* connecting
    // socket can miss the connect-completion edge entirely when the connect was
    // already in-progress at registration time — the selector then blocks
    // forever and `finishConnect()` never runs (e.g. Jetty's HttpClient against
    // a loopback server). Probing the original fd is deterministic. Any
    // candidate that has completed (or failed) is delivered as OP_CONNECT-ready
    // this cycle, and we drop the kernel wait to a non-blocking poll so we
    // return immediately instead of parking. Opt out via
    // CRATONVM_NO_SELECTOR_CONNECT_PROBE for A/B debugging.
    let mut timeout_ms = timeout_ms;
    let mut conn_ready: Vec<i32> = Vec::new();
    if !connect_probe_disabled() {
        let mut had_pending = false;
        let mut not_connecting = 0usize;
        let ncand = connect_candidates.len();
        for net_fd in connect_candidates {
            match crate::socket_channel::probe_connect_status(net_fd) {
                crate::socket_channel::SelectorConnectProbe::Ready => conn_ready.push(net_fd),
                crate::socket_channel::SelectorConnectProbe::Pending => had_pending = true,
                crate::socket_channel::SelectorConnectProbe::NotConnecting => not_connecting += 1,
            }
        }
        if sel_dbg_enabled() && ncand > 0 {
            sel_dbg(format!(
                "connect-probe cand={ncand} ready={} pending={had_pending} notconn={not_connecting}",
                conn_ready.len()
            ));
        }
        if !conn_ready.is_empty() {
            // Don't block: we have OP_CONNECT readiness to deliver now.
            timeout_ms = 0;
        } else if had_pending && (timeout_ms < 0 || timeout_ms > CONNECT_REPOLL_MS) {
            // A still-connecting candidate is in the wait set. WSAPoll(POLLOUT)
            // of its cloned handle may never fire for the connect-completion
            // edge, so bound the block and re-probe the original fd promptly
            // rather than parking for the full (possibly 30 s) select timeout.
            timeout_ms = CONNECT_REPOLL_MS;
        }
    }

    // Phase 2: WSAPoll without selector lock.
    let n = if pollfds.is_empty() {
        // Nothing to wait on. Sleep for the timeout to honor select(t)
        // semantics, breaking out if wakeup() fires.
        if timeout_ms > 0 {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            while Instant::now() < deadline {
                {
                    let regs = selectors().read();
                    if let Some(s) = regs.get(&id) {
                        let mut st = s.lock();
                        if !st.open {
                            return Ok(0);
                        }
                        if st.woken {
                            st.woken = false;
                            st.drain_wakeup_udp();
                            return Ok(0);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        0i32
    } else {
        // SAFETY: pollfds is a valid slice, length fits in u32.
        unsafe { WSAPoll(pollfds.as_mut_ptr(), pollfds.len() as u32, timeout_ms) }
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        return Err(ioex(format!("WSAPoll: {err}")));
    }

    // Phase 3: re-acquire lock, translate revents -> readyOps.
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        // A close may race a select that had already entered WSAPoll. The
        // JDK wakes that in-flight select and lets it return; only a select
        // that begins after closure throws ClosedSelectorException. Propagating
        // an exception here leaks the teardown race into Tomcat's Poller.
        return Ok(0);
    }

    let mut count = 0;
    let mut woken = false;
    let mut accepted_streams: Vec<(i32, TcpStream)> = Vec::new();
    for (i, pfd) in pollfds.iter().enumerate() {
        if pfd.revents == 0 {
            continue;
        }
        if Some(i) == wakeup_idx {
            woken = true;
            continue;
        }
        let Some((net_fd, interest, is_listener)) = key_index.get(i).copied() else {
            continue;
        };
        let revents = pfd.revents;
        let err_ready = revents & (WSAPOLLHUP | WSAPOLLERR) != 0;
        let in_ready = revents & WSAPOLLRDNORM != 0 || err_ready;
        let out_ready = revents & WSAPOLLWRNORM != 0;
        let mut ready = 0;
        if is_listener {
            if in_ready && interest & OP_ACCEPT != 0 {
                ready |= OP_ACCEPT;
            }
        } else {
            if in_ready && interest & OP_READ != 0 {
                ready |= OP_READ;
            }
            if out_ready && interest & OP_WRITE != 0 {
                ready |= OP_WRITE;
            }
            // A non-blocking connect that FAILED signals via WSAPOLLERR /
            // WSAPOLLHUP (the OS exceptfds set), not WSAPOLLWRNORM. Surface
            // OP_CONNECT in that case too so the reactor calls finishConnect(),
            // which reads SO_ERROR and reports the failure (onFailure / a
            // ConnectException) instead of waiting for a readiness that the
            // OS will never deliver on the writable set.
            if (out_ready || err_ready) && interest & OP_CONNECT != 0 {
                ready |= OP_CONNECT;
            }
        }
        if ready != 0 {
            if let Some(k) = st.keys.get_mut(&net_fd) {
                k.ready_ops = ready;
                count += 1;
                if is_listener && ready & OP_ACCEPT != 0 {
                    if let SelectableHandle::Listener(listener) = &k.handle {
                        if let Ok((stream, _)) = listener.accept() {
                            accepted_streams.push((net_fd, stream));
                        }
                    }
                }
            }
        }
    }
    for (fd, stream) in accepted_streams {
        st.pending_accepted.push_back((fd, stream));
    }

    // Deliver OP_CONNECT readiness detected by the original-fd probe (Phase 1b).
    // Idempotent w.r.t. the WSAPoll-derived readiness above: only flips a key's
    // ready_ops and counts it once.
    for net_fd in conn_ready {
        if let Some(k) = st.keys.get_mut(&net_fd) {
            if k.interest_ops & OP_CONNECT != 0 {
                let was_zero = k.ready_ops == 0;
                k.ready_ops |= OP_CONNECT;
                if was_zero {
                    count += 1;
                }
            }
        }
    }

    if woken || st.woken {
        st.woken = false;
        st.drain_wakeup_udp();
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Kernel-backed select — generic poll(2) fallback (macOS / BSD)
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "linux")))]
fn kernel_select_poll(id: i32, timeout_ms: i32) -> Result<i32, MethodCallFailed> {
    // Phase 1: snapshot.
    let (mut pollfds, key_index, wakeup_recv_fd) = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return Err(closed_selector());
        };
        let mut st = s.lock();
        if !st.open {
            return Err(closed_selector());
        }
        st.keys.retain(|_, v| !v.cancelled);
        let _ = st.init_wakeup_udp();

        // Sticky wakeup (see the Windows path / selector_wakeup contract).
        if st.woken {
            st.woken = false;
            st.drain_wakeup_udp();
            return Ok(0);
        }

        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(st.keys.len() + 1);
        let mut key_index: Vec<(i32, i32, bool)> = Vec::with_capacity(st.keys.len());
        for k in st.keys.values_mut() {
            k.ready_ops = 0;
            let Some(os) = k.handle.os_handle() else {
                continue;
            };
            let mut events: i16 = 0;
            if k.interest_ops & (OP_READ | OP_ACCEPT) != 0 {
                events |= libc::POLLIN;
            }
            if k.interest_ops & (OP_WRITE | OP_CONNECT) != 0 {
                events |= libc::POLLOUT;
            }
            if events == 0 {
                continue;
            }
            pollfds.push(libc::pollfd {
                fd: os as libc::c_int,
                events,
                revents: 0,
            });
            key_index.push((k.net_fd, k.interest_ops, k.handle.is_listener()));
        }
        let wakeup_recv_fd = if let Some(recv) = &st.wakeup_receiver {
            use std::os::unix::io::AsRawFd;
            let fd = recv.as_raw_fd();
            pollfds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            Some(pollfds.len() - 1)
        } else {
            None
        };
        (pollfds, key_index, wakeup_recv_fd)
    };

    // Phase 2: poll().
    let n = if pollfds.is_empty() {
        if timeout_ms > 0 {
            std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        }
        0
    } else {
        // SAFETY: pollfds is valid slice; len fits in nfds_t.
        unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms,
            )
        }
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted || err.raw_os_error() == Some(libc::EINTR) {
            return Ok(0);
        }
        return Err(ioex(format!("poll: {err}")));
    }

    // Phase 3: translate.
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        // Same in-flight-close rule as the Linux and Windows selector paths.
        return Ok(0);
    }

    let mut count = 0;
    let mut woken = false;
    let mut accepted_streams: Vec<(i32, TcpStream)> = Vec::new();
    for (i, pfd) in pollfds.iter().enumerate() {
        if pfd.revents == 0 {
            continue;
        }
        if Some(i) == wakeup_recv_fd {
            woken = true;
            continue;
        }
        let Some((net_fd, interest, is_listener)) = key_index.get(i).copied() else {
            continue;
        };
        let in_ready = pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        let out_ready = pfd.revents & libc::POLLOUT != 0;
        let mut ready = 0;
        if is_listener {
            if in_ready && interest & OP_ACCEPT != 0 {
                ready |= OP_ACCEPT;
            }
        } else {
            if in_ready && interest & OP_READ != 0 {
                ready |= OP_READ;
            }
            if out_ready && interest & OP_WRITE != 0 {
                ready |= OP_WRITE;
            }
            if out_ready && interest & OP_CONNECT != 0 {
                ready |= OP_CONNECT;
            }
        }
        if ready != 0 {
            if let Some(k) = st.keys.get_mut(&net_fd) {
                k.ready_ops = ready;
                count += 1;
                if is_listener && ready & OP_ACCEPT != 0 {
                    if let SelectableHandle::Listener(listener) = &k.handle {
                        if let Ok((stream, _)) = listener.accept() {
                            accepted_streams.push((net_fd, stream));
                        }
                    }
                }
            }
        }
    }
    for (fd, stream) in accepted_streams {
        st.pending_accepted.push_back((fd, stream));
    }
    if woken || st.woken {
        st.woken = false;
        st.drain_wakeup_udp();
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Probe-based fallback for platforms without kernel wait (or for `Dummy`)
// ---------------------------------------------------------------------------

/// Probe a single SelectableHandle. Returns (readyOps bitmask, optional
/// accepted stream if this is a listener and accept succeeded).
#[allow(dead_code)]
fn probe_handle(h: &SelectableHandle, interest: i32) -> (i32, Option<TcpStream>) {
    let mut ready = 0;
    let mut accepted = None;
    match h {
        SelectableHandle::Listener(listener) => {
            if interest & OP_ACCEPT != 0 {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        ready |= OP_ACCEPT;
                        accepted = Some(stream);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            }
        }
        SelectableHandle::Stream(stream) => {
            if interest & OP_CONNECT != 0 && stream.peer_addr().is_ok() {
                ready |= OP_CONNECT;
            }
            if interest & OP_READ != 0 {
                let mut buf = [0u8; 1];
                match stream.peek(&mut buf) {
                    Ok(n) if n > 0 => ready |= OP_READ,
                    Ok(_) => {
                        // EOF — JDK signals readable so read() returns -1.
                        ready |= OP_READ;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            }
            if interest & OP_WRITE != 0 {
                ready |= OP_WRITE;
            }
        }
        SelectableHandle::Udp(socket) => {
            if interest & OP_READ != 0 {
                let mut buf = [0u8; 1];
                match socket.peek(&mut buf) {
                    Ok(n) if n > 0 => ready |= OP_READ,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            }
            if interest & OP_WRITE != 0 {
                ready |= OP_WRITE;
            }
        }
        SelectableHandle::Dummy => {}
    }
    (ready, accepted)
}

/// Slow-path probe loop used when no kernel implementation is compiled in
/// (i.e. `cfg(not(any(unix, windows)))`). Also used as a per-cycle final
/// pass in tests where the listener was already drained but a fresh probe
/// can still report readable for stream-half checks.
#[allow(dead_code)]
fn probe_cycle(id: i32) -> Result<i32, MethodCallFailed> {
    let regs = selectors().read();
    let Some(s) = regs.get(&id) else {
        return Err(closed_selector());
    };
    let mut st = s.lock();
    if !st.open {
        return Err(closed_selector());
    }
    st.keys.retain(|_, v| !v.cancelled);

    let mut count = 0;
    let mut accepted_streams: Vec<(i32, TcpStream)> = Vec::new();
    for (fd, k) in st.keys.iter_mut() {
        let (ready, accepted) = probe_handle(&k.handle, k.interest_ops);
        k.ready_ops = ready;
        if ready != 0 {
            count += 1;
        }
        if let Some(stream) = accepted {
            accepted_streams.push((*fd, stream));
        }
    }
    for (fd, stream) in accepted_streams {
        st.pending_accepted.push_back((fd, stream));
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Public select() entry point — dispatches to the right kernel impl.
// ---------------------------------------------------------------------------

/// Run one `select()` call:
///   * `timeout < 0`        → IllegalArgumentException
///   * `timeout == 0`       → selectNow: one probe pass, no kernel sleep
///   * `0 < timeout < MAX`  → kernel wait up to `timeout` ms
///   * `timeout == i64::MAX` → block indefinitely until ready or wakeup
///
/// NOTE [nio-selector]: the `timeout == 0 → poll` rule here is the
/// low-level selectNow contract and is ONLY reached via the dedicated
/// `selectNow0`/`selectNow` native (`selector_select_now_native`, which
/// calls `selector_select(id, 0)` directly). The public blocking overloads
/// `Selector.select(long)` map their own argument 0 to *block indefinitely*
/// (i64::MAX) up in `selector_select_native` BEFORE reaching this function —
/// do not "simplify" that translation away or idiomatic `select(0)` event
/// loops will busy-spin again.
pub fn selector_select(id: i32, timeout_ms: i64) -> Result<i32, MethodCallFailed> {
    if timeout_ms < 0 {
        return Err(illegal_arg(format!(
            "Selector.select: negative timeout {timeout_ms}"
        )));
    }

    // Short-circuit a pre-existing wakeup before we blot the syscall.
    {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return Err(closed_selector());
        };
        let mut st = s.lock();
        if !st.open {
            return Err(closed_selector());
        }
        if st.woken {
            st.woken = false;
            // Drain whichever wakeup mechanism is in use.
            #[cfg(target_os = "linux")]
            st.drain_wakeup_pipe();
            #[cfg(not(target_os = "linux"))]
            st.drain_wakeup_udp();
            return Ok(0);
        }
    }

    // Translate timeout into a c_int for the syscall (-1 = infinite,
    // 0 = poll, >0 = ms). epoll_wait/WSAPoll/poll all accept i32 ms.
    let timeout_c: i32 = if timeout_ms == 0 {
        0
    } else if timeout_ms == i64::MAX {
        -1
    } else if timeout_ms > i32::MAX as i64 {
        i32::MAX
    } else {
        timeout_ms as i32
    };

    // Self-healing cap on an INDEFINITE wait. An infinite kernel wait that
    // misses its wakeup() — a dropped/raced loopback nudge while the thread is
    // already inside the kernel poll — would park the worker FOREVER. That is
    // the intermittent reactor-worker leak at client shutdown: the leaked
    // thread is RUNNABLE with an empty Java stack (parked in this select native,
    // which deposits no frame snapshot), stuck because its `select()`/`select(0)`
    // mapped to an infinite wait and the shutdown wakeup was lost. Every OTHER
    // CratonVM blocking primitive (`LockSupport.park`, `Object.wait`) polls every
    // 5 ms so a missed signal self-heals; the selector was the lone outlier that
    // could block forever. Cap an infinite wait at a coarse interval and return
    // (0 — a permitted spurious select wakeup) so the caller's event loop
    // re-checks its own running/shutdown flag and exits, exactly as it would on
    // a real wakeup. Finite timeouts are left alone (a missed wakeup there only
    // delays them by ≤ the timeout, which the reactor's default 1 s select
    // already bounds). Tunable via CRATONVM_SELECT_MAX_BLOCK_MS.
    let timeout_c = if timeout_c < 0 {
        select_infinite_cap_ms()
    } else {
        timeout_c
    };

    // Even-loop wrapper: epoll_wait/WSAPoll honor the deadline themselves
    // for blocking selects, so we don't need our own timer. For selectNow
    // we still want the listener-accept side-effect via probe_cycle so the
    // `take_pending_accepted` test passes — but we rely on the kernel for
    // ready-detection.
    if sel_dbg_enabled() {
        sel_dbg(format!("ENTER id={id} timeout_c={timeout_c}"));
    }
    #[cfg(target_os = "linux")]
    let count = kernel_select_linux(id, timeout_c)?;
    #[cfg(windows)]
    let count = kernel_select_windows(id, timeout_c)?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let count = kernel_select_poll(id, timeout_c)?;
    #[cfg(any(unix, windows))]
    if sel_dbg_enabled() {
        sel_dbg(format!("EXIT  id={id} n={count}"));
    }
    #[cfg(not(any(unix, windows)))]
    let count = {
        // Fallback for unusual targets: probe loop.
        let deadline = if timeout_ms == i64::MAX {
            None
        } else if timeout_ms == 0 {
            Some(Instant::now())
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };
        loop {
            let n = probe_cycle(id)?;
            if n > 0 {
                break n;
            }
            {
                let regs = selectors().read();
                let Some(s) = regs.get(&id) else {
                    return Err(closed_selector());
                };
                let mut st = s.lock();
                if st.woken {
                    st.woken = false;
                    st.drain_wakeup_udp();
                    return Ok(0);
                }
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    break 0;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    Ok(count)
}

/// Peek the size of the registered key map. Exposed for tests.
pub fn selector_key_count(id: i32) -> usize {
    let regs = selectors().read();
    regs.get(&id).map(|s| s.lock().keys.len()).unwrap_or(0)
}

/// Pop the next accepted stream for a given listener fd, if any. XNIO /
/// ServerSocketChannelImpl uses this to retrieve the stream that `select`
/// handed back from `listener.accept()`.
pub fn take_pending_accepted(id: i32, listener_fd: i32) -> Option<TcpStream> {
    let regs = selectors().read();
    let s = regs.get(&id)?;
    let mut st = s.lock();
    let pos = st
        .pending_accepted
        .iter()
        .position(|(fd, _)| *fd == listener_fd)?;
    let (_, stream) = st.pending_accepted.remove(pos)?;
    Some(stream)
}

/// Pop the oldest accepted stream sitting in any selector's pending queue
/// for the given listener id (search across all open selectors). Used by
/// `ServerSocketChannel.accept()` so the user-visible accept call returns
/// the connection that the selector loop already drained.
pub fn take_any_pending_accepted(listener_fd: i32) -> Option<TcpStream> {
    let regs = selectors().read();
    for (_id, s) in regs.iter() {
        let mut st = s.lock();
        if let Some(pos) = st
            .pending_accepted
            .iter()
            .position(|(fd, _)| *fd == listener_fd)
        {
            if let Some((_, stream)) = st.pending_accepted.remove(pos) {
                return Some(stream);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Object-field helpers
// ---------------------------------------------------------------------------

/// GC-stable identity-hash → native selector id.
///
/// `selector_open_native` allocates a REAL `sun.nio.ch.SelectorImpl`, whose
/// slots `SI_ID` (0) and `SI_OPEN_FLAG` (4) are reference-typed JDK fields
/// (`selectorOpen`, `selectedKeys`). Writing our int id/open-flag there is
/// silently descriptor-coerced to null, so `open_flag` read back false and
/// every `select` threw `ClosedSelectorException`. We key the selector
/// object to its native id by identity hash instead (the same pattern as the
/// SelectionKey `sk_table`); openness lives in the native `SelectorState`.
struct SelectorObjId {
    object: ObjectRef,
    id: i32,
}

fn sel_obj_ids() -> &'static RwLock<HashMap<i32, Vec<SelectorObjId>>> {
    static T: OnceLock<RwLock<HashMap<i32, Vec<SelectorObjId>>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

fn selector_id_from_obj(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    let hash = ctx.identity_hash_code(obj);
    if let Some(id) = sel_obj_ids()
        .read()
        .get(&hash)
        .and_then(|bucket| bucket.iter().find(|entry| entry.object == obj))
        .map(|entry| entry.id)
    {
        return id;
    }
    // Legacy fallback for any synthetic-layout selector object.
    if ctx.object_num_fields(obj) <= SI_ID {
        return 0;
    }
    ctx.get_field(obj, SI_ID).as_int().unwrap_or(0)
}

fn open_flag(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    let id = selector_id_from_obj(ctx, obj);
    if id != 0 {
        if let Some(open) = selectors().read().get(&id).map(|s| s.lock().open) {
            return open;
        }
    }
    // Legacy fallback (synthetic-layout selector object).
    if ctx.object_num_fields(obj) <= SI_OPEN_FLAG {
        return false;
    }
    ctx.get_field(obj, SI_OPEN_FLAG).as_int().unwrap_or(0) != 0
}

fn key_fd(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Option<i32> {
    // C27: side-table is now keyed by GC-stable identity hash code; the
    // stored `channel` is an `ObjectRef`, not a raw pointer.
    let hash = ctx.identity_hash_code(key_obj);
    let table = sk_table().read();
    let channel = sk_find(&table, key_obj, hash)?.channel;
    // The channel's registry id lives in the socket_channel side-table now
    // (its F_REG_ID object slot collides with a real-JDK reference field).
    crate::socket_channel::channel_net_fd(ctx, channel)
        .or_else(|| crate::datagram_channel_fd(ctx, channel))
}

fn key_selector_id(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Option<i32> {
    // C27: identity-hash-code key + stored `ObjectRef` value (no
    // from_raw resurrection).
    let hash = ctx.identity_hash_code(key_obj);
    let table = sk_table().read();
    let s = sk_find(&table, key_obj, hash)?.selector;
    Some(selector_id_from_obj(ctx, s))
}

// ---------------------------------------------------------------------------
// Native method impls — SelectorImpl
// ---------------------------------------------------------------------------

/// `Selector.open()` — static factory returning a fresh SelectorImpl.
fn selector_open_native(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = ctx
        .new_object("sun/nio/ch/SelectorImpl")?
        .and_then(|v| match v {
            Value::Object(o) => o,
            _ => None,
        })
        .ok_or_else(|| ioex("Selector.open: could not allocate SelectorImpl"))?;
    let id = selector_open();
    // Bind the (real-JDK-layout) selector object to its native id by GC-stable
    // identity hash — its int slots are reference-typed and would coerce to
    // null (see `sel_obj_ids`). The field writes below are kept as a
    // best-effort legacy path but are not relied upon.
    sel_obj_ids()
        .write()
        .entry(ctx.identity_hash_code(obj))
        .or_default()
        .push(SelectorObjId { object: obj, id });
    let n = ctx.object_num_fields(obj);
    if n > SI_ID {
        ctx.set_field(obj, SI_ID, Value::Int(id));
    }
    if n > SI_OPEN_FLAG {
        ctx.set_field(obj, SI_OPEN_FLAG, Value::Int(1));
    }
    Ok(Some(Value::Object(Some(obj))))
}

/// `SelectorImpl.close0()` — release native state.
fn selector_close_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(None);
    };
    let id = selector_id_from_obj(ctx, obj);
    if id != 0 {
        selector_close(id);
    }
    let hash = ctx.identity_hash_code(obj);
    let mut ids = sel_obj_ids().write();
    let remove_bucket = if let Some(bucket) = ids.get_mut(&hash) {
        bucket.retain(|entry| entry.object != obj);
        bucket.is_empty()
    } else {
        false
    };
    if remove_bucket {
        ids.remove(&hash);
    }
    if ctx.object_num_fields(obj) > SI_OPEN_FLAG {
        ctx.set_field(obj, SI_OPEN_FLAG, Value::Int(0));
    }
    Ok(None)
}

/// `SelectorImpl.wakeup0()` — no args beyond `this`.
fn selector_wakeup_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(None);
    };
    if !open_flag(ctx, obj) {
        return Err(closed_selector());
    }
    let id = selector_id_from_obj(ctx, obj);
    if id != 0 {
        selector_wakeup(id)?;
    }
    Ok(None)
}

/// `SelectorImpl.select0(long timeout)` → int count
/// Re-resolve the OS handle of any registered key whose channel has become
/// connected/bound since it was registered. NIO reactors (e.g. Apache
/// httpasyncclient's `DefaultConnectingIOReactor`) `register(OP_CONNECT)` a
/// `SocketChannel` *before* `connect()` completes; at registration time the
/// channel has no live socket so the key holds a `Dummy` handle and the kernel
/// wait skips it — `select()` then returns 0 forever and the connection never
/// progresses. CratonVM connects loopback synchronously, so by the first
/// `select()` the socket IS connected; pull its now-live clone into the key so
/// `OP_CONNECT`/`OP_READ`/`OP_WRITE` readiness is reported. Called on every
/// blocking/poll select entry (it has the `ctx` needed to read channel state).
fn refresh_selector_handles(ctx: &mut dyn NativeContext, id: i32) {
    // Snapshot keys that currently have no pollable OS handle but want events.
    let candidates: Vec<(i32, ObjectRef)> = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return;
        };
        let st = s.lock();
        st.keys
            .values()
            .filter(|k| k.interest_ops != 0 && k.handle.os_handle().is_none())
            .filter_map(|k| k.key_obj.map(|key| (k.net_fd, key)))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    for (old_fd, key_obj) in candidates {
        let channel = {
            let hash = ctx.identity_hash_code(key_obj);
            let t = sk_table().read();
            match sk_find(&t, key_obj, hash) {
                Some(s) => s.channel,
                None => continue,
            }
        };
        let new_fd = crate::socket_channel::channel_net_fd(ctx, channel).unwrap_or(-1);
        if new_fd < 0 {
            continue;
        }
        let Some(clone) = crate::socket_channel::tcp_clone_for_selector(new_fd) else {
            continue;
        };
        let handle = match clone {
            crate::socket_channel::TcpHandleClone::Listener(l) => {
                let _ = l.set_nonblocking(true);
                SelectableHandle::Listener(l)
            }
            crate::socket_channel::TcpHandleClone::Stream(s) => {
                let _ = s.set_nonblocking(true);
                SelectableHandle::Stream(s)
            }
        };
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else {
            return;
        };
        let mut st = s.lock();
        // Re-key under the now-resolved net_fd (was likely -1 while unconnected).
        if let Some(mut ks) = st.keys.remove(&old_fd) {
            ks.net_fd = new_fd;
            ks.handle = handle;
            #[cfg(target_os = "linux")]
            let epoll_update = st.epoll_fd.and_then(|efd| {
                ks.handle
                    .os_handle()
                    .map(|os| (efd, os, ks.handle.is_listener(), ks.interest_ops))
            });
            st.keys.insert(new_fd, ks);
            #[cfg(target_os = "linux")]
            if let Some((efd, os, is_listener, interest_ops)) = epoll_update {
                let mut ev = libc::epoll_event {
                    events: linux_events_for(interest_ops, is_listener) as u32,
                    u64: new_fd as u64,
                };
                let rc = unsafe {
                    libc::epoll_ctl(efd, libc::EPOLL_CTL_ADD, os as libc::c_int, &mut ev)
                };
                if rc < 0 {
                    let _ = unsafe {
                        libc::epoll_ctl(efd, libc::EPOLL_CTL_MOD, os as libc::c_int, &mut ev)
                    };
                }
            }
        }
    }
}

fn selector_select_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    if !open_flag(ctx, obj) {
        return Err(closed_selector());
    }
    let timeout = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    // BUGFIX [nio-selector]: JDK `Selector.select(long)` contract — a timeout
    // of 0 means *block indefinitely* until a channel is ready or wakeup()
    // fires; ONLY `selectNow()` polls. Every public-API blocking overload
    // (`select0(J)`, `select(J)`, `select(Consumer,long)` via
    // `lockAndDoSelect`) funnels through this native, so an idiomatic event
    // loop `while (running) selector.select(0);` was spinning at 100% CPU
    // because we mapped 0 -> 0ms poll. Translate the public 0 to block-
    // indefinitely (i64::MAX, which `selector_select` lowers to timeout_c=-1).
    // The genuine non-blocking probe is preserved on the dedicated
    // `selectNow0`/`selectNow` path: `selector_select_now_native` calls
    // `selector_select(id, 0)` DIRECTLY, bypassing this translation, so
    // `selector_select`'s own `timeout == 0 -> poll` rule still serves it.
    let timeout = if timeout == 0 { i64::MAX } else { timeout };
    let id = selector_id_from_obj(ctx, obj);
    if id == 0 {
        return Ok(Some(Value::Int(0)));
    }
    refresh_selector_handles(ctx, id);
    // GC-blocking audit (gc-blocked-thread-frame-stale-thread-mirror, proper-
    // fix item 2): every call reaching this native is a BLOCKING select (the
    // non-blocking probe rides the dedicated selectNow0 path), parking the
    // thread in the kernel wait for up to `timeout` — indefinitely for the
    // translated select(0). Without the blocking-region bracket every
    // cross-thread STW GC must wait for each selector loop to tick out of
    // epoll_wait (Tribes/Tomcat NIO threads stalled every collection), and an
    // indefinite select wedges `wait_for_all` outright. `obj` is dispatched
    // on after the wait, so re-sync it through `end_blocking_region_refs`.
    let mut held = vec![Value::Object(Some(obj))];
    ctx.begin_blocking_region();
    let select_res = selector_select(id, timeout);
    ctx.end_blocking_region_refs(&mut held);
    let obj = match held[0] {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let n = select_res?;
    apply_ready_ops(ctx, id);
    populate_selected_keys_field(ctx, obj, id);
    Ok(Some(Value::Int(n)))
}

/// Mirror the native readyOps onto the SelectionKey side-table so user
/// code reading `key.readyOps()` sees the post-select state.
fn apply_ready_ops(_ctx: &mut dyn NativeContext, id: i32) {
    // C27: side-table is keyed by the SelectionKey's identity hash
    // code; carry the hash through instead of a raw pointer.
    let snap: Vec<(ObjectRef, i32)> = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else { return };
        let st = s.lock();
        st.keys
            .values()
            .filter_map(|k| k.key_obj.map(|obj| (obj, k.ready_ops)))
            .collect()
    };
    let mut table = sk_table().write();
    for (key, ready) in snap {
        let hash = _ctx.identity_hash_code(key);
        if let Some(state) = sk_find_mut(&mut table, key, hash) {
            state.ready_ops = ready;
        }
    }
}

/// Push the ready SelectionKeys into the selector's LIVE `selectedKeys` field Set.
///
/// CratonVM normally surfaces readiness two ways: (a) `apply_ready_ops` mirrors
/// readyOps into `sk_table` (so `key.readyOps()` works), and (b) it overrides
/// `Selector.selectedKeys()` to BUILD a fresh set on demand from the ready keys
/// (how Tomcat/ES consume readiness). But **Netty** reflectively REPLACES
/// `sun.nio.ch.SelectorImpl.selectedKeys` (+ `publicSelectedKeys`) with its own
/// `SelectedSelectionKeySet` and reads THAT field directly in `processSelectedKeys()`
/// — it never calls `selectedKeys()`. Since CratonVM never wrote to the field, Netty's
/// set stayed empty and a bound Vert.x/Netty server accepted nothing (the connection
/// was detected + drained into `pending_accepted`, but the reactor was never told).
/// Mirror the JDK native `doSelect`: add each ready key to the selector's current
/// `selectedKeys` field. Idempotent for a JDK HashSet (dedups); Netty resets its set
/// before each select. Skips silently when the field is null / not a Set (then nothing
/// reads it and the on-demand `selectedKeys()` path is authoritative).
fn populate_selected_keys_field(ctx: &mut dyn NativeContext, selector: ObjectRef, id: i32) {
    let Value::Object(Some(set)) = ctx.get_field_by_name(selector, "selectedKeys") else {
        return;
    };
    // Collect ready key objects OUTSIDE the selectors() lock — invoke_virtual runs
    // Java bytecode (Set.add) that may re-enter selector code.
    let ready: Vec<ObjectRef> = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else { return };
        let st = s.lock();
        st.keys
            .values()
            .filter(|k| !k.cancelled && k.ready_ops != 0)
            .filter_map(|k| k.key_obj)
            .collect()
    };
    if ready.is_empty() {
        return;
    }
    // Each Set.add below runs Java bytecode and may trigger a moving GC, so
    // the raw `set` receiver and the still-pending key refs go stale after
    // the first add (observed as an all-zero-header java/util/Set receiver +
    // `NoSuchMethodError Object.add` under Tribes' ParallelNioSender.doLoop).
    // Pin them all and re-read through the pins before every dispatch. The
    // single unpin pops the whole watermark (set pin + key pins).
    let set_pin = ctx.pin_native_root(set);
    let key_pins: Vec<_> = ready
        .iter()
        .map(|k| (ctx.pin_native_root(*k), *k))
        .collect();
    for (pin, orig) in key_pins {
        let set_cur = ctx.read_native_pin(set_pin, set);
        let key_cur = ctx.read_native_pin(pin, orig);
        let _ = ctx.invoke_virtual(
            set_cur,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(key_cur))],
        );
    }
    ctx.unpin_native_roots(set_pin);
}

/// `SelectorImpl.selectNow0()` → int
fn selector_select_now_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    if !open_flag(ctx, obj) {
        return Err(closed_selector());
    }
    let id = selector_id_from_obj(ctx, obj);
    if id == 0 {
        return Ok(Some(Value::Int(0)));
    }
    refresh_selector_handles(ctx, id);
    let n = selector_select(id, 0)?;
    apply_ready_ops(ctx, id);
    populate_selected_keys_field(ctx, obj, id);
    Ok(Some(Value::Int(n)))
}

// ---------------------------------------------------------------------------
// Native method impls — SelectableChannel.register
// ---------------------------------------------------------------------------

// Side-table mapping SelectionKey object → (channel, selector, interestOps,
// readyOps, attachment). Used because `sun/nio/ch/SelectionKeyImpl` has its
// own JDK-managed instance layout and writing to ad-hoc slots collides.
//
// C27 (Round-11 GC-safety fix): the previous incarnation keyed this table
// by `key.as_ptr() as usize` and stored channel/selector/attachment as
// raw `usize` pointers, resurrecting them via
// `unsafe { ObjectRef::from_raw(raw as *mut u8) }` at read time. Under
// CratonVM's moving GC any compaction relocated the keys (so lookups
// missed and a freshly-allocated object at the old address silently
// collided with the orphaned row) and the embedded values (so the
// resurrected ObjectRef referred to whatever now lived at the old
// address — wrong-object dispatch in the lucky case, wild dereference
// in the unlucky one).
//
// The fix:
//   * Re-key on the SelectionKey's GC-stable identity hash code (i32) —
//     the GC's HashCodeTable preserves the hash word across compaction
//     (see `gc/src/compact_header.rs::HashCodeTable::update_after_gc`).
//   * Store the embedded references as actual `ObjectRef` so a future
//     post-compaction hook (`sk_table_update_after_gc`, below) can
//     remap them through the GC's pointer_map. Until that hook is
//     wired into `vm/src/memory/gc.rs`, the worst-case outcome of an
//     intervening compaction is a missed accessor (returns Object(None)
//     / Int(0)) rather than a wild dereference.
//
// Mirrors `SEED_TABLE` (native-builtins/src/securerandom.rs) and the
// `VH_META_TABLE` pattern (native-builtins/src/lang_invoke.rs).
struct SkState {
    key: ObjectRef,
    channel: ObjectRef,
    selector: ObjectRef,
    interest_ops: i32,
    ready_ops: i32,
    attachment: Option<ObjectRef>,
    cancelled: bool,
}

fn sk_table() -> &'static parking_lot::RwLock<FxHashMap<i32, Vec<SkState>>> {
    static REG: std::sync::OnceLock<parking_lot::RwLock<FxHashMap<i32, Vec<SkState>>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| parking_lot::RwLock::new(FxHashMap::default()))
}

fn sk_find(table: &FxHashMap<i32, Vec<SkState>>, key: ObjectRef, hash: i32) -> Option<&SkState> {
    table.get(&hash)?.iter().find(|state| state.key == key)
}

fn sk_find_mut(
    table: &mut FxHashMap<i32, Vec<SkState>>,
    key: ObjectRef,
    hash: i32,
) -> Option<&mut SkState> {
    table
        .get_mut(&hash)?
        .iter_mut()
        .find(|state| state.key == key)
}

/// Post-GC hook — remap every ObjectRef stored in `sk_table` through the
/// GC's pointer map. The keys (identity hash codes) are GC-stable on
/// their own; only the embedded `channel` / `selector` / `attachment`
/// ObjectRef values need rewriting after compaction. Mirrors
/// `gc_update_lambda_callsite_cache_refs` in
/// `native-builtins/src/lang_invoke.rs`. Until this hook is wired into
/// `vm/src/memory/gc.rs`'s post-compaction step, the table will return
/// stale ObjectRef values for any entry whose underlying objects moved;
/// see the SkState doc-block for the residual behaviour.
#[allow(dead_code)]
pub fn sk_table_update_after_gc<S: std::hash::BuildHasher>(
    pointer_map: &std::collections::HashMap<usize, usize, S>,
) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |obj: ObjectRef| -> ObjectRef {
        let old = obj.as_ptr() as usize;
        match pointer_map.get(&old) {
            Some(&new_addr) if new_addr != 0 => {
                // SAFETY: `new_addr` is the GC's relocated address for
                // the same logical object; the GC guarantees the new
                // address satisfies ObjectRef's non-null/8-byte-align
                // invariants.
                unsafe { ObjectRef::from_raw(new_addr as *mut u8) }
            }
            _ => obj,
        }
    };
    // Lock order: `selectors()` before `sk_table()` (matches
    // `apply_ready_ops` which takes the same pair in this order).
    {
        let regs = selectors().read();
        for sel in regs.values() {
            let mut st = sel.lock();
            for k in st.keys.values_mut() {
                if let Some(obj) = k.key_obj {
                    k.key_obj = Some(remap(obj));
                }
            }
        }
    }
    {
        let mut ids = sel_obj_ids().write();
        for bucket in ids.values_mut() {
            for entry in bucket {
                entry.object = remap(entry.object);
            }
        }
    }
    let mut table = sk_table().write();
    for bucket in table.values_mut() {
        for state in bucket {
            state.key = remap(state.key);
            state.channel = remap(state.channel);
            state.selector = remap(state.selector);
            if let Some(att) = state.attachment {
                state.attachment = Some(remap(att));
            }
        }
    }
}

/// GC root-scan hook — push every live `ObjectRef` reachable only through
/// the selector tables onto `roots` so the moving GC keeps them alive (and
/// records them for relocation). The orchestrator in `vm/src/memory/gc.rs`
/// calls this during root enumeration; the symmetric post-compaction remap
/// is `sk_table_update_after_gc` above.
///
/// Symmetry contract: this scans the EXACT same set of refs that
/// `sk_table_update_after_gc` remaps — the per-selector `key_obj`
/// (`SelectableKind`-bearing `KeyState`s in `selectors()`) plus each
/// `SkState`'s `channel`, `selector`, and `attachment`. A ref that is
/// remapped after GC but not rooted before it could be reclaimed (or
/// relocated to an unmarked slot), so scan and remap must cover the
/// identical fields.
///
/// Without this hook a SelectionKey whose only live references are in
/// `sk_table` / the selector `keys` map is invisible to the collector:
/// it can be swept before `sk_table_update_after_gc` ever runs (a UAF the
/// remap cannot repair), or moved to a slot the scan never marked live.
///
/// `ObjectRef` is internally non-null, but we defensively skip any ref
/// whose `as_ptr()` is null (the canonical null test — there is no
/// `ObjectRef::is_null` that applies here) so a malformed entry can never
/// inject a null root.
pub fn gc_scan_selector_roots(roots: &mut Vec<cratonvm_types::ObjectRef>) {
    let mut push = |obj: cratonvm_types::ObjectRef| {
        if !obj.as_ptr().is_null() {
            roots.push(obj);
        }
    };
    // Lock order: `selectors()` before `sk_table()` — matches
    // `sk_table_update_after_gc` and `apply_ready_ops`, so a concurrent GC
    // hook can never deadlock against the remap path.
    {
        let regs = selectors().read();
        for sel in regs.values() {
            let st = sel.lock();
            for k in st.keys.values() {
                if let Some(obj) = k.key_obj {
                    push(obj);
                }
            }
        }
    }
    {
        let ids = sel_obj_ids().read();
        for bucket in ids.values() {
            for entry in bucket {
                push(entry.object);
            }
        }
    }
    let table = sk_table().read();
    for bucket in table.values() {
        for state in bucket {
            push(state.key);
            push(state.channel);
            push(state.selector);
            if let Some(att) = state.attachment {
                push(att);
            }
        }
    }
}

fn sk_state_get_field<F: FnOnce(&SkState) -> Value>(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    f: F,
) -> Option<Value> {
    let hash = ctx.identity_hash_code(key);
    let table = sk_table().read();
    sk_find(&table, key, hash).map(f)
}

fn sk_state_with_mut<F: FnOnce(&mut SkState) -> R, R>(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    f: F,
) -> Option<R> {
    let hash = ctx.identity_hash_code(key);
    let mut table = sk_table().write();
    sk_find_mut(&mut table, key, hash).map(f)
}

fn channel_register_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(channel))) = args.first().copied() else {
        return Err(ioex("register: null channel"));
    };
    let selector_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(ioex("register: null selector")),
    };
    let ops = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let attachment = match args.get(3) {
        Some(Value::Object(o)) => Value::Object(*o),
        Some(Value::Int(v)) => Value::Int(*v),
        _ => Value::Object(None),
    };

    if !open_flag(ctx, selector_obj) {
        return Err(closed_selector());
    }

    let sel_id = selector_id_from_obj(ctx, selector_obj);
    if sel_id == 0 {
        return Err(closed_selector());
    }

    // The channel's `tcp_registry` id lives in the socket_channel side-table
    // (its F_REG_ID object slot collides with a real-JDK reference field and
    // would coerce to null). A negative fd means the channel is not bound /
    // connected; selector_register still records the key (interest only).
    let net_fd = crate::socket_channel::channel_net_fd(ctx, channel)
        .or_else(|| crate::datagram_channel_fd(ctx, channel))
        .unwrap_or(-1);

    // If the registered channel is backed by an entry in the WP3.4
    // tcp_registry, hand the selector a clone of the live socket so
    // `selector_select` can actually poll readiness. The clone is
    // independent of the JDK-visible handle (so accept()/read() still
    // return the real connection).
    let kind = match crate::socket_channel::tcp_clone_for_selector(net_fd) {
        Some(crate::socket_channel::TcpHandleClone::Listener(l)) => {
            Some(SelectableKind::Listener(l))
        }
        Some(crate::socket_channel::TcpHandleClone::Stream(s)) => Some(SelectableKind::Stream(s)),
        None => crate::datagram_channel_udp_clone(ctx, channel).map(SelectableKind::Udp),
    };

    let key_obj = ctx
        .new_object("sun/nio/ch/SelectionKeyImpl")?
        .and_then(|v| match v {
            Value::Object(o) => o,
            _ => None,
        })
        .ok_or_else(|| ioex("register: could not allocate SelectionKeyImpl"))?;
    // `AbstractSelectionKey.isValid()` is FINAL and reads the `valid` field
    // directly; its `ensureValid()` (called from `SelectionKeyImpl.interestOps`
    // etc.) throws `CancelledKeyException` when `valid` is false. We build the key
    // via `new_object` without running `<init>` (which sets `valid = true`), so
    // the field defaults to 0 → a freshly-registered key the Apache NIO reactor
    // configures (`processNewChannels`) spuriously threw, abandoning that session
    // and losing the request (ES MultipleHosts testAsyncRequests). Seed it true;
    // `cancel()` flips it false (see key_cancel/sk_cancel_public).
    ctx.set_field_by_name(key_obj, "valid", Value::Int(1));
    // C27: stash the key state in a side-table keyed by the
    // SelectionKey's GC-stable identity hash code (i32). The embedded
    // ObjectRefs are stored directly so a future post-GC hook can
    // remap them through the GC's pointer_map.
    let key_hash = ctx.identity_hash_code(key_obj);
    let attachment_obj = match attachment {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    sk_table()
        .write()
        .entry(key_hash)
        .or_default()
        .push(SkState {
            key: key_obj,
            channel,
            selector: selector_obj,
            interest_ops: ops,
            ready_ops: 0,
            attachment: attachment_obj,
            cancelled: false,
        });

    selector_register(sel_id, net_fd, ops, Some(key_obj), key_hash, kind)?;

    Ok(Some(Value::Object(Some(key_obj))))
}

/// `SelectableChannel.keyFor(Selector)` — return the `SelectionKey` this channel
/// is currently registered with on `sel`, or `null`.
///
/// The real `AbstractSelectableChannel.keyFor` bytecode is
/// `synchronized (keyLock) { ...walk keys[]... }`. CratonVM's channel objects keep
/// their state in the `socket_channel` identity-hash side-table and never
/// initialise the reference-typed `keyLock` slot, so that bytecode does
/// `monitorenter` on a null `keyLock` → `NullPointerException`. Tomcat's
/// `NioEndpoint` reports this as "Error in selector loop" and the poller thread
/// dies, so every embedded-server test hangs (DF01). Answer from the native key
/// registry `channel_register_native` populated instead of running the bytecode.
fn channel_key_for_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(channel))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(Value::Object(Some(selector_obj))) = args.get(1).copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let sel_id = selector_id_from_obj(ctx, selector_obj);
    if sel_id == 0 {
        return Ok(Some(Value::Object(None)));
    }
    // The channel's net fd (tcp_registry id) is the per-selector key into the
    // registration map — exactly what `channel_register_native` stored under.
    let Some(net_fd) = crate::socket_channel::channel_net_fd(ctx, channel)
        .or_else(|| crate::datagram_channel_fd(ctx, channel))
    else {
        // Not bound / connected → no live registration to find.
        return Ok(Some(Value::Object(None)));
    };
    let key_obj = selectors().read().get(&sel_id).and_then(|s| {
        let guard = s.lock();
        guard
            .keys
            .get(&net_fd)
            .filter(|k| !k.cancelled)
            .and_then(|k| k.key_obj)
    });
    Ok(Some(Value::Object(key_obj)))
}

// ---------------------------------------------------------------------------
// Native method impls — SelectionKeyImpl
// ---------------------------------------------------------------------------

fn key_set_interest_ops_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(key))) = args.first().copied() else {
        return Ok(None);
    };
    let ops = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if ctx.object_num_fields(key) > SK_INTEREST_OPS {
        ctx.set_field(key, SK_INTEREST_OPS, Value::Int(ops));
    }
    // `interestOps0` is used by the real JDK implementation. A channel can
    // be registered while still unconnected, in which case its selector key
    // is currently stored under the placeholder fd (-1), whereas
    // `channel_net_fd` already observes the later live socket id. Looking up
    // by that current id loses the update, leaving OP_CONNECT at zero and the
    // async reactor never receives its deferred connection result. Locate the
    // key by its stable Java object instead, then keep both native tables in
    // sync until `refresh_selector_handles` re-keys it to the live fd.
    sk_state_with_mut(ctx, key, |s| {
        s.interest_ops = ops;
    });
    let target: Option<(i32, i32)> = {
        let regs = selectors().read();
        let mut found = None;
        for (sel_id, sel) in regs.iter() {
            let mut st = sel.lock();
            if let Some(fd) = st
                .keys
                .values_mut()
                .find(|k| k.key_obj == Some(key))
                .map(|k| {
                    k.interest_ops = ops;
                    k.net_fd
                })
            {
                found = Some((*sel_id, fd));
                break;
            }
        }
        found
    };
    if let Some((sel_id, fd)) = target {
        let _ = selector_set_interest(sel_id, fd, ops);
    }
    Ok(None)
}

fn key_cancel_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(key))) = args.first().copied() else {
        return Ok(None);
    };
    // Mirror cancellation into the real `valid` field (final
    // AbstractSelectionKey.isValid reads it directly) and the sk_table.
    ctx.set_field_by_name(key, "valid", Value::Int(0));
    sk_state_with_mut(ctx, key, |s| {
        s.cancelled = true;
        s.ready_ops = 0;
    });
    let Some(fd) = key_fd(ctx, key) else {
        return Ok(None);
    };
    let Some(sel_id) = key_selector_id(ctx, key) else {
        return Ok(None);
    };
    selector_cancel(sel_id, fd);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public-facing SelectionKey accessors — back the abstract methods on
// java.nio.channels.SelectionKey + sun.nio.ch.SelectionKeyImpl by reading
// directly out of our 5-field synthetic. The concrete is{Readable,
// Writable, Connectable, Acceptable} stay as JDK bytecode (final methods
// that mask readyOps() against OP_*).
// ---------------------------------------------------------------------------

fn sk_channel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    // C27: stored as a real ObjectRef now (no more from_raw resurrection).
    let v = sk_state_get_field(ctx, this, |s| Value::Object(Some(s.channel)))
        .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_selector(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let v = sk_state_get_field(ctx, this, |s| Value::Object(Some(s.selector)))
        .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_interest_ops(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let v = sk_state_get_field(ctx, this, |s| Value::Int(s.interest_ops)).unwrap_or(Value::Int(0));
    Ok(Some(v))
}

fn sk_set_interest_ops(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let ops = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let key_hash = ctx.identity_hash_code(this);
    // Snapshot sk_table membership under a brief read lock, then drop
    // it before grabbing `selectors()` to keep the canonical
    // lock-order (selectors() before sk_table()) that `apply_ready_ops`
    // and `sk_table_update_after_gc` use.
    let known = sk_state_get_field(ctx, this, |_| Value::Int(1)).is_some();
    sk_state_with_mut(ctx, this, |s| {
        s.interest_ops = ops;
    });
    // Mirror to native KeyState so the next select() picks up the
    // change. C27: cross-match on the GC-stable identity hash code
    // rather than the raw pointer (which can drift under compaction).
    //
    // DEADLOCK FIX: `selector_set_interest` re-acquires the per-selector
    // mutex, so we must NOT hold `sel.lock()` across the call (parking_lot
    // mutexes are non-reentrant — Tomcat's NioEndpoint.unreg → interestOps()
    // would otherwise wedge the Poller thread forever, and the request never
    // gets read). Resolve the (selector id, net fd) under the lock, set the
    // interest there, then drop the lock before the OS-level update.
    if known {
        let target: Option<(i32, i32)> = {
            let regs = selectors().read();
            let mut found = None;
            for (sel_id, sel) in regs.iter() {
                let mut st = sel.lock();
                let net_fd = st
                    .keys
                    .values_mut()
                    .find(|k| k.key_obj == Some(this))
                    .map(|k| {
                        k.interest_ops = ops;
                        k.net_fd
                    });
                if let Some(fd) = net_fd {
                    found = Some((*sel_id, fd));
                    break;
                }
            }
            found
        };
        if let Some((sel_id, net_fd)) = target {
            let _ = selector_set_interest(sel_id, net_fd, ops);
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

fn sk_ready_ops(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let v = sk_state_get_field(ctx, this, |s| Value::Int(s.ready_ops)).unwrap_or(Value::Int(0));
    Ok(Some(v))
}

fn sk_is_valid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    // A registered key with no side-table row is treated as valid: it was just
    // created by channel_register and only an explicit cancel() marks it invalid.
    let valid = sk_state_get_field(ctx, this, |s| Value::Int(if s.cancelled { 0 } else { 1 }))
        .unwrap_or(Value::Int(1));
    Ok(Some(valid))
}

fn sk_attach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let new_att_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // C27: store the attachment as a real ObjectRef (was a raw usize
    // before, with a from_raw resurrection at read time).
    let prev = sk_state_with_mut(ctx, this, |s| {
        let prev = s.attachment.take();
        s.attachment = new_att_obj;
        prev
    })
    .flatten();
    let prev_v = match prev {
        Some(obj) => Value::Object(Some(obj)),
        None => Value::Object(None),
    };
    Ok(Some(prev_v))
}

fn sk_attachment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let v = sk_state_get_field(ctx, this, |s| match s.attachment {
        Some(obj) => Value::Object(Some(obj)),
        None => Value::Object(None),
    })
    .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_cancel_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let key_hash = ctx.identity_hash_code(this);
    // Reflect cancellation in the real `valid` field too (final
    // AbstractSelectionKey.isValid reads it directly).
    ctx.set_field_by_name(this, "valid", Value::Int(0));
    sk_state_with_mut(ctx, this, |s| {
        s.cancelled = true;
        s.ready_ops = 0;
    });
    // Walk all selectors looking for this key and mark cancelled.
    // C27: identity-hash comparison instead of raw-pointer comparison.
    let regs = selectors().read();
    for (sel_id, sel) in regs.iter() {
        let mut st = sel.lock();
        let Some(target) = st
            .keys
            .iter()
            .find(|(_, k)| k.key_obj == Some(this))
            .map(|(fd, _)| *fd)
        else {
            continue;
        };
        drop(st);
        selector_cancel(*sel_id, target);
        break;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public-facing Selector accessors that bypass the JDK bytecode (which
// reaches into uninitialized HashMaps populated only when the JDK's own
// SelectorImpl.<init> chain runs).
// ---------------------------------------------------------------------------

fn selector_selected_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let id = selector_id_from_obj(ctx, obj);
    // C27: collect ObjectRefs directly (was raw usize + from_raw).
    let key_objs: Vec<ObjectRef> = if id == 0 {
        Vec::new()
    } else {
        let regs = selectors().read();
        match regs.get(&id) {
            Some(s) => s
                .lock()
                .keys
                .values()
                .filter(|k| !k.cancelled && k.ready_ops != 0)
                .filter_map(|k| k.key_obj)
                .collect(),
            None => Vec::new(),
        }
    };
    Ok(Some(Value::Object(Some(build_set(ctx, &key_objs)))))
}

fn selector_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let id = selector_id_from_obj(ctx, obj);
    let key_objs: Vec<ObjectRef> = if id == 0 {
        Vec::new()
    } else {
        let regs = selectors().read();
        match regs.get(&id) {
            Some(s) => s
                .lock()
                .keys
                .values()
                .filter(|k| !k.cancelled)
                .filter_map(|k| k.key_obj)
                .collect(),
            None => Vec::new(),
        }
    };
    Ok(Some(Value::Object(Some(build_set(ctx, &key_objs)))))
}

/// Build a HashSet populated with the given key objects. Allocates a real
/// JDK HashSet (so its bytecode iterator/contains/remove work) and adds
/// each key via `add(Object)`. The selector calls this on every
/// `selectedKeys()` / `keys()` invocation; lifetimes are short so we keep
/// it simple.
///
/// C27: takes a slice of real ObjectRefs (was a slice of raw usize with
/// a from_raw resurrection per element — GC-unsafe).
fn build_set(ctx: &mut dyn NativeContext, keys: &[ObjectRef]) -> ObjectRef {
    // Allocate + run the no-arg constructor so the backing HashMap field
    // (`map`) is non-null. Without <init> the JDK HashSet bytecode NPEs at
    // `map.keySet()` during iterator()/size()/etc.
    let set_value = ctx
        .new_object("java/util/HashSet")
        .ok()
        .and_then(|v| match v {
            Some(Value::Object(Some(o))) => Some(o),
            _ => None,
        });
    let set = match set_value {
        Some(o) => o,
        None => match ctx.ensure_class_initialized("java/util/HashSet") {
            Ok(cid) => ctx.alloc_object(cid, 1),
            Err(_) => ctx.alloc_object(ClassId::new(0), 1),
        },
    };
    // Cross-call GC-safety fix (2026-07-07, companion to
    // `populate_selected_keys_field`'s identical fix, same file): `set` is a
    // bare Rust local held across `invoke_special`/`invoke_virtual` calls
    // below, each of which runs Java bytecode (HashSet.<init> / Set.add) that
    // can trigger a moving GC. Reproduced under CRATONVM_GC_STRESS
    // amplification as an all-zero-header `java/util/Set` receiver +
    // `NoSuchMethodError Object.add` at `NioReceiver.listen()`'s
    // `selectedKeys().iterator()` call — i.e. `Selector.selectedKeys()` /
    // `.keys()` (the only callers of `build_set`) handed back a HashSet that
    // had already gone stale while building itself, before the caller's
    // bytecode ever got to dereference it. Pin `set` (and each pending key)
    // and re-read through the pin before every dispatch — the same pattern
    // `populate_selected_keys_field` already uses for its own Set.add loop.
    let set_pin = ctx.pin_native_root(set);
    let _ = ctx.invoke_special(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ctx.read_native_pin(set_pin, set)))],
    );
    let key_pins: Vec<_> = keys.iter().map(|k| (ctx.pin_native_root(*k), *k)).collect();
    for (pin, orig) in key_pins {
        let set_cur = ctx.read_native_pin(set_pin, set);
        let key_cur = ctx.read_native_pin(pin, orig);
        let _ = ctx.invoke_virtual(
            set_cur,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(key_cur))],
        );
    }
    let result = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin);
    result
}

fn selector_select_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Selector.select() — blocking, no timeout. Cap at 30s so unit-tests
    // never wedge if a wakeup is missed.
    let mut new_args: Vec<Value> = Vec::with_capacity(2);
    if let Some(this) = args.first().copied() {
        new_args.push(this);
    } else {
        return Ok(Some(Value::Int(0)));
    }
    new_args.push(Value::Long(30_000));
    selector_select_native(ctx, &new_args)
}

fn selector_wakeup_public(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    selector_wakeup_native(ctx, args)?;
    Ok(args.first().copied())
}

fn selector_is_open_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    Ok(Some(Value::Int(if open_flag(ctx, obj) { 1 } else { 0 })))
}

/// `SelectorImpl.lockAndDoSelect(Consumer<SelectionKey> action, long timeout)`
/// — the JDK protected method called by every public select(...) overload.
/// It synchronizes on internal locks and walks ready key state we don't
/// populate, so we route the timeout straight to our native select and
/// invoke the optional consumer over freshly ready keys (Selector.select(Consumer)).
fn selector_lock_and_do_select(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(v @ Value::Object(Some(_))) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let consumer = args.get(1).copied().unwrap_or(Value::Object(None));
    let timeout = match args.get(2) {
        Some(Value::Long(t)) => *t,
        Some(Value::Int(t)) => *t as i64,
        _ => 0,
    };
    let n = selector_select_native(ctx, &[this, Value::Long(timeout)])?;
    // If a Consumer<SelectionKey> was supplied, drive it across the ready keys.
    if let Value::Object(Some(c)) = consumer {
        if let Value::Object(Some(set)) =
            selector_selected_keys(ctx, &[this])?.unwrap_or(Value::Object(None))
        {
            // The synthetic HashSet's backing array is at slot 0, size at slot 1.
            let arr = match ctx.get_field(set, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(n),
            };
            let len = match ctx.get_field(set, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            for i in 0..len {
                if let Value::Object(Some(k)) = ctx.get_array_element(arr, i) {
                    let _ = ctx.invoke_virtual(
                        c,
                        "accept",
                        "(Ljava/lang/Object;)V",
                        &[Value::Object(Some(k))],
                    );
                }
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// IOUtil.fdVal — extract raw fd from a FileDescriptor.
//
// JDK pattern: every SocketChannelImpl / FileChannelImpl owns a
// `FileDescriptor` whose internal `fd` int is the OS-level handle.
// Our synthetic FileDescriptor stores the fd at slot 0 (matching the
// existing FileDispatcherImpl pattern in nio_native.rs).
// ---------------------------------------------------------------------------

fn ioutil_fdval_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(fd_obj))) = args.first().copied() else {
        return Ok(Some(Value::Int(-1)));
    };
    if ctx.object_num_fields(fd_obj) == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    match ctx.get_field(fd_obj, 0) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        Value::Long(v) => Ok(Some(Value::Int(v as i32))),
        _ => Ok(Some(Value::Int(-1))),
    }
}

// ---------------------------------------------------------------------------
// Linux-only EPoll natives — JDK uses static methods on sun.nio.ch.EPoll.
// On non-Linux these methods aren't expected at all, so registration is a
// no-op for those targets.
// ---------------------------------------------------------------------------

fn eventfd0_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(ioex(format!(
                "eventfd: {}",
                std::io::Error::last_os_error()
            )));
        }
        return Ok(Some(Value::Int(fd as i32)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Err(ioex("EventFD.eventfd0: not supported on this platform"));
    }
}

fn eventfd_set0_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        let fd = match args.first() {
            Some(Value::Int(v)) => *v as libc::c_int,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let value: u64 = 1;
        let rc = unsafe {
            libc::write(
                fd,
                (&value as *const u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(Some(Value::Int(0)));
            }
            return Err(ioex(format!("eventfd write: {err}")));
        }
        return Ok(Some(Value::Int(0)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        return Err(ioex("EventFD.set0: not supported on this platform"));
    }
}

fn ioutil_drain_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    #[cfg(unix)]
    {
        let fd = match args.first() {
            Some(Value::Int(v)) => *v as libc::c_int,
            _ => return Ok(Some(Value::Int(0))),
        };
        let mut drained = false;
        loop {
            let mut value: u64 = 0;
            let rc = unsafe {
                libc::read(
                    fd,
                    (&mut value as *mut u64).cast::<libc::c_void>(),
                    std::mem::size_of::<u64>(),
                )
            };
            if rc > 0 {
                drained = true;
                continue;
            }
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) => break,
                Some(libc::EINTR) => continue,
                _ => return Err(ioex(format!("IOUtil.drain: {err}"))),
            }
        }
        return Ok(Some(Value::Int(if drained { 1 } else { 0 })));
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        return Ok(Some(Value::Int(0)));
    }
}

fn fd_close_int_fd_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    #[cfg(unix)]
    {
        let fd = match args.first() {
            Some(Value::Int(v)) => *v as libc::c_int,
            _ => return Ok(None),
        };
        if fd >= 0 {
            let rc = unsafe { libc::close(fd) };
            if rc < 0 {
                return Err(ioex(format!(
                    "closeIntFD: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        return Ok(None);
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        return Ok(None);
    }
}

fn epoll_event_size_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        return Ok(Some(Value::Int(
            std::mem::size_of::<libc::epoll_event>() as i32
        )));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Ok(Some(Value::Int(0)));
    }
}

fn epoll_events_offset_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn epoll_data_offset_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        let event = std::mem::MaybeUninit::<libc::epoll_event>::uninit();
        let base = event.as_ptr();
        let offset = unsafe {
            let data = std::ptr::addr_of!((*base).u64);
            data as usize - base as usize
        };
        return Ok(Some(Value::Int(offset as i32)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Ok(Some(Value::Int(0)));
    }
}

fn epoll_create_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: epoll_create1 is a syscall returning an fd or -1.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(ioex(format!(
                "epoll_create1: {}",
                std::io::Error::last_os_error()
            )));
        }
        return Ok(Some(Value::Int(fd as i32)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Err(ioex("EPoll.epollCreate: not supported on this platform"));
    }
}

fn epoll_ctl_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    #[cfg(target_os = "linux")]
    {
        let epfd = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let op = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let fd = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let events = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let mut ev = libc::epoll_event {
            events: events as u32,
            u64: fd as u64,
        };
        // SAFETY: epfd, fd valid integers; ev points to a stack value.
        let rc = unsafe {
            libc::epoll_ctl(
                epfd as libc::c_int,
                op as libc::c_int,
                fd as libc::c_int,
                &mut ev,
            )
        };
        return Ok(Some(Value::Int(rc as i32)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        return Err(ioex("EPoll.epollCtl: not supported on this platform"));
    }
}

fn epoll_wait_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // JDK signature: static native int epollWait(int epfd, long pollAddress,
    // int numfds, int timeout). The `pollAddress` field points into a
    // direct-buffer region the JDK pre-allocates for kernel-side event
    // delivery — we have no way to safely write into a Java-side direct
    // buffer from native context without wiring up unsafe.put_int there.
    // Real-mode JDK code paths use SelectorImpl.select0() above, which
    // operates on our own buffer. This native exists so JDK's EPoll
    // static-init resolves on Linux hosts; returning 0 ("no events") is
    // benign because the caller polls again with the next interest set.
    #[cfg(target_os = "linux")]
    {
        return Ok(Some(Value::Int(0)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Err(ioex("EPoll.epollWait: not supported on this platform"));
    }
}

fn netty_epoll_unavailable_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Err(RuntimeError::UnsatisfiedLinkError {
        message: "io.netty.channel.epoll.Native is not supported by CratonVM; use JDK NIO selector"
            .to_string(),
    }
    .into())
}

fn netty_epoll_register_unix_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn netty_epoll_false_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn netty_unix_socket_false_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

#[cfg(target_os = "linux")]
fn epollin_const() -> i32 {
    libc::EPOLLIN
}

#[cfg(not(target_os = "linux"))]
fn epollin_const() -> i32 {
    0x001
}

#[cfg(target_os = "linux")]
fn epollout_const() -> i32 {
    libc::EPOLLOUT
}

#[cfg(not(target_os = "linux"))]
fn epollout_const() -> i32 {
    0x004
}

#[cfg(target_os = "linux")]
fn epollrdhup_const() -> i32 {
    libc::EPOLLRDHUP
}

#[cfg(not(target_os = "linux"))]
fn epollrdhup_const() -> i32 {
    0x2000
}

#[cfg(target_os = "linux")]
fn epollet_const() -> i32 {
    libc::EPOLLET
}

#[cfg(not(target_os = "linux"))]
fn epollet_const() -> i32 {
    0x8000_0000u32 as i32
}

#[cfg(target_os = "linux")]
fn epollerr_const() -> i32 {
    libc::EPOLLERR
}

#[cfg(not(target_os = "linux"))]
fn epollerr_const() -> i32 {
    0x008
}

fn netty_epoll_const_epollin(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(epollin_const())))
}

fn netty_epoll_const_epollout(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(epollout_const())))
}

fn netty_epoll_const_epollrdhup(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(epollrdhup_const())))
}

fn netty_epoll_const_epollet(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(epollet_const())))
}

fn netty_epoll_const_epollerr(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(epollerr_const())))
}

fn netty_epoll_kernel_version(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = ctx.create_string_uninterned("0.0.0-cratonvm");
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// Windows-only WindowsSelectorImpl.SubSelector.poll0 — defer to the
// process-wide selector implementation by mapping the supplied fd-array
// into a transient selector. Most JDK code paths use SelectorImpl.select0
// (registered above) directly; poll0 exists so the static-init path of
// WindowsSelectorImpl resolves cleanly when its constructor probes for it.
// ---------------------------------------------------------------------------

fn windows_subselector_poll0_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Returning 0 indicates "no descriptors became ready" — the JDK's
    // WindowsSelectorImpl wraps this and falls back to the wakeup-socket
    // round-trip we already drive via select0. Real-mode WildFly uses
    // SelectorProvider -> our SelectorImpl path, not this.
    Ok(Some(Value::Int(0)))
}

fn windows_set_wakeup_socket0_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn windows_reset_wakeup_socket0_native(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all `Selector` / `SelectionKey` / `SelectableChannel` natives
/// at the JDK's canonical FQN strings. This is the single entry point
/// `lib.rs` calls at boot.
pub fn register_nio_selector_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sel = "sun/nio/ch/SelectorImpl";
    r.register(
        "java/nio/channels/Selector",
        "open",
        "()Ljava/nio/channels/Selector;",
        selector_open_native,
    );
    r.register(
        sel,
        "open0",
        "()Lsun/nio/ch/SelectorImpl;",
        selector_open_native,
    );
    // JDK 21+ on Windows defaults to `sun.nio.ch.WEPollSelectorProvider`, whose
    // `openSelector()` builds a `WEPollSelectorImpl` backed by the native
    // `sun.nio.ch.WEPoll` (a wepoll/epoll-emulation layer) that CratonVM does
    // not implement. Crucially, Netty's `NioEventLoop.openSelector()` calls
    // `provider.openSelector()` DIRECTLY on its cached provider instance — it
    // does NOT go through `java.nio.channels.Selector.open()` (handled above) —
    // so without this the real `WEPollSelectorImpl.<init>` runs and dies with
    // `UnsatisfiedLinkError: sun/nio/ch/WEPoll.eventSize()I`, taking the whole
    // Vert.x/Netty event-loop group (and thus the Quarkus/Keycloak HTTP server)
    // down. Route the provider's `openSelector()` to our own `SelectorImpl`
    // (a `Selector`/`AbstractSelector`), bypassing the WEPoll path entirely.
    // `selector_open_native` ignores its receiver arg, so the instance form is
    // safe. (The matching `WEPollSelectorImpl` natives are never reached.)
    for prov in [
        "sun/nio/ch/WEPollSelectorProvider",
        "sun/nio/ch/EPollSelectorProvider",
        "sun/nio/ch/SelectorProviderImpl",
    ] {
        r.register(
            prov,
            "openSelector",
            "()Ljava/nio/channels/spi/AbstractSelector;",
            selector_open_native,
        );
    }
    r.register(sel, "close0", "()V", selector_close_native);
    r.register(sel, "wakeup0", "()V", selector_wakeup_native);
    r.register(sel, "select0", "(J)I", selector_select_native);
    r.register(sel, "selectNow0", "()I", selector_select_now_native);

    // SelectableChannel.register.
    r.register(
        "java/nio/channels/SelectableChannel",
        "register0",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        channel_register_native,
    );
    r.register(
        "java/nio/channels/SelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        channel_register_native,
    );
    // Native dispatch (WP0.1) keys on the receiver's concrete class, and the
    // public `register` methods live on `AbstractSelectableChannel` whose real
    // bytecode touches uninitialized `regLock`/`keyLock` + abstract `validOps`.
    // Tomcat's Poller calls the 3-arg `register(sel, OP_READ, wrapper)` on the
    // accepted `SocketChannel`, so override both arities on every concrete
    // channel class our synthetic factories return.
    // `keyFor(Selector)` is `final` on AbstractSelectableChannel; its real
    // bytecode locks the (null-on-CratonVM) `keyLock` field → NPE that kills the
    // NioEndpoint poller (DF01). Override it on every concrete channel class
    // (native dispatch keys on the concrete receiver class) plus the abstract
    // bases for completeness.
    for c in [
        "java/nio/channels/spi/AbstractSelectableChannel",
        "java/nio/channels/SelectableChannel",
        "java/nio/channels/SocketChannel",
        "sun/nio/ch/SocketChannelImpl",
        "java/nio/channels/ServerSocketChannel",
        "sun/nio/ch/ServerSocketChannelImpl",
        "java/nio/channels/DatagramChannel",
        "sun/nio/ch/DatagramChannelImpl",
    ] {
        r.register(
            c,
            "register",
            "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
            channel_register_native,
        );
        r.register(
            c,
            "register",
            "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
            channel_register_native,
        );
        r.register(
            c,
            "keyFor",
            "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
            channel_key_for_native,
        );
    }

    // SelectionKeyImpl.
    let ski = "sun/nio/ch/SelectionKeyImpl";
    r.register(ski, "interestOps0", "(I)V", key_set_interest_ops_native);
    r.register(ski, "cancel0", "()V", key_cancel_native);

    // SelectionKey accessors. The real-JDK abstract methods on
    // java.nio.channels.SelectionKey have no Code attribute, and the
    // concrete sun.nio.ch.SelectionKeyImpl bytecode reads internal
    // state we don't initialize via <init>. Register native overrides
    // that read out of our 5-field SelectionKeyImpl synthetic.
    let sk_class = "java/nio/channels/SelectionKey";
    for c in [sk_class, ski] {
        r.register(
            c,
            "channel",
            "()Ljava/nio/channels/SelectableChannel;",
            sk_channel,
        );
        r.register(c, "selector", "()Ljava/nio/channels/Selector;", sk_selector);
        r.register(c, "interestOps", "()I", sk_interest_ops);
        r.register(
            c,
            "interestOps",
            "(I)Ljava/nio/channels/SelectionKey;",
            sk_set_interest_ops,
        );
        r.register(c, "readyOps", "()I", sk_ready_ops);
        r.register(c, "isValid", "()Z", sk_is_valid);
        r.register(
            c,
            "attach",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            sk_attach,
        );
        r.register(c, "attachment", "()Ljava/lang/Object;", sk_attachment);
        r.register(c, "cancel", "()V", sk_cancel_public);
    }

    // Selector.selectedKeys / Selector.keys — bytecode in SelectorImpl
    // walks internal HashMaps populated by registerImpl/processFdSet,
    // neither of which our shim wires up. Synthesize the result Set
    // directly from our native KeyState registry.
    let sel_iface = "java/nio/channels/Selector";
    for c in [sel_iface, sel] {
        r.register(
            c,
            "selectedKeys",
            "()Ljava/util/Set;",
            selector_selected_keys,
        );
        r.register(c, "keys", "()Ljava/util/Set;", selector_keys);
    }
    // Public Selector entry points — short-circuit the JDK bytecode (which
    // marshals through SelectorImpl.lockAndDoSelect / processFdSet against
    // internal HashMaps that we don't initialise).
    for c in [sel_iface, sel] {
        r.register(c, "select", "()I", selector_select_blocking);
        r.register(c, "select", "(J)I", selector_select_native);
        r.register(c, "selectNow", "()I", selector_select_now_native);
        r.register(
            c,
            "wakeup",
            "()Ljava/nio/channels/Selector;",
            selector_wakeup_public,
        );
        r.register(c, "close", "()V", selector_close_native);
        r.register(c, "isOpen", "()Z", selector_is_open_native);
        // SelectorImpl.lockAndDoSelect bypass: route directly to our select.
        r.register(
            c,
            "lockAndDoSelect",
            "(Ljava/util/function/Consumer;J)I",
            selector_lock_and_do_select,
        );
    }

    // IOUtil.fdVal — used by every SocketChannelImpl to extract the raw
    // OS fd from a FileDescriptor before passing it to native ops.
    r.register(
        "sun/nio/ch/IOUtil",
        "fdVal",
        "(Ljava/io/FileDescriptor;)I",
        ioutil_fdval_native,
    );

    r.register("sun/nio/ch/EventFD", "eventfd0", "()I", eventfd0_native);
    r.register("sun/nio/ch/EventFD", "set0", "(I)I", eventfd_set0_native);
    r.register("sun/nio/ch/IOUtil", "drain", "(I)Z", ioutil_drain_native);
    r.register(
        "sun/nio/ch/FileDispatcherImpl",
        "closeIntFD",
        "(I)V",
        fd_close_int_fd_native,
    );

    // Linux: sun.nio.ch.EPoll static natives. Registered unconditionally
    // (so JDK static-init resolves on any host) but only the Linux
    // implementation actually performs the syscall; on Windows / macOS
    // the methods return an IOException, which matches the JDK's behavior
    // when EPoll is unavailable.
    r.register(
        "sun/nio/ch/EPoll",
        "eventSize",
        "()I",
        epoll_event_size_native,
    );
    r.register(
        "sun/nio/ch/EPoll",
        "eventsOffset",
        "()I",
        epoll_events_offset_native,
    );
    r.register(
        "sun/nio/ch/EPoll",
        "dataOffset",
        "()I",
        epoll_data_offset_native,
    );
    r.register(
        "sun/nio/ch/EPoll",
        "epollCreate",
        "()I",
        epoll_create_native,
    );
    r.register("sun/nio/ch/EPoll", "create", "()I", epoll_create_native);
    r.register("sun/nio/ch/EPoll", "epollCtl", "(IIII)I", epoll_ctl_native);
    r.register("sun/nio/ch/EPoll", "ctl", "(IIII)I", epoll_ctl_native);
    r.register(
        "sun/nio/ch/EPoll",
        "epollWait",
        "(IJII)I",
        epoll_wait_native,
    );
    r.register("sun/nio/ch/EPoll", "wait", "(IJII)I", epoll_wait_native);

    // Netty ships a separate JNI epoll transport (`io.netty.channel.epoll.Native`).
    // CratonVM supports the JDK's selector-facing EPoll surface above, but not
    // Netty's full native transport ABI. Make Netty's availability probe fail
    // explicitly so Reactor/Netty falls back to the JDK NIO selector path instead
    // of half-initializing native epoll and then dropping event-loop tasks.
    let netty_epoll = "io/netty/channel/epoll/Native";
    r.register(
        netty_epoll,
        "registerUnix",
        "()I",
        netty_epoll_register_unix_native,
    );
    r.register(
        netty_epoll,
        "sizeofEpollEvent",
        "()I",
        epoll_event_size_native,
    );
    r.register(
        netty_epoll,
        "offsetofEpollData",
        "()I",
        epoll_data_offset_native,
    );
    r.register(
        netty_epoll,
        "isSupportingUdpSegment",
        "()Z",
        netty_epoll_false_native,
    );
    for (name, sig) in [
        ("epollCreate", "()I"),
        ("eventFd", "()I"),
        ("timerFd", "()I"),
    ] {
        r.register(netty_epoll, name, sig, netty_epoll_unavailable_native);
    }

    // Netty's shared Unix helper initializes even when the native epoll transport
    // is unavailable. Keep its static IPv6 probes harmless so the NIO transport
    // can continue to bootstrap on top of CratonVM's JDK channel shims.
    let netty_unix_socket = "io/netty/channel/unix/Socket";
    r.register(
        netty_unix_socket,
        "isIPv6Preferred0",
        "(Z)Z",
        netty_unix_socket_false_native,
    );
    r.register(
        netty_unix_socket,
        "isIPv6",
        "(I)Z",
        netty_unix_socket_false_native,
    );

    let netty_epoll_static = "io/netty/channel/epoll/NativeStaticallyReferencedJniMethods";
    r.register(
        netty_epoll_static,
        "epollin",
        "()I",
        netty_epoll_const_epollin,
    );
    r.register(
        netty_epoll_static,
        "epollout",
        "()I",
        netty_epoll_const_epollout,
    );
    r.register(
        netty_epoll_static,
        "epollrdhup",
        "()I",
        netty_epoll_const_epollrdhup,
    );
    r.register(
        netty_epoll_static,
        "epollet",
        "()I",
        netty_epoll_const_epollet,
    );
    r.register(
        netty_epoll_static,
        "epollerr",
        "()I",
        netty_epoll_const_epollerr,
    );
    r.register(
        netty_epoll_static,
        "tcpMd5SigMaxKeyLen",
        "()I",
        netty_epoll_false_native,
    );
    r.register(
        netty_epoll_static,
        "isSupportingSendmmsg",
        "()Z",
        netty_epoll_false_native,
    );
    r.register(
        netty_epoll_static,
        "isSupportingRecvmmsg",
        "()Z",
        netty_epoll_false_native,
    );
    r.register(
        netty_epoll_static,
        "tcpFastopenMode",
        "()I",
        netty_epoll_false_native,
    );
    r.register(
        netty_epoll_static,
        "kernelVersion",
        "()Ljava/lang/String;",
        netty_epoll_kernel_version,
    );

    // Windows: WindowsSelectorImpl.SubSelector internals — registered on
    // every host so static-init resolves.
    r.register(
        "sun/nio/ch/WindowsSelectorImpl$SubSelector",
        "poll0",
        "(JI[I[I[IJ)I",
        windows_subselector_poll0_native,
    );
    r.register(
        "sun/nio/ch/WindowsSelectorImpl",
        "setWakeupSocket0",
        "(II)V",
        windows_set_wakeup_socket0_native,
    );
    r.register(
        "sun/nio/ch/WindowsSelectorImpl",
        "resetWakeupSocket0",
        "(I)V",
        windows_reset_wakeup_socket0_native,
    );
    r.set_category(__prev_cat);
}

/// Backwards-compat shim — the existing wire-up in `lib.rs` calls this
/// name. Keeping it as an alias of `register_nio_selector_real` so the
/// integration step does not have to be touched.
pub fn register_nio_selector(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_nio_selector_real(r);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::thread;

    /// Allocate a unique fake fd for test registration. These are NEVER
    /// handed to the net.rs registry — they're purely selector-local ids.
    fn fake_fd() -> i32 {
        static NEXT: AtomicI32 = AtomicI32::new(0x7000_0001);
        NEXT.fetch_add(1, Ordering::SeqCst)
    }

    /// Create a connected client/server TcpStream pair bound to 127.0.0.1:0.
    /// Returns (client, server).
    fn make_stream_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn t19_7_a_selector_open_returns_valid_selector() {
        let id = selector_open();
        assert!(id > 0);
        assert_eq!(selector_key_count(id), 0);
        selector_close(id);
    }

    #[test]
    fn t19_7_a_selector_close_invalidates_keys() {
        let id = selector_open();
        let fd = fake_fd();
        // C27: signature is now (id, fd, ops, key_obj: Option<ObjectRef>,
        // key_hash: i32, kind). Tests use None + a sentinel hash.
        selector_register(id, fd, OP_READ, None, 0xdead_beef_u32 as i32, None).unwrap();
        assert_eq!(selector_key_count(id), 1);
        selector_close(id);
        assert_eq!(selector_key_count(id), 0);
        // Further select returns ClosedSelectorException.
        let r = selector_select(id, 0);
        assert!(r.is_err(), "select after close must fail");
    }

    #[test]
    fn t19_7_a_channel_register_returns_selection_key_with_interest_ops() {
        let id = selector_open();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = fake_fd();
        selector_register(
            id,
            fd,
            OP_ACCEPT,
            None,
            0xcafe_babe_u32 as i32,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();
        assert_eq!(selector_key_count(id), 1);
        let regs = selectors().read();
        let st = regs.get(&id).unwrap().lock();
        let k = st.keys.get(&fd).unwrap();
        assert_eq!(k.interest_ops, OP_ACCEPT);
        assert_eq!(k.key_hash, 0xcafe_babe_u32 as i32);
        assert!(k.key_obj.is_none());
        drop(st);
        drop(regs);
        selector_close(id);
    }

    #[test]
    fn t19_7_a_select_returns_zero_with_no_ready_channels() {
        let id = selector_open();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = fake_fd();
        selector_register(
            id,
            fd,
            OP_ACCEPT,
            None,
            0,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();
        let r = selector_select(id, 0).unwrap();
        assert_eq!(r, 0);
        selector_close(id);
    }

    #[test]
    fn t19_7_a_select_now_non_blocking() {
        let id = selector_open();
        let start = Instant::now();
        let r = selector_select(id, 0).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, 0);
        assert!(elapsed < Duration::from_millis(50), "elapsed={elapsed:?}");
        selector_close(id);
    }

    #[test]
    fn t19_7_a_select_with_timeout_respects_deadline() {
        let id = selector_open();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = fake_fd();
        selector_register(
            id,
            fd,
            OP_ACCEPT,
            None,
            0,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();
        let start = Instant::now();
        let r = selector_select(id, 50).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, 0);
        assert!(
            elapsed >= Duration::from_millis(40),
            "should block ~50ms, got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "should not block >> 50ms, got {elapsed:?}"
        );
        selector_close(id);
    }

    #[test]
    fn t19_7_a_select_returns_ready_read_after_accept() {
        let id = selector_open();
        let (mut client, server) = make_stream_pair();
        let server_fd = fake_fd();
        selector_register(
            id,
            server_fd,
            OP_READ,
            None,
            0,
            Some(SelectableKind::Stream(server)),
        )
        .unwrap();

        client.write_all(b"x").unwrap();
        client.flush().unwrap();
        thread::sleep(Duration::from_millis(20));

        let n = selector_select(id, 500).unwrap();
        assert_eq!(n, 1);

        let regs = selectors().read();
        let st = regs.get(&id).unwrap().lock();
        let k = st.keys.get(&server_fd).unwrap();
        assert_eq!(k.ready_ops & OP_READ, OP_READ);
        drop(st);
        drop(regs);
        selector_close(id);
        drop(client);
    }

    #[test]
    fn t19_7_a_wakeup_unblocks_concurrent_select() {
        let id = selector_open();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = fake_fd();
        selector_register(
            id,
            fd,
            OP_ACCEPT,
            None,
            0,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();

        let handle = thread::spawn(move || {
            let start = Instant::now();
            let r = selector_select(id, 10_000).unwrap();
            (r, start.elapsed())
        });

        thread::sleep(Duration::from_millis(50));
        selector_wakeup(id).unwrap();

        let (r, elapsed) = handle.join().unwrap();
        assert_eq!(r, 0);
        assert!(
            elapsed < Duration::from_millis(2_000),
            "wakeup should unblock within 2s, got {elapsed:?}"
        );
        selector_close(id);
    }

    #[test]
    fn t19_7_a_interest_ops_change_takes_effect_next_select() {
        let id = selector_open();
        let (mut client, server) = make_stream_pair();
        let server_fd = fake_fd();
        selector_register(
            id,
            server_fd,
            0,
            None,
            0,
            Some(SelectableKind::Stream(server)),
        )
        .unwrap();
        client.write_all(b"y").unwrap();
        thread::sleep(Duration::from_millis(20));
        let n = selector_select(id, 0).unwrap();
        assert_eq!(n, 0, "zero interest produces no ready keys");

        selector_set_interest(id, server_fd, OP_READ).unwrap();
        let n = selector_select(id, 500).unwrap();
        assert_eq!(n, 1);
        selector_close(id);
        drop(client);
    }

    #[test]
    fn t19_7_a_key_cancel_removes_from_next_select() {
        let id = selector_open();
        let (mut client, server) = make_stream_pair();
        let server_fd = fake_fd();
        selector_register(
            id,
            server_fd,
            OP_READ,
            None,
            0,
            Some(SelectableKind::Stream(server)),
        )
        .unwrap();
        assert_eq!(selector_key_count(id), 1);
        selector_cancel(id, server_fd);
        client.write_all(b"z").unwrap();
        thread::sleep(Duration::from_millis(20));
        let _ = selector_select(id, 0).unwrap();
        assert_eq!(selector_key_count(id), 0, "cancelled key pruned");
        selector_close(id);
        drop(client);
    }

    #[test]
    fn t19_7_a_negative_timeout_throws_illegal_argument() {
        let id = selector_open();
        let r = selector_select(id, -5);
        assert!(r.is_err(), "negative timeout must fail");
        match r {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message: _ },
            ))) => {}
            _ => panic!("expected IllegalArgumentException, got {r:?}"),
        }
        selector_close(id);
    }

    #[test]
    fn t19_7_a_wakeup_before_select_returns_immediately() {
        let id = selector_open();
        selector_wakeup(id).unwrap();
        let start = Instant::now();
        let r = selector_select(id, 1_000).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, 0);
        assert!(
            elapsed < Duration::from_millis(500),
            "pre-wakeup should short-circuit, got {elapsed:?}"
        );
        selector_close(id);
    }

    #[test]
    fn nio_selector_indefinite_block_path_honors_wakeup() {
        // BUGFIX [nio-selector] regression: the public `Selector.select(0)`
        // overload maps argument 0 to *block indefinitely* (i64::MAX) in
        // `selector_select_native`. The low-level `selector_select` proves
        // that the indefinite-block path (timeout == i64::MAX → timeout_c -1)
        // is reachable and is correctly short-circuited by a pre-existing
        // wakeup — i.e. it blocks until wakeup rather than busy-spinning, the
        // exact property `select(0)` event loops rely on. (We can't call
        // `selector_select(id, i64::MAX)` without a prior wakeup here because
        // it would block the test forever — which is the whole point of the
        // fix.)
        let id = selector_open();
        selector_wakeup(id).unwrap();
        let start = Instant::now();
        let r = selector_select(id, i64::MAX).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, 0);
        assert!(
            elapsed < Duration::from_millis(500),
            "indefinite block must short-circuit on a pending wakeup, got {elapsed:?}"
        );
        selector_close(id);
    }

    #[test]
    fn t19_7_a_op_constants_match_jdk() {
        assert_eq!(OP_READ, 1);
        assert_eq!(OP_WRITE, 4);
        assert_eq!(OP_CONNECT, 8);
        assert_eq!(OP_ACCEPT, 16);
    }

    #[test]
    fn t19_7_a_take_pending_accepted_returns_stream_after_select() {
        let id = selector_open();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fd = fake_fd();
        selector_register(
            id,
            fd,
            OP_ACCEPT,
            None,
            0,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();

        // Connect a client — select should mark it ready.
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        thread::sleep(Duration::from_millis(20));
        let n = selector_select(id, 500).unwrap();
        assert_eq!(n, 1);
        let accepted = take_pending_accepted(id, fd);
        assert!(accepted.is_some(), "accepted stream should be available");
        selector_close(id);
        drop(client);
    }

    #[test]
    fn t19_7_a_dummy_handle_never_ready() {
        // A selector key with no live handle never reports ready.
        let id = selector_open();
        let fd = fake_fd();
        selector_register(id, fd, OP_READ | OP_WRITE | OP_ACCEPT, None, 0, None).unwrap();
        let n = selector_select(id, 0).unwrap();
        assert_eq!(n, 0, "dummy handle never reports ready");
        selector_close(id);
    }

    #[test]
    fn wp3_1_kernel_select_handles_many_concurrent_streams() {
        // Sanity check: the kernel-backed selector can multiplex N streams
        // and waking one of them unblocks select with the right key.
        let id = selector_open();
        let mut clients = Vec::new();
        let mut server_fds = Vec::new();
        for _ in 0..8 {
            let (client, server) = make_stream_pair();
            let fd = fake_fd();
            server_fds.push(fd);
            clients.push(client);
            selector_register(
                id,
                fd,
                OP_READ,
                None,
                0,
                Some(SelectableKind::Stream(server)),
            )
            .unwrap();
        }
        // Write to one client.
        clients[3].write_all(b"hello").unwrap();
        clients[3].flush().unwrap();
        thread::sleep(Duration::from_millis(20));
        let n = selector_select(id, 1_000).unwrap();
        assert!(n >= 1, "expected at least one ready key, got {n}");
        selector_close(id);
    }

    #[test]
    fn wp3_1_register_real_entry_point_compiles() {
        // Confirm the public registration surface exists.
        let mut reg = cratonvm_native_api::NativeMethodRegistry::new();
        register_nio_selector_real(&mut reg);
        // No assertion on internal counts — just that it runs without panic.
    }

    /// Build a fake 8-byte-aligned `ObjectRef` from a small ordinal. Never
    /// dereferenced — `gc_scan_selector_roots` only compares pointer
    /// identity, so a synthetic non-heap pointer is sufficient here.
    fn fake_ref(ordinal: usize) -> ObjectRef {
        // 8-byte aligned, non-null, well clear of the low page.
        let ptr = ((ordinal + 1) * 8 + 0x1_0000) as *mut u8;
        // SAFETY: non-null and 8-byte aligned; only used for identity, the
        // pointer is never read through.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    #[test]
    fn gc_scan_selector_roots_collects_sk_table_and_key_obj_refs() {
        // Seed a `SkState` carrying channel + selector + attachment, plus a
        // per-selector `key_obj`, then prove the root scan reports every one
        // — i.e. exactly the refs `sk_table_update_after_gc` remaps.
        let channel = fake_ref(0x5100);
        let selector = fake_ref(0x5200);
        let attachment = fake_ref(0x5300);
        let key_obj = fake_ref(0x5400);

        // Unique key so the row is isolated from any other test's entries in
        // the process-wide table.
        let sk_key = 0x7E57_0001_u32 as i32;
        {
            let mut table = sk_table().write();
            table.entry(sk_key).or_default().push(SkState {
                key: fake_ref(0x5500),
                channel,
                selector,
                interest_ops: OP_READ,
                ready_ops: 0,
                attachment: Some(attachment),
                cancelled: false,
            });
        }

        // A selector whose key carries a non-null `key_obj`.
        let id = selector_open();
        let fd = fake_fd();
        selector_register(id, fd, OP_READ, Some(key_obj), 0x7E57_0002_u32 as i32, None).unwrap();

        let mut roots: Vec<ObjectRef> = Vec::new();
        gc_scan_selector_roots(&mut roots);

        let contains = |r: ObjectRef| roots.iter().any(|x| x.as_ptr() == r.as_ptr());
        assert!(contains(channel), "channel must be rooted");
        assert!(contains(selector), "selector must be rooted");
        assert!(contains(attachment), "attachment must be rooted");
        assert!(contains(key_obj), "per-selector key_obj must be rooted");

        // No null roots may ever be injected.
        assert!(
            roots.iter().all(|r| !r.as_ptr().is_null()),
            "scan must never push a null root"
        );

        // Cleanup the process-wide seed so sibling tests are unaffected.
        sk_table().write().remove(&sk_key);
        selector_close(id);
    }

    #[test]
    fn gc_scan_selector_roots_skips_absent_attachment() {
        // An entry with no attachment contributes only channel + selector.
        let channel = fake_ref(0x6100);
        let selector = fake_ref(0x6200);
        let sk_key = 0x7E57_0003_u32 as i32;
        {
            let mut table = sk_table().write();
            table.entry(sk_key).or_default().push(SkState {
                key: fake_ref(0x6500),
                channel,
                selector,
                interest_ops: 0,
                ready_ops: 0,
                attachment: None,
                cancelled: false,
            });
        }

        let mut roots: Vec<ObjectRef> = Vec::new();
        gc_scan_selector_roots(&mut roots);

        let contains = |r: ObjectRef| roots.iter().any(|x| x.as_ptr() == r.as_ptr());
        assert!(contains(channel), "channel must be rooted");
        assert!(contains(selector), "selector must be rooted");

        sk_table().write().remove(&sk_key);
    }
}
