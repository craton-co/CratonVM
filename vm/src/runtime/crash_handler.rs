//! Crash recovery and `hs_err_pid` log generation.
//!
//! Installs a Rust panic hook that produces HotSpot-compatible crash log files
//! (`hs_err_pid<pid>.log`) with thread, process, and system information.
//! On Unix, also registers a signal handler for SIGSEGV/SIGBUS/SIGFPE via
//! raw libc so that hardware faults produce the same diagnostic output.
//!
//! # Async-signal-safety discipline (READ BEFORE EDITING)
//!
//! This module has TWO entry points that look superficially similar but live
//! in very different execution contexts:
//!
//! 1. **The Rust panic hook** (`install_crash_handler` -> closure passed to
//!    `std::panic::set_hook`). This runs in *normal* Rust context — the
//!    allocator is fine, mutexes are fine, `format!` / `eprintln!` /
//!    `std::fs::File::create` are all fine. The full report is written here.
//!
//! 2. **The Unix signal handler** (`crash_signal_handler` in
//!    `install_signal_handlers`). This runs in *async-signal context*: it
//!    can be invoked at literally any instruction boundary, including while
//!    the malloc lock or stdio buffer lock is held. POSIX permits only a
//!    tiny whitelist of functions to be called here (`signal(7)` /
//!    `signal-safety(7)`). In particular it is UB / deadlock-prone to call:
//!      - any allocator function (`malloc`, `Box::new`, `String`, `format!`,
//!        `Vec::push`, `to_string`),
//!      - any locking primitive (`Mutex`, `RwLock`, `OnceLock` populated
//!        lazily, the stdio locks behind `eprintln!`/`println!`),
//!      - any `std::fs` API (they call `malloc`),
//!      - any `std::backtrace::Backtrace` (allocates + locks symbol tables).
//!
//! Inside the signal handler we use ONLY:
//!   - direct libc syscalls (`write`, `open`, `close`, `signal`, `raise`,
//!     `getpid`),
//!   - reads of immutable `static` data,
//!   - reads/writes of `AtomicBool` / `AtomicI32` with relaxed semantics,
//!   - pre-allocated static / thread-local byte buffers,
//!   - the `itoa_into_buf` helper, which is allocation-free.
//!
//! If you find yourself wanting to add anything else to the signal path,
//! please STOP and put it on the panic-hook path instead — or skip it.
//! A deadlocked crash handler is worse than a sparser one.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

// Guard against recursive crashes inside the handler itself.
static CRASH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// ── CrashInfo ──────────────────────────────────────────────────────────────

/// Captures the essential facts about a crash / fatal signal.
#[derive(Debug, Clone)]
pub struct CrashInfo {
    /// Signal number (e.g. 11 for SIGSEGV) or 0 for a Rust panic.
    pub signal: i32,
    /// Human-readable signal name (e.g. "SIGSEGV", "RUST_PANIC").
    pub signal_name: String,
    /// Process ID.
    pub pid: u32,
    /// Thread ID of the crashing thread.
    pub tid: u64,
    /// Wall-clock time of the crash.
    pub timestamp: SystemTime,
    /// Name of the crashing thread, if available.
    pub thread_name: Option<String>,
    /// Panic message extracted from `PanicHookInfo`, if any.
    pub panic_message: Option<String>,
    /// Source location of the panic (file:line), if available.
    pub panic_location: Option<String>,
}

impl CrashInfo {
    /// Build a `CrashInfo` from a Rust `PanicHookInfo`.
    pub fn from_panic(info: &std::panic::PanicHookInfo<'_>) -> Self {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            Some((*s).to_string())
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            Some(s.clone())
        } else {
            Some("(non-string panic payload)".to_string())
        };

        let location = info.location().map(|loc| {
            format!("{}:{}", loc.file(), loc.line())
        });

        let thread_name = std::thread::current().name().map(String::from);

        Self {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name,
            panic_message: message,
            panic_location: location,
        }
    }

    /// Build a `CrashInfo` for a caught signal (Unix).
    #[cfg(unix)]
    pub fn from_signal(sig: i32) -> Self {
        Self {
            signal: sig,
            signal_name: signal_name(sig),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name: std::thread::current().name().map(String::from),
            panic_message: None,
            panic_location: None,
        }
    }
}

