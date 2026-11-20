#[allow(unused_imports)]
use super::super::*;

/// How the Java `main` method ended, as the launcher's teardown needs it
/// (gc-common w12-e).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainEnding {
    /// `main` returned normally.
    Returned,
    /// `main` threw a Java exception that nothing caught.
    Threw,
    /// The VM itself failed (`MethodCallFailed::InternalError`).
    InternalError,
}

impl MainEnding {
    pub(crate) fn of<T>(result: &Result<T, MethodCallFailed>) -> Self {
        match result {
            Ok(_) => MainEnding::Returned,
            Err(MethodCallFailed::ExceptionThrown(_)) => MainEnding::Threw,
            Err(MethodCallFailed::InternalError(_)) => MainEnding::InternalError,
        }
    }

    /// Whether the launcher waits for the non-daemon threads before running
    /// the shutdown hooks and exiting. HotSpot's `JavaMain` reaches
    /// `DestroyJavaVM`, and so waits, whether `main` returned or threw. An
    /// internal VM failure has no HotSpot counterpart and exits at once, as
    /// it always has: the VM that would run those threads is suspect.
    pub(crate) fn waits_for_non_daemon_threads(self) -> bool {
        matches!(self, MainEnding::Returned | MainEnding::Threw)
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            MainEnding::Returned => "returned",
            MainEnding::Threw => "threw",
            MainEnding::InternalError => "failed",
        }
    }
}

