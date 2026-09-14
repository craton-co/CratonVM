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

    #[test]
    fn a_panicking_body_returns_the_sentinel_and_is_counted() {
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
}