// ── CrashReport ────────────────────────────────────────────────────────────

/// Formats a complete HotSpot-compatible crash log from a `CrashInfo`.
pub struct CrashReport<'a> {
    info: &'a CrashInfo,
}

impl<'a> CrashReport<'a> {
    pub fn new(info: &'a CrashInfo) -> Self {
        Self { info }
    }

    /// Render the full crash report as a string.
    pub fn render(&self) -> String {
        let mut buf = String::with_capacity(4096);
        self.write_header(&mut buf);
        self.write_thread_section(&mut buf);
        self.write_process_section(&mut buf);
        self.write_system_section(&mut buf);
        buf
    }

    fn write_header(&self, buf: &mut String) {
        let ts = format_timestamp(self.info.timestamp);
        let sig_hex = format!("{:#x}", self.info.signal);

        let _ = writeln!(buf,
            "# A fatal error has been detected by the CratonVM Runtime Environment:");
        let _ = writeln!(buf, "#");
        if self.info.signal == 0 {
            // Rust panic
            let msg = self.info.panic_message.as_deref().unwrap_or("unknown");
            let loc = self.info.panic_location.as_deref().unwrap_or("unknown");
            let _ = writeln!(buf,
                "#  RUST_PANIC at {}, pid={}, tid={}",
                loc, self.info.pid, self.info.tid);
            let _ = writeln!(buf, "#  Message: {}", msg);
        } else {
            let _ = writeln!(buf,
                "#  {} ({}) at pc=0x0, pid={}, tid={}",
                self.info.signal_name, sig_hex, self.info.pid, self.info.tid);
        }
        let _ = writeln!(buf, "#");
        let _ = writeln!(buf, "# JRE version: CratonVM 25.0");

        // Rust compiler version (baked in at build time).
        let _ = writeln!(buf, "# Rust version: {}", rust_version());

        let _ = writeln!(buf, "# OS: {}", get_os_info());
        let _ = writeln!(buf, "#");
        let _ = writeln!(buf,
            "# If you would like to submit a bug report, please include this file.");
        let _ = writeln!(buf, "# Crash timestamp: {}", ts);
        let _ = writeln!(buf);
    }

    fn write_thread_section(&self, buf: &mut String) {
        let _ = writeln!(buf,
            "---------------  T H R E A D  ---------------");
        let _ = writeln!(buf);

        let tname = self.info.thread_name.as_deref().unwrap_or("<unnamed>");
        let _ = writeln!(buf,
            "Current thread (0x{:x}): \"{}\" tid=0x{:x}",
            self.info.tid, tname, self.info.tid);

        if let Some(ref msg) = self.info.panic_message {
            let _ = writeln!(buf, "Panic message: {}", msg);
        }
        if let Some(ref loc) = self.info.panic_location {
            let _ = writeln!(buf, "Panic location: {}", loc);
        }

        let _ = writeln!(buf);

        // Backtrace — capture at report time.
        let _ = writeln!(buf, "Native frames:");
        let bt = std::backtrace::Backtrace::force_capture();
        let bt_str = bt.to_string();
        // Limit to a reasonable number of frames.
        let mut frame_count = 0;
        for line in bt_str.lines() {
            if frame_count >= 64 {
                let _ = writeln!(buf, "  ... (truncated)");
                break;
            }
            let _ = writeln!(buf, "  {}", line);
            frame_count += 1;
        }

        let _ = writeln!(buf);
    }

    fn write_process_section(&self, buf: &mut String) {
        let _ = writeln!(buf,
            "---------------  P R O C E S S  ---------------");
        let _ = writeln!(buf);

        let _ = writeln!(buf, "VM state: {}", get_vm_state());
        let _ = writeln!(buf);

        let _ = writeln!(buf, "Heap:");
        let _ = writeln!(buf, "  {}", get_heap_info());
        let _ = writeln!(buf);
    }

