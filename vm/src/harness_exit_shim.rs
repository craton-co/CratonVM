use std::sync::atomic::{AtomicBool, Ordering};

// Windows ExitProcess(u32) — unconditional process termination.
// Calling this bypasses any further CRT teardown that would
// otherwise crash 0xC0000005.
#[link(name = "kernel32")]
extern "system" {
    fn ExitProcess(uExitCode: u32) -> !;
    fn SetUnhandledExceptionFilter(
        filter: Option<unsafe extern "system" fn(*mut ()) -> i32>,
    ) -> Option<unsafe extern "system" fn(*mut ()) -> i32>;
}

// MSVC CRT atexit registration. Returns 0 on success. We don't
// care about the return value — if registration fails the
// original crash is what we would have hit anyway.
extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

/// One line to stderr, bypassing `eprintln!`.
///
/// Every path that calls this is a path that terminates the process with
/// `ExitProcess`, so the message has to reach the real handle: libtest
/// redirects the print macros per test thread, and anything written
/// through them dies with the process instead of reaching cargo. A silent
/// `ExitProcess(1)` is what made this shim's own tripwire look like an
/// unexplained teardown crash for a whole afternoon.
fn report(msg: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = err.write_all(b"\n[harness-exit-shim] ");
    let _ = err.write_all(msg.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

extern "C" fn on_exit() {
    // atexit path: runs when `main` returns cleanly. Bypasses the
    // subsequent CRT static-destructor pass.
    //
    // Unconditionally 0. Reaching here means libtest's `main`
    // returned; libtest terminates a failing run through
    // `process::exit(ERROR_EXIT_CODE)`, which on Windows is
    // `ExitProcess` and does not run `atexit` handlers. So arriving
    // here is already proof that every test passed. This used to
    // consult a panic-count heuristic instead, which could only
    // overturn that proof with a guess — and did, for a permanent
    // silent exit-1 behind a `test result: ok` line.
    unsafe { ExitProcess(0) }
}

/// Windows unhandled-exception filter. Fires when the harness
/// trips an SEH exception such as `STATUS_ACCESS_VIOLATION`.
///
/// **Always reports failure (101).** An unhandled hardware fault
/// means some part of this run did not happen: either a test died
/// mid-run and everything queued behind it never executed, or the
/// harness faulted during teardown. Neither is a passing run, and
/// neither can be distinguished from a real failure by anything
/// observable from inside this filter.
///
/// It used to guess instead, routing a fault on `main` through a
/// panic-count heuristic that could answer **success**. That is
/// not hypothetical: before the `widened_obj_key` VM-scoping fix,
/// `cargo test -p cratonvm-vm --lib --features synthetic-jdk
/// vm::tests::linked_hashmap_` crashed in `lhm_link_tail` and
/// exited **0** — a green run with the remaining tests unexecuted.
/// The heuristic is gone (see the module header); the thread name
/// now only selects which explanation to print.
///
/// This is a deliberate policy change from the shim's original
/// intent, which was to convert a teardown `0xC0000005` into a
/// green run. That crash no longer reproduces — 20 consecutive
/// runs, filter never fired — so what remains is a trap-door that
/// only ever fires on something genuinely wrong. If it returns, it
/// should be fixed, not painted green.
unsafe extern "system" fn unhandled_filter(_info: *mut ()) -> i32 {
    // The filter runs on the crashing thread. `ExitProcess` is the
    // one Win32 call that's always safe here: it never unwinds, it
    // ignores corrupted CRT state, and it returns the exit code we
    // want cargo to see.
    //
    // The name comes from the OS, NOT from `std::thread::current()`.
    //
    // This filter runs on a thread that has just faulted, and
    // `std::thread::current()` PANICS -- it does not return `None` --
    // once that thread's thread-local data has been destroyed. This
    // function is `extern "system"`, so an unwind out of it is undefined
    // behaviour rather than a diagnosable failure, and the panic would
    // arrive while the process is already reporting a crash.
    //
    // Same species as the panic-hook site that aborted 183 Hibernate
    // Reactive classes on rc=134 while trying to *report* their first
    // panic; see `crash_handler::current_thread_name`, whose whole reason
    // for existing is that a handler must not call an API that panics.
    //
    // libtest runs each concurrent test on its own thread named
    // after the test, and does result collection on `main` — so the
    // name says which of the two stories to tell. Under
    // `--test-threads=1` tests run inline on `main` and the two are
    // indistinguishable; the exit code is 101 either way, so the
    // worst case is a slightly wrong explanation, not a wrong verdict.
    let thread_name = crate::runtime::crash_handler::current_thread_name();
    let on_worker = matches!(thread_name.as_deref(), Some(name) if name != "main");
    // The hardware-fault handler installed alongside this filter has
    // already printed the faulting PC, the thread name and a
    // symbolized backtrace.
    if on_worker {
        report(
            "a test crashed mid-run; tests after it never executed. \
             Reporting failure (101) - see the fatal-error report above \
             for the faulting test and PC.",
        );
    } else {
        report(
            "the harness faulted on `main`, after tests ran but before it \
             could finish reporting. Reporting failure (101) - the run is \
             not trustworthy, see the fatal-error report above.",
        );
    }
    // libtest's own ERROR_EXIT_CODE, so cargo reports this exactly as
    // it would an ordinary test failure.
    ExitProcess(101)
}

/// Records whether the one-shot `install` path has already run.
/// Used by the regression test to assert the shim is wired in.
pub static CTOR_RAN: AtomicBool = AtomicBool::new(false);

/// Install the atexit handler + unhandled-exception filter. Safe to
/// call more than once — `std::sync::Once` guards against
/// double-install.
///
/// No longer wraps the panic hook. The wrapper existed solely to
/// feed a panic-count tripwire, which is gone; leaving it would keep
/// a `set_hook` in the path of every panicking test for a counter
/// nobody reads.
pub fn install() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        unsafe {
            atexit(on_exit);
            // Install the unhandled-exception filter. This is the
            // one that actually catches the 0xC0000005 fired on
            // test-harness teardown — the atexit path alone cannot
            // fire because libtest's `main` never returns.
            SetUnhandledExceptionFilter(Some(unhandled_filter));
        }
        // The vectored handler runs *before* the unhandled filter and
        // prints the faulting PC plus a symbolized native backtrace,
        // which is the difference between "a test crashed" and knowing
        // where. `vm-cli` and `libcratonvm` both install it; the test
        // harness did not, which is why hardware faults under
        // `cargo test` produced no diagnostic at all. It is a pure
        // diagnostic tap (returns EXCEPTION_CONTINUE_SEARCH), so it
        // does not change which handler ultimately terminates us.
        crate::runtime::crash_handler::install_hardware_fault_handler();
        CTOR_RAN.store(true, Ordering::SeqCst);
    });
}

// CRT init hook: MSVC walks the `.CRT$XCU` section at process
// startup and calls every function pointer there before `main`.
// This is the same mechanism the `ctor` crate uses and it
// guarantees our atexit registration and exception filter are in
// place before libtest's `main` begins running tests.
#[used]
#[allow(non_upper_case_globals)]
#[link_section = ".CRT$XCU"]
static INIT_CTOR: extern "C" fn() = {
    extern "C" fn init() {
        install();
    }
    init
};

/// Testing hook: returns `true` iff the `.CRT$XCU` init ran during
/// process startup (i.e. the `Once` guarding `install` has already
/// fired). Used by the regression test to prove the shim is wired
/// in without relying on test teardown behaviour.
pub fn ctor_ran() -> bool {
    CTOR_RAN.load(Ordering::SeqCst)
}