pub(crate) fn main() {
    // Phase 5 boot image: with `CRATONVM_BOOT_IMAGE` on, run without ASLR (one
    // re-exec), so an image's pointers into the C libraries stay valid; a
    // no-op with the knob off. See `boot_image_launch::reexec_without_aslr`.
    boot_image_launch::reexec_without_aslr();
    // `sun.rt.internalVersion`'s `built on <time> with <compiler>` tail, from
    // this binary's build script (`build.rs`). Records two static strings; boots
    // nothing.
    cratonvm_vm::runtime::set_build_stamp(
        env!("CRATONVM_BUILD_TIME"),
        env!("CRATONVM_BUILD_COMPILER"),
    );
    // `--diff-hotspot`: run this same program under CratonVM and under a
    // reference JDK and report the FIRST divergence, then exit. Handled here,
    // as the very first statement of `main`, for three reasons:
    //
    //  * this mode boots no VM in *this* process — it spawns one CratonVM child
    //    and one `java` — so it must not install flags, expand the grouped
    //    configuration variables, or latch the immutable snapshot the child
    //    under test will latch for itself;
    //  * `diff_hotspot::scan` removes its own `--diff-*` tokens before
    //    `expand_argfiles` / `insert_program_args_separator` ever see them, so
    //    the pre-clap argv pipeline (and its ~60 positional-semantics tests) is
    //    untouched by this feature;
    //  * on a normal launch it costs one pass over argv and returns `None`.
    //
    // See `docs/testing/diff-hotspot.md`.
    if let Some(code) = diff_hotspot::maybe_run() {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }

    // Install the immutable runtime configuration before crash handlers or
    // any other subsystem can read a CratonVM flag. `--nojit` is detected
    // from the launcher portion of the expanded argv (never from Java program
    // arguments) and applied as a typed overlay. `run()` performs the full
    // parse again; this early pass exists solely to close the configuration
    // ordering boundary.
    // zt t14/x11: with `JDK_JAVA_OPTIONS` ([`launch_argv`]; its `NOTE:` line
    // and refusals come here, first on stderr, as HotSpot's launcher prints
    // them before anything else).
    // zt t16/tg7: reshaped as `run` reshapes it ([`hotspot_launcher_argv_shape`]),
    // so `-jar --nojit app.jar` is read here as `run` reads it.
    let early_argv = insert_program_args_separator(hotspot_launcher_argv_shape(launch_argv(
        std::env::args().collect::<Vec<_>>(),
    )));
    // A launcher flag parked in the program-args tail is discarded in silence
    // — exit 0, no file, no diagnostic. Say so before anything else runs, so
    // the warning is the first thing on stderr rather than the last, and so it
    // is emitted even on the paths that exit before `run()` ever parses argv.
    // This is the ONLY consumer of `early_argv` that does not also change
    // behaviour: nothing is rewritten, `java` positional semantics are intact,
    // and the misplaced token still reaches the Java program verbatim.
    warn_about_misplaced_launcher_flags(&early_argv);
    let mut flag_overrides = cratonvm_types::MapSource::empty();
    if launcher_nojit_requested(&early_argv) {
        flag_overrides = flag_overrides.with("CRATONVM_DISABLE_JIT", "1");
    }
    // `--dump-phase-report <FILE>` is sugar for
    // `CRATONVM_PHASE_ACCOUNTING=coarse CRATONVM_PHASE_ACCOUNTING_OUT=<FILE>`,
    // applied here because a declared flag is served from the snapshot latched
    // three lines below and is immutable afterwards. The level is only
    // *defaulted*: an explicit `CRATONVM_PHASE_ACCOUNTING`, or a
    // `CRATONVM_DBG=phase-accounting=fine` token, still chooses it, so the
    // option composes with `fine` instead of silently downgrading it.
    if let Some(path) = launcher_phase_report_path(&early_argv) {
        flag_overrides = flag_overrides.with(phase::FLAG_JSON_OUT, &path);
        let level_set_explicitly = std::env::var_os(phase::FLAG_ENABLE).is_some()
            || std::env::var(cratonvm_types::flag_groups::Group::DBG.var())
                .is_ok_and(|spec| spec.contains("phase-accounting"));
        if !level_set_explicitly {
            flag_overrides = flag_overrides.with(phase::FLAG_ENABLE, "coarse");
        }
    }
    // `-ea` / `-da` / `-esa` / `-dsa`: the HotSpot spelling of
    // `CRATONVM_ENABLE_ASSERTIONS`, applied here for the same reason as the two
    // overrides above — the flag is declared, so the snapshot latched three
    // lines below is the last point at which it can be set at all.
    let mut flag_unsets: Vec<&str> = Vec::new();
    match launcher_assertions_requested(&early_argv) {
        Some(true) => flag_overrides = flag_overrides.with("CRATONVM_ENABLE_ASSERTIONS", "1"),
        // `-da` has to make the name *absent*, not set it to "0": the flag is
        // parsed with `present`, under which `=0` still reads as enabled. An
        // explicit `-da` therefore also overrides an inherited export, which is
        // what HotSpot does.
        Some(false) => flag_unsets.push("CRATONVM_ENABLE_ASSERTIONS"),
        None => {}
    }
    let runtime_flags =
        cratonvm_types::VmFlags::from_env_with_overrides_and_unsets(flag_overrides, &flag_unsets);
    if cratonvm_types::install_flags(runtime_flags).is_err() {
        eprintln!("[cratonvm] runtime flags were read before launcher configuration");
        std::process::exit(1);
    }

    // Expand the ten grouped configuration variables (`CRATONVM_JIT=-bce,unroll`
    // and friends) into the per-knob keys the rest of the VM reads.
    //
    // This runs first, before the symbolize/SEGV hooks below, because those
    // read `CRATONVM_SYMBOLIZE` and `CRATONVM_TEST_SEGV` — both of which are
    // now tokens (`CRATONVM_DBG=symbolize=...`, `CRATONVM_TEST=segv`) and would
    // otherwise be missed on this run. It is also the only point in the process
    // guaranteed to be single-threaded, which is what makes writing back to
    // `environ` sound: 431 read sites still call `std::env::var` directly and
    // cannot see a resolved source. See `cratonvm_types::flag_groups`.
    let (legacy_direct, unknown_tokens) = cratonvm_types::flag_groups::expand_process_env();
    // An unrecognised token is FATAL, not a warning.
    //
    // It used to print and continue, which is the worst of both worlds: the
    // knob the caller asked for is not applied, the run completes normally, and
    // a scripted A/B that captures only the tail of the output — or only greps
    // for PASS/FAIL — never sees the notice. The experiment then measures
    // nothing while reading as a clean result. That is not hypothetical: it is
    // how `CRATONVM_JIT=no-self-cache-inherit` (the disable spelling is
    // `-self-cache-inherit`) produced a confident, wrong "hypothesis
    // falsified" in
    // `docs/feature-designs/jit-ir-optimizations.md`.
    //
    // Failing closed makes a typo impossible to mistake for a measurement.
    // `CRATONVM_ALLOW_UNKNOWN_TOKENS=1` restores the old warn-and-continue for
    // anyone who genuinely needs it (e.g. a shared script that must run against
    // several VM revisions whose token sets differ).
    if !unknown_tokens.is_empty() {
        let allow = std::env::var_os("CRATONVM_ALLOW_UNKNOWN_TOKENS").is_some();
        for t in &unknown_tokens {
            eprintln!("[cratonvm] unknown configuration token: {t}");
            // `GROUPVAR=token` — split back so a suggestion can be offered.
            if let Some((var, tok)) = t.split_once('=') {
                let tok = tok.split('=').next().unwrap_or(tok);
                let tok = tok.strip_prefix('-').unwrap_or(tok);
                let tok = tok.strip_prefix('+').unwrap_or(tok);
                if let Some(group) = cratonvm_types::flag_groups::Group::ALL
                    .iter()
                    .find(|g| g.var() == var)
                {
                    if let Some(hint) = cratonvm_types::flag_groups::suggest(*group, tok) {
                        eprintln!("[cratonvm]   {hint}");
                    }
                }
            }
        }
        if allow {
            eprintln!(
                "[cratonvm] continuing anyway: CRATONVM_ALLOW_UNKNOWN_TOKENS is set. \
                 The knob(s) above are NOT applied."
            );
        } else {
            eprintln!(
                "[cratonvm] refusing to start: an unknown token is not applied, so this run \
                 would silently not be the configuration you asked for. Fix the spelling, or \
                 set CRATONVM_ALLOW_UNKNOWN_TOKENS=1 to continue with it ignored."
            );
            std::process::exit(2);
        }
    }

    // Seed the phase-accounting epoch. This is the earliest point it can go:
    // `CRATONVM_PHASE_ACCOUNTING` is a `CRATONVM_DBG` token, so it is not
    // readable until the group expansion immediately above has run, and
    // `phase::level()` latches on first read. Everything before this line —
    // the allocator init, `clap`'s argfile expansion, the flag install — is
    // outside the epoch rather than unattributed inside it, which is
    // deliberate and is item 2 of `docs/observability/phase-accounting.md`
    // §11. A no-op (not even a clock read) when accounting is off.
    //
    // This also provisionally claims the launcher as the reconciliation basis;
    // the `main-vm` thread takes that role back below, because the launcher
    // spends the rest of the process blocked in `join`.
    phase::mark_process_start();

    // `CRATONVM_DBG=-deprecations` expands to `CRATONVM_QUIET_DEPRECATIONS=1`,
    // which the call above has already written back, so this read sees it.
    if !legacy_direct.is_empty() && None::<std::ffi::OsString>.is_none() {
        // One line, not one per variable: a debugging session routinely exports
        // a dozen of these and a dozen warnings would just train people to
        // ignore them.
        let shown: Vec<&str> = legacy_direct.iter().map(|(_, to)| to.as_str()).collect();
        eprintln!(
            "[cratonvm] {} per-flag variable(s) set directly; the supported \
             spelling is now: {}",
            legacy_direct.len(),
            shown.join(" ")
        );
    }
    // gen r4w3/obs2 (2026-09-23): the other half of the same report — a
    // `CRATONVM_*` variable that NO knob claims is ignored, and until now
    // silently, so a misspelt kill switch measured the default while reading as
    // the A/B arm (`docs/feature-designs/generative-gc-architecture.md`).
    // Advice, never fatal: the namespace is open (harness scripts, embedders),
    // unlike a grouped variable's closed token set. One line, same silencer as
    // the line above. Names come from the environment AFTER the group
    // expansion, whose written-back keys are all claimed.
    if None::<std::ffi::OsString>.is_none() {
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .collect();
        let unclaimed =
            cratonvm_types::flag_groups::unclaimed_env_names(names.iter().map(String::as_str));
        if let Some(line) = cratonvm_types::flag_groups::unclaimed_env_advisory(&unclaimed) {
            eprintln!("{line}");
        }
    }
    // Env tokens that a launcher flag now supersedes — notably
    // `CRATONVM_REAL=-stubs`, which `--real-jdk` covers and extends (contract
    // §9). Printed beside the `legacy_direct` line above because it is the same
    // class of message ("your configuration has a newer spelling"), and once
    // each: a token that supersedes twice is still one fact.
    // …except when the run is already under the policy the note recommends.
    // The only supersession today is `CRATONVM_REAL=-stubs` -> real-jdk mode,
    // and since 2026-09-20 that is the default — so this is now silent unless
    // the run opted out with `--compatible` (or `--real-jdk`) and then set the
    // env token anyway, which is the one case the advice still applies to.
    if !scan_requested_compatibility_mode(&early_argv).is_jdk_only() {
        let mut noted: Vec<String> = Vec::new();
        for superseded in cratonvm_types::flag_groups::process_env_supersessions() {
            let note = superseded.note().to_string();
            if noted.iter().any(|seen| *seen == note) {
                continue;
            }
            eprintln!("[cratonvm] {note}");
            noted.push(note);
        }
    }

    // Hardware-fault diagnostics. On Windows a SEGV/access violation is a
    // structured exception that bypasses the Rust panic hook below entirely;
    // without this, a native fault (e.g. the JIT-dispatch SEGV) kills the
    // process with empty stderr and a bare STATUS_ACCESS_VIOLATION exit code.
    // This registers a vectored exception handler that prints the faulting PC
    // + a symbolized backtrace and then lets the process die as before. It
    // does NOT install a panic hook, so the visibility-first hook set up just
    // below is preserved. No-op on non-Windows targets.
    cratonvm_vm::runtime::crash_handler::install_hardware_fault_handler();

    // Diagnostic: CRATONVM_SYMBOLIZE=0x11BF183,0xAB10E resolves exe-relative
    // RVAs (as printed by the VEH crash report) against THIS binary's symbols
    // in a clean context, then exits. Used to symbolize a multi-threaded crash
    // whose racy teardown truncated the in-handler symbolization.
    if let Ok(spec) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let rvas: Vec<usize> = spec
            .split(',')
            .filter_map(|s| {
                let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
                usize::from_str_radix(s, 16).ok()
            })
            .collect();
        for (rva, name) in cratonvm_vm::runtime::crash_handler::symbolize_rvas(&rvas) {
            match name {
                Some(n) => println!("0x{:X}\t{}", rva, n),
                None => println!("0x{:X}\t<unresolved>", rva),
            }
        }
        std::process::exit(0);
    }

    // Self-test hook for the hardware-fault handler: when CRATONVM_TEST_SEGV=1,
    // deliberately trigger an access violation right after installing the
    // handler so the VEH path (faulting PC + symbolized backtrace) can be
    // validated without needing to reproduce a real crash. Gated behind an env
    // var so it never affects normal runs.
    if std::env::var("CRATONVM_TEST_SEGV").as_deref() == Ok("1") {
        eprintln!("[cratonvm] CRATONVM_TEST_SEGV=1: forcing an access violation");
        // SAFETY: intentional null/wild dereference to exercise the fault
        // handler. This is dead code on every normal run.
        unsafe {
            let p = 0xdead_beef_usize as *mut u8;
            std::ptr::write_volatile(p, 0);
        }
    }

    // I1 — Visibility-first panic hook.
    //
    // The previous T14 hook silenced **every** Rust panic by routing it to
    // `tracing::debug!`. That worked for the well-known initPhase1
    // bootstrap-path panics (unaligned-pointer reads, transient null
    // dereferences) which the outer `safe_native_call` already logs once
    // via a user-facing warning — but it also silenced *real* panics that
    // escaped a `catch_unwind`. Because the tracing subscriber installed
    // by `run()` filters at WARN+, debug-level panic notices were never
    // written to stderr, and any genuine VM crash showed up in the logs
    // with an empty stderr and an exit code derived from the OS abort
    // (rc=-1 on Windows when the runner kills the process; STATUS_ACCESS_
    // VIOLATION when the abort fires from native code). The CGLIB probe
    // hit exactly this surface: an interpreter loop hung inside
    // `String.indexOf`, the watchdog never fired (the user did not pass
    // `--stack-dump-on-timeout`), and the failure was indistinguishable
    // from a successful run that produced no output.
    //
    // The new hook routes everything to `stderr` directly — the only sink
    // that is guaranteed to survive every other failure mode (tracing
    // subscriber not initialized, WARN-level filter, panic firing from a
    // worker thread before `run()` builds the subscriber). For
    // bootstrap-path panics that are still expected to be quiet, the
    // outer `safe_native_call` continues to swallow them via its own
    // `catch_unwind` — the hook fires *before* `catch_unwind` catches
    // the unwind, but only the recovery path knows the panic was caught,
    // so we always emit at hook time. The cost is a couple of extra
    // stderr lines on the (rare) bootstrap-panic path; the gain is that
    // every crash is now traceable.
    //
    // Honors `RUST_BACKTRACE=1` / `RUST_BACKTRACE=full` the same way the
    // default Rust hook does (we use `std::backtrace::Backtrace::capture`
    // which respects the env var).
    let _default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        // Handled heap-exhaustion unwind from the native allocators — the VM
        // converts it into a catchable java.lang.OutOfMemoryError, so it is not
        // a panic the user needs to see. (See
        // `cratonvm_vm::runtime::native_oom`.)
        if cratonvm_vm::runtime::native_oom::is_native_oom_panic(info) {
            return;
        }
        // A boot failure HotSpot reports with `vm_exit_during_initialization`
        // (a startup agent that does not load, interpreter round i2 wave 54):
        // its two lines on stdout, nothing on stderr, exit status 1.
        if let Some(error) = cratonvm_vm::error::vm_initialization_error_of(info) {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(error.report().as_bytes());
            let _ = out.flush();
            std::process::exit(1);
        }
        // zki: a failed `JNI_CreateJavaVM` after HotSpot's two stdout lines
        // (a ZGC initial heap the hugetlbfs pool cannot hold): the launcher's
        // two `Error:` lines on stderr, exit status 1.
        if let Some(error) = cratonvm_vm::error::vm_creation_error_of(info) {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(error.report().as_bytes());
            let _ = out.flush();
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(cratonvm_vm::error::VmCreationError::LAUNCHER_LINES.as_bytes());
            let _ = err.flush();
            std::process::exit(1);
        }
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<panic payload was not a string>".to_string());
        // NOT `std::thread::current()`. That call PANICS once this thread's
        // thread-local data has been destroyed, and a panic inside a panic
        // hook is a panic-while-panicking: Rust aborts the process on the
        // spot, before this hook has printed a single byte about the panic it
        // was called for. Reading one cosmetic name that way cost 183 of 206
        // Hibernate Reactive classes under `--real-jdk` -- every one of them
        // rc=134 (SIGABRT), and every one of them reporting `thread/
        // current.rs:315:9`, which is the *second* panic and says nothing
        // about the first. See
        // `hibernate-reactive-double-panic-abort-FIXED-20260901` and
        // `crash_handler::current_thread_name`.
        let thread_name_owned = cratonvm_vm::runtime::crash_handler::current_thread_name();
        let thread_name = thread_name_owned.as_deref().unwrap_or("<unnamed>");

        // Is this thread's thread-local storage still usable?
        //
        // `tracing`'s dispatcher and its subscriber stack reach several
        // `thread_local!`s and PANIC (they do not degrade) once those are
        // gone, and a panic raised inside a panic hook aborts the process
        // immediately. That is the second half of
        // `hibernate-reactive-double-panic-abort-FIXED-20260901`:
        // after the `std::thread::current()` call above was fixed, this
        // hook printed the real panic correctly and then aborted anyway on
        // its own `tracing::warn!` tail.
        //
        // A never-otherwise-touched, destructor-bearing key is a sound probe
        // for the phase that matters. MEASURED (glibc 2.39, rustc 1.97.1):
        // throughout `__nptl_deallocate_tsd` — the `pthread` thread-specific-
        // data phase this hook is reached from when a JNI library's TSD
        // destructor panics — every `try_with` on the thread answers
        // `Err(AccessError)`, initialized or not; during Rust's own TLS
        // destructors even an untouched key still initializes. So `Ok` here
        // means the tracing stack is safe to enter, and the normal panic
        // keeps its tracing mirror.
        thread_local! {
            static TLS_LIVENESS_PROBE: Box<u8> = Box::new(0);
        }
        let tls_usable = TLS_LIVENESS_PROBE.try_with(|_| ()).is_ok();

        // Mirror `safe_native_call`'s well-known bootstrap-path quiet
        // list (vm/src/vm/vm_exec.rs:253). These are caught one frame
        // up and surfaced via a single user-facing warning; we route
        // them to `tracing::debug!` so the full panic doesn't spam the
        // user's stderr during initPhase1. Anything *not* in this list
        // gets the full visibility treatment.
        // Keycloak Round 71: only quiet the bootstrap-class panics while
        // bootstrap is actually running (init level < 4). Once `main()`
        // is executing, an unaligned/null pointer panic from a native is
        // a real fault that must be visible — silently demoting it to
        // debug produces the "Keycloak exits in 5s with no output"
        // failure mode where 19 caught NPEs corrupt picocli state and
        // `parseAndRun` returns without ever calling `start-dev`.
        let in_bootstrap = cratonvm_native_api::init_level::get_init_level() < 4;
        let is_known_bootstrap_quiet =
            (msg.contains("unaligned pointer") || msg.contains("null pointer")) && in_bootstrap;

        if is_known_bootstrap_quiet {
            if !tls_usable {
                // Quiet by intent and unable to reach `tracing` — say nothing
                // rather than abort the process to log a panic that is caught
                // one frame up anyway.
                return;
            }
            if let Some(loc) = info.location() {
                tracing::debug!(
                    target: "cratonvm::panic",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            } else {
                tracing::debug!(
                    target: "cratonvm::panic",
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            }
            return;
        }

        // Visible path: write directly to stderr because tracing may
        // be filtering at WARN+ and we cannot rely on the subscriber
        // having been initialised (e.g. when the panic fires before
        // `run()` builds it).
        let mut stderr = std::io::stderr().lock();
        if let Some(loc) = info.location() {
            let _ = writeln!(
                stderr,
                "thread '{thread_name}' panicked at {}:{}:{}:\n{msg}",
                loc.file(),
                loc.line(),
                loc.column(),
            );
        } else {
            let _ = writeln!(stderr, "thread '{thread_name}' panicked:\n{msg}");        }
        // Backtrace only when explicitly requested — matches the stock
        // Rust hook semantics so users opting out of backtrace still see
        // the panic message but no overhead.
        // Stamp the crash with the standard library in play. Two complete
        // class-library implementations ship in this binary and they fail
        // differently; a panic report that doesn't say which one ran costs
        // a round trip to triage.
        let _ = writeln!(stderr, "[cratonvm] {}", active_jdk_mode_line());
        let bt = std::backtrace::Backtrace::capture();
        if bt.status() == std::backtrace::BacktraceStatus::Captured {
            let _ = writeln!(stderr, "stack backtrace:\n{bt}");
        } else {
            let _ = writeln!(
                stderr,
                "note: run with `RUST_BACKTRACE=1` environment variable \
                 to display a backtrace",
            );
        }
        let _ = stderr.flush();
        // ALSO mirror through tracing at WARN level so log aggregators
        // that key on tracing still see the panic. We do this *after*
        // the direct stderr write so the user-visible message lands
        // even when the tracing subscriber is unavailable or filtering
        // it out.
        if !tls_usable {
            // The stderr write above already carries the whole panic. Entering
            // `tracing` from a thread whose TLS is gone would panic inside this
            // hook and abort the process, discarding what we just printed.
            let _ = writeln!(
                std::io::stderr(),
                "[cratonvm] (panic raised after this thread's TLS was destroyed -- the tracing mirror of this panic is skipped)",
            );
            return;
        }
        if let Some(loc) = info.location() {
            tracing::warn!(
                target: "cratonvm::panic",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                thread = thread_name,
                "panic: {msg}",
            );
        } else {
            tracing::warn!(target: "cratonvm::panic", thread = thread_name, "panic: {msg}");        }    }));

    // The interpreter uses recursive Rust calls for Java method invocations.
    // Deep Java call stacks (e.g. Quarkus bootstrap, binary-trees-style
    // recursion under JIT dispatch) can exceed the default 8 MB Rust stack.
    // 64 MB cleared every workload up to and including QuickBenchLong's
    // first four kernels but `binaryTrees(18)` (~524 k recursive
    // invocations through `jit_invoke_dispatch` / interpreter fallback
    // helpers, each adding one Rust frame) drove the main-vm thread past
    // it on some platforms — manifesting as `thread 'main-vm' has
    // overflowed its stack` (rc=139 on Linux) before reaching the GC
    // safepoint that would have triggered an OOME. Bump to 128 MB so
    // even the deepest recursive workloads have headroom; the upper
    // bound is virtual-address-space-only on 64-bit OSes (no commit
    // until the page is touched), so the practical cost is zero.
    // DBG (CRATONVM_DBG_HEARTBEAT=<ms>): forensic liveness heartbeat, armed
    // from the launcher thread (not main-vm) so it keeps writing even if
    // main-vm itself hangs or dies without unwinding. See its own doc
    // comment for what it's for.
    cratonvm_vm::runtime::heartbeat_watch::arm_from_env();

    let builder = std::thread::Builder::new()
        .name("main-vm".into())
        .stack_size(128 * 1024 * 1024);
    // gc-common w11-f: the thread returns the process exit code instead of
    // ending the process itself. See `exit_from_launcher` for why the process
    // must not be ended from this thread.
    let handler = builder
        .spawn(|| -> i32 {
            // A sigaltstack big enough for the crash REPORT. Rust gives every
            // thread 8 KiB and `SA_ONSTACK` runs the handler there; on a debug
            // build `crash_report_handler` does not fit, and a handler that
            // faults on the altstack while handling SIGSEGV is a silent kill --
            // no banner and no `hs_err_pid<pid>.log`. This is the thread that
            // runs the interpreter and compiled code, so it is the one whose
            // crashes have to be readable. Recovery of implicit null checks
            // does NOT depend on this (`crash_signal_handler` is split so its
            // own frame fits the small stack); only the report does.
            cratonvm_vm::runtime::crash_handler::ensure_crash_altstack();
            // DBG (CRATONVM_DBG_HANGWALK=<secs>): arm the native-stack-walk
            // watchdog on THIS (main-vm) thread — the one that runs the
            // interpreter — not the launcher thread that just joins it.
            cratonvm_vm::runtime::stwhang_watch::arm_from_env();
            // Phase accounting: move the reconciliation basis onto THIS thread.
            // `mark_process_start` had to run on the launcher (it is the first
            // point the `CRATONVM_DBG` token is readable), but the launcher does
            // nothing after this spawn except block in `join` — leaving the
            // basis there would make `reconciles` a verdict about a waiter, and
            // every per-category value on the summary line would be zero. The
            // epoch is untouched, so `process_wall_ns` still covers the
            // launcher prologue even though `basis_wall_ns` starts here.
            phase::claim_reconciliation_basis();
            // Diagnosability (Keycloak Gap 9): the boot can exit SILENTLY — `run()`
            // returns `Ok` (e.g. waitForExit returned / VM main finished) or an `Err`
            // whose `Display` ({e:#}) renders empty, so the prior `eprintln!("{e:#}")`
            // could print nothing before `exit(1)`. Always surface the outcome
            // (Display AND Debug) and flush stderr so a startup failure that ends the
            // process is never invisible. Additive logging only — no behaviour change.
            use std::io::Write as _;
            let result = run();
            // Everything past `run()` is teardown: report emission and outcome
            // rendering. Charging it keeps the tail of the basis thread's
            // timeline out of `unattributed_ns`. Held to the end of the
            // closure, so the report below is taken from *inside* it — which is
            // why `anomalies.open_spans == 1` is the expected reading and not a
            // leak (`docs/observability/phase-accounting.md` §9).
            let _phase_shutdown = phase::enter(phase::Category::VmShutdown);
            // A normal Java-main return never reaches the System.exit hook.
            // Flush controlled-exit diagnostics before rendering the outcome.
            maybe_dump_shutdown_reports();
            // The corrupt-cell census, on every exit arm. See
            // `reclaim_guard::corrupt_cell_exit_summary`: a run that tripped the
            // collector's guard and named nothing must SAY so, because silence
            // from a diagnostic reads as "clean" and is not.
            cratonvm_vm::memory::corrupt_cell_exit_summary();
            // Same reasoning for the post-remap stale-frame-word detector: its
            // `System.exit` printer sits in the shutdown trailer, and this is
            // the arm a program that returns from `main` takes instead.
            cratonvm_types::stale_remap_census::exit_summary();
            // A3 (2026-09-01): the coercion / ref-word degradation censuses,
            // on the arm a program that returns from `main` takes. Same pair as
            // the shutdown trailer in `lang_system.rs`; both are `Once`-guarded,
            // so whichever exit path runs first prints and the other is a no-op.
            cratonvm_types::compact_value::coercion_census::exit_summary();
            cratonvm_types::compact_value::degradation_exit_summary();
            // The JVMS 6.5 uninstantiable-receiver census (A12), on the arm a
            // program that returns from `main` takes. `Once`-guarded and silent
            // unless a native handed back an abstract/interface receiver, so
            // having it on both exit paths is correct.
            cratonvm_native_api::instantiable::exit_summary();
            // On BOTH arms: the A5 fallback census explains a run whose test
            // FAILED at least as often as one that returned Ok, and
            // `report_a5_engagement` below is on the Ok arm only.
            cratonvm_vm::jit::conservative_roots::report_a5_fallback_census();
            // Same both-arms reasoning: the IR transform census answers
            // "did this pass fire at all", and a run that failed is exactly
            // when that question gets asked.
            cratonvm_jit::ir_evidence::exit_summary();
            // HANDLER8-1 census (once per core; nothing when unarmed).
            cratonvm_jit::tiered::print_outside_manager_c2_census_at_exit();
            // GPU offload engagement and dispatch censuses (each `Once`-guarded and
            // silent when no GPU work ran). `lang_system::run_shutdown_hooks` calls
            // this in `--compatible` mode; `--real-jdk` returns here from `main()`.
            cratonvm_native_builtins::craton_gpu::dispatch_timing::report();
            match result {
                Ok(()) => {
                    cratonvm_vm::jit::conservative_roots::report_a5_engagement();
                    // nio21 wave 8 lane L4: HotSpot prints nothing on a clean
                    // exit; the line is `CRATONVM_DBG=launcher-outcome` only.
                    if launcher_outcome_lines_on() {
                        eprintln!(
                            "[cratonvm] main-vm run() returned Ok — VM main exiting normally"
                        );
                    }
                    let _ = std::io::stderr().flush();
                    0
                }
                // nio21 wave 8 lane L4: `main` died on an uncaught exception
                // whose report is already out, unprefixed, in HotSpot's words
                // and order. HotSpot's launcher adds nothing and exits 1.
                Err(e) if e.downcast_ref::<UncaughtReported>().is_some() => {
                    if launcher_outcome_lines_on() {
                        eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
                        eprintln!("[cratonvm] {}", active_jdk_mode_line());
                    }
                    let _ = std::io::stderr().flush();
                    // The same unterminated-`System.out` flush as the arm
                    // below (G11-1).
                    let _ = std::io::stdout().flush();
                    1
                }
                Err(e) => {
                    eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
                    // The `Debug` rendering adds something only for an error
                    // with a cause chain (anyhow lists it as `Caused by:`
                    // lines). For a chainless one -- every uncaught Java
                    // exception, whose text is the `printStackTrace` report --
                    // it repeated the whole report, so a trace's `Caused by:`
                    // and `\tat` lines reached stderr twice (interpreter round
                    // i2 wave 59, lane L5; probe `L5W59LauncherClinitThrows`).
                    if e.chain().nth(1).is_some() {
                        eprintln!("[cratonvm] main-vm run() Err (debug): {e:?}");
                    }
                    // Always stamp the failure with the standard library it
                    // ran against. CratonVM has two of them with different
                    // bug sets, so a stack trace or exception without the
                    // mode is not actionable — this line is what makes a
                    // pasted terminal transcript sufficient for triage.
                    eprintln!("[cratonvm] {}", active_jdk_mode_line());
                    let _ = std::io::stderr().flush();
                    // G11-1: STDOUT too, and only here — this arm ends the
                    // process with `std::process::exit` (from the launcher
                    // thread, since gc-common w11-f), which runs no
                    // destructors, and the
                    // fd table's fd-1 entry is a `Mutex<io::Stdout>`, i.e. a
                    // LINE writer. A Java program whose last `System.out`
                    // write had no trailing newline (`System.out.print`, a
                    // partial `write`) and which then died on an uncaught
                    // exception lost those bytes: the stderr flush three lines
                    // up never touched them. HotSpot does not lose them —
                    // MEASURED on Temurin 25.0.3+9, `HookProbe noflush` and
                    // `haltnoflush`, where an unterminated unflushed
                    // `System.out.print` survives both `System.exit(0)` and
                    // `Runtime.halt(6)`.
                    //
                    // Deliberately additive: no output is produced, no
                    // ordering changes (stderr is flushed first, as before),
                    // and the exit code stays 1. `let _` because a broken pipe
                    // on stdout must not turn a Java-level failure into a
                    // different one.
                    let _ = std::io::stdout().flush();
                    // gc-common w11-f: RETURN the code, do not exit here. This
                    // arm used to call `std::process::exit(1)` on this thread,
                    // and on Windows that hung 3 runs in 8 with a busy G1
                    // (`common-w10v-uncaught-error-exit-can-hang-in-process-exit`).
                    // The launcher ends the process once this thread is gone;
                    // see `exit_from_launcher`.
                    1
                }
            }
        })
        .expect("failed to spawn main-vm thread");
    exit_from_launcher(join_main_vm_thread(handler));
}