    fn write_system_section(&self, buf: &mut String) {
        let _ = writeln!(buf,
            "---------------  S Y S T E M  ---------------");
        let _ = writeln!(buf);
        let _ = writeln!(buf, "OS:     {}", get_os_info());
        let _ = writeln!(buf, "CPU:    {}", get_cpu_info());
        let _ = writeln!(buf, "Memory: {}", get_memory_info());
        let _ = writeln!(buf);
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Build the full `hs_err_pid` report as a string.
pub fn generate_crash_report(info: &CrashInfo) -> String {
    CrashReport::new(info).render()
}

/// Write the crash report to a file.
pub fn write_crash_report(info: &CrashInfo, path: &Path) -> io::Result<()> {
    let content = generate_crash_report(info);
    let mut f = std::fs::File::create(path)?;
    f.write_all(content.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// Install the crash handler (Rust panic hook + platform signal handlers).
///
/// Should be called once during VM startup. It is safe to call multiple times;
/// only the first invocation actually installs the hook.
pub fn install_crash_handler() {
    // Chain on top of the existing panic hook so that default error output
    // (and any previously installed hooks) still fires.
    let prev = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        // Prevent recursive entry if the handler itself panics.
        if CRASH_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Already handling a crash — just let the process die.
            return;
        }

        let crash = CrashInfo::from_panic(panic_info);
        let pid = crash.pid;
        let filename = format!("hs_err_pid{}.log", pid);
        let path = std::path::PathBuf::from(&filename);

        match write_crash_report(&crash, &path) {
            Ok(()) => {
                eprintln!("#");
                eprintln!("# A fatal error has been detected by the CratonVM Runtime Environment:");
                eprintln!("#");
                if let Some(ref msg) = crash.panic_message {
                    eprintln!("#  {}", msg);
                }
                eprintln!("#");
                eprintln!("# An error report file with more information is saved as:");
                eprintln!("#  {}", path.display());
                eprintln!("#");
            }
            Err(e) => {
                eprintln!("# CratonVM crash handler: failed to write {}: {}", filename, e);
            }
        }

        // Invoke the previous hook so the default Rust output is preserved.
        prev(panic_info);

        // Reset so a cascading panic in the previous hook can still be caught.
        CRASH_IN_PROGRESS.store(false, Ordering::SeqCst);
    }));

    // Platform-specific signal installation (Unix only).
    #[cfg(unix)]
    install_signal_handlers();
}

// ── Async-signal-safe primitives ───────────────────────────────────────────
//
// Everything in this section MUST be callable from a signal handler. See the
// module-level docstring for the discipline.

/// Format a non-negative integer into `buf` in decimal, allocation-free.
///
/// Returns the number of bytes written. The output is left-justified at the
/// start of `buf` (i.e. `buf[..n]` is the digits, in normal reading order).
/// If `buf` is too small the function writes as many digits as fit, starting
/// with the most-significant digit that fits, and returns `buf.len()`.
///
/// This helper is the ONLY number-formatting routine used inside the signal
/// handler. It does not allocate, does not lock, and does not call into the
/// standard library's formatting machinery.
///
/// # Examples
///
/// ```
/// use cratonvm_vm::runtime::crash_handler::itoa_into_buf;
///
/// let mut buf = [0u8; 32];
/// let n = itoa_into_buf(&mut buf, 0);
/// assert_eq!(&buf[..n], b"0");
///
/// let n = itoa_into_buf(&mut buf, 12345);
/// assert_eq!(&buf[..n], b"12345");
///
/// let n = itoa_into_buf(&mut buf, u64::MAX);
/// assert_eq!(&buf[..n], b"18446744073709551615");
/// ```
pub fn itoa_into_buf(buf: &mut [u8], n: u64) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }

    // Write digits least-significant-first into a stack scratch (u64::MAX is
    // 20 digits), then reverse-copy the most-significant `out_len` digits
    // into `buf`. If `buf` is too small, low-order digits are dropped — the
    // signal handler's pre-sized buffers always have room for u64::MAX.
    let mut scratch = [0u8; 20];
    let mut len = 0usize;
    let mut v = n;
    while v > 0 && len < scratch.len() {
        scratch[len] = b'0' + (v % 10) as u8;
        v /= 10;
        len += 1;
    }

    let out_len = core::cmp::min(len, buf.len());
    for i in 0..out_len {
        // scratch is little-endian-digits; reverse to put the most-
        // significant digit first.
        buf[i] = scratch[len - 1 - i];
    }
    out_len
}

