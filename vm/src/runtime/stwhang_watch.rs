//! DBG (CRATONVM_DBG_HANGWALK=<secs>[,continue|,repeat]): native-stack-walk
//! watchdog.
//!
//! When the env var is set, [`arm_from_env`] (called once from the main
//! thread at startup) records the main thread id and spawns a watchdog
//! thread. After `<secs>` seconds it dumps every thread of the process (the
//! main-vm thread tagged `<main-vm>`): its `CONTEXT` (Rip/Rbp/Rsp) and its
//! NATIVE Rust stack,
//!
//!   * an RBP frame-pointer walk (clean, when frame pointers are present —
//!     debug builds keep them), and
//!   * a conservative scan of the top of stack for any 8-aligned word that
//!     dbghelp resolves to a function (robust even without frame pointers),
//!
//! symbolizing every address via dbghelp; a `rip` or stack word inside
//! compiled code is labelled with its JIT buffer, and with its method when
//! `CRATONVM_DBG_JIT_NAMES=1` (round 13 wave 10, lane spin). This is the tool
//! of last resort for a hang where the interpreter frame dump only shows the
//! last Java safepoint and the real park is below it in Rust code.
//!
//! # A snapshot, and what happens after it (gci i3/hd)
//!
//! Page `gci-i2st-hangwalk-dump-is-not-a-snapshot-and-aborts-the-run-FIXED-20261012`:
//! the dump used to suspend, read, print and resume one thread at a time, so
//! its threads were sampled at different instants (a progressing pause read as
//! a stuck one), and then it ABORTED the process without saying so (an abort
//! read as "the run completed after the watchdog"). Now:
//!
//!   * every buffer is allocated first; then ALL threads are suspended
//!     together, each one's context and the 128 KiB of stack above its `rsp`
//!     are copied with `GetThreadContext` / `ReadProcessMemory` only (no
//!     allocation, no print, no dbghelp, no JIT-registry lock while anything
//!     is suspended: a suspended thread may hold the allocator, the stderr
//!     lock or the JIT table lock), and all are resumed; the walks and the
//!     symbols are then read from the copies. Each `=== tid=` line carries its
//!     capture time `t+<ms>` since the snapshot began, and the snapshot's
//!     whole suspended span is printed;
//!   * the header says the process aborts afterwards, and `dump took <ms> ms;
//!     aborting` is printed before `std::process::abort()`. The abort is kept
//!     by default (it is what makes the tool usable as a battery timeout), but
//!     `,continue` dumps once and lets the run go on, and `,repeat` dumps
//!     every `<secs>` seconds and never aborts -- the way to tell a progressing
//!     regime from a stuck one (compare consecutive dumps).
//!
//! Off by default and Windows-only; a no-op everywhere else.

/// What the watchdog does after a dump (gci i3/hd).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AfterDump {
    /// `std::process::abort()` (the default, and the only behaviour before
    /// gci i3/hd).
    Abort,
    /// `,continue`: dump once, then let the run go on.
    Continue,
    /// `,repeat`: dump every `<secs>` seconds, never abort.
    Repeat,
}

/// `CRATONVM_DBG_HANGWALK=<secs>[,continue|,repeat]` (gci i3/hd). `<secs>`
/// not a positive integer reads as 7 (as before); an unknown word after a
/// comma is ignored, so the old `<secs>` form keeps its meaning. Pure.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_spec(value: &str) -> (u64, AfterDump) {
    let mut parts = value.split(',');
    let secs = parts
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(7);
    let mut after = AfterDump::Abort;
    for word in parts {
        match word.trim() {
            "continue" => after = AfterDump::Continue,
            "repeat" => after = AfterDump::Repeat,
            _ => {}
        }
    }
    (secs, after)
}

/// The machine word at `addr` in a stack copy whose first byte is the word at
/// `base`, or `None` outside the copy (gci i3/hd: the walks read the snapshot,
/// never the live stack). Pure.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn copied_word(base: usize, bytes: &[u8], addr: usize) -> Option<usize> {
    let off = addr.checked_sub(base)?;
    let end = off.checked_add(8)?;
    let w = bytes.get(off..end)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(w);
    Some(u64::from_le_bytes(a) as usize)
}

