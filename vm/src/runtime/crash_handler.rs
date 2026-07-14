// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

// spring-bug-10 watchpoint: set true once the VEH catches the -2 write to a
// shadow savebase slot, so the JIT arm-helper stops re-arming the HW breakpoint.
pub static SAVEBASE_WATCH_CAUGHT: AtomicBool = AtomicBool::new(false);
// Count of -2 writes the watchpoint VEH has observed (caps log spam).
pub static SAVEBASE_WATCH_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// ES-FAIL-FAMILY-20260710 hunt: generalization of the spring-bug-10
/// hardware watchpoint (see `jit::helpers::savebase_watcher`, reused
/// as-is via `arm_generic_heap_watch` below) for "report the writer of
/// ANY value at this address" instead of savebase's specific `-2`
/// sentinel match. Shares the watcher-thread's arm/disarm signaling
/// (`SAVEBASE_WATCH_CAUGHT`) since only one hardware watch is ever
/// active at a time in practice. When set, the VEH's DR0 branch reports
/// + disarms on the FIRST write to the watched address, regardless of
/// the value written.
pub static GENERIC_HEAP_WATCH_MODE: AtomicBool = AtomicBool::new(false);

/// Arm a hardware data-write breakpoint on `addr` (8 bytes) for the
/// CURRENT thread via the existing spring-bug-10 watcher-thread
/// infrastructure, but in "generic" mode: the VEH reports the RIP of
/// whatever instruction writes there NEXT, for any value — not just
/// savebase's `-2` sentinel. One-shot: disarms itself after the first
/// report (shared `SAVEBASE_WATCH_CAUGHT` latch).
#[cfg(windows)]
pub fn arm_generic_heap_watch(addr: usize) {
    GENERIC_HEAP_WATCH_MODE.store(true, Ordering::SeqCst);
    SAVEBASE_WATCH_CAUGHT.store(false, Ordering::SeqCst);
    // SAFETY: `publish` only suspends/resumes/SetThreadContext's a
    // duplicated handle to the CURRENT thread (see its own SAFETY
    // comment); calling it here from a normal native-call context is
    // exactly the same calling convention as its JIT-prologue caller.
    unsafe {
        crate::jit::helpers::savebase_watcher::publish(addr);
    }
}

#[cfg(not(windows))]
pub fn arm_generic_heap_watch(_addr: usize) {}

/// True once the savebase-watchpoint VEH has reported the -2 writer.
pub fn savebase_watch_caught() -> bool {
    SAVEBASE_WATCH_CAUGHT.load(Ordering::Relaxed)
}

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

        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()));

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

        let _ = writeln!(
            buf,
            "# A fatal error has been detected by the CratonVM Runtime Environment:"
        );
        let _ = writeln!(buf, "#");
        if self.info.signal == 0 {
            // Rust panic
            let msg = self.info.panic_message.as_deref().unwrap_or("unknown");
            let loc = self.info.panic_location.as_deref().unwrap_or("unknown");
            let _ = writeln!(
                buf,
                "#  RUST_PANIC at {}, pid={}, tid={}",
                loc, self.info.pid, self.info.tid
            );
            let _ = writeln!(buf, "#  Message: {}", msg);
        } else {
            let _ = writeln!(
                buf,
                "#  {} ({}) at pc=0x0, pid={}, tid={}",
                self.info.signal_name, sig_hex, self.info.pid, self.info.tid
            );
        }
        let _ = writeln!(buf, "#");
        let _ = writeln!(buf, "# JRE version: CratonVM 25.0");

        // Rust compiler version (baked in at build time).
        let _ = writeln!(buf, "# Rust version: {}", rust_version());

        let _ = writeln!(buf, "# OS: {}", get_os_info());
        let _ = writeln!(buf, "#");
        let _ = writeln!(
            buf,
            "# If you would like to submit a bug report, please include this file."
        );
        let _ = writeln!(buf, "# Crash timestamp: {}", ts);
        let _ = writeln!(buf);
    }

    fn write_thread_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  T H R E A D  ---------------");
        let _ = writeln!(buf);

        let tname = self.info.thread_name.as_deref().unwrap_or("<unnamed>");
        let _ = writeln!(
            buf,
            "Current thread (0x{:x}): \"{}\" tid=0x{:x}",
            self.info.tid, tname, self.info.tid
        );

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
        let _ = writeln!(buf, "---------------  P R O C E S S  ---------------");
        let _ = writeln!(buf);

        let _ = writeln!(buf, "VM state: {}", get_vm_state());
        let _ = writeln!(buf);

        let _ = writeln!(buf, "Heap:");
        let _ = writeln!(buf, "  {}", get_heap_info());
        let _ = writeln!(buf);
    }

    fn write_system_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  S Y S T E M  ---------------");
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