/// Wait for the `main-vm` thread and turn how it ended into the process exit
/// code: the code it returned, or 1 if it panicked.
///
/// The panic arm used to print, flush and `std::process::exit(1)` inline; the
/// print is unchanged and the flush and exit moved to [`exit_from_launcher`],
/// which does both in the same order.
pub(crate) fn join_main_vm_thread(handler: std::thread::JoinHandle<i32>) -> i32 {
    match handler.join() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("main-vm thread panicked: {:?}", e);
            1
        }
    }
}

/// End the process with `code`, called on the launcher thread after the
/// `main-vm` thread has finished. Code 0 returns, so `main` returns exactly as
/// the normal exit always has.
///
/// # Why the process is not ended on the `main-vm` thread (gc-common w11-f)
///
/// On Windows `std::process::exit` is `ExitProcess`. That first kills every
/// other thread wherever it is, then runs the calling thread's Rust
/// thread-local destructors (from the TLS callback, on `DLL_PROCESS_DETACH`).
/// The `main-vm` thread has run Java, so its thread-locals include the
/// collector's per-thread barrier buffers. `card_table::DirtyBufferGuard` and
/// `satb::SatbBufferGuard` lock their buffer and a process-wide registry in
/// `Drop`, and collector threads (pause workers, the G1 marker, a daemon
/// thread's collection) take the same locks. If a killed thread held one of
/// them, the destructor waits for ever: a `parking_lot` or SRW lock whose
/// owner was killed is never released. That is the hang in
/// `common-w10v-uncaught-error-exit-can-hang-in-process-exit`: the whole
/// trailer printed, one thread left, no CPU used.
///
/// The normal exit never had the problem, and this is the difference: the
/// `main-vm` thread returns, its thread-locals are destroyed while every other
/// thread is still alive (so a held lock is released), and only then does
/// `main` return on this thread, whose thread-locals hold nothing of the VM.
/// The error and panic arms now leave the same way. Stdout and stderr are
/// flushed first, as the panic arm always did.
///
/// On Linux `exit` does not kill the other threads first, so a held lock is
/// released and the old shape did not hang there; the change is harmless on
/// Linux.
///
/// Not `TerminateProcess`: that would also skip `DLL_PROCESS_DETACH` for every
/// loaded library (a JNI library's C runtime buffers, for one), which the
/// normal exit runs. Shutdown hooks and the JFR exit dump run inside `run()`,
/// before the `main-vm` thread returns, so neither depends on this choice.
pub(crate) fn exit_from_launcher(code: i32) {
    if code == 0 {
        return;
    }
    destroy_java_vm_before_exit();
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(code);
}

