// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Panic containment for the `extern "C"` runtime helpers compiled code calls.
//!
//! # Why this exists
//!
//! The workspace builds with `panic = "unwind"`, and since Rust 1.81 a panic
//! that tries to unwind out of an `extern "C"` function aborts the process on
//! the spot: no Java-visible error, no `hs_err` context, no metric. The helpers
//! in [`crate::jit::helpers`] that re-enter the VM (resolve and load classes, run
//! `<clinit>`, allocate, take monitors, build exceptions, dispatch, walk frames)
//! reach deep call graphs full of `unwrap`, `expect`, indexing and `RefCell`
//! borrows, so one bad receiver used to kill the whole JVM.
//!
//! Every guarded helper is now a thin `extern "C"` shell whose body is a plain
//! Rust function run under [`contain`]. A panic in the body is caught at the
//! shell, and the helper returns its documented failure answer instead.
//!
//! # What happens on a panic
//!
//! 1. A process-wide counter ([`jit_helper_panic_count`]) is incremented, and
//!    the helper's name goes into a small ring ([`jit_helper_last_panic`],
//!    [`jit_helper_recent_panics`]). This is metrics state, not compatibility
//!    state.
//! 2. The first panic in each helper prints ONE `[jit-helper-panic]` line to
//!    stderr naming the helper and the panic payload. Later panics in the same
//!    helper are counted, not printed.
//! 3. The [`OnPanic`] policy the wrap site chose runs (below).
//! 4. The helper returns the sentinel the wrap site passed, so compiled code
//!    takes the exit it already has for that value.
//!
//! Steps 2 to 4 are what containment IS, and they are also why containment is
//! not a recovery: the sentinel is a wrong answer that nothing downstream can
//! tell from a right one. Two things exist to stop that being invisible.
//! `CRATONVM_JIT_HELPER_PANICS_FATAL=1` ([`helper_panics_fatal`]) replaces the
//! whole sequence with an `abort` at the FIRST panic, which is what a fuzz or
//! differential lane wants; and [`jit_helper_panic_summary`] is the end-of-run
//! line an exit-time surface prints unconditionally, so a run that took a
//! contained panic no longer looks like one that did not.
//!
//! The body runs inside `cratonvm_jit::tiered::contain_compile_panic`'s scope.
//! The VM's crash handler (`runtime::crash_handler::install_crash_handler`)
//! consults that scope from its panic hook, so a contained helper panic writes
//! no `hs_err_pid<pid>.log` and does not latch the one-report-per-process
//! guard. The scope's name predates this module; the contract it states is
//! "a `catch_unwind` on this thread is about to receive this unwind", which is
//! exactly what holds here. Panic hooks still run before the unwind reaches
//! the guard: that handler chains to the previous hook, which prints the
//! default panic message, and `vm-cli` installs its own hook, which prints
//! every panic and does not consult the scope. The one-line-per-helper promise
//! above is about this module's own report.
//!
//! # The three policies
//!
//! * [`OnPanic::Throw`]: for helpers whose normal path can already allocate,
//!   run Java or park. Their call sites are GC safepoints by construction, so
//!   building a `java.lang.InternalError` there adds no new hazard. The error
//!   names the helper and is stashed on the same pending-exception channel the
//!   helpers use for NPE, CCE and OOM (`set_jit_pending_exception`). If it
//!   cannot be built (no JIT thread, no VM, heap exhausted), only the sentinel
//!   is returned, the same fallback `jit_alloc_oom` has.
//! * [`OnPanic::Deopt`]: for `uncommon_trap`, a leaf call site that publishes
//!   no oop map and whose answer is already "reinterpret". Allocating a
//!   throwable there could move an object that
//!   the compiled frame still names from an unpublished slot, so this policy
//!   allocates nothing. It raises the out-of-band deopt flag
//!   (`set_jit_deopt_pending`), which is what tells the interpreter that an
//!   `i64::MIN` return is a sentinel and not a real `Long.MIN_VALUE`.
//! * [`OnPanic::Record`]: counter and stderr only, for fast paths whose
//!   failure answer is "declined" (`ffm_segment_get` / `_set`). There the
//!   caller runs the authoritative slow path, which surfaces its own errors, so
//!   a stashed throwable would be a second, unrelated report.
//!
//! A void helper guarded with `Throw` has no way to tell its caller anything.
//! The stashed `InternalError` is delivered at the next pending-exception drain
//! on that thread, not at the faulting instruction.
//!
//! # Guarded helpers (65)
//!
//! `Throw`, sentinel `i64::MIN`: `service_callee_deopt`, `monitor_enter`,
//! `monitor_exit`, `aastore_type_check`, `getstatic`, `putstatic_int`,
//! `putstatic_long`, `putstatic_float`, `putstatic_double`, `putstatic_object`,
//! `checkcast`, `invoke_dispatch`, `invoke_virtual_mic`,
//! `varhandle_read_direct`, `varhandle_cas_direct`, `indy_bridge`,
//! `integer_value_of_direct`, `integer_int_value_direct`,
//! `long_value_of_direct`, `long_long_value_direct`, `dbb_put_byte_direct`,
//! `dbb_get_byte_direct`, `md_update_byte_direct`,
//! `preconditions_check_index_direct`, `buffer_session_direct`,
//! `thread_current_thread_direct`, `concurrent_hashmap_get_direct`,
//! `hashmap_get_direct`, `string_latin1_to_lower_direct`,
//! `hashmap_put_direct`, `lambda_int_to_double`, `self_call_stack_guard`.
//!
//! `Throw`, sentinel `0`/null (`emit_post_alloc_oom_check`, or the helper's own
//! `0`-means-pending convention): `newarray`, `new_object`, `new_object_cp`,
//! `anewarray_object`, `anewarray_object_cp`, `multianewarray_2d`,
//! `ldc_class_cp`, `ldc_string_cp`, `ldc_string`, and `instanceof` (which has
//! no post-call check, so `0`, "not an instance", is the least wrong answer).
//!
//! `Throw`, other sentinels: `local_handler_lookup` returns `-1` (propagate),
//! and `aastore`, `varhandle_write_direct` and `safepoint_slow_path` return
//! `()`.
//!
//! `Deopt`: `uncommon_trap` (`DEOPT_ACTION_REINTERPRET`), leaf readers
//! (`jit_baload`, `jit_iaload`, `jit_aaload`, `jit_arraylength`, `jit_getfield`),
//! throw stubs (`jit_throw_aioobe`, `jit_throw_arithmetic`,
//! `jit_throw_exception`, `jit_npe_with_action`), `jit_post_tlab_init`, and
//! primitive stores (`jit_bastore`, `jit_iastore`, `jit_putfield_int`,
//! `jit_putfield_long`, `jit_putfield_float`, `jit_putfield_double`).
//!
//! `Record`: `ffm_segment_get` and `ffm_segment_set` (`0`, declined).
//!
//! # Deliberately NOT guarded
//!
//! These must stay panic-free. A new helper that cannot meet that bar must be
//! guarded instead.
//!
//! * `jit_set_deopt_pending`, `jit_set_throw_bci`, `jit_get_current_thread`
//!   and `jit_dispatch_threw` touch one thread-local each. `dispatch_threw` is
//!   also the peek every `J`/`D` sentinel check relies on, and must never stash
//!   anything itself.
//! * `jit_native_stack_floor` is a stack-bounds query with saturating
//!   arithmetic, called from a prologue, and has no failure answer.
//! * `jit_frame_record` and `jit_verify_inline_frame_record` are prologue
//!   bookkeeping: one TLS store, or one read and a log line.
//! * `jit_math_fma_double`, `jit_math_fma_float`, `jit_frem` and `jit_drem`
//!   are pure arithmetic.
//! * `jit_reachability_fence_direct` is `black_box` and nothing else.
//! * `jit_resolve_static_base` is the compile-time statics resolver: three
//!   atomic loads, called by the compiler rather than by compiled code.
//! * `jit_arm_savebase_watch` is `#[naked]` and cannot be wrapped;
//!   `arm_savebase_watch_inner` and `jit_disarm_savebase_watch` belong to the
//!   crash-handler watch.
//! * `jit_write_barrier`, `jit_g1_post_write_barrier`,
//!   `jit_satb_pre_write_barrier` and `jit_putfield_object` are left out on
//!   purpose, although they reach collector code. Their call sites publish no
//!   oop map, so a guard could only drop the card mark, remembered-set entry
//!   or SATB record the store needed and carry on. That is a latent
//!   use-after-free, and the abort is the better outcome.
//!
//! # What containment does not restore
//!
//! Unwinding runs destructors, so every `JitThreadGuard`, lock guard and
//! `RefCell` borrow in the body is released. State that is restored by an
//! explicit call rather than by `Drop` is not. That includes a
//! `set_jit_thread` scope, a thread-state transition, a monitor entered but not
//! yet recorded, and a half-written object. A panic raised while a panic is
//! already unwinding (a panicking `Drop`) still aborts. So does a panic in an
//! unguarded helper reached through compiled code called from a guarded one.