// ── Windows hardware-fault handler (vectored exception handler) ─────────────
//
// On Windows a hardware fault (access violation, illegal instruction, …) is a
// *structured exception*, NOT a Rust panic — so it bypasses the panic hook
// entirely and the process dies via the OS default handler with no diagnostic
// (a bare `STATUS_ACCESS_VIOLATION` exit code and empty stderr). That is why
// the JIT-dispatch SEGV, the sunflow Java2D SEGV, and the fop SEGV were all
// "un-localizable" on this dev box: there was nothing capturing the faulting
// PC or a backtrace.
//
// `install_hardware_fault_handler` registers a vectored exception handler
// (VEH) that, on a genuinely fatal fault, prints the faulting exception code,
// the faulting instruction address, and a symbolized native backtrace to
// stderr (and writes an `hs_err_pid<pid>.log`), THEN returns
// `EXCEPTION_CONTINUE_SEARCH` so normal exception processing continues and the
// process still terminates exactly as before. It does not swallow the fault
// and it does not alter control flow — it is a pure diagnostic tap.
//
// Symbol resolution requires the binary to carry debug info: build with the
// `release-with-debug` profile (`--profile release-with-debug`), which keeps
// line tables and does not strip. A plain `release` build (which strips
// debuginfo) still prints the faulting address, usable with the `.map` file.
#[cfg(windows)]
mod windows_fault {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    #[repr(C)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut ExceptionRecord,
        exception_address: *mut core::ffi::c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut core::ffi::c_void,
    }

    type VectoredHandler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: VectoredHandler,
        ) -> *mut core::ffi::c_void;
        fn RtlCaptureStackBackTrace(
            frames_to_skip: u32,
            frames_to_capture: u32,
            back_trace: *mut *mut core::ffi::c_void,
            back_trace_hash: *mut u32,
        ) -> u16;
        fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
        fn GetModuleFileNameW(module: *mut core::ffi::c_void, filename: *mut u16, size: u32)
            -> u32;
        fn VirtualQuery(
            address: *const core::ffi::c_void,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
    }

    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut core::ffi::c_void,
        allocation_base: *mut core::ffi::c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }

    const MEM_COMMIT: u32 = 0x1000;
    // Page protections that permit reads (any of these ⇒ readable).
    const PAGE_READABLE_MASK: u32 = 0x02  /* READONLY */
        | 0x04  /* READWRITE */
        | 0x08  /* WRITECOPY */
        | 0x20  /* EXECUTE_READ */
        | 0x40  /* EXECUTE_READWRITE */
        | 0x80; /* EXECUTE_WRITECOPY */

    /// True if `[addr, addr+len)` lies in a single committed, readable region.
    /// Used to guard raw memory dumps in the crash handler so probing a wild
    /// pointer cannot itself fault and abort the report.
    unsafe fn is_readable(addr: usize, len: usize) -> bool {
        if addr == 0 {
            return false;
        }
        let mut mbi: MemoryBasicInformation = core::mem::zeroed();
        let n = VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<MemoryBasicInformation>(),
        );
        if n == 0 || mbi.state != MEM_COMMIT || (mbi.protect & PAGE_READABLE_MASK) == 0 {
            return false;
        }
        // Ensure the whole window stays inside this region.
        let region_end = (mbi.base_address as usize).saturating_add(mbi.region_size);
        addr.saturating_add(len) <= region_end
    }

    // dbghelp symbolization of a *known* address. Unlike a full stack walk
    // (std's `Backtrace::force_capture`, which re-faults on the corrupted /
    // JIT-mixed stacks these crashes produce), SymFromAddr only resolves one
    // address and does not touch the broken stack — so it survives.
    #[link(name = "dbghelp")]
    extern "system" {
        fn SymSetOptions(options: u32) -> u32;
        fn SymInitializeW(
            process: *mut core::ffi::c_void,
            search_path: *const u16,
            invade_process: i32,
        ) -> i32;
        fn SymLoadModuleExW(
            process: *mut core::ffi::c_void,
            file: *mut core::ffi::c_void,
            image_name: *const u16,
            module_name: *const u16,
            base_of_dll: u64,
            dll_size: u32,
            data: *mut core::ffi::c_void,
            flags: u32,
        ) -> u64;
        fn SymFromAddr(
            process: *mut core::ffi::c_void,
            address: u64,
            displacement: *mut u64,
            symbol: *mut SymbolInfo,
        ) -> i32;
        fn SymGetLineFromAddrW64(
            process: *mut core::ffi::c_void,
            address: u64,
            displacement: *mut u32,
            line: *mut ImagehlpLineW64,
        ) -> i32;
    }

    // Matches DbgHelp.h IMAGEHLP_LINEW64. repr(C) reproduces the x64 padding
    // (4 bytes after `size_of_struct` before the pointer, 4 after `line_number`).
    #[repr(C)]
    struct ImagehlpLineW64 {
        size_of_struct: u32,
        key: *mut core::ffi::c_void,
        line_number: u32,
        file_name: *mut u16,
        address: u64,
    }

    // Matches DbgHelp.h SYMBOL_INFO (the trailing `name` is a flexible array;
    // callers over-allocate). repr(C) reproduces the field padding exactly.
    #[repr(C)]
    struct SymbolInfo {
        size_of_struct: u32,
        type_index: u32,
        reserved: [u64; 2],
        index: u32,
        size: u32,
        mod_base: u64,
        flags: u32,
        value: u64,
        address: u64,
        register: u32,
        scope: u32,
        tag: u32,
        name_len: u32,
        max_name_len: u32,
        name: [u8; 1],
    }

    const SYMOPT_UNDNAME: u32 = 0x0000_0002;
    const SYMOPT_DEFERRED_LOADS: u32 = 0x0000_0004;
    const SYMOPT_LOAD_LINES: u32 = 0x0000_0010;

    /// Resolve a single instruction address to `function+0xNN` via dbghelp.
    /// `process` must be the value from `GetCurrentProcess()` and dbghelp must
    /// already be initialized (see the handler). Returns None if unresolved.
    unsafe fn symbolize(process: *mut core::ffi::c_void, addr: usize) -> Option<String> {
        if addr == 0 {
            return None;
        }
        // Over-allocate: header + room for a long demangled Rust symbol.
        let mut buf = [0u64; 320];
        let sym = buf.as_mut_ptr() as *mut SymbolInfo;
        (*sym).size_of_struct = core::mem::size_of::<SymbolInfo>() as u32;
        (*sym).max_name_len = 2000;
        let mut disp: u64 = 0;
        if SymFromAddr(process, addr as u64, &mut disp, sym) == 0 {
            return None;
        }
        let name_off = core::mem::offset_of!(SymbolInfo, name);
        let name_ptr = (sym as *const u8).add(name_off);
        let len = ((*sym).name_len as usize).min(2000);
        let bytes = core::slice::from_raw_parts(name_ptr, len);
        let s = String::from_utf8_lossy(bytes).into_owned();
        let mut out = if disp != 0 {
            format!("{}+0x{:X}", s, disp)
        } else {
            s
        };
        // Best-effort file:line via the line table (line-tables-only builds
        // still populate this). Disambiguates fat-LTO inlined frames where
        // `function+0xDISP` alone points into inlined code.
        let mut line: ImagehlpLineW64 = core::mem::zeroed();
        line.size_of_struct = core::mem::size_of::<ImagehlpLineW64>() as u32;
        let mut line_disp: u32 = 0;
        if SymGetLineFromAddrW64(process, addr as u64, &mut line_disp, &mut line) != 0
            && !line.file_name.is_null()
        {
            // file_name is a NUL-terminated wide string.
            let mut n = 0usize;
            while n < 4096 && *line.file_name.add(n) != 0 {
                n += 1;
            }
            let wfile = core::slice::from_raw_parts(line.file_name, n);
            let file = String::from_utf16_lossy(wfile);
            out.push_str(&format!("  [{}:{}]", file, line.line_number));
        }
        Some(out)
    }

    // Win32 NTSTATUS exception codes we treat as fatal hardware faults.
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const EXCEPTION_IN_PAGE_ERROR: u32 = 0xC000_0006;
    const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
    const EXCEPTION_PRIV_INSTRUCTION: u32 = 0xC000_0096;
    const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
    const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;
    // Raised by `__fastfail` / `RaiseFailFastException` — in particular the
    // MSVC `/GS` stack-cookie check calling `__report_gsfailure` on a
    // detected stack-buffer overrun. This is exactly the "Exception code:
    // 0xc0000409" signature Windows Error Reporting shows for these crashes
    // when there is no VEH watching for it: the process fastfails with an
    // empty stderr and no hs_err log. Treating it as fatal here lets us
    // capture the faulting PC/RVA and a raw backtrace before the OS
    // terminates the process (fastfail exceptions still reach registered
    // vectored handlers even though they are non-continuable).
    const EXCEPTION_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    // 0 = idle, 1 = handling. A plain bool swap would let a fault *inside* the
    // handler recurse forever; this latches so a second fault falls straight
    // through to the OS.
    static HANDLING: AtomicU8 = AtomicU8::new(0);

    fn code_name(code: u32) -> &'static str {
        match code {
            EXCEPTION_ACCESS_VIOLATION => "EXCEPTION_ACCESS_VIOLATION (SIGSEGV)",
            EXCEPTION_IN_PAGE_ERROR => "EXCEPTION_IN_PAGE_ERROR",
            EXCEPTION_ILLEGAL_INSTRUCTION => "EXCEPTION_ILLEGAL_INSTRUCTION (SIGILL)",
            EXCEPTION_PRIV_INSTRUCTION => "EXCEPTION_PRIV_INSTRUCTION",
            EXCEPTION_INT_DIVIDE_BY_ZERO => "EXCEPTION_INT_DIVIDE_BY_ZERO (SIGFPE)",
            EXCEPTION_STACK_OVERFLOW => "EXCEPTION_STACK_OVERFLOW",
            EXCEPTION_STACK_BUFFER_OVERRUN => "EXCEPTION_STACK_BUFFER_OVERRUN (fastfail)",
            _ => "UNKNOWN",
        }
    }

    fn is_fatal(code: u32) -> bool {
        matches!(
            code,
            EXCEPTION_ACCESS_VIOLATION
                | EXCEPTION_IN_PAGE_ERROR
                | EXCEPTION_ILLEGAL_INSTRUCTION
                | EXCEPTION_PRIV_INSTRUCTION
                | EXCEPTION_INT_DIVIDE_BY_ZERO
                | EXCEPTION_STACK_OVERFLOW
                | EXCEPTION_STACK_BUFFER_OVERRUN
        )
    }

    unsafe extern "system" fn vectored_handler(info: *mut ExceptionPointers) -> i32 {
        if info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let rec = (*info).exception_record;
        if rec.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = (*rec).exception_code;
        // spring-bug-10 watchpoint: a HW data breakpoint fires STATUS_SINGLE_STEP.
        // If DR6 shows one of DR0-3 tripped, this is OUR savebase watchpoint —
        // inspect the value just written; if it is the corrupt 0xFFFF…FFFE, report
        // the writing instruction's RIP (the long-hunted -2 writer) and disarm.
        // Non-(-2) writes (the legitimate push, or other frames reusing the stack
        // qword) are acknowledged silently and the watchpoint stays armed.
        const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;
        const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
        if code == EXCEPTION_SINGLE_STEP {
            let ctx = (*info).context_record as *mut u8;
            if ctx.is_null() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let dr6 = core::ptr::read_unaligned(ctx.add(0x68) as *const u64);
            if dr6 & 0xF == 0 {
                return EXCEPTION_CONTINUE_SEARCH; // not our HW breakpoint
            }
            let dr0 = core::ptr::read_unaligned(ctx.add(0x48) as *const u64);
            let rip = core::ptr::read_unaligned(ctx.add(0xF8) as *const u64);
            // ES-FAIL-FAMILY-20260710 hunt: generic mode reports the writer of
            // ANY value at the watched address (armed via
            // `arm_generic_heap_watch`), unlike savebase's `-2`-specific match
            // below. HEAP address, not a stack slot, so there's no
            // "cross-frame" stack-reuse distinction to make — report and
            // disarm on the very first hit.
            if dr0 != 0
                && crate::runtime::crash_handler::GENERIC_HEAP_WATCH_MODE.load(Ordering::Relaxed)
                && !crate::runtime::crash_handler::SAVEBASE_WATCH_CAUGHT
                    .swap(true, Ordering::SeqCst)
            {
                let val = core::ptr::read_unaligned(dr0 as *const u64);
                let mb = GetModuleHandleW(core::ptr::null()) as u64;
                let jit = cratonvm_jit::lookup_jit_method_name(rip as usize)
                    .unwrap_or_else(|| "<none>".to_string());
                let rva = if rip >= mb { rip - mb } else { 0 };
                let wrsp = core::ptr::read_unaligned(ctx.add(0x98) as *const u64);
                let wrbp = core::ptr::read_unaligned(ctx.add(0xA0) as *const u64);
                eprintln!(
                    "[HEAPWATCH] write @0x{dr0:016X} val=0x{val:016X} RIP=0x{rip:016X} (exe+0x{rva:X}) jit={jit} wrsp=0x{wrsp:016X} wrbp=0x{wrbp:016X}\n{}",
                    std::backtrace::Backtrace::force_capture(),
                );
                core::ptr::write_unaligned(ctx.add(0x70) as *mut u64, 0); // disarm DR7
                core::ptr::write_unaligned(ctx.add(0x68) as *mut u64, 0); // clear DR6 (ack)
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            if dr0 != 0 {
                let val = core::ptr::read_unaligned(dr0 as *const u64);
                if val == 0xFFFF_FFFF_FFFF_FFFE {
                    let mb = GetModuleHandleW(core::ptr::null()) as u64;
                    let wrsp = core::ptr::read_unaligned(ctx.add(0x98) as *const u64);
                    let wrbp = core::ptr::read_unaligned(ctx.add(0xA0) as *const u64);
                    let jit = cratonvm_jit::lookup_jit_method_name(rip as usize)
                        .unwrap_or_else(|| "<none>".to_string());
                    let rva = if rip >= mb { rip - mb } else { 0 };
                    // cross-frame: the written addr is ABOVE the writer's whole
                    // frame ⇒ it reaches into a live CALLER (the real corruptor).
                    // Within the writer's frame ⇒ a frame-local write to reused
                    // stack (coincidental, after reset returned).
                    let cross = dr0 > wrsp.wrapping_add(0x800);
                    // Log the first several -2 writes (cap to avoid spam).
                    let n = crate::runtime::crash_handler::SAVEBASE_WATCH_HITS
                        .fetch_add(1, Ordering::Relaxed);
                    if n < 24 {
                        eprintln!(
                            "[WATCH] -2 write @0x{:016X} RIP=0x{:016X} (exe+0x{:X}) jit={} wrsp=0x{:016X} wrbp=0x{:016X} cross_frame={}",
                            dr0, rip, rva, jit, wrsp, wrbp, cross
                        );
                    }
                    // One-shot DISARM only on the live (cross-frame) corruptor.
                    if cross
                        && !crate::runtime::crash_handler::SAVEBASE_WATCH_CAUGHT
                            .swap(true, Ordering::SeqCst)
                    {
                        core::ptr::write_unaligned(ctx.add(0x70) as *mut u64, 0); // disarm DR7
                        eprintln!(
                            "[WATCH] *** LIVE CORRUPTOR: -2 -> savebase 0x{:016X} by exe+0x{:X} (jit={}) ***",
                            dr0, rva, jit
                        );
                    }
                }
            }
            core::ptr::write_unaligned(ctx.add(0x68) as *mut u64, 0); // clear DR6 (ack)
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        // Ignore everything that is not a genuine fatal hardware fault — in
        // particular Rust's own SEH unwind exceptions and debugger
        // breakpoints must pass through untouched.
        if !is_fatal(code) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // Re-entry latch. If a fault occurs while we are already reporting one
        // (e.g. dbghelp itself trips), bail to the OS immediately rather than
        // looping. Never reset — one report per process is enough.
        if HANDLING.swap(1, Ordering::SeqCst) != 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let fault_addr = (*rec).exception_address as usize;
        // For an access violation, exception_information[0] is the access type
        // (0=read, 1=write, 8=execute) and [1] is the faulting data address.
        let (op, data_addr) = if code == EXCEPTION_ACCESS_VIOLATION && (*rec).number_parameters >= 2
        {
            let op = match (*rec).exception_information[0] {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "?",
            };
            (op, (*rec).exception_information[1])
        } else {
            ("", 0)
        };

        // We are about to die regardless. A Windows VEH runs in ordinary thread
        // context (not an interrupt), so allocation / stdio are permitted.
        //
        // CRITICAL ORDERING for multi-threaded crashes: several worker threads
        // can fault on the same bug almost simultaneously. The first one in
        // latches HANDLING; a second faulting thread sees the latch, returns
        // CONTINUE_SEARCH, and its unhandled fault TerminateProcess-es us — which
        // can truncate a slow, per-line stderr dump mid-walk. So we:
        //   1. capture the raw stack ONCE (fast, fills the whole array),
        //   2. build the ENTIRE report into a String (no I/O yet),
        //   3. write it to the hs_err file in a SINGLE write_all + flush, then
        //      mirror to stderr in a single write,
        //   4. only THEN attempt fragile dbghelp symbolization.
        // That makes the complete raw frame list survive the race.
        //
        // `writeln!` into the `report` String needs `fmt::Write` in scope; the
        // file/stderr sinks use fully-qualified `std::io::Write` calls so the
        // two traits don't collide.
        use core::fmt::Write as _;

        let module_base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let mut raw: [*mut core::ffi::c_void; 62] = [core::ptr::null_mut(); 62];
        // EXCEPTION_STACK_OVERFLOW fires on an exhausted stack: walking it
        // (RtlCaptureStackBackTrace / dbghelp) would itself fault. Skip the walk
        // (n=0 disables both the raw loop and the symbolize loop); the faulting
        // PC + RVA from the exception record still pinpoint the recursion site.
        let n = if code == EXCEPTION_STACK_OVERFLOW {
            0
        } else {
            let captured =
                unsafe { RtlCaptureStackBackTrace(0, 62, raw.as_mut_ptr(), core::ptr::null_mut()) };
            captured as usize
        };

        let tname = std::thread::current()
            .name()
            .map(String::from)
            .unwrap_or_else(|| "<unnamed>".to_string());
        let pid = std::process::id();

        let mut report = String::with_capacity(4096);
        let _ = writeln!(report, "\n#");
        let _ = writeln!(
            report,
            "# A fatal error has been detected by the CratonVM Runtime Environment:"
        );
        let _ = writeln!(report, "#");
        let _ = writeln!(
            report,
            "#  {} (0x{:08X}) at pc=0x{:016X}",
            code_name(code),
            code,
            fault_addr
        );
        if !op.is_empty() {
            let _ = writeln!(
                report,
                "#  Faulting access: {} at address 0x{:016X}",
                op, data_addr
            );
        }
        let _ = writeln!(report, "#  pid={} tid={}", pid, super::get_tid());
        let _ = writeln!(report, "#  thread: \"{}\"", tname);
        let _ = writeln!(report, "#  exe module base: 0x{:016X}", module_base);
        if module_base != 0 && fault_addr >= module_base {
            let _ = writeln!(report, "#  faulting RVA: 0x{:X}", fault_addr - module_base);
        }
        if let Some(name) = cratonvm_jit::lookup_jit_method_name(fault_addr) {
            let _ = writeln!(report, "#  faulting JIT method: {}", name);
        }
        let _ = writeln!(report, "#");
        let _ = writeln!(report, "Native frames (most recent call first) [raw]:");
        for (i, &a) in raw.iter().take(n).enumerate() {
            let a = a as usize;
            if module_base != 0 && a >= module_base && a < module_base + 0x8000_0000 {
                let _ = writeln!(
                    report,
                    "  {:2}: 0x{:016X}  (exe+0x{:X})",
                    i,
                    a,
                    a - module_base
                );
            } else if let Some(name) = cratonvm_jit::lookup_jit_method_name(a) {
                // spring-bug-11: name the JIT method whose code range contains
                // this return address (requires CRATONVM_DBG_JIT_NAMES=1).
                let _ = writeln!(report, "  {:2}: 0x{:016X}  (jit: {})", i, a, name);
            } else {
                let _ = writeln!(report, "  {:2}: 0x{:016X}  (external/jit)", i, a);
            }
        }
        let _ = writeln!(report, "#");

        // Register dump (x64 CONTEXT). The GPRs pinpoint which value was used
        // as a bad pointer / branch target — for an `execute` fault, the
        // register whose value == the faulting address is the corrupted call
        // target, and rsp/rbp distinguish a corrupted-return-address `ret`
        // from an indirect `call reg`.
        let ctx = (*info).context_record as *const u8;
        if !ctx.is_null() {
            unsafe fn rd(ctx: *const u8, off: usize) -> u64 {
                core::ptr::read_unaligned(ctx.add(off) as *const u64)
            }
            let _ = writeln!(report, "Registers:");
            // x86-64 CONTEXT integer-register byte offsets (winnt.h).
            let names_offs: [(&str, usize); 17] = [
                ("rax", 0x78),
                ("rcx", 0x80),
                ("rdx", 0x88),
                ("rbx", 0x90),
                ("rsp", 0x98),
                ("rbp", 0xA0),
                ("rsi", 0xA8),
                ("rdi", 0xB0),
                ("r8", 0xB8),
                ("r9", 0xC0),
                ("r10", 0xC8),
                ("r11", 0xD0),
                ("r12", 0xD8),
                ("r13", 0xE0),
                ("r14", 0xE8),
                ("r15", 0xF0),
                ("rip", 0xF8),
            ];
            for chunk in names_offs.chunks(4) {
                let mut line = String::new();
                for (name, off) in chunk {
                    let _ = write!(line, "  {:>3}=0x{:016X}", name, unsafe { rd(ctx, *off) });
                }
                let _ = writeln!(report, "{}", line);
            }
        }

        // Name the JIT method whose body made the bad call, if recorded
        // (requires CRATONVM_DBG_JIT_PUTFIELD=1 to populate the thread-local).
        let callee = crate::jit::helpers::current_jit_callee_for_crash();
        if !callee.is_empty() {
            let _ = writeln!(report, "current_jit_callee = {}", callee);
        }

        // Name the innermost native (Rust) callback active on THIS (faulting)
        // thread, if the lightweight per-thread tracker is armed
        // (CRATONVM_TRACK_NATIVE=1). For the HIB-CV-37 lambda/native GC-stranding
        // class this is the stream/collection intrinsic that was driving a lambda
        // while holding a now-stale Rust-local ObjectRef — i.e. the native that
        // needs `pin_native_root`/`read_native_pin`. Print both the absolute fn
        // pointer and its exe-relative RVA so it can be symbolized offline with
        // `CRATONVM_SYMBOLIZE` against the SAME binary (closure symbols survive in
        // the profsym/debug build). Best-effort `name_of` too (resolves only if
        // the registry's ring names were flushed). Zero cost when disabled.
        let cur_native = cratonvm_native_api::native_ring::innermost_native_cb();
        if cur_native != 0 {
            let name = cratonvm_native_api::native_ring::name_of(cur_native)
                .unwrap_or_else(|| "<unflushed; symbolize the RVA>".to_string());
            if module_base != 0 && cur_native >= module_base {
                let _ = writeln!(
                    report,
                    "current_native (faulting thread) = exe+0x{:X}  {}",
                    cur_native - module_base,
                    name
                );
            } else {
                let _ = writeln!(
                    report,
                    "current_native (faulting thread) = 0x{:016X}  {}",
                    cur_native, name
                );
            }
        }

        // Disassembly aid: dump the instruction bytes immediately *before* each
        // JIT-region return address (most-recent first). A return address points
        // just past the CALL that pushed it, so the preceding ~32 bytes contain
        // the faulting indirect call and whatever set up its target register —
        // exactly what is needed to see why R10 held a bad pointer.
        // VirtualQuery-guarded so a wild RA can't re-fault the handler.
        let dump_before = |report: &mut String, ra: usize, label: &str| {
            const PRE: usize = 32;
            if ra <= PRE {
                return;
            }
            let start = ra - PRE;
            if !unsafe { is_readable(start, PRE) } {
                let _ = writeln!(report, "  [{}] 0x{:016X}: <unreadable>", label, ra);
                return;
            }
            let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, PRE) };
            let mut hex = String::new();
            for b in bytes {
                let _ = write!(hex, "{:02X} ", b);
            }
            let _ = writeln!(
                report,
                "  [{}] bytes [RA-0x{:X}..RA] @0x{:016X}:\n    {}",
                label, PRE, start, hex
            );
        };
        if n >= 2 {
            let _ = writeln!(report, "Code bytes preceding JIT return addresses:");
            for (i, &a) in raw.iter().take(n).enumerate().skip(1).take(3) {
                let a = a as usize;
                if !(module_base != 0 && a >= module_base && a < module_base + 0x8000_0000) {
                    dump_before(&mut report, a, &format!("frame{}", i));
                }
            }
        }

        // Dump the memory the bad call dereferenced through R10 (the MIC/PIC
        // slot the dispatch used `CALL [R10+8]` on): [R10-0x10 .. R10+0x20].
        if !ctx.is_null() {
            let r10 = unsafe { core::ptr::read_unaligned(ctx.add(0xC8) as *const u64) } as usize;
            let win_start = r10.wrapping_sub(0x10);
            if unsafe { is_readable(win_start, 0x30) } {
                let qs = unsafe { core::slice::from_raw_parts(win_start as *const u64, 6) };
                let _ = writeln!(report, "Memory around R10 (0x{:016X}):", r10);
                for (j, q) in qs.iter().enumerate() {
                    let off = -0x10i64 + (j as i64) * 8;
                    let _ = writeln!(report, "    [R10{:+#x}] = 0x{:016X}", off, q);
                }
            } else {
                let _ = writeln!(report, "Memory around R10 (0x{:016X}): <unreadable>", r10);
            }
        }

        // spring-bug-10: dump the live ShadowStack fields (top@+0x1B8,
        // end@+0x1C0, base@+0x1C8) via R10 (= the cached thread ptr the faulting
        // reload reloaded), so a corrupt `top` vs a corrupt savebase slot can be
        // told apart.
        if !ctx.is_null() {
            let r10 = unsafe { core::ptr::read_unaligned(ctx.add(0xC8) as *const u64) } as usize;
            let ss = r10.wrapping_add(0x1B8);
            if r10 != 0 && unsafe { is_readable(ss, 0x18) } {
                let qs = unsafe { core::slice::from_raw_parts(ss as *const u64, 3) };
                let _ = writeln!(
                    report,
                    "ShadowStack @ R10+0x1B8: top=0x{:016X} end=0x{:016X} base=0x{:016X}",
                    qs[0], qs[1], qs[2]
                );
            }
        }

        // spring-bug-10: dump the current JIT frame's slots [rbp-0x40 .. rbp]
        // so the shadow-stack slots (thread @ rbp-thread_off, savetop, savebase)
        // and the locals are visible at the fault — to see whether savebase was
        // ever written or got corrupted by the safepoint call/GC.
        if !ctx.is_null() {
            let rbp = unsafe { core::ptr::read_unaligned(ctx.add(0xA0) as *const u64) } as usize;
            let win_start = rbp.wrapping_sub(0x40);
            if rbp != 0 && unsafe { is_readable(win_start, 0x48) } {
                let qs = unsafe { core::slice::from_raw_parts(win_start as *const u64, 9) };
                let _ = writeln!(report, "Frame slots around RBP (0x{:016X}):", rbp);
                for (j, q) in qs.iter().enumerate() {
                    let off = -0x40i64 + (j as i64) * 8;
                    let _ = writeln!(report, "    [RBP{:+#06x}] = 0x{:016X}", off, q);
                }
            }
        }

        let _ = writeln!(report, "#");
        let _ = writeln!(
            report,
            "# Symbolize offline with the SAME binary:\n\
             #   CRATONVM_SYMBOLIZE=<comma-separated exe+0x RVAs> cratonvm X"
        );

        // (2->3) Single-shot file write FIRST (most reliable sink).
        if let Ok(mut f) =
            std::fs::File::create(std::path::PathBuf::from(format!("hs_err_pid{}.log", pid)))
        {
            let _ = std::io::Write::write_all(&mut f, report.as_bytes());
            let _ = std::io::Write::flush(&mut f);
        }
        // Mirror to stderr in one write.
        {
            let mut err = std::io::stderr().lock();
            let _ = std::io::Write::write_all(&mut err, report.as_bytes());
            let _ = std::io::Write::flush(&mut err);
        }

        // (4) Best-effort in-process symbolization. dbghelp can itself re-fault
        // on a wrecked thread state; the raw report above is already persisted,
        // so a death here loses nothing actionable.
        let process = unsafe { GetCurrentProcess() };
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
            SymInitializeW(process, core::ptr::null(), 0);
        }
        let mut sym_report = String::with_capacity(2048);
        let _ = writeln!(sym_report, "Native frames [symbolized, best-effort]:");
        for (i, &a) in raw.iter().take(n).enumerate() {
            if let Some(name) = unsafe { symbolize(process, a as usize) } {
                let _ = writeln!(sym_report, "  {:2}: {}", i, name);
            }
        }
        {
            let mut err = std::io::stderr().lock();
            let _ = std::io::Write::write_all(&mut err, sym_report.as_bytes());
            let _ = std::io::Write::flush(&mut err);
        }

        // Continue searching — let the OS finish terminating the process the
        // same way it would have without us. We only added the diagnostic.
        EXCEPTION_CONTINUE_SEARCH
    }

    /// Register the vectored exception handler. Idempotent.
    pub fn install() {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        // `first = 1` -> run before any frame-based (SEH) handlers, so we see
        // the fault even if some inner frame would otherwise swallow it. We
        // still return CONTINUE_SEARCH, so a legitimate handler downstream is
        // unaffected.
        unsafe {
            AddVectoredExceptionHandler(1, vectored_handler);
        }
    }

    /// Symbolize exe-relative RVAs in a NORMAL (non-crash) context, where
    /// dbghelp is reliable — used by the `CRATONVM_SYMBOLIZE` startup hook to
    /// resolve the raw addresses the VEH prints for a multi-threaded crash
    /// (whose racy teardown truncates in-handler symbolization). Resolves each
    /// `module_base + rva` against the running exe's own symbols/PDB.
    pub fn symbolize_rvas(rvas: &[usize]) -> Vec<(usize, Option<String>)> {
        let module_base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let process = unsafe { GetCurrentProcess() };
        let verbose = std::env::var("CRATONVM_SYMBOLIZE_DBG").as_deref() == Ok("1");
        // Build a search path = the exe's own directory, so dbghelp finds the
        // co-located cratonvm.pdb regardless of cwd / _NT_SYMBOL_PATH.
        let mut exe_path = [0u16; 1024];
        let exe_len =
            unsafe { GetModuleFileNameW(core::ptr::null_mut(), exe_path.as_mut_ptr(), 1024) }
                as usize;
        // Strip the file name to get the directory (find last '\\').
        let dir_end = exe_path[..exe_len]
            .iter()
            .rposition(|&c| c == b'\\' as u16)
            .unwrap_or(exe_len);
        let mut search_path: Vec<u16> = exe_path[..dir_end].to_vec();
        search_path.push(0);
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
            let ok = SymInitializeW(process, search_path.as_ptr(), 1);
            if verbose {
                let dir = String::from_utf16_lossy(&exe_path[..dir_end]);
                eprintln!(
                    "[sym] SymInitialize ok={} base=0x{:X} dir={} err={}",
                    ok,
                    module_base,
                    dir,
                    GetLastError()
                );
            }
            // Explicitly load the exe's own module + PDB so SymFromAddr can
            // resolve addresses inside it. invade=FALSE above keeps init cheap;
            // we load just the main module here. image_name = full exe path.
            let mut path_buf = [0u16; 1024];
            let len = GetModuleFileNameW(
                core::ptr::null_mut(),
                path_buf.as_mut_ptr(),
                path_buf.len() as u32,
            );
            let loaded = if len > 0 {
                // DllSize=0 leaves SymFromAddr unable to tell whether a
                // queried address falls within this module's range (it
                // reliably fails with ERROR_INVALID_ADDRESS/487 even though
                // SymLoadModuleExW itself reports success) — pass a
                // generous over-estimate; SymLoadModuleExW only needs the
                // registered range to COVER real addresses, not match the
                // image size exactly.
                const DLL_SIZE_OVERESTIMATE: u32 = 0x8000000; // 128 MiB
                SymLoadModuleExW(
                    process,
                    core::ptr::null_mut(),
                    path_buf.as_ptr(),
                    core::ptr::null(),
                    module_base as u64,
                    DLL_SIZE_OVERESTIMATE,
                    core::ptr::null_mut(),
                    0,
                )
            } else {
                0
            };
            if verbose {
                eprintln!(
                    "[sym] SymLoadModuleExW -> base=0x{:X} err={}",
                    loaded,
                    GetLastError()
                );
            }
        }
        rvas.iter()
            .map(|&rva| {
                let abs = module_base.wrapping_add(rva);
                let r = unsafe { symbolize(process, abs) };
                if verbose && r.is_none() {
                    eprintln!("[sym] SymFromAddr 0x{:X} failed err={}", abs, unsafe {
                        GetLastError()
                    });
                }
                (rva, r)
            })
            .collect()
    }
}