/// Parse a JVM-style memory size string (e.g., "256m", "1g", "1024k").
/// `-XX:MaxMetaspaceSize=<size>` → the per-VM class-metadata limit
/// (gc-common w6-f). `None` when the flag is absent (unbounded, the default)
/// or unparsable — the latter with a warning, like `-XX:MaxDirectMemorySize`,
/// rather than a launch failure.
/// `-XX:SoftRefLRUPolicyMSPerMB=<n>`'s value: a non-negative decimal integer
/// (HotSpot's `intx`, of which only the non-negative range is meaningful).
/// `None` for anything else, which the caller reports and ignores.
pub(crate) fn parse_soft_ref_lru_policy(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// gcg g4/yg: HotSpot Serial's `HeapAlignment` on x64 / aarch64 with 4 KiB
/// pages (measured on 25.0.3: `-Xmx9m` prints `Heap Max Capacity: 10M`,
/// `-Xmx100000k` `98M`, `-Xms5000k` `Heap Initial Capacity: 6M`).
pub(crate) const HOTSPOT_SERIAL_HEAP_ALIGNMENT: usize = 2 * 1024 * 1024;

/// `bytes` rounded up to [`HOTSPOT_SERIAL_HEAP_ALIGNMENT`]; the largest
/// aligned size when the rounding would overflow.
pub(crate) fn hotspot_heap_align_up(bytes: usize) -> usize {
    hotspot_heap_align_up_to(bytes, HOTSPOT_SERIAL_HEAP_ALIGNMENT)
}

/// gci zl1: `bytes` rounded up to `alignment` (a `HeapAlignment` from
/// [`hotspot_heap_alignment`]); the largest aligned size when the rounding
/// would overflow. An alignment of 0 is taken as 1.
pub(crate) fn hotspot_heap_align_up_to(bytes: usize, alignment: usize) -> usize {
    let a = alignment.max(1);
    match bytes.div_ceil(a).checked_mul(a) {
        Some(b) => b,
        None => usize::MAX / a * a,
    }
}

/// gcg g4/yg: `(max, initial, min)` as HotSpot sizes them -- each aligned up
/// to the collector's `HeapAlignment` ([`hotspot_heap_alignment`]; Serial's
/// is [`HOTSPOT_SERIAL_HEAP_ALIGNMENT`]), the initial and minimum sizes held
/// at or below the aligned maximum.
pub(crate) fn hotspot_aligned_heap_sizes(
    max: usize,
    initial: usize,
    min: Option<usize>,
    alignment: usize,
) -> (usize, usize, Option<usize>) {
    let max = hotspot_heap_align_up_to(max, alignment);
    (
        max,
        hotspot_heap_align_up_to(initial, alignment).min(max),
        min.map(|m| hotspot_heap_align_up_to(m, alignment).min(max)),
    )
}

/// gcg g4/yg (`CRATONVM_GC_HOTSPOT_INITIAL_HEAP`): HotSpot's ergonomic
/// `InitialHeapSize` -- `basis * InitialRAMPercentage` (1.5625 % = `/ 64`) of
/// the physical (or container) memory, clamped to `[MinHeapSize, max]` and
/// aligned up, never above `max` (measured on 25.0.3, 16 GiB: `-Xmx64m` ->
/// 64M, `-Xmx512m` -> 254M, `-Xmx10m` -> 10M).
pub(crate) fn hotspot_ergonomic_initial_heap(basis: u64, min: Option<usize>, max: usize) -> usize {
    let want = usize::try_from(basis / 64).unwrap_or(usize::MAX);
    let lo = min.unwrap_or(0).min(max);
    hotspot_heap_align_up(want.clamp(lo, max)).min(max)
}

/// Interpreter round i5 wave 102, lane L6: validate
/// `-XX:DiagnoseSyncOnValueBasedClasses=<raw>` as HotSpot 25 does (an `int`
/// flag, range `[0, 2]`), answering the mode or the lines HotSpot prints on
/// stderr before `Error: Could not create the Java Virtual Machine.`:
/// `Improperly specified VM option 'DiagnoseSyncOnValueBasedClasses=x'` for a
/// value that is not an integer, preceded by `int
/// DiagnoseSyncOnValueBasedClasses=3 is outside the allowed range [ 0 ... 2 ]`
/// for one out of range.
pub(crate) fn diagnose_sync_on_value_based_classes_from_flag(raw: &str) -> Result<u8, Vec<String>> {
    const NAME: &str = "DiagnoseSyncOnValueBasedClasses";
    let improper = format!("Improperly specified VM option '{NAME}={raw}'");
    let t = raw.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let magnitude = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => digits.parse::<i64>().ok(),
    };
    let Some(magnitude) = magnitude.filter(|_| !digits.is_empty()) else {
        return Err(vec![improper]);
    };
    let v = if negative { -magnitude } else { magnitude };
    match u8::try_from(v) {
        Ok(mode) if mode <= 2 => Ok(mode),
        _ => Err(vec![
            format!("int {NAME}={v} is outside the allowed range [ 0 ... 2 ]"),
            improper,
        ]),
    }
}

