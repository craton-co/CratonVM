//! DBG (CRATONVM_DBG_HANGWALK=<secs>): native-stack-walk watchdog.
//!
//! When the env var is set, [`arm_from_env`] (called once from the main
//! thread at startup) records the main thread id and spawns a watchdog
//! thread. After `<secs>` seconds it suspends the main thread, reads its
//! `CONTEXT` (Rip/Rbp/Rsp), and prints the main thread's NATIVE Rust stack:
//!
//!   * an RBP frame-pointer walk (clean, when frame pointers are present —
//!     debug builds keep them), and
//!   * a conservative scan of the top of stack for any 8-aligned word that
//!     dbghelp resolves to a function (robust even without frame pointers),
//!
//! symbolizing every address via dbghelp. This is the tool of last resort
//! for a *single-threaded* hang where the interpreter frame dump only shows
//! the last Java safepoint and the real park is below it in Rust code.
//!
//! Off by default and Windows-only; a no-op everywhere else.

#[cfg(windows)]
pub fn arm_from_env() {
    let Ok(v) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_HANGWALK") else {
        return;
    };
    let secs = v.trim().parse::<u64>().ok().filter(|s| *s > 0).unwrap_or(7);
    imp::arm(secs);
}

#[cfg(not(windows))]
pub fn arm_from_env() {}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

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

    static MAIN_TID: AtomicU32 = AtomicU32::new(0);

    pub fn arm(secs: u64) {
        let tid = unsafe { GetCurrentThreadId() };
        MAIN_TID.store(tid, Ordering::SeqCst);
        eprintln!("[stwwatch] armed: will native-stack-walk main tid={tid} after {secs}s");
        std::thread::Builder::new()
            .name("stwwatch".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(secs));
                unsafe { dump_all() };
                std::process::abort();
            })
            .ok();
    }

    /// Enumerate every thread in this process and dump each one's native
    /// stack (skipping the watchdog thread itself — suspending self hangs).
    unsafe fn dump_all() {
        let main_tid = MAIN_TID.load(Ordering::SeqCst);
        let self_tid = GetCurrentThreadId();
        let pid = GetCurrentProcessId();
        let exe_base = GetModuleHandleW(std::ptr::null()) as usize;
        let process = GetCurrentProcess();
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
        // dbghelp may already be initialized by the crash handler; in that
        // case SymInitializeW fails — refresh the module list either way so
        // the exe's own PDB is loaded for symbolization.
        SymInitializeW(process, std::ptr::null(), 1);
        SymRefreshModuleList(process);
        eprintln!("[stwwatch] === enumerating threads of pid={pid} (main-vm tid={main_tid}) ===");
        eprintln!("[stwwatch] (symbolize exe+RVA frames via: CRATONVM_SYMBOLIZE=0xRVA1,0xRVA2 cratonvm.exe)");

        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == -1 || snap == 0 {
            eprintln!("[stwwatch] CreateToolhelp32Snapshot failed");
            return;
        }
        let mut e: ThreadEntry32 = core::mem::zeroed();
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = Thread32First(snap, &mut e);
        while ok != 0 {
            if e.th32_owner_process_id == pid && e.th32_thread_id != self_tid {
                dump_thread(
                    process,
                    exe_base,
                    e.th32_thread_id,
                    e.th32_thread_id == main_tid,
                );
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = Thread32Next(snap, &mut e);
        }
        CloseHandle(snap);
    }

    /// Fault-safe read of one usize from `addr` via ReadProcessMemory
    /// (returns None on unmapped/guard pages instead of faulting).
    unsafe fn rd(process: *mut c_void, addr: usize) -> Option<usize> {
        let mut v: usize = 0;
        let mut got: usize = 0;
        let ok = ReadProcessMemory(
            process,
            addr as *const c_void,
            &mut v as *mut usize as *mut c_void,
            8,
            &mut got,
        );
        if ok != 0 && got == 8 {
            Some(v)
        } else {
            None
        }
    }

    unsafe fn dump_thread(process: *mut c_void, exe_base: usize, tid: u32, is_main: bool) {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return;
        }
        SuspendThread(h);

        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
            ResumeThread(h);
            CloseHandle(h);
            return;
        }
        let rip = *(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize;
        let rbp = *(ctx.0.as_ptr().add(OFF_RBP) as *const u64) as usize;
        let rsp = *(ctx.0.as_ptr().add(OFF_RSP) as *const u64) as usize;
        let tag = if is_main { " <main-vm>" } else { "" };
        eprintln!("[stwwatch] === tid={tid}{tag} parked at rip=0x{rip:x} rbp=0x{rbp:x} rsp=0x{rsp:x} exe_base=0x{exe_base:x} ===");

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

        // ── RBP frame-pointer walk ─────────────────────────────────────────
        eprintln!("[stwwatch] --- RBP frame-pointer walk ---");
        let mut fp = rbp;
        let mut frame = 0usize;
        show(frame, rip);
        let stack_hi = rsp.saturating_add(16 << 20);
        while frame < 80 {
            if fp == 0 || fp & 0x7 != 0 || fp < rsp || fp >= stack_hi {
                break;
            }
            let (Some(ret), Some(next)) = (rd(process, fp + 8), rd(process, fp)) else {
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
        // Bound the scan to the committed stack region via VirtualQuery so a
        // read can never cross the guard page. Print every 8-aligned word in
        // the exe code range as exe+RVA (resolve offline).
        eprintln!("[stwwatch] --- conservative stack scan (rsp..+48KiB, exe frames) ---");
        // Only exe-range words reach dbghelp (avoids faulting on garbage); the
        // stack reads stay within the already-committed used region above rsp.
        let region_end = rsp.saturating_add(48 << 10);
        let mut p = rsp & !0x7;
        let mut printed = 0;
        let mut last = 0usize;
        while p + 8 <= region_end && printed < 100 {
            let word = match rd(process, p) {
                Some(w) => w,
                None => {
                    p += 8;
                    continue;
                }
            };
            if word >= exe_base && word < exe_base + (512 << 20) && word != last {
                let rva = word - exe_base;
                let name = sym_opt(process, word).unwrap_or_default();
                eprintln!("[stwwatch] @+0x{:<5x} exe+0x{rva:x}  {name}", p - rsp);
                last = word;
                printed += 1;
            }
            p += 8;
        }

        ResumeThread(h);
        CloseHandle(h);
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
}