/// Symbolize a list of exe-relative RVAs against the running binary's symbols.
/// Windows-only diagnostic helper for the `CRATONVM_SYMBOLIZE` startup hook;
/// returns `(rva, Some("function+0xNN"))` per input. Empty on non-Windows.
pub fn symbolize_rvas(_rvas: &[usize]) -> Vec<(usize, Option<String>)> {
    #[cfg(windows)]
    {
        windows_fault::symbolize_rvas(_rvas)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Install a process-wide hardware-fault diagnostic handler.
///
/// On Windows this registers a vectored exception handler that prints the
/// faulting PC + a symbolized backtrace on an access violation / illegal
/// instruction / etc., then lets the process terminate normally (see
/// [`windows_fault`]). On other platforms it is currently a no-op (the Unix
/// signal path lives in [`install_crash_handler`]).
///
/// Unlike [`install_crash_handler`], this does NOT touch the Rust panic hook,
/// so it can be called alongside a caller that installs its own panic hook
/// (as `vm-cli` does).
pub fn install_hardware_fault_handler() {
    #[cfg(windows)]
    windows_fault::install();
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
            // SECURITY FIX (V17): A report is already being (or has been)
            // written by the first panic — do NOT write a second one (that
            // race produced interleaved/garbled hs_err_pid<pid>.log output).
            // Still chain to the previous hook so this panic's default output
            // fires and the process aborts exactly as before; we just skip the
            // report-writing section.
            prev(panic_info);
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
                eprintln!(
                    "# CratonVM crash handler: failed to write {}: {}",
                    filename, e
                );
            }
        }

        // Invoke the previous hook so the default Rust output is preserved.
        prev(panic_info);

        // SECURITY FIX (V17): Do NOT reset CRASH_IN_PROGRESS here. The previous
        // code stored `false`, which let two near-simultaneous panics both pass
        // the CAS guard above and race to write the same hs_err_pid<pid>.log,
        // producing interleaved output. We now latch the guard for the life of
        // the process — matching the Windows VEH `HANDLING` latch (one report
        // per process). Any subsequent/concurrent panic short-circuits at the
        // guard above: it still chains to `prev` (so the process aborts) but
        // does not write a second, interleaved report.
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

    /// Read the calling thread's `errno`. Async-signal-safe: POSIX guarantees
    /// `errno` is thread-local and that *reading* it is permitted from a signal
    /// handler. We deliberately only ever READ it here (never set it), so there
    /// is no reentrancy hazard — the pointer returned by `__errno_location` /
    /// `__error` is a stable per-thread address obtained without allocation.
    fn errno() -> i32 {
        // The accessor symbol differs per libc:
        //   glibc/musl (Linux)            -> __errno_location
        //   macOS/iOS/FreeBSD/Dragonfly   -> __error
        //   Android/NetBSD/OpenBSD        -> __errno
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location()
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        unsafe {
            *libc::__error()
        }
        #[cfg(any(target_os = "android", target_os = "openbsd", target_os = "netbsd"))]
        unsafe {
            *libc::__errno()
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "android",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        {
            // Unknown Unix: we cannot read errno portably, so treat every
            // short/failed write as non-retryable (return 0, never == EINTR).
            0
        }
    }

    /// Write the entire `bytes` slice to `fd` using raw `write(2)`, correctly
    /// resuming from the unwritten offset on a short (partial) write and
    /// retrying the remainder on `EINTR`.
    ///
    /// `write(2)` may return fewer bytes than requested (a short write) or fail
    /// with `EINTR` if a signal interrupts it before any byte is transferred.
    /// In both cases the loop advances `off` by the number of bytes ACTUALLY
    /// written and re-issues the syscall for the remaining tail, so the crash
    /// report is never truncated. Any other error (or a `write` returning 0)
    /// is unrecoverable and we give up silently — a partial report is better
    /// than spinning forever inside a crashing process.
    ///
    /// Async-signal-safe: `write` is on the POSIX whitelist, and [`errno`] is
    /// read-only (see its doc).
    pub fn write_all(fd: i32, bytes: &[u8]) {
        let mut off = 0usize;
        while off < bytes.len() {
            let ptr = unsafe { bytes.as_ptr().add(off) } as *const libc::c_void;
            let want = bytes.len() - off;
            let r = unsafe { libc::write(fd, ptr, want) };
            if r > 0 {
                // Partial or full write: advance by exactly what was written
                // and continue with the remaining tail.
                off += r as usize;
            } else if r < 0 {
                // EINTR -> the syscall was interrupted before transferring any
                // byte; retry the same remainder. Any other errno is a real,
                // non-transient failure -> stop.
                if errno() == libc::EINTR {
                    continue;
                }
                return;
            } else {
                // r == 0: no progress is possible (e.g. fd closed / zero-length
                // device). Avoid an infinite loop.
                return;
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
    const HDR: &[u8] =
        b"\n#\n# A fatal error has been detected by the CratonVM Runtime Environment:\n#  ";
    const AT_PC: &[u8] = b" at pc=0x0, pid=";
    const TID_LBL: &[u8] = b", tid=";
    const NL_REPORT: &[u8] = b"\n#  Error report saved to: ";
    const FOOTER: &[u8] = b"\n#\n";
    const FILE_PREFIX: &[u8] = b"hs_err_pid";
    const FILE_SUFFIX: &[u8] = b".log";

    // Cache the pid in normal context so the handler doesn't need libc::getpid
    // (which is technically signal-safe, but we minimize syscalls).
    async_signal_safe::CACHED_PID.store(std::process::id() as i32, Ordering::Relaxed);

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
        let parts: [&[u8]; 3] = [FILE_PREFIX, &pid_buf[..pid_len], FILE_SUFFIX];
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
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &pid_buf[..pid_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, TID_LBL);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &tid_buf[..tid_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, NL_REPORT);
        // Write filename without the trailing NUL.
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &filename[..fpos]);
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
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                year, month, day, hours, minutes, seconds
            )
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
            content
                .lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_string()
                })
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
        format!(
            "{} cores",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        )
    }
}