use std::any::Any;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// What a guarded helper does, besides counting and reporting, when its body
/// panics. See the module documentation for when each applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnPanic {
    /// Stash a `java.lang.InternalError` naming the helper on the JIT
    /// pending-exception channel. `vm_ptr` is the helper's own `SharedVm`
    /// argument, or `0` for a helper that has none (the process VM is used).
    Throw { vm_ptr: i64 },
    /// Allocate nothing; raise the out-of-band deopt flag.
    Deopt,
    /// Count and report only.
    Record,
}

/// Every helper panic contained since process start.
static HELPER_PANICS: AtomicU64 = AtomicU64::new(0);

/// `CRATONVM_JIT_HELPER_PANICS_FATAL=1` — abort on the FIRST contained panic
/// instead of returning the helper's failure sentinel.
///
/// # Why a switch and not a policy
///
/// Containment is the right default in production: a panicking helper that
/// takes the whole JVM with it turns one bad receiver into a total loss, and
/// the three [`OnPanic`] policies exist so the failure lands on a channel the
/// caller already handles. But containment is *not* a recovery. Every
/// contained panic is a silent wrong answer — `jit_instanceof` returns `0`
/// ("not an instance"), `jit_bastore` drops the store, `jit_aastore` drops the
/// store and stashes an `InternalError` delivered at some later drain — and a
/// run that produced wrong results FOR THAT REASON scores exactly the same as
/// one that did not on stdout, on a checksum, and on the exit code.
///
/// So the fuzz and differential lanes want the opposite default: stop at the
/// first one, in the process that caused it, with the panicking helper still
/// on the stack. That is what this switch buys, and it is why the body uses
/// `abort` rather than `exit` — the artefact wanted is a core file, not a
/// status code.
///
/// Latched in a `OnceLock` because it is read on the panic path, which must
/// not allocate or take the flag machinery's locks more than once, and because
/// the answer cannot legitimately change during a run.
fn helper_panics_fatal() -> bool {
    static FATAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FATAL
        .get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_HELPER_PANICS_FATAL"))
}

