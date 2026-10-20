//! DBG (CRATONVM_DBG_HEARTBEAT=<ms>): forensic liveness heartbeat.
//!
//! When the env var is set, [`arm_from_env`] (called once from the main
//! thread at startup) spawns a background thread that writes one line to
//! `heartbeat.log` (in the process's working directory) every `<ms>`
//! milliseconds, `flush()`ing and `sync_all()`ing after every write. The
//! goal is purely forensic: if the process is later killed by something
//! that leaves no other trace (no panic, no `hs_err` log, no exit banner —
//! see the `jetty-servlet-cumulative-crash` residual this was built for),
//! the LAST line of this file pins the last instant the process was known
//! to still be alive and scheduling threads, to within one heartbeat
//! interval. Comparing that timestamp against the last thing printed on
//! stdout/stderr tells you whether death was instantaneous (heartbeat and
//! stdout stop at the same moment — consistent with an external kill or a
//! `TerminateProcess`-class event) or whether something hung first (a gap
//! between the last heartbeat and the actual death — consistent with an
//! internal deadlock/livelock immediately preceding termination).
//!
//! Off by default; a no-op when the env var is unset. Deliberately simple
//! (no dependency on the interpreter/GC/JIT internals) so it keeps working
//! even if whatever is killing the process has already destabilized some
//! other subsystem.

use std::io::Write;
use std::time::Duration;

pub fn arm_from_env() {
    let Ok(v) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_HEARTBEAT") else {
        return;
    };
    let ms = v
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|m| *m > 0)
        .unwrap_or(100);
    eprintln!("[heartbeat] armed: writing heartbeat.log every {ms}ms");
    std::thread::Builder::new()
        .name("heartbeat".into())
        .spawn(move || run(ms))
        .ok();
}

fn run(ms: u64) {
    let mut counter: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_millis(ms));
        counter += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let thread_count = thread_count_best_effort();
        let line = format!("{counter}\t{now}\t{thread_count}\n");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("heartbeat.log")
        {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
            let _ = f.sync_all();
        }
    }
}

#[cfg(windows)]
fn thread_count_best_effort() -> u32 {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }

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

    unsafe {
        let pid = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == -1 || snap == 0 {
            return 0;
        }
        let mut e: ThreadEntry32 = core::mem::zeroed();
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = Thread32First(snap, &mut e);
        let mut count = 0u32;
        while ok != 0 {
            if e.th32_owner_process_id == pid {
                count += 1;
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = Thread32Next(snap, &mut e);
        }
        CloseHandle(snap);
        let _ = std::ptr::null::<c_void>();
        count
    }
}

#[cfg(not(windows))]
fn thread_count_best_effort() -> u32 {
    0
}