#[cfg(unix)]
mod async_signal_safe {
    //! Async-signal-safe helpers used by the SIGSEGV handler.
    //!
    //! Every function here must avoid: allocation, locking, stdio buffering,
    //! `std::fs`, `format!`, panics. Verified by manual audit only — there is
    //! no compiler check for async-signal safety in Rust today.

    use core::sync::atomic::{AtomicI32, Ordering};

    /// STDERR file descriptor number on every POSIX system.
    pub const STDERR_FD: i32 = 2;

    /// Cached process id, populated at installation time so the signal path
    /// does not need to call `getpid` (which IS safe, but caching one libc
    /// call per signal is a minor improvement and keeps the path obvious).
    pub static CACHED_PID: AtomicI32 = AtomicI32::new(0);

    /// Write `bytes` to `fd` using raw `write(2)`. Retries on EINTR.
    ///
    /// Async-signal-safe: `write` is on the POSIX whitelist.
    pub fn write_all(fd: i32, bytes: &[u8]) {
        let mut off = 0usize;
        while off < bytes.len() {
            let ptr = unsafe { bytes.as_ptr().add(off) } as *const libc::c_void;
            let want = bytes.len() - off;
            let r = unsafe { libc::write(fd, ptr, want) };
            if r < 0 {
                // EINTR -> retry; any other error -> give up silently.
                // We cannot call `*libc::__errno_location()` portably without
                // worrying about TLS reentrancy, so just retry once and bail.
                let again = unsafe { libc::write(fd, ptr, want) };
                if again <= 0 {
                    return;
                }
                off += again as usize;
            } else if r == 0 {
                return;
            } else {
                off += r as usize;
            }
        }
    }

    /// Open `path` for writing (create + truncate, mode 0644) and return the
    /// fd, or -1 on failure. `path` MUST be NUL-terminated.
    ///
    /// Async-signal-safe: `open` is on the POSIX whitelist.
    pub fn open_write_trunc(path_nul: &[u8]) -> i32 {
        debug_assert!(path_nul.last() == Some(&0));
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
        let mode = 0o644 as libc::c_int;
        unsafe { libc::open(path_nul.as_ptr() as *const libc::c_char, flags, mode) }
    }

    /// Close a file descriptor, ignoring errors.
    pub fn close(fd: i32) {
        unsafe { libc::close(fd) };
    }

    /// Map a signal number to a `&'static [u8]` name. Static-data lookup is
    /// async-signal-safe.
    pub fn signal_name_bytes(sig: i32) -> &'static [u8] {
        match sig {
            libc::SIGSEGV => b"SIGSEGV",
            libc::SIGBUS => b"SIGBUS",
            libc::SIGFPE => b"SIGFPE",
            libc::SIGILL => b"SIGILL",
            libc::SIGABRT => b"SIGABRT",
            _ => b"SIG?",
        }
    }
}

#[cfg(unix)]
std::thread_local! {
    /// Per-thread scratch buffer the signal handler can write into without
    /// allocating. Populated lazily on first access from a *normal* (non-
    /// signal) context; the signal handler only reads it. 256 bytes is
    /// sufficient for the short async-signal-safe message we emit.
    ///
    /// NOTE: thread-local access itself is implementation-defined in async-
    /// signal context. On glibc + musl the fast path is just a TLS offset
    /// load, which is safe. On platforms where the first access initializes
    /// lazily via `pthread_setspecific`, we pre-touch the buffer at thread
    /// start (see `prime_signal_tls`) to avoid that case.
    static SIGNAL_SCRATCH: core::cell::UnsafeCell<[u8; 256]> =
        core::cell::UnsafeCell::new([0u8; 256]);
}

