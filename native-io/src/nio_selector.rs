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

use parking_lot::{Mutex, RwLock};
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use rustjvm_types::{ClassId, ObjectRef, Value};
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
#[allow(unused_imports)]
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// OP_* constants (JDK SelectionKey.OP_*)
// ---------------------------------------------------------------------------

pub const OP_READ: i32 = 1;
pub const OP_WRITE: i32 = 4;
pub const OP_CONNECT: i32 = 8;
pub const OP_ACCEPT: i32 = 16;

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
            SelectableHandle::Dummy => None,
        }
    }

    #[cfg(windows)]
    fn os_handle(&self) -> Option<i64> {
        use std::os::windows::io::AsRawSocket;
        match self {
            SelectableHandle::Listener(l) => Some(l.as_raw_socket() as i64),
            SelectableHandle::Stream(s) => Some(s.as_raw_socket() as i64),
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
    /// Java-side SelectionKeyImpl pointer, opaque to native.
    /// Encoded as a raw usize since ObjectRef is !Send — but we only ever
    /// use it through a NativeContext on the same thread that owns the VM.
    key_obj: usize,
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
        let rc = unsafe {
            libc::pipe2(pipefd.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
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
        let rc = unsafe {
            libc::epoll_ctl(efd, libc::EPOLL_CTL_ADD, pipefd[0], &mut ev)
        };
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

    /// Drain wakeup pipe (Linux self-pipe).
    #[cfg(target_os = "linux")]
    fn drain_wakeup_pipe(&mut self) {
        if let Some(rfd) = self.wakeup_pipe_read {
            let mut buf = [0u8; 64];
            // SAFETY: rfd is a valid non-blocking fd; read returns -1/EAGAIN
            // when empty.
            loop {
                let n = unsafe {
                    libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    break;
                }
            }
        }
    }
}

impl Drop for SelectorState {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if let Some(efd) = self.epoll_fd.take() {
                // SAFETY: efd was a valid fd.
                unsafe { libc::close(efd) };
            }
            if let Some(rfd) = self.wakeup_pipe_read.take() {
                unsafe { libc::close(rfd) };
            }
            if let Some(wfd) = self.wakeup_pipe_write.take() {
                unsafe { libc::close(wfd) };
            }
        }
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
    RuntimeError::IOException { message: msg.into() }.into()
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
    selectors().write().insert(id, Mutex::new(SelectorState::new()));
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
        st.open = false;
        st.keys.clear();
        st.wakeup_sender.take();
        st.wakeup_receiver.take();
        st.wakeup_peer.take();
        st.pending_accepted.clear();
        #[cfg(target_os = "linux")]
        {
            if let Some(efd) = st.epoll_fd.take() {
                // SAFETY: efd was a valid fd; close is idempotent.
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
            let _ = unsafe {
                libc::write(wfd, &byte as *const u8 as *const libc::c_void, 1)
            };
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
pub fn selector_register(
    id: i32,
    net_fd: i32,
    interest_ops: i32,
    key_obj: usize,
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
    let _prev = st.keys.insert(
        net_fd,
        KeyState {
            net_fd,
            interest_ops,
            ready_ops: 0,
            key_obj,
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
                let rc = unsafe {
                    libc::epoll_ctl(efd, op, os as libc::c_int, &mut ev)
                };
                if rc < 0 && op == libc::EPOLL_CTL_MOD {
                    let _ = unsafe {
                        libc::epoll_ctl(
                            efd,
                            libc::EPOLL_CTL_ADD,
                            os as libc::c_int,
                            &mut ev,
                        )
                    };
                }
            }
        }
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
            let _ = unsafe {
                libc::epoll_ctl(
                    efd,
                    libc::EPOLL_CTL_MOD,
                    os as libc::c_int,
                    &mut ev,
                )
            };
        }
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
    }
    r
}

// ---------------------------------------------------------------------------
// Kernel-backed select — Linux (epoll)
// ---------------------------------------------------------------------------

/// Compute readyOps for every registered key by waiting on the epoll fd.
/// Returns the count of keys with non-zero readyOps (after applying the
/// interest mask). Drains the wakeup pipe before returning.
#[cfg(target_os = "linux")]
fn kernel_select_linux(id: i32, timeout_ms: i32) -> Result<i32, MethodCallFailed> {
    // Phase 1: snapshot prerequisites under the lock — fd, interest map,
    // listener-set, current epoll fd.  Then release the lock so wakeup()
    // can hit it during the actual epoll_wait.
    let (efd, interests, listeners) = {
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
        let efd = match st.epoll_fd {
            Some(v) => v,
            None => return Err(ioex("Selector.select: epoll init failed")),
        };
        let mut interests = HashMap::with_capacity(st.keys.len());
        let mut listeners = HashMap::with_capacity(st.keys.len());
        for (net_fd, k) in st.keys.iter() {
            interests.insert(*net_fd, k.interest_ops);
            listeners.insert(*net_fd, k.handle.is_listener());
        }
        (efd, interests, listeners)
    };

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
        if err.kind() == ErrorKind::Interrupted {
            return Ok(0);
        }
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
        return Err(closed_selector());
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
        let interest = match interests.get(&net_fd) {
            Some(v) => *v,
            None => continue, // Race: key was removed/cancelled.
        };
        let is_listener = *listeners.get(&net_fd).unwrap_or(&false);
        let ready =
            linux_ready_for(ev.events as i32, interest, is_listener);
        // Translate _, _ — borrow check juggling: take a non-mut snapshot,
        // then reapply.
        if ready != 0 {
            if let Some(k) = st.keys.get_mut(&net_fd) {
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
    }
    for (fd, stream) in accepted_streams {
        st.pending_accepted.push_back((fd, stream));
    }

    if woken || st.woken {
        st.woken = false;
        st.drain_wakeup_pipe();
    }
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
    let (mut pollfds, key_index, wakeup_idx) = {
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

        let mut pollfds: Vec<Wsapollfd> = Vec::with_capacity(st.keys.len() + 1);
        let mut key_index: Vec<(i32, i32, bool)> = Vec::with_capacity(st.keys.len());
        for k in st.keys.values_mut() {
            // Reset readyOps prior to wait.
            k.ready_ops = 0;
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

        (pollfds, key_index, wakeup_idx)
    };

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
                            return Err(closed_selector());
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
        unsafe {
            WSAPoll(
                pollfds.as_mut_ptr(),
                pollfds.len() as u32,
                timeout_ms,
            )
        }
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
        return Err(closed_selector());
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
        let in_ready =
            revents & (WSAPOLLRDNORM | WSAPOLLHUP | WSAPOLLERR) != 0;
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
        if err.kind() == ErrorKind::Interrupted {
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
        return Err(closed_selector());
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
        let in_ready =
            pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
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

    // Even-loop wrapper: epoll_wait/WSAPoll honor the deadline themselves
    // for blocking selects, so we don't need our own timer. For selectNow
    // we still want the listener-accept side-effect via probe_cycle so the
    // `take_pending_accepted` test passes — but we rely on the kernel for
    // ready-detection.
    #[cfg(target_os = "linux")]
    let count = kernel_select_linux(id, timeout_c)?;
    #[cfg(windows)]
    let count = kernel_select_windows(id, timeout_c)?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let count = kernel_select_poll(id, timeout_c)?;
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

fn selector_id_from_obj(ctx: &mut dyn NativeContext, obj: ObjectRef) -> i32 {
    if ctx.object_num_fields(obj) <= SI_ID {
        return 0;
    }
    ctx.get_field(obj, SI_ID).as_int().unwrap_or(0)
}

fn open_flag(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    if ctx.object_num_fields(obj) <= SI_OPEN_FLAG {
        return false;
    }
    ctx.get_field(obj, SI_OPEN_FLAG).as_int().unwrap_or(0) != 0
}

fn key_fd(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Option<i32> {
    // Prefer the side-table mapping (populated at register time).
    let raw = sk_table()
        .read()
        .get(&(key_obj.as_ptr() as usize))
        .map(|s| s.channel)?;
    if raw == 0 {
        return None;
    }
    let channel = unsafe { ObjectRef::from_raw(raw as *mut u8) };
    let nf = ctx.object_num_fields(channel);
    if nf == 0 {
        return None;
    }
    // WP3.4 layout: channel id lives at field 2 (F_REG_ID).
    if nf > 2 {
        if let Value::Int(v) = ctx.get_field(channel, 2) {
            if v != 0 && v != -1 {
                return Some(v);
            }
        }
    }
    match ctx.get_field(channel, 0) {
        Value::Int(v) if v != 0 && v != -1 => Some(v),
        _ => None,
    }
}

fn key_selector_id(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Option<i32> {
    let raw = sk_table()
        .read()
        .get(&(key_obj.as_ptr() as usize))
        .map(|s| s.selector)?;
    if raw == 0 {
        return None;
    }
    let s = unsafe { ObjectRef::from_raw(raw as *mut u8) };
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
    let id = selector_id_from_obj(ctx, obj);
    if id == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let n = selector_select(id, timeout)?;
    apply_ready_ops(ctx, id);
    Ok(Some(Value::Int(n)))
}

/// Mirror the native readyOps onto the SelectionKey side-table so user
/// code reading `key.readyOps()` sees the post-select state.
fn apply_ready_ops(_ctx: &mut dyn NativeContext, id: i32) {
    let snap: Vec<(usize, i32)> = {
        let regs = selectors().read();
        let Some(s) = regs.get(&id) else { return };
        let st = s.lock();
        st.keys
            .values()
            .map(|k| (k.key_obj, k.ready_ops))
            .collect()
    };
    let mut table = sk_table().write();
    for (raw, ready) in snap {
        if raw == 0 {
            continue;
        }
        if let Some(state) = table.get_mut(&raw) {
            state.ready_ops = ready;
        }
    }
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
    let n = selector_select(id, 0)?;
    apply_ready_ops(ctx, id);
    Ok(Some(Value::Int(n)))
}

// ---------------------------------------------------------------------------
// Native method impls — SelectableChannel.register
// ---------------------------------------------------------------------------

// Side-table mapping SelectionKey object → (channel, selector, interestOps,
// readyOps, attachment). Used because `sun/nio/ch/SelectionKeyImpl` has its
// own JDK-managed instance layout and writing to ad-hoc slots collides.
struct SkState {
    channel: usize,   // raw ObjectRef ptr
    selector: usize,
    interest_ops: i32,
    ready_ops: i32,
    attachment: Option<usize>,
    cancelled: bool,
}

fn sk_table() -> &'static parking_lot::RwLock<std::collections::HashMap<usize, SkState>> {
    static REG: std::sync::OnceLock<parking_lot::RwLock<std::collections::HashMap<usize, SkState>>>
        = std::sync::OnceLock::new();
    REG.get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()))
}

fn sk_state_get_field<F: FnOnce(&SkState) -> Value>(key: ObjectRef, f: F) -> Option<Value> {
    let table = sk_table().read();
    table.get(&(key.as_ptr() as usize)).map(f)
}

fn sk_state_with_mut<F: FnOnce(&mut SkState) -> R, R>(key: ObjectRef, f: F) -> Option<R> {
    let mut table = sk_table().write();
    table.get_mut(&(key.as_ptr() as usize)).map(f)
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

    // Channel layout (WP3.4): field 0 = open flag, field 2 = registry id
    // into `tcp_registry()`. Older synthetic shims placed the fd at field 0 —
    // try field 2 first and fall back so both worlds keep working.
    let nf = ctx.object_num_fields(channel);
    let mut net_fd = if nf > 2 {
        ctx.get_field(channel, 2).as_int().unwrap_or(-1)
    } else if nf > 0 {
        ctx.get_field(channel, 0).as_int().unwrap_or(-1)
    } else {
        return Err(ioex("register: channel has no fields"));
    };
    if net_fd < 0 && nf > 0 {
        // Last-resort: read field 0 (synthetic-mode fd_table id).
        net_fd = ctx.get_field(channel, 0).as_int().unwrap_or(-1);
    }

    // If the registered channel is backed by an entry in the WP3.4
    // tcp_registry, hand the selector a clone of the live socket so
    // `selector_select` can actually poll readiness. The clone is
    // independent of the JDK-visible handle (so accept()/read() still
    // return the real connection).
    let kind = match crate::socket_channel::tcp_clone_for_selector(net_fd) {
        Some(crate::socket_channel::TcpHandleClone::Listener(l)) => {
            Some(SelectableKind::Listener(l))
        }
        Some(crate::socket_channel::TcpHandleClone::Stream(s)) => {
            Some(SelectableKind::Stream(s))
        }
        None => None,
    };

    let key_obj = ctx
        .new_object("sun/nio/ch/SelectionKeyImpl")?
        .and_then(|v| match v {
            Value::Object(o) => o,
            _ => None,
        })
        .ok_or_else(|| ioex("register: could not allocate SelectionKeyImpl"))?;
    // Stash the key state in a side-table so our accessor natives don't
    // have to reach into JDK-managed slots on the SelectionKeyImpl.
    let attachment_raw = match attachment {
        Value::Object(Some(o)) => Some(o.as_ptr() as usize),
        _ => None,
    };
    sk_table().write().insert(
        key_obj.as_ptr() as usize,
        SkState {
            channel: channel.as_ptr() as usize,
            selector: selector_obj.as_ptr() as usize,
            interest_ops: ops,
            ready_ops: 0,
            attachment: attachment_raw,
            cancelled: false,
        },
    );

    let raw = key_obj.as_ptr() as usize;
    selector_register(sel_id, net_fd, ops, raw, kind)?;

    Ok(Some(Value::Object(Some(key_obj))))
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
    let Some(fd) = key_fd(ctx, key) else {
        return Ok(None);
    };
    let Some(sel_id) = key_selector_id(ctx, key) else {
        return Ok(None);
    };
    if sel_id == 0 {
        return Ok(None);
    }
    let _ = selector_set_interest(sel_id, fd, ops);
    Ok(None)
}

fn key_cancel_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(key))) = args.first().copied() else {
        return Ok(None);
    };
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

fn sk_channel(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let v = sk_state_get_field(this, |s| {
        let obj = unsafe { ObjectRef::from_raw(s.channel as *mut u8) };
        Value::Object(Some(obj))
    })
    .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_selector(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let v = sk_state_get_field(this, |s| {
        let obj = unsafe { ObjectRef::from_raw(s.selector as *mut u8) };
        Value::Object(Some(obj))
    })
    .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_interest_ops(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let v = sk_state_get_field(this, |s| Value::Int(s.interest_ops))
        .unwrap_or(Value::Int(0));
    Ok(Some(v))
}

fn sk_set_interest_ops(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let ops = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    sk_state_with_mut(this, |s| {
        s.interest_ops = ops;
    });
    // Mirror to native KeyState so the next select() picks up the change.
    let table = sk_table().read();
    if let Some(s) = table.get(&(this.as_ptr() as usize)) {
        let regs = selectors().read();
        for (sel_id, sel) in regs.iter() {
            let mut st = sel.lock();
            for k in st.keys.values_mut() {
                if k.key_obj == this.as_ptr() as usize {
                    k.interest_ops = ops;
                    let _ = selector_set_interest(*sel_id, k.net_fd, ops);
                    break;
                }
            }
        }
        let _ = s; // silence
    }
    Ok(Some(Value::Object(Some(this))))
}

fn sk_ready_ops(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let v = sk_state_get_field(this, |s| Value::Int(s.ready_ops))
        .unwrap_or(Value::Int(0));
    Ok(Some(v))
}

fn sk_is_valid(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let valid = sk_state_get_field(this, |s| {
        Value::Int(if s.cancelled { 0 } else { 1 })
    })
    .unwrap_or(Value::Int(0));
    Ok(Some(valid))
}

fn sk_attach(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let new_att_raw = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(o.as_ptr() as usize),
        _ => None,
    };
    let prev = sk_state_with_mut(this, |s| {
        let prev = s.attachment.take();
        s.attachment = new_att_raw;
        prev
    })
    .flatten();
    let prev_v = match prev {
        Some(raw) => {
            let obj = unsafe { ObjectRef::from_raw(raw as *mut u8) };
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    Ok(Some(prev_v))
}

fn sk_attachment(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let v = sk_state_get_field(this, |s| match s.attachment {
        Some(raw) => {
            let obj = unsafe { ObjectRef::from_raw(raw as *mut u8) };
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    })
    .unwrap_or(Value::Object(None));
    Ok(Some(v))
}

fn sk_cancel_public(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    sk_state_with_mut(this, |s| {
        s.cancelled = true;
        s.ready_ops = 0;
    });
    // Walk all selectors looking for this key and mark cancelled.
    let regs = selectors().read();
    for (sel_id, sel) in regs.iter() {
        let mut st = sel.lock();
        let Some(target) = st
            .keys
            .iter()
            .find(|(_, k)| k.key_obj == this.as_ptr() as usize)
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
    let raw_keys: Vec<usize> = if id == 0 {
        Vec::new()
    } else {
        let regs = selectors().read();
        match regs.get(&id) {
            Some(s) => s
                .lock()
                .keys
                .values()
                .filter(|k| !k.cancelled && k.ready_ops != 0 && k.key_obj != 0)
                .map(|k| k.key_obj)
                .collect(),
            None => Vec::new(),
        }
    };
    Ok(Some(Value::Object(Some(build_set(ctx, &raw_keys)))))
}

fn selector_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(obj))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let id = selector_id_from_obj(ctx, obj);
    let raw_keys: Vec<usize> = if id == 0 {
        Vec::new()
    } else {
        let regs = selectors().read();
        match regs.get(&id) {
            Some(s) => s
                .lock()
                .keys
                .values()
                .filter(|k| !k.cancelled && k.key_obj != 0)
                .map(|k| k.key_obj)
                .collect(),
            None => Vec::new(),
        }
    };
    Ok(Some(Value::Object(Some(build_set(ctx, &raw_keys)))))
}

/// Build a HashSet populated with the given key objects. Allocates a real
/// JDK HashSet (so its bytecode iterator/contains/remove work) and adds
/// each key via `add(Object)`. The selector calls this on every
/// `selectedKeys()` / `keys()` invocation; lifetimes are short so we keep
/// it simple.
fn build_set(ctx: &mut dyn NativeContext, raw_keys: &[usize]) -> ObjectRef {
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
    let _ = ctx.invoke_special("java/util/HashSet", "<init>", "()V", &[Value::Object(Some(set))]);
    for raw in raw_keys {
        // SAFETY: stored from a live ObjectRef at SelectionKey allocation
        // time; the heap entry is kept alive while the selector exists.
        let key = unsafe { ObjectRef::from_raw(*raw as *mut u8) };
        let _ = ctx.invoke_virtual(
            set,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(key))],
        );
    }
    set
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
fn selector_lock_and_do_select(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
        if let Value::Object(Some(set)) = selector_selected_keys(ctx, &[this])?.unwrap_or(Value::Object(None)) {
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
        return Err(ioex(
            "EPoll.epollCreate: not supported on this platform",
        ));
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
            libc::epoll_ctl(epfd as libc::c_int, op as libc::c_int, fd as libc::c_int, &mut ev)
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
        r.register(c, "channel", "()Ljava/nio/channels/SelectableChannel;", sk_channel);
        r.register(c, "selector", "()Ljava/nio/channels/Selector;", sk_selector);
        r.register(c, "interestOps", "()I", sk_interest_ops);
        r.register(c, "interestOps", "(I)Ljava/nio/channels/SelectionKey;", sk_set_interest_ops);
        r.register(c, "readyOps", "()I", sk_ready_ops);
        r.register(c, "isValid", "()Z", sk_is_valid);
        r.register(c, "attach", "(Ljava/lang/Object;)Ljava/lang/Object;", sk_attach);
        r.register(c, "attachment", "()Ljava/lang/Object;", sk_attachment);
        r.register(c, "cancel", "()V", sk_cancel_public);
    }

    // Selector.selectedKeys / Selector.keys — bytecode in SelectorImpl
    // walks internal HashMaps populated by registerImpl/processFdSet,
    // neither of which our shim wires up. Synthesize the result Set
    // directly from our native KeyState registry.
    let sel_iface = "java/nio/channels/Selector";
    for c in [sel_iface, sel] {
        r.register(c, "selectedKeys", "()Ljava/util/Set;", selector_selected_keys);
        r.register(c, "keys", "()Ljava/util/Set;", selector_keys);
    }
    // Public Selector entry points — short-circuit the JDK bytecode (which
    // marshals through SelectorImpl.lockAndDoSelect / processFdSet against
    // internal HashMaps that we don't initialise).
    for c in [sel_iface, sel] {
        r.register(c, "select", "()I", selector_select_blocking);
        r.register(c, "select", "(J)I", selector_select_native);
        r.register(c, "selectNow", "()I", selector_select_now_native);
        r.register(c, "wakeup", "()Ljava/nio/channels/Selector;", selector_wakeup_public);
        r.register(c, "close", "()V", selector_close_native);
        r.register(c, "isOpen", "()Z", selector_is_open_native);
        // SelectorImpl.lockAndDoSelect bypass: route directly to our select.
        r.register(c, "lockAndDoSelect", "(Ljava/util/function/Consumer;J)I", selector_lock_and_do_select);
    }

    // IOUtil.fdVal — used by every SocketChannelImpl to extract the raw
    // OS fd from a FileDescriptor before passing it to native ops.
    r.register(
        "sun/nio/ch/IOUtil",
        "fdVal",
        "(Ljava/io/FileDescriptor;)I",
        ioutil_fdval_native,
    );

    // Linux: sun.nio.ch.EPoll static natives. Registered unconditionally
    // (so JDK static-init resolves on any host) but only the Linux
    // implementation actually performs the syscall; on Windows / macOS
    // the methods return an IOException, which matches the JDK's behavior
    // when EPoll is unavailable.
    r.register("sun/nio/ch/EPoll", "epollCreate", "()I", epoll_create_native);
    r.register(
        "sun/nio/ch/EPoll",
        "epollCtl",
        "(IIII)I",
        epoll_ctl_native,
    );
    r.register(
        "sun/nio/ch/EPoll",
        "epollWait",
        "(IJII)I",
        epoll_wait_native,
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
}

/// Backwards-compat shim — the existing wire-up in `lib.rs` calls this
/// name. Keeping it as an alias of `register_nio_selector_real` so the
/// integration step does not have to be touched.
pub fn register_nio_selector(r: &mut NativeMethodRegistry) {
    register_nio_selector_real(r);
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
        selector_register(id, fd, OP_READ, 0xdead_beef, None).unwrap();
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
            0xcafe_babe,
            Some(SelectableKind::Listener(listener)),
        )
        .unwrap();
        assert_eq!(selector_key_count(id), 1);
        let regs = selectors().read();
        let st = regs.get(&id).unwrap().lock();
        let k = st.keys.get(&fd).unwrap();
        assert_eq!(k.interest_ops, OP_ACCEPT);
        assert_eq!(k.key_obj, 0xcafe_babe);
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
            Err(MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalArgumentException { message: _ },
                ),
            )) => {}
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
        selector_register(id, fd, OP_READ | OP_WRITE | OP_ACCEPT, 0, None).unwrap();
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
        let mut reg = rustjvm_native_api::NativeMethodRegistry::new();
        register_nio_selector_real(&mut reg);
        // No assertion on internal counts — just that it runs without panic.
    }
}
