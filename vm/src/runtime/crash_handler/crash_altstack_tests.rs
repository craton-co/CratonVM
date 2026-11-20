use super::*;

/// What the RUNNING kernel says a signal frame needs, which is the number
/// the compile-time `SIGSTKSZ` stopped tracking.
///
/// `_SC_SIGSTKSZ` is not in the `libc` crate, and its value is
/// glibc-specific (250 there), so this asks and falls back rather than
/// baking it in: a platform that does not answer simply does not
/// strengthen the assertion. Measured on the x86-64 build host:
/// `sysconf(_SC_SIGSTKSZ)` = 13 504 and `sysconf(_SC_MINSIGSTKSZ)` =
/// 3 376, against a compile-time `SIGSTKSZ` of 8 192.
fn platform_recommended() -> usize {
    #[cfg(target_env = "gnu")]
    const SC_SIGSTKSZ: libc::c_int = 250;
    #[cfg(target_env = "gnu")]
    // SAFETY: a plain `sysconf` query; it writes nothing and returns -1
    // for an unknown name rather than failing.
    let queried = unsafe { libc::sysconf(SC_SIGSTKSZ) };
    #[cfg(not(target_env = "gnu"))]
    let queried = -1_i64;
    if queried > 0 {
        queried as usize
    } else {
        libc::SIGSTKSZ
    }
}

/// The defect this whole change is about, stated as a test: after the
/// guard runs, this thread's alternate signal stack must be big enough
/// for the crash report to run on.
///
/// Before 2026-09-21 the only altstack was the one Rust installs, the
/// `libc` crate's compile-time `SIGSTKSZ` of 8 192 -- and on this CPU the
/// kernel's own signal frame takes `_SC_MINSIGSTKSZ` = 3 376 of it before
/// the handler's first instruction. A debug `crash_signal_handler`
/// reserved 7 096 bytes in its prologue, so it faulted ON the altstack
/// while handling SIGSEGV, and the kernel killed the process with no
/// banner, no `hs_err_pid<pid>.log` and no recovered null check.
#[test]
fn the_guard_gives_this_thread_a_stack_the_report_fits_on() {
    ensure_crash_altstack();
    // SAFETY: querying this thread's own alternate stack; writes only the
    // local `stack_t`.
    let mut ss: libc::stack_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaltstack(std::ptr::null(), &mut ss) };
    assert_eq!(rc, 0, "sigaltstack query failed");
    assert_eq!(
        ss.ss_flags & libc::SS_DISABLE,
        0,
        "the alternate stack is disabled, so SA_ONSTACK has nowhere to run"
    );
    assert!(
        ss.ss_size >= CRASH_ALTSTACK_BYTES,
        "alternate stack is {} bytes; the guard promises at least {}. \
         A debug `crash_report_handler` reserves about 7 KB in its \
         prologue and the kernel spends {} more on the signal frame \
         before it runs.",
        ss.ss_size,
        CRASH_ALTSTACK_BYTES,
        platform_recommended(),
    );
}

/// It has to be callable from a thread's start without anyone tracking
/// whether it already ran, so a second call must not install a second
/// stack (which would leak the first and, worse, leave two owners of one
/// thread's altstack).
#[test]
fn the_guard_is_idempotent_and_does_not_reinstall() {
    ensure_crash_altstack();
    let mut first: libc::stack_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaltstack(std::ptr::null(), &mut first) },
        0
    );
    ensure_crash_altstack();
    ensure_crash_altstack();
    let mut again: libc::stack_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaltstack(std::ptr::null(), &mut again) },
        0
    );
    assert_eq!(
        first.ss_sp, again.ss_sp,
        "a repeat call moved the alternate stack"
    );
    assert_eq!(first.ss_size, again.ss_size, "a repeat call resized it");
}

/// Each thread owns its own: `sigaltstack` is per-thread, so a guard that
/// only ever ran on the installing thread would leave every Java thread
/// on the 8 KiB one.
#[test]
fn a_second_thread_gets_its_own_stack() {
    ensure_crash_altstack();
    let mut here: libc::stack_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigaltstack(std::ptr::null(), &mut here) }, 0);
    let here_sp = here.ss_sp as usize;
    let there_sp = std::thread::spawn(|| {
        ensure_crash_altstack();
        let mut ss: libc::stack_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::sigaltstack(std::ptr::null(), &mut ss) }, 0);
        assert!(
            ss.ss_size >= CRASH_ALTSTACK_BYTES,
            "a spawned thread kept the small alternate stack"
        );
        ss.ss_sp as usize
    })
    .join()
    .expect("spawned thread panicked");
    assert_ne!(
        here_sp, there_sp,
        "two threads were handed the same alternate stack"
    );
}

/// The promise has to beat what the platform asks for, or the number is
/// decoration. `_SC_SIGSTKSZ` was 13 504 on the x86-64 build host, which
/// is already above the 8 192 Rust installs.
#[test]
fn the_promise_clears_what_the_platform_asks_for() {
    assert!(
        CRASH_ALTSTACK_BYTES > platform_recommended(),
        "CRASH_ALTSTACK_BYTES ({}) must exceed sysconf(_SC_SIGSTKSZ) ({})",
        CRASH_ALTSTACK_BYTES,
        platform_recommended(),
    );
    assert!(
        CRASH_ALTSTACK_BYTES > libc::SIGSTKSZ,
        "and the compile-time SIGSTKSZ ({}) Rust's own altstack uses",
        libc::SIGSTKSZ,
    );
}