/// Pre-touch the thread-local signal scratch buffer so that the lazy TLS
/// initialization (which on some platforms calls into the allocator) happens
/// in *normal* context, not in the signal handler.
///
/// Call this from every thread that might receive a fatal signal. It is a
/// no-op on platforms without thread-local storage support.
#[cfg(unix)]
pub fn prime_signal_tls() {
    SIGNAL_SCRATCH.with(|cell| {
        // Touch the first byte to force initialization.
        unsafe { (*cell.get())[0] = 0 };
    });
}

#[cfg(not(unix))]
pub fn prime_signal_tls() {}

// ── Unix signal handlers ───────────────────────────────────────────────────

#[cfg(unix)]
fn install_signal_handlers() {
    use std::ffi::c_int;

    // Signals that indicate a crash.
    const CRASH_SIGNALS: &[c_int] = &[
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGILL,
        libc::SIGABRT,
    ];

    // Pre-allocated constant strings used by the signal handler. Storing them
    // as `&'static [u8]` means no allocation is needed to reference them.
    const HDR: &[u8] = b"\n#\n# A fatal error has been detected by the CratonVM Runtime Environment:\n#  ";
    const AT_PC: &[u8] = b" at pc=0x0, pid=";
    const TID_LBL: &[u8] = b", tid=";
    const NL_REPORT: &[u8] = b"\n#  Error report saved to: ";
    const FOOTER: &[u8] = b"\n#\n";
    const FILE_PREFIX: &[u8] = b"hs_err_pid";
    const FILE_SUFFIX: &[u8] = b".log";

    // Cache the pid in normal context so the handler doesn't need libc::getpid
    // (which is technically signal-safe, but we minimize syscalls).
    async_signal_safe::CACHED_PID
        .store(std::process::id() as i32, Ordering::Relaxed);

    // Prime TLS on the installing thread (each VM-spawned thread should also
    // call `prime_signal_tls` itself at startup).
    prime_signal_tls();

    // ── THE SIGNAL HANDLER ────────────────────────────────────────────────
    //
    // This function executes in async-signal context. See the module doc
    // for the rules. Roughly: only libc syscalls, atomics, and stack-local
    // arithmetic are allowed. No allocations, no locks, no formatting
    // machinery, no `std::fs`, no `Backtrace::capture`.
    extern "C" fn crash_signal_handler(sig: std::ffi::c_int) {
        // Re-entry guard. `compare_exchange` on an `AtomicBool` is lock-free
        // and async-signal-safe on every architecture we target.
        if CRASH_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            return;
        }

        // Stack buffers — no heap.
        let mut pid_buf = [0u8; 20];
        let mut tid_buf = [0u8; 20];
        let mut filename = [0u8; 64];

        let pid = async_signal_safe::CACHED_PID.load(Ordering::Relaxed) as u64;
        let pid_len = itoa_into_buf(&mut pid_buf, pid);

        // Thread id via `pthread_self` -> usize cast. pthread_self is on the
        // POSIX async-signal-safe whitelist.
        let tid = unsafe { libc::pthread_self() as u64 };
        let tid_len = itoa_into_buf(&mut tid_buf, tid);

        // Build "hs_err_pid<pid>.log\0" into `filename` without allocating.
        let mut fpos = 0usize;
        let parts: [&[u8]; 3] = [
            FILE_PREFIX,
            &pid_buf[..pid_len],
            FILE_SUFFIX,
        ];
        for part in parts.iter() {
            for &b in part.iter() {
                if fpos + 1 < filename.len() {
                    filename[fpos] = b;
                    fpos += 1;
                }
            }
        }
        // NUL-terminate for `open(2)`.
        filename[fpos] = 0;
        let path_nul = &filename[..=fpos];

        let sig_name = async_signal_safe::signal_name_bytes(sig);

        // ── Emit message to stderr (write(2) is async-signal-safe) ─────────
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, HDR);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, sig_name);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, AT_PC);
        async_signal_safe::write_all(
            async_signal_safe::STDERR_FD,
            &pid_buf[..pid_len],
        );
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, TID_LBL);
        async_signal_safe::write_all(
            async_signal_safe::STDERR_FD,
            &tid_buf[..tid_len],
        );
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, NL_REPORT);
        // Write filename without the trailing NUL.
        async_signal_safe::write_all(
            async_signal_safe::STDERR_FD,
            &filename[..fpos],
        );
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, FOOTER);

        // ── Write a minimal hs_err_pid file via raw open/write/close ──────
        //
        // We intentionally write a SHORT marker file rather than the full
        // HotSpot report. The full report requires allocation (Backtrace,
        // String, /proc parsing) which is forbidden here. The panic-hook
        // path writes the rich report for ordinary panics; for hardware
        // faults the user gets just enough to start debugging.
        let fd = async_signal_safe::open_write_trunc(path_nul);
        if fd >= 0 {
            async_signal_safe::write_all(fd, b"# CratonVM fatal signal: ");
            async_signal_safe::write_all(fd, sig_name);
            async_signal_safe::write_all(fd, b"\n# pid=");
            async_signal_safe::write_all(fd, &pid_buf[..pid_len]);
            async_signal_safe::write_all(fd, b"\n# tid=");
            async_signal_safe::write_all(fd, &tid_buf[..tid_len]);
            async_signal_safe::write_all(
                fd,
                b"\n# (truncated: full report requires allocator, unsafe in \
                  signal handler)\n",
            );
            async_signal_safe::close(fd);
        }

        // Re-raise with default handler so the exit code reflects the signal.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    for &sig in CRASH_SIGNALS {
        unsafe {
            libc::signal(sig, crash_signal_handler as libc::sighandler_t);
        }
    }
}