const RECENT_CAPACITY: usize = 8;
/// Upper bound on distinct helper names remembered for the one-line-per-helper
/// stderr report. There are fewer guarded helpers than this, so it is a bound
/// on memory, not a policy.
const REPORTED_CAPACITY: usize = 256;

struct PanicLog {
    /// Ring of the most recent panicking helpers; `next` is the write cursor.
    recent: [Option<&'static str>; RECENT_CAPACITY],
    next: usize,
    last: Option<&'static str>,
    /// Helpers that have already printed their stderr line.
    reported: Vec<&'static str>,
}

/// Touched only on the panic path and by the metric readers, never by a helper
/// that returns normally.
static PANIC_LOG: Mutex<PanicLog> = Mutex::new(PanicLog {
    recent: [None; RECENT_CAPACITY],
    next: 0,
    last: None,
    reported: Vec::new(),
});

/// Run a helper body with its panics contained.
///
/// On the normal path this is `catch_unwind` plus a thread-local scope depth.
/// It takes no lock and allocates nothing. On a panic it counts, reports,
/// applies `on_panic` and returns `sentinel`. Pass the helper's DOCUMENTED
/// failure sentinel. Two helpers with the same return type do not share one,
/// which is why it is an argument rather than a trait.
#[inline(always)]
pub(crate) fn contain<R>(
    name: &'static str,
    on_panic: OnPanic,
    sentinel: R,
    body: impl FnOnce() -> R,
) -> R {
    match cratonvm_jit::tiered::contain_compile_panic(body) {
        Ok(value) => value,
        Err(payload) => {
            helper_panicked(name, on_panic, payload);
            sentinel
        }
    }
}

/// The panic path of [`contain`]. Must itself never unwind: it runs inside the
/// `extern "C"` shell, where an escaping panic is the abort this module exists
/// to prevent.
#[cold]
#[inline(never)]
fn helper_panicked(name: &'static str, on_panic: OnPanic, payload: Box<dyn Any + Send>) {
    HELPER_PANICS.fetch_add(1, Ordering::Relaxed);
    if helper_panics_fatal() {
        // `abort`, not `exit`: the whole point of the switch is a core file
        // with the panicking helper still on the stack. Reported BEFORE the
        // ordinary one-line-per-helper report below, because that report is
        // suppressed for a repeat of a helper that has already printed and
        // this line must never be.
        //
        // `writeln!` on a locked handle, not `eprintln!`, for the same reason
        // the report below uses it: the macro panics when stderr is closed,
        // and this function must not unwind. The report is best-effort and
        // the `abort` is not — rendering the payload is itself arbitrary user
        // code (`Display` on a `String` today, anything tomorrow), so it runs
        // inside a containment scope and a failure there costs the line, not
        // the core file.
        let _ = cratonvm_jit::tiered::contain_compile_panic(|| {
            let detail = cratonvm_jit::tiered::panic_payload_message(&*payload);
            let _ = writeln!(
                std::io::stderr(),
                "[jit-helper-panic] FATAL ({name}): CRATONVM_JIT_HELPER_PANICS_FATAL \
                 is set. A contained panic is a wrong answer, not a recovery — this \
                 helper would have returned its failure sentinel and compiled code \
                 would have carried on with it ({on_panic:?}): {detail}"
            );
            let _ = std::io::stderr().flush();
        });
        std::process::abort();
    }
    let _ = cratonvm_jit::tiered::contain_compile_panic(|| {
        let detail = cratonvm_jit::tiered::panic_payload_message(&*payload);
        if record_helper_panic(name) {
            // `writeln!` on a locked handle, not `eprintln!`: the macro panics
            // when stderr is closed.
            let _ = writeln!(
                std::io::stderr(),
                "[jit-helper-panic] {name} panicked; contained ({on_panic:?}); \
                 later panics in this helper are counted, not printed: {detail}"
            );
        }
        match on_panic {
            OnPanic::Throw { vm_ptr } => {
                let _ = crate::jit::helpers::stash_jit_helper_panic(vm_ptr, name, &detail);
            }
            OnPanic::Deopt => crate::jit::helpers::set_jit_deopt_pending(),
            OnPanic::Record => {}
        }
    });
    // A payload whose destructor panics must not escape either.
    let _ = cratonvm_jit::tiered::contain_compile_panic(move || drop(payload));
}

/// Record one panic in `name`. Returns `true` the first time `name` is seen,
/// which is when its stderr line is owed.
fn record_helper_panic(name: &'static str) -> bool {
    let mut log = PANIC_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cursor = log.next;
    log.recent[cursor] = Some(name);
    log.next = (cursor + 1) % RECENT_CAPACITY;
    log.last = Some(name);
    if log.reported.contains(&name) {
        return false;
    }
    if log.reported.len() < REPORTED_CAPACITY {
        log.reported.push(name);
    }
    true
}

/// Number of JIT helper panics contained since process start.
pub fn jit_helper_panic_count() -> u64 {
    HELPER_PANICS.load(Ordering::Relaxed)
}

/// One line naming every contained panic this process took, or `None` when it
/// took none.
///
/// # Why this exists
///
/// Until it did, [`jit_helper_panic_count`] had exactly ONE consumer in the
/// whole tree — `runtime::crash_handler`, which is reached only when something
/// *else* has already gone fatally wrong. Nothing in `cargo test`, in the
/// difftest gate, or in any benchmark read it. A run in which a helper
/// panicked, returned its failure sentinel and produced a wrong answer looked
/// identical to a clean one everywhere a human or a CI job was looking.
///
/// The stderr line the guard prints is one per DISTINCT helper and is emitted
/// at the moment of the first panic, which is the right thing for a log and
/// the wrong thing for a verdict: it is thousands of lines back by the time
/// the run ends, and a repeat is not printed at all. This is the end-of-run
/// form — a total, and the distinct names behind it — for an exit-time surface
/// to print unconditionally.
///
/// Never blocks, for the same reason [`jit_helper_recent_panics`] does not: a
/// caller may be running on a thread that faulted while holding the log's
/// lock. A contended lock costs the names, not the count.
pub fn jit_helper_panic_summary() -> Option<String> {
    let count = jit_helper_panic_count();
    if count == 0 {
        return None;
    }
    let names: Vec<&'static str> = match PANIC_LOG.try_lock() {
        Ok(guard) => guard.reported.clone(),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner().reported.clone(),
        Err(std::sync::TryLockError::WouldBlock) => Vec::new(),
    };
    let helpers = if names.is_empty() {
        // The log was locked by a thread that is not going to release it.
        "helper names unavailable (the panic log was contended)".to_string()
    } else {
        format!("{} distinct helper(s): {}", names.len(), names.join(", "))
    };
    Some(format!(
        "[jit-helper-panic] {count} contained helper panic(s), {helpers}. A contained \
         panic is a WRONG ANSWER, not a recovery — the helper returned its failure \
         sentinel and compiled code carried on with it. Re-run with \
         CRATONVM_JIT_HELPER_PANICS_FATAL=1 to abort at the first one."
    ))
}