#[cfg(target_os = "linux")]
fn get_cpu_info_linux() -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
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
            format!(
                "total={} MB, available={} MB",
                total_kb / 1024,
                avail_kb / 1024
            )
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
        assert_eq!(
            info.panic_message.as_deref(),
            Some("null pointer dereference")
        );
        assert_eq!(
            info.panic_location.as_deref(),
            Some("vm/src/runtime/interpreter.rs:42")
        );
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
        assert!(
            report.contains("A fatal error has been detected by the CratonVM Runtime Environment")
        );
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
        assert!(
            info.contains("core") || info.contains("thread") || info.contains("unknown"),
            "CPU info should mention cores/threads: {}",
            info
        );
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

    // The async-signal-safe `write_all` only exists on Unix.
    #[cfg(unix)]
    #[test]
    fn write_all_handles_short_writes_via_pipe() {
        use super::async_signal_safe::write_all;
        use std::io::Read;

        // A pipe's kernel buffer is finite, so writing a payload larger than
        // the buffer forces `write(2)` to return short — exactly the partial
        // write the resume-from-offset loop must handle. A reader thread drains
        // the pipe so the writes can complete.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // Build a payload comfortably larger than a typical 64 KiB pipe buffer
        // and with a recognizable, position-dependent byte pattern so any
        // dropped/duplicated/reordered chunk is detected.
        let payload: Vec<u8> = (0..(512 * 1024)).map(|i| (i % 251) as u8).collect();

        let reader = {
            let expected_len = payload.len();
            std::thread::spawn(move || {
                let mut f = unsafe {
                    <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(read_fd)
                };
                let mut got = Vec::with_capacity(expected_len);
                f.read_to_end(&mut got).expect("read pipe");
                got
            })
        };

        write_all(write_fd, &payload);
        // Close the write end so the reader sees EOF.
        unsafe { libc::close(write_fd) };

        let got = reader.join().expect("reader thread");
        assert_eq!(got.len(), payload.len(), "byte count mismatch");
        assert_eq!(got, payload, "payload corrupted across short writes");
    }

    // Writing to a closed/invalid fd must return promptly (real error, not
    // EINTR) rather than spinning forever.
    #[cfg(unix)]
    #[test]
    fn write_all_on_bad_fd_returns() {
        use super::async_signal_safe::write_all;
        // -1 is never a valid fd; `write` returns EBADF, which is not EINTR,
        // so the loop must give up immediately.
        write_all(-1, b"this should not hang");
    }
}