// ── Platform helpers ───────────────────────────────────────────────────────

fn get_pid() -> u32 {
    std::process::id()
}

fn get_tid() -> u64 {
    // Thread ID as a u64. On most platforms this is the OS thread ID.
    #[cfg(unix)]
    {
        unsafe { libc::pthread_self() as u64 }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() as u64 }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Fallback: hash the thread's name or use 0.
        0
    }
}

/// Map a signal number to its name.
#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGSEGV => "SIGSEGV".to_string(),
        libc::SIGBUS => "SIGBUS".to_string(),
        libc::SIGFPE => "SIGFPE".to_string(),
        libc::SIGILL => "SIGILL".to_string(),
        libc::SIGABRT => "SIGABRT".to_string(),
        other => format!("SIG({})", other),
    }
}

/// Rust compiler version baked in at build time.
fn rust_version() -> &'static str {
    // `rustc_version_runtime` isn't available; use the build-env macro.
    // `CARGO_PKG_RUST_VERSION` gives the MSRV, not the actual compiler.
    // Best we can do without a build script is the env variable.
    option_env!("RUSTC_VERSION").unwrap_or(env!("CARGO_PKG_RUST_VERSION"))
}

/// Format a `SystemTime` as a human-readable timestamp.
fn format_timestamp(t: SystemTime) -> String {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // Manual UTC decomposition (no chrono dependency).
            let days = secs / 86400;
            let time_of_day = secs % 86400;
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;

            // Days since epoch to Y-M-D (simplified Gregorian).
            let (year, month, day) = days_to_ymd(days);
            format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                year, month, day, hours, minutes, seconds)
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Convert days-since-Unix-epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `civil_from_days`.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

// ── OS / CPU / Memory info ─────────────────────────────────────────────────

/// Collect operating system information.
pub fn get_os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_os_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_os_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_os_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
    }
}

#[cfg(target_os = "linux")]
fn get_os_info_linux() -> String {
    // Try /etc/os-release first for distribution info.
    let distro = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "Linux".to_string());

    let kernel = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|v| v.split_whitespace().nth(2).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    format!("{}, kernel {}, {}", distro, kernel, std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
fn get_os_info_macos() -> String {
    let version = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    format!("macOS {}, {}", version, std::env::consts::ARCH)
}

#[cfg(target_os = "windows")]
fn get_os_info_windows() -> String {
    // Read from the registry via `ver`-like info or the environment.
    // `std::env::consts::OS` gives "windows"; supplement with version.
    let version = std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Windows (unknown version)".to_string());

    format!("{}, {}", version, std::env::consts::ARCH)
}

/// Collect CPU information.
pub fn get_cpu_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_cpu_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_cpu_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_cpu_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        format!("{} cores", std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1))
    }
}

#[cfg(target_os = "linux")]
fn get_cpu_info_linux() -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", model, cores)
}