/// gcg g3/x (`gcg-g2e-heap-free-ratio-flags-are-not-parsed`): HotSpot 25's
/// defaults for `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`.
pub(crate) const HOTSPOT_MIN_HEAP_FREE_RATIO: u64 = 40;
pub(crate) const HOTSPOT_MAX_HEAP_FREE_RATIO: u64 = 70;
/// [`heap_free_ratios_from_flags`], reading the two values in HotSpot's
/// `uintx` syntax ([`hotspot_parse_uintx_flag`]: one `k` / `m` / `g` / `t`
/// suffix) when `hotspot_syntax`. zt t12/tg5 (`zgc-zt11tg4-*` item 5):
/// local 25.0.3 boots `-XX:MinHeapFreeRatio=0k` (rc 0) and reads `=1k` as
/// 1024 (the range line); the decimal / hex reading refused `0k` as
/// `Improperly specified`. The launcher passes `true` on an effective ZGC
/// command line under `CRATONVM_LAUNCHER_ZGC_GC_VALUE_FLAG_REFUSALS` (whose
/// ordered scan already reads these flags so), `false` elsewhere: G1 and
/// Generational command lines are unchanged. Pure.
pub(crate) fn heap_free_ratios_from_flags_in(
    min: Option<&str>,
    max: Option<&str>,
    hotspot_syntax: bool,
) -> Result<(Option<u8>, Option<u8>), Vec<String>> {
    let one = |name: &str, raw: Option<&str>| -> Result<Option<u64>, Vec<String>> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let t = raw.trim();
        let parsed = if hotspot_syntax {
            hotspot_parse_uintx_flag(t)
        } else {
            match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => t.parse::<u64>().ok(),
            }
        };
        let improper = format!("Improperly specified VM option '{name}={raw}'");
        match parsed {
            None => Err(vec![improper]),
            Some(v) if v > 100 => Err(vec![
                format!("uintx {name}={v} is outside the allowed range [ 0 ... 100 ]"),
                improper,
            ]),
            Some(v) => Ok(Some(v)),
        }
    };
    let min_v = one("MinHeapFreeRatio", min)?;
    let max_v = one("MaxHeapFreeRatio", max)?;
    let lo = min_v.unwrap_or(HOTSPOT_MIN_HEAP_FREE_RATIO);
    let hi = max_v.unwrap_or(HOTSPOT_MAX_HEAP_FREE_RATIO);
    if lo > hi {
        return Err(vec![
            format!(
                "MinHeapFreeRatio ({lo}) must be less than or equal to MaxHeapFreeRatio ({hi})"
            ),
            format!(
                "MaxHeapFreeRatio ({hi}) must be greater than or equal to MinHeapFreeRatio ({lo})"
            ),
        ]);
    }
    // Both are <= 100 here.
    let narrow = |v: Option<u64>| v.and_then(|v| u8::try_from(v).ok());
    Ok((narrow(min_v), narrow(max_v)))
}