/// The helper that most recently panicked, if any has.
pub fn jit_helper_last_panic() -> Option<&'static str> {
    PANIC_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .last
}

/// Up to the last eight helpers that panicked, oldest first. A name repeats
/// when that helper panicked more than once.
///
/// Never blocks: the crash report reads this, possibly on a thread that
/// faulted while holding the log's lock, so a contended lock yields an empty
/// list instead.
pub fn jit_helper_recent_panics() -> Vec<&'static str> {
    let log = match PANIC_LOG.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Vec::new(),
    };
    (0..RECENT_CAPACITY)
        .filter_map(|i| log.recent[(log.next + i) % RECENT_CAPACITY])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_jit::tiered::compile_panic_is_contained;

    // The counter and the ring are process-wide and `cargo test` runs tests
    // concurrently, so every test uses its own helper name and asserts
    // "at least" on the counter rather than an exact delta.
    //
    // That handles COLLISION. It does not handle CAPACITY, which is what
    // actually broke: `RECENT_CAPACITY` is 8, eight tests in this module panic
    // a helper, and they run concurrently -- so under load the ring can wrap
    // and evict a test's own entry between the `contain` that wrote it and the
    // `jit_helper_recent_panics()` that reads it back. Observed once in a
    // full-workspace run on 2026-09-16
    // (`a_panicking_body_returns_the_sentinel_and_is_counted`), never in
    // isolation. Distinct names cannot help; eight slots shared by eight
    // writers is a capacity problem.
    //
    // Every test that panics a helper takes this lock -- WRITERS as well as
    // readers. Guarding only the readers is not enough and was measured not to
    // be: an unguarded test that panics still writes the ring, so it can evict
    // a lock-holder's entry between that holder's write and its read. One
    // failure in sixteen concurrent runs survived the readers-only version.
    // Poison is ignored: a panicking test here is a real failure to report,
    // not a reason to fail the others.
    static RING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_panicking_body_returns_the_sentinel_and_is_counted() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::panics_record";
        let before = jit_helper_panic_count();
        let got = contain(NAME, OnPanic::Record, -7i64, || -> i64 {
            panic!("deliberate helper panic (test)")
        });
        assert_eq!(
            got, -7,
            "a contained panic must return the wrap site's sentinel"
        );
        assert!(
            jit_helper_panic_count() > before,
            "a contained panic must increment the counter"
        );
        assert!(
            jit_helper_recent_panics().contains(&NAME),
            "a contained panic must record the helper name"
        );
    }

    #[test]
    fn a_body_that_returns_normally_is_untouched() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::returns_normally";
        assert_eq!(contain(NAME, OnPanic::Record, i64::MIN, || 42i64), 42);
        assert_eq!(
            contain(NAME, OnPanic::Record, 0usize, || usize::MAX),
            usize::MAX
        );
        contain(NAME, OnPanic::Record, (), || ());
        assert!(
            !jit_helper_recent_panics().contains(&NAME),
            "a body that did not panic must not be recorded"
        );
    }

    /// The crash handler's panic hook skips `hs_err` only inside this scope,
    /// so a body that stopped running inside it would write a false crash
    /// report for every contained panic.
    #[test]
    fn the_body_runs_inside_the_scope_the_crash_handler_honours() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::scope";
        assert!(!compile_panic_is_contained());
        let inside = contain(NAME, OnPanic::Record, false, compile_panic_is_contained);
        assert!(
            inside,
            "the guarded body must run inside the contained-panic scope"
        );
        assert!(!compile_panic_is_contained());
    }

    #[test]
    fn only_the_first_panic_in_a_helper_is_reported() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::dedup";
        assert!(
            record_helper_panic(NAME),
            "the first panic in a helper owes a line"
        );
        assert!(
            !record_helper_panic(NAME),
            "a repeat panic must not print again"
        );
        let recorded = jit_helper_recent_panics()
            .iter()
            .filter(|n| **n == NAME)
            .count();
        assert_eq!(recorded, 2, "both panics are still recorded");
    }

    #[test]
    fn a_non_string_payload_is_still_contained() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::any_payload";
        let got = contain(NAME, OnPanic::Record, 3u8, || -> u8 {
            std::panic::panic_any(1234u32)
        });
        assert_eq!(got, 3);
    }

    /// `Deopt` must raise the flag that turns an `i64::MIN` return into a
    /// sentinel. The flag is thread-local, so this is deterministic.
    #[test]
    fn the_deopt_policy_raises_the_deopt_flag() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::deopt";
        let _ = crate::jit::helpers::take_jit_deopt_pending();
        let got = contain(NAME, OnPanic::Deopt, i64::MIN, || -> i64 {
            panic!("deliberate leaf helper panic (test)")
        });
        assert_eq!(got, i64::MIN);
        assert!(
            crate::jit::helpers::take_jit_deopt_pending(),
            "a contained leaf panic must leave the deopt flag set"
        );
    }

    /// `Deopt` on a void helper (e.g. `jit_bastore`, `jit_putfield_*`) must also
    /// raise the deopt flag so the interpreter drains and recovers.
    #[test]
    fn the_deopt_policy_works_for_void_helpers() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::deopt_void";
        let _ = crate::jit::helpers::take_jit_deopt_pending();
        contain(NAME, OnPanic::Deopt, (), || {
            panic!("deliberate void helper panic (test)")
        });
        assert!(
            crate::jit::helpers::take_jit_deopt_pending(),
            "a contained void helper panic must leave the deopt flag set"
        );
    }

    /// The `Throw` policy with no JIT thread installed, which is every unit
    /// test thread: the stash declines, and the sentinel is still returned. A
    /// stash that actually builds the `InternalError` needs a booted VM and is
    /// not covered here.
    #[test]
    fn the_throw_policy_degrades_to_the_sentinel_without_a_jit_thread() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::throw_no_thread";
        assert!(!crate::jit::helpers::is_jit_thread_set());
        assert!(
            !crate::jit::helpers::stash_jit_helper_panic(0, NAME, "detail"),
            "no JIT thread means nothing can be stashed"
        );
        let got = contain(NAME, OnPanic::Throw { vm_ptr: 0 }, 0i64, || -> i64 {
            panic!("deliberate allocating helper panic (test)")
        });
        assert_eq!(got, 0);
        assert!(jit_helper_recent_panics().contains(&NAME));
    }

    /// The end-of-run surface, which is the whole point of RT-1's second half:
    /// a process that took a contained panic must be able to SAY SO at exit,
    /// rather than the fact living only in a stderr line thousands of lines
    /// back and in a counter the crash handler alone reads.
    ///
    /// Asserted on content, not on formatting: the count is a number the
    /// summary must carry, and the panicking helper's own name must appear,
    /// because "N panics happened" without "in which helper" is not actionable
    /// and was the state this replaced. The exact wording is deliberately not
    /// pinned — the nightly sensor greps for the `[jit-helper-panic]` tag and
    /// nothing else.
    #[test]
    fn the_end_of_run_summary_names_the_helpers_that_panicked() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "helper_guard_test::summary";
        let _ = contain(NAME, OnPanic::Record, 0i64, || -> i64 {
            panic!("deliberate helper panic (test)")
        });
        let summary = jit_helper_panic_summary()
            .expect("a process that contained a panic must produce a summary");
        assert!(
            summary.starts_with("[jit-helper-panic]"),
            "the summary must carry the tag the nightly sensor greps for: {summary}"
        );
        assert!(
            summary.contains(&jit_helper_panic_count().to_string()),
            "the summary must carry the count: {summary}"
        );
        assert!(
            summary.contains(NAME),
            "the summary must name the helper that panicked: {summary}"
        );
    }

    /// The counter is process-wide and monotone, so the zero arm cannot be
    /// asserted in a process that also runs the tests above. What CAN be
    /// asserted, and is the part that would actually break, is that the
    /// summary is keyed on the counter rather than on the name log: a fresh
    /// process has taken no panic, so it must produce no line at all — an
    /// exit-time surface that printed "0 contained helper panics" on every
    /// clean run would be noise nobody reads, which is how the previous
    /// sensor failed.
    #[test]
    fn the_summary_is_absent_exactly_when_the_counter_is_zero() {
        let _ring = RING.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            jit_helper_panic_summary().is_none(),
            jit_helper_panic_count() == 0,
            "the summary's presence must be exactly the counter's non-zero-ness"
        );
    }

    /// The fatal switch is read once and latched, and it must be OFF unless
    /// the operator asked for it. This cannot test the `abort` arm — a test
    /// that aborts takes the whole test binary with it — so what is pinned is
    /// the default, which is the half that a mis-parsed flag would break: a
    /// presence-only read of `CRATONVM_JIT_HELPER_PANICS_FATAL=0` would turn
    /// the switch ON, and every contained panic in production would become a
    /// crash. `runtime_flag_on` is the parser that gets that right; this
    /// asserts the wiring reaches it.
    #[test]
    fn the_fatal_switch_is_off_unless_it_is_asked_for() {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_HELPER_PANICS_FATAL").is_some() {
            // Someone is deliberately running the suite with the switch armed.
            // Nothing to assert, and nothing is wrong.
            return;
        }
        assert!(
            !helper_panics_fatal(),
            "CRATONVM_JIT_HELPER_PANICS_FATAL must be off in an ordinary run; \
             a contained panic is the default because aborting the JVM over one \
             bad receiver is worse than the wrong answer"
        );
    }
}