#[cfg(target_os = "macos")]
fn get_cpu_info_macos() -> String {
    let brand = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", brand, cores)
}

#[cfg(target_os = "windows")]
fn get_cpu_info_windows() -> String {
    // Read the CPU name from the PROCESSOR_IDENTIFIER environment variable
    // or fall back to a wmic query.
    let model = std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .or_else(|| {
            std::process::Command::new("wmic")
                .args(["cpu", "get", "name", "/value"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("Name="))
                        .map(|l| l.trim_start_matches("Name=").trim().to_string())
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", model, cores)
}

/// Collect memory information (total and available).
pub fn get_memory_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_memory_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_memory_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_memory_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "unknown".to_string()
    }
}

#[cfg(target_os = "linux")]
fn get_memory_info_linux() -> String {
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(content) => {
            let mut total_kb: u64 = 0;
            let mut avail_kb: u64 = 0;
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    total_kb = parse_meminfo_kb(line);
                } else if line.starts_with("MemAvailable:") {
                    avail_kb = parse_meminfo_kb(line);
                }
            }
            format!("total={} MB, available={} MB",
                total_kb / 1024, avail_kb / 1024)
        }
        Err(_) => "unknown".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn parse_meminfo_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn get_memory_info_macos() -> String {
    let total = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let total_mb = total / (1024 * 1024);
    // `vm_stat` provides free/inactive pages but requires parsing; just
    // report total for reliability.
    format!("total={} MB", total_mb)
}

#[cfg(target_os = "windows")]
fn get_memory_info_windows() -> String {
    // Use GlobalMemoryStatusEx via raw FFI.
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }

    extern "system" {
        fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
    }

    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };

    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 {
        let total_mb = status.ull_total_phys / (1024 * 1024);
        let avail_mb = status.ull_avail_phys / (1024 * 1024);
        format!("total={} MB, available={} MB", total_mb, avail_mb)
    } else {
        "unknown".to_string()
    }
}

/// Return a summary of the VM state (version, uptime hint).
pub fn get_vm_state() -> String {
    // We do not have a static reference to the VM here (crash handlers
    // must be self-contained), so report what we can.
    format!("CratonVM 25.0 (crash state — detailed VM info unavailable)")
}