#[cfg(windows)]
pub fn arm_from_env() {
    let Ok(v) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_HANGWALK") else {
        return;
    };
    let (secs, after) = parse_spec(&v);
    imp::arm(secs, after);
}

#[cfg(not(windows))]
pub fn arm_from_env() {}

#[cfg(windows)]
mod imp {
    use super::{copied_word, AfterDump};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
        fn SuspendThread(h: isize) -> u32;
        fn ResumeThread(h: isize) -> u32;
        fn GetThreadContext(h: isize, ctx: *mut u8) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(h: isize) -> i32;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn ReadProcessMemory(
            process: *mut c_void,
            base: *const c_void,
            buf: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: isize, entry: *mut ThreadEntry32) -> i32;
    }

    // THREADENTRY32 (28 bytes).
    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

    #[link(name = "dbghelp")]
    extern "system" {
        fn SymSetOptions(options: u32) -> u32;
        fn SymInitializeW(process: *mut c_void, search_path: *const u16, invade: i32) -> i32;
        fn SymRefreshModuleList(process: *mut c_void) -> i32;
        fn SymFromAddr(
            process: *mut c_void,
            address: u64,
            displacement: *mut u64,
            symbol: *mut SymbolInfo,
        ) -> i32;
    }

    // Matches DbgHelp.h SYMBOL_INFO (see crash_handler.rs).
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

    const THREAD_GET_CONTEXT: u32 = 0x0008;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const THREAD_QUERY_INFORMATION: u32 = 0x0040;
    const SYMOPT_UNDNAME: u32 = 0x0000_0002;
    const SYMOPT_DEFERRED_LOADS: u32 = 0x0000_0004;
    const SYMOPT_LOAD_LINES: u32 = 0x0000_0010;

    // x64 CONTEXT: 1232 bytes, 16-byte aligned. Well-known field offsets:
    // ContextFlags=0x30, Rsp=0x98, Rbp=0xA0, Rip=0xF8.
    const CTX_SIZE: usize = 1232;
    const OFF_FLAGS: usize = 0x30;
    const OFF_RSP: usize = 0x98;
    const OFF_RBP: usize = 0xA0;
    const OFF_RIP: usize = 0xF8;
    // CONTEXT_CONTROL | CONTEXT_INTEGER for AMD64.
    const CONTEXT_CONTROL_INTEGER: u32 = 0x0010_0001 | 0x0010_0002;

    /// Stack bytes copied above each thread's `rsp` while it is suspended
    /// (gci i3/hd). The RBP walk stops at the copy's end.
    const STACK_COPY: usize = 128 << 10;
    /// The conservative scan's reach above `rsp` (as before).
    const SCAN_REACH: usize = 48 << 10;
    const PAGE: usize = 4096;

    static MAIN_TID: AtomicU32 = AtomicU32::new(0);

    pub fn arm(secs: u64, after: AfterDump) {
        let tid = unsafe { GetCurrentThreadId() };
        MAIN_TID.store(tid, Ordering::SeqCst);
        let then = match after {
            AfterDump::Abort => "then ABORT the process",
            AfterDump::Continue => "then continue the run",
            AfterDump::Repeat => "and again every interval, never aborting",
        };
        eprintln!(
            "[stwwatch] armed: will snapshot every thread's native stack (main-vm tid={tid}) \
             after {secs}s, {then}"
        );
        std::thread::Builder::new()
            .name("stwwatch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(secs));
                let t0 = Instant::now();
                unsafe { dump_all(after) };
                let ms = t0.elapsed().as_millis();
                match after {
                    AfterDump::Abort => {
                        eprintln!("[stwwatch] dump took {ms} ms; aborting");
                        std::process::abort();
                    }
                    AfterDump::Continue => {
                        eprintln!("[stwwatch] dump took {ms} ms; continuing (,continue)");
                        return;
                    }
                    AfterDump::Repeat => {
                        eprintln!(
                            "[stwwatch] dump took {ms} ms; next dump in {secs}s (,repeat)"
                        );
                    }
                }
            })
            .ok();
    }

    /// One thread's state as the snapshot copied it.
    #[repr(C, align(16))]
    struct Ctx([u8; CTX_SIZE]);

    pub(super) struct Capture {
        pub(super) tid: u32,
        handle: isize,
        suspended: bool,
        pub(super) ctx_ok: bool,
        ctx: Box<Ctx>,
        pub(super) rip: usize,
        pub(super) rbp: usize,
        pub(super) rsp: usize,
        /// `rsp & !7`: the address of `stack[0]`.
        pub(super) base: usize,
        /// `STACK_COPY` bytes, allocated before any thread is suspended;
        /// `copied` of them are the thread's stack.
        pub(super) stack: Vec<u8>,
        pub(super) copied: usize,
        /// Microseconds from the snapshot's start to this thread's copy.
        pub(super) at_us: u64,
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            if self.handle != 0 {
                // SAFETY: a handle from `OpenThread`, closed once.
                unsafe { CloseHandle(self.handle) };
            }
        }
    }

    /// Copy up to `buf.len()` bytes of this process's memory from `start`,
    /// one page at a time, stopping at the first page that cannot be read
    /// (the stack's top, a guard page). Returns the bytes copied. Allocates
    /// nothing.
    unsafe fn copy_stack(process: *mut c_void, start: usize, buf: &mut [u8]) -> usize {
        let mut done = 0usize;
        while done < buf.len() {
            let addr = start + done;
            let n = (PAGE - addr % PAGE).min(buf.len() - done);
            let mut got = 0usize;
            let ok = ReadProcessMemory(
                process,
                addr as *const c_void,
                buf.as_mut_ptr().add(done) as *mut c_void,
                n,
                &mut got,
            );
            if ok == 0 || got != n {
                break;
            }
            done += n;
        }
        done
    }

    /// The snapshot (gci i3/hd): open every thread in `tids` and allocate its
    /// buffers; then suspend them ALL, copy each one's context and stack
    /// window, and resume them all. Between the first `SuspendThread` and the
    /// last `ResumeThread` nothing allocates, prints, locks or symbolizes.
    /// Returns the captures (in `tids` order) and the suspended span in
    /// microseconds.
    pub(super) unsafe fn snapshot(tids: &[u32], t0: Instant) -> (Vec<Capture>, u64) {
        let mut caps: Vec<Capture> = Vec::with_capacity(tids.len());
        for &tid in tids {
            let h = OpenThread(
                THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
                0,
                tid,
            );
            if h == 0 {
                continue;
            }
            caps.push(Capture {
                tid,
                handle: h,
                suspended: false,
                ctx_ok: false,
                ctx: Box::new(Ctx([0u8; CTX_SIZE])),
                rip: 0,
                rbp: 0,
                rsp: 0,
                base: 0,
                stack: vec![0u8; STACK_COPY],
                copied: 0,
                at_us: 0,
            });
        }
        let process = GetCurrentProcess();
        // ---- every thread suspended from here: no allocation, no print ----
        let suspend_start = Instant::now();
        for c in caps.iter_mut() {
            c.suspended = SuspendThread(c.handle) != u32::MAX;
        }
        for c in caps.iter_mut() {
            if !c.suspended {
                continue;
            }
            let ctx = c.ctx.0.as_mut_ptr();
            *(ctx.add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
            if GetThreadContext(c.handle, ctx) == 0 {
                continue;
            }
            c.ctx_ok = true;
            c.rip = *(ctx.add(OFF_RIP) as *const u64) as usize;
            c.rbp = *(ctx.add(OFF_RBP) as *const u64) as usize;
            c.rsp = *(ctx.add(OFF_RSP) as *const u64) as usize;
            c.base = c.rsp & !0x7;
            c.copied = copy_stack(process, c.base, &mut c.stack);
            c.at_us = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
        }
        for c in caps.iter() {
            if c.suspended {
                ResumeThread(c.handle);
            }
        }
        // ---- every thread running again ----
        let span = u64::try_from(suspend_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        (caps, span)
    }

    /// Every OS thread of this process but the caller (suspending oneself
    /// hangs), in Toolhelp order.
    unsafe fn process_threads() -> Option<Vec<u32>> {
        let pid = GetCurrentProcessId();
        let self_tid = GetCurrentThreadId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == -1 || snap == 0 {
            return None;
        }
        let mut tids = Vec::new();
        let mut e: ThreadEntry32 = core::mem::zeroed();
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = Thread32First(snap, &mut e);
        while ok != 0 {
            if e.th32_owner_process_id == pid && e.th32_thread_id != self_tid {
                tids.push(e.th32_thread_id);
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = Thread32Next(snap, &mut e);
        }
        CloseHandle(snap);
        Some(tids)
    }

    /// Snapshot every thread in this process, then print each one's native
    /// stack from its copy.
    unsafe fn dump_all(after: AfterDump) {
        let main_tid = MAIN_TID.load(Ordering::SeqCst);
        let pid = GetCurrentProcessId();
        let exe_base = GetModuleHandleW(std::ptr::null()) as usize;
        let process = GetCurrentProcess();
        let Some(tids) = process_threads() else {
            eprintln!("[stwwatch] CreateToolhelp32Snapshot failed");
            return;
        };
        let then = match after {
            AfterDump::Abort => "the process ABORTS after this dump",
            AfterDump::Continue => "the run continues after this dump",
            AfterDump::Repeat => "the run continues; the next dump follows",
        };
        eprintln!(
            "[stwwatch] === snapshot of pid={pid} (main-vm tid={main_tid}): suspending all {} \
             threads together, copying contexts and {} KiB stacks, symbolizing after resume; {then} ===",
            tids.len(),
            STACK_COPY >> 10
        );
        let t0 = Instant::now();
        let (caps, span_us) = snapshot(&tids, t0);
        eprintln!(
            "[stwwatch] snapshot: {} of {} threads captured; all held suspended for {span_us} us",
            caps.iter().filter(|c| c.ctx_ok).count(),
            tids.len()
        );
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
        // dbghelp may already be initialized by the crash handler; in that
        // case SymInitializeW fails — refresh the module list either way so
        // the exe's own PDB is loaded for symbolization.
        SymInitializeW(process, std::ptr::null(), 1);
        SymRefreshModuleList(process);
        eprintln!("[stwwatch] (symbolize exe+RVA frames via: CRATONVM_SYMBOLIZE=0xRVA1,0xRVA2 cratonvm.exe)");
        for c in &caps {
            print_capture(process, exe_base, c, c.tid == main_tid);
        }
    }

    unsafe fn print_capture(process: *mut c_void, exe_base: usize, c: &Capture, is_main: bool) {
        let tag = if is_main { " <main-vm>" } else { "" };
        let at = format!("t+{}.{:03}ms", c.at_us / 1000, c.at_us % 1000);
        if !c.ctx_ok {
            eprintln!("[stwwatch] === tid={}{tag} not captured (suspend or context failed) ===", c.tid);
            return;
        }
        let (rip, rbp, rsp) = (c.rip, c.rbp, c.rsp);
        eprintln!(
            "[stwwatch] === tid={}{tag} parked at rip=0x{rip:x} rbp=0x{rbp:x} rsp=0x{rsp:x} \
             exe_base=0x{exe_base:x} captured {at} ===",
            c.tid
        );
        // Round 13 wave 10 (lane spin): a thread spinning in COMPILED code has
        // a `rip` outside the exe, which dbghelp cannot name. Say which body.
        if let Some(label) = jit_label(rip, true) {
            eprintln!("[stwwatch] rip is in {label}");
        }
        let copy = &c.stack[..c.copied];
        let rd = |addr: usize| copied_word(c.base, copy, addr);

        let show = |frame: usize, addr: usize| {
            if addr >= exe_base && addr < exe_base + (512 << 20) {
                let rva = addr - exe_base;
                match sym_opt(process, addr) {
                    Some(n) => eprintln!("[stwwatch] #{frame:02} exe+0x{rva:x}  {n}"),
                    None => {
                        eprintln!("[stwwatch] #{frame:02} exe+0x{rva:x}  (RVA — symbolize offline)")
                    }
                }
            } else {
                eprintln!("[stwwatch] #{frame:02} 0x{addr:x}  {}", sym(process, addr));
            }
        };

        // ── RBP frame-pointer walk, over the copy ─────────────────────────
        eprintln!("[stwwatch] --- RBP frame-pointer walk ---");
        let mut fp = rbp;
        let mut frame = 0usize;
        show(frame, rip);
        while frame < 80 {
            if fp == 0 || fp & 0x7 != 0 || fp < rsp {
                break;
            }
            let (Some(ret), Some(next)) = (rd(fp + 8), rd(fp)) else {
                eprintln!(
                    "[stwwatch] (rbp walk left the {} KiB copied above rsp)",
                    c.copied >> 10
                );
                break;
            };
            if ret != 0 {
                frame += 1;
                show(frame, ret);
            }
            if next <= fp {
                break;
            }
            fp = next;
        }

        // ── Conservative stack scan (robust without frame pointers) ─────────
        // Every 8-aligned word of the copy within SCAN_REACH of rsp in the exe
        // code range, as exe+RVA (resolve offline), or inside compiled code.
        eprintln!("[stwwatch] --- conservative stack scan (rsp..+48KiB, exe frames) ---");
        let region_end = c.base.saturating_add(SCAN_REACH.min(c.copied));
        let mut p = c.base;
        let mut printed = 0;
        let mut last = 0usize;
        while p + 8 <= region_end && printed < 100 {
            let Some(word) = rd(p) else {
                break;
            };
            if word >= exe_base && word < exe_base + (512 << 20) && word != last {
                let rva = word - exe_base;
                let name = sym_opt(process, word).unwrap_or_default();
                eprintln!("[stwwatch] @+0x{:<5x} exe+0x{rva:x}  {name}", p - c.base);
                last = word;
                printed += 1;
            } else if word != last {
                // A return address into compiled code: which Java frames
                // (compiled bodies) this native stack runs under.
                if let Some(label) = jit_label(word, false) {
                    eprintln!("[stwwatch] @+0x{:<5x} {label}", p - c.base);
                    last = word;
                    printed += 1;
                }
            }
            p += 8;
        }
    }

    /// Which JIT code `addr` is in (round 13 wave 10, lane spin): the live
    /// executable buffer covering it and, when `CRATONVM_DBG_JIT_NAMES=1`
    /// populated the name registry, the method. `None` outside every buffer.
    /// Both registries are read with `try_lock`, so a thread suspended while
    /// holding one costs the name, never a deadlock of this dump; with
    /// `report_locked` a held registry says so instead of reading as "not JIT".
    /// gci i3/hd: called only after every thread is resumed.
    fn jit_label(addr: usize, report_locked: bool) -> Option<String> {
        let (base, capacity) = match cratonvm_jit::jit_code_region_covering(addr) {
            cratonvm_jit::JitRegionLookup::Found(base, capacity) => (base, capacity),
            cratonvm_jit::JitRegionLookup::NotFound => return None,
            cratonvm_jit::JitRegionLookup::Locked => {
                return report_locked
                    .then(|| "JIT code? (the code-region registry was locked)".to_string());
            }
        };
        let name = match cratonvm_jit::lookup_jit_method_name_detailed(addr) {
            cratonvm_jit::JitNameLookup::Found(name) => name,
            cratonvm_jit::JitNameLookup::Locked => "<name registry locked>".to_string(),
            cratonvm_jit::JitNameLookup::NotFound => {
                "<unnamed: rerun with CRATONVM_DBG_JIT_NAMES=1>".to_string()
            }
        };
        Some(format!(
            "jit buffer 0x{base:x}+0x{:x} (capacity 0x{capacity:x}) {name}",
            addr - base
        ))
    }

    unsafe fn sym(process: *mut c_void, addr: usize) -> String {
        sym_opt(process, addr).unwrap_or_else(|| "??".into())
    }

    unsafe fn sym_opt(process: *mut c_void, addr: usize) -> Option<String> {
        if addr == 0 {
            return None;
        }
        let mut buf = [0u64; 320];
        let s = buf.as_mut_ptr() as *mut SymbolInfo;
        (*s).size_of_struct = core::mem::size_of::<SymbolInfo>() as u32;
        (*s).max_name_len = 2000;
        let mut disp: u64 = 0;
        if SymFromAddr(process, addr as u64, &mut disp, s) == 0 {
            return None;
        }
        let name_off = core::mem::offset_of!(SymbolInfo, name);
        let name_ptr = (s as *const u8).add(name_off);
        let len = ((*s).name_len as usize).min(2000);
        let bytes = core::slice::from_raw_parts(name_ptr, len);
        let n = String::from_utf8_lossy(bytes).into_owned();
        if n.is_empty() {
            return None;
        }
        Some(if disp != 0 {
            format!("{n}+0x{disp:X}")
        } else {
            n
        })
    }

    #[cfg(test)]
    mod tests {
        /// gci i3/hd: the snapshot captures a parked thread (context, a stack
        /// copy holding its `rsp` word) and RESUMES it: the thread then wakes
        /// and exits, so the join returns.
        #[test]
        fn gci_i3hd_snapshot_captures_and_resumes_a_parked_thread() {
            let (tid_tx, tid_rx) = std::sync::mpsc::channel::<u32>();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let t = std::thread::spawn(move || {
                // SAFETY: a documented Win32 call with no arguments.
                tid_tx.send(unsafe { super::GetCurrentThreadId() }).unwrap();
                go_rx.recv().unwrap();
                7u32
            });
            let tid = tid_rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            // SAFETY: `tid` is a live thread of this process that is not the
            // caller.
            let (caps, span_us) =
                unsafe { super::snapshot(&[tid], std::time::Instant::now()) };
            assert_eq!(caps.len(), 1);
            let c = &caps[0];
            assert_eq!(c.tid, tid);
            assert!(c.ctx_ok, "context read while suspended");
            assert!(c.rsp != 0 && c.rip != 0);
            assert!(c.copied >= 8, "the page holding rsp is readable");
            assert!(super::super::copied_word(c.base, &c.stack[..c.copied], c.base).is_some());
            assert!(c.at_us <= span_us.saturating_add(1_000_000));
            drop(caps);
            go_tx.send(()).unwrap();
            assert_eq!(t.join().unwrap(), 7, "the thread was resumed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gci i3/hd: `<secs>[,continue|,repeat]`; the old `<secs>` form and a
    /// bad value keep their meaning (abort, 7 s default).
    #[test]
    fn gci_i3hd_hangwalk_spec_parses() {
        assert_eq!(parse_spec("120"), (120, AfterDump::Abort));
        assert_eq!(parse_spec(" 30 "), (30, AfterDump::Abort));
        assert_eq!(parse_spec("30,continue"), (30, AfterDump::Continue));
        assert_eq!(parse_spec("30, repeat"), (30, AfterDump::Repeat));
        assert_eq!(parse_spec("30,bogus"), (30, AfterDump::Abort));
        assert_eq!(parse_spec("x"), (7, AfterDump::Abort));
        assert_eq!(parse_spec("0,continue"), (7, AfterDump::Continue));
        assert_eq!(parse_spec(""), (7, AfterDump::Abort));
    }

    /// gci i3/hd: words are read from the copy only, little-endian, and
    /// nothing outside it.
    #[test]
    fn gci_i3hd_copied_word_reads_inside_the_copy_only() {
        let mut bytes = vec![0u8; 24];
        bytes[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(copied_word(0x1000, &bytes, 0x1008), Some(0x1122_3344_5566_7788));
        assert_eq!(copied_word(0x1000, &bytes, 0x1010), Some(0));
        assert_eq!(copied_word(0x1000, &bytes, 0x1011), None, "would cross the end");
        assert_eq!(copied_word(0x1000, &bytes, 0x0ff8), None, "below the copy");
        assert_eq!(copied_word(0x1000, &[], 0x1000), None);
        assert_eq!(copied_word(0, &bytes, usize::MAX), None, "no overflow");
    }
}