/// gci i11/gm finding 13: HotSpot 25's `G1HeapRegionSize` range ceiling
/// (`range(0, NOT_LP64(32M) LP64_ONLY(512M))`).
pub(crate) const HOTSPOT_G1_HEAP_REGION_SIZE_MAX: usize = 512 * 1024 * 1024;

/// gci i11/gm finding 13: validate `-XX:G1HeapRegionSize=` as HotSpot 25 does.
/// `Ok(Some(bytes))` for a value in `1..=512M` (the collector then raises it to
/// 1 MiB and rounds it up to a power of two, `gc::vm_heap::explicit_region_size`),
/// `Ok(None)` for 0 (ergonomic) or an unparsable value (the caller warns, as
/// before), or the lines HotSpot prints before `Error: Could not create the
/// Java Virtual Machine.` for a value above 512 MiB (measured on 25.0.3:
/// `size_t G1HeapRegionSize=1073741824 is outside the allowed range [ 0 ...
/// 536870912 ]`), followed by the `Improperly specified VM option` line every
/// range error of a `-XX:` flag carries (`Arguments::process_argument`; same
/// pair as `heap_free_ratios_from_flags`).
pub(crate) fn g1_heap_region_size_from_flag(raw: &str) -> Result<Option<usize>, Vec<String>> {
    match parse_size(raw) {
        Some(v) if v > HOTSPOT_G1_HEAP_REGION_SIZE_MAX => Err(vec![
            format!(
                "size_t G1HeapRegionSize={v} is outside the allowed range [ 0 ... {HOTSPOT_G1_HEAP_REGION_SIZE_MAX} ]"
            ),
            format!("Improperly specified VM option 'G1HeapRegionSize={raw}'"),
        ]),
        Some(0) | None => Ok(None),
        Some(v) => Ok(Some(v)),
    }
}

pub(crate) fn max_metaspace_limit(arg: Option<&str>) -> Option<u64> {
    let s = arg?;
    match parse_size(s) {
        Some(bytes) => Some(bytes as u64),
        None => {
            eprintln!("Warning: ignoring -XX:MaxMetaspaceSize={s} (expected a byte size)");
            None
        }
    }
}

pub(crate) fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, multiplier) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1024),
        b'm' | b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };

    // Use checked_mul so a huge value (e.g. `99999999999g`) fails the parse
    // rather than wrapping silently in release / panicking in debug. A parsed
    // size of 0 is also rejected as invalid (a 0-byte heap is meaningless).
    let n = num_str.parse::<usize>().ok()?;
    let bytes = n.checked_mul(multiplier)?;
    if bytes == 0 {
        return None;
    }
    Some(bytes)
}

/// Total physical RAM in bytes, or `None` if it can't be determined.
///
/// Re-exported from `cratonvm_vm::runtime::container`, which owns the platform
/// probes now that both the launcher and `SharedVm::new` size the default heap
/// through the same code.
pub(crate) fn physical_ram_bytes() -> Option<u64> {
    cratonvm_vm::runtime::container::physical_ram_bytes()
}