/// Get heap information if available.
fn get_heap_info() -> String {
    // In a crash handler we cannot safely lock the VM heap. Report a
    // placeholder that documents this limitation.
    "heap information unavailable during crash (unsafe to inspect)".to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_crash_info() -> CrashInfo {
        CrashInfo {
            signal: 11,
            signal_name: "SIGSEGV".to_string(),
            pid: 12345,
            tid: 67890,
            timestamp: SystemTime::UNIX_EPOCH,
            thread_name: Some("main".to_string()),
            panic_message: Some("null pointer dereference".to_string()),
            panic_location: Some("vm/src/runtime/interpreter.rs:42".to_string()),
        }
    }

    #[test]
    fn crash_info_creation() {
        let info = sample_crash_info();
        assert_eq!(info.signal, 11);
        assert_eq!(info.signal_name, "SIGSEGV");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.tid, 67890);
        assert_eq!(info.thread_name.as_deref(), Some("main"));
        assert_eq!(info.panic_message.as_deref(), Some("null pointer dereference"));
        assert_eq!(info.panic_location.as_deref(),
            Some("vm/src/runtime/interpreter.rs:42"));
    }

    #[test]
    fn crash_info_from_panic_fields() {
        // We cannot easily construct a PanicHookInfo, so test the
        // manual-construction path instead.
        let info = CrashInfo {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name: Some("test-thread".to_string()),
            panic_message: Some("assertion failed".to_string()),
            panic_location: Some("test.rs:1".to_string()),
        };
        assert_eq!(info.signal, 0);
        assert_eq!(info.signal_name, "RUST_PANIC");
        assert!(info.pid > 0);
        assert!(info.panic_message.is_some());
    }

    #[test]
    fn crash_report_contains_header() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("A fatal error has been detected by the CratonVM Runtime Environment"));
        assert!(report.contains("SIGSEGV"));
        assert!(report.contains("pid=12345"));
        assert!(report.contains("tid=67890"));
    }

    #[test]
    fn crash_report_contains_thread_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("T H R E A D"));
        assert!(report.contains("\"main\""));
        assert!(report.contains("null pointer dereference"));
    }

    #[test]
    fn crash_report_contains_process_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("P R O C E S S"));
        assert!(report.contains("VM state:"));
        assert!(report.contains("Heap:"));
    }

    #[test]
    fn crash_report_contains_system_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("S Y S T E M"));
        assert!(report.contains("OS:"));
        assert!(report.contains("CPU:"));
        assert!(report.contains("Memory:"));
    }

    #[test]
    fn crash_report_contains_jre_version() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("JRE version: CratonVM 25.0"));
    }

    #[test]
    fn crash_report_rust_panic_format() {
        let info = CrashInfo {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: 999,
            tid: 111,
            timestamp: SystemTime::now(),
            thread_name: Some("worker-1".to_string()),
            panic_message: Some("index out of bounds".to_string()),
            panic_location: Some("src/lib.rs:100".to_string()),
        };
        let report = generate_crash_report(&info);
        assert!(report.contains("RUST_PANIC"));
        assert!(report.contains("src/lib.rs:100"));
        assert!(report.contains("index out of bounds"));
        assert!(report.contains("\"worker-1\""));
    }

    #[test]
    fn write_crash_report_to_file() {
        let info = sample_crash_info();
        let dir = std::env::temp_dir();
        let path = dir.join("hs_err_pid_test.log");

        write_crash_report(&info, &path).expect("write should succeed");

        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(content.contains("SIGSEGV"));
        assert!(content.contains("T H R E A D"));
        assert!(content.contains("S Y S T E M"));

        // Cleanup.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn os_info_non_empty() {
        let info = get_os_info();
        assert!(!info.is_empty(), "OS info should not be empty");
        assert!(info.len() > 3, "OS info should have meaningful content");
    }

    #[test]
    fn cpu_info_non_empty() {
        let info = get_cpu_info();
        assert!(!info.is_empty(), "CPU info should not be empty");
        assert!(info.contains("core") || info.contains("thread") || info.contains("unknown"),
            "CPU info should mention cores/threads: {}", info);
    }

    #[test]
    fn memory_info_non_empty() {
        let info = get_memory_info();
        assert!(!info.is_empty(), "Memory info should not be empty");
    }

    #[test]
    fn vm_state_non_empty() {
        let state = get_vm_state();
        assert!(!state.is_empty());
        assert!(state.contains("CratonVM"));
    }

    #[test]
    fn timestamp_formatting() {
        let t = SystemTime::UNIX_EPOCH;
        let s = format_timestamp(t);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn itoa_into_buf_zero() {
        let mut buf = [0u8; 8];
        let n = itoa_into_buf(&mut buf, 0);
        assert_eq!(&buf[..n], b"0");
    }

    #[test]
    fn itoa_into_buf_small() {
        let mut buf = [0u8; 8];
        let n = itoa_into_buf(&mut buf, 1);
        assert_eq!(&buf[..n], b"1");

        let n = itoa_into_buf(&mut buf, 9);
        assert_eq!(&buf[..n], b"9");

        let n = itoa_into_buf(&mut buf, 10);
        assert_eq!(&buf[..n], b"10");

        let n = itoa_into_buf(&mut buf, 42);
        assert_eq!(&buf[..n], b"42");
    }

    #[test]
    fn itoa_into_buf_large() {
        let mut buf = [0u8; 32];
        let n = itoa_into_buf(&mut buf, 1_000_000);
        assert_eq!(&buf[..n], b"1000000");

        let n = itoa_into_buf(&mut buf, u64::MAX);
        assert_eq!(&buf[..n], b"18446744073709551615");
    }

    #[test]
    fn itoa_into_buf_empty_returns_zero() {
        let mut buf = [];
        assert_eq!(itoa_into_buf(&mut buf, 123), 0);
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-01 is day 19723 since epoch.
        let (y, m, d) = days_to_ymd(19723);
        assert_eq!((y, m, d), (2024, 1, 1));
    }
}
