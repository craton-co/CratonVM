// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! # Two `i64::MIN` sentinel checks, and the one every helper must satisfy
//!
//! (r10 help2 item 3, written down JIT round 22 lane sync.) Two distinct checks
//! read a helper's `i64::MIN`, and comments elsewhere describe one or the other
//! without saying which:
//!
//! * the INLINE post-call check in a compiled caller: type-specific, a plain
//!   `CMP RAX, i64::MIN; JE bail` for `int` / reference / void returns, and for
//!   `J` / `D` returns a `CALL` to `helpers::jit_dispatch_threw`, which peeks
//!   the out-of-band signals;
//! * the OUTER gate in `runtime::interpreter::jit_bridge`, reached when the
//!   whole compiled activation returns to the interpreter (its "MEDIUM fix"
//!   comment): `result == i64::MIN` is treated as a sentinel only beside a
//!   raised signal (the deopt flag, a pending implicit exception, a stashed
//!   frame), uniformly, whatever the Java return type; otherwise it is
//!   returned as the value.
//!
//! Every sentinel ends at the outer gate once control unwinds, so a helper
//! arm that returns `i64::MIN` for "should be unreachable" (no live JIT
//! thread, a null `vm_ptr`) must raise a signal beside it --
//! `helpers::set_jit_deopt_pending()` -- or the gate pushes the sentinel as the
//! method's real return value (a wild reference, for a reference-returning
//! helper). `jit_monitor_enter` / `jit_monitor_exit`, `native_integer_value_of`,
//! `native_long_value_of`, and (round 22) `jit_field_site_cp`,
//! `mh_hidden_static_call` and `reflect_ctor_link_redirect` each needed exactly
//! this one-line fix. An arm whose failure value is not `i64::MIN` (`0`, `-1`, a
//! NaN, `None`) is outside this rule.
//!
//! # Guarded helpers (65 named here; NOT exhaustive)
//!
//! Round 11: `vm/src/jit/helpers.rs` has ~90 `contain(` wrap sites. The list
//! below is the classified core as of its last audit; helpers added since (the
//! `Unsafe` / `ScopedMemoryAccess` accessors, the `VarHandle` write slots,
//! ...) carry their own policy at their wrap site, which
//! is the authority — `rg -n 'contain\(' vm/src/jit/helpers.rs`.
//!
//! `Throw`, sentinel `i64::MIN`: `service_callee_deopt`, `monitor_enter`,
//! `monitor_exit`, `aastore_type_check`, `getstatic`, `putstatic_int`,
//! `putstatic_long`, `putstatic_float`, `putstatic_double`, `putstatic_object`,
//! `checkcast`, `invoke_dispatch`, `invoke_virtual_mic`,
//! `varhandle_read_direct`, `varhandle_write_direct` (since round 11 wave 8;
//! it returned `()` before and its native's refusal escaped the caller's
//! `catch`), `varhandle_cas_direct`, `indy_bridge`,
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
//! `aastore` returns `()`, and `safepoint_slow_path` returns `0` (the
//! loop-exit verdict "stay compiled").
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
    /// Deoptimize and record against the helper's explicit VM.
    DeoptFor { vm_ptr: i64 },
    /// Count and report only.
    Record,
    /// Record a declining helper against its explicit VM without throwing.
    RecordFor { vm_ptr: i64 },
}

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
///
/// Opt-in. `1eb8ed7865`'s bulk flip of performance candidates turned it on by
/// default, which made a production VM abort at the first contained helper
/// panic and aborted this module's own tests (`the_deopt_policy_*`).
fn helper_panics_fatal() -> bool {
    static FATAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FATAL.get_or_init(|| false)}

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

/// VM-owned diagnostics; VM-less helpers use a thread-local instance.
pub struct HelperPanicState {
    count: AtomicU64,
    log: Mutex<PanicLog>,
}

impl Default for HelperPanicState {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            log: Mutex::new(PanicLog {
                recent: [None; RECENT_CAPACITY],
                next: 0,
                last: None,
                reported: Vec::new(),
            }),
        }
    }
}

thread_local! {
    static VMLESS_HELPER_PANICS: HelperPanicState = HelperPanicState::default();
}

fn with_current_panic_state<R>(f: impl FnOnce(&HelperPanicState) -> R) -> R {
    if let Some(vm) = crate::native::jni::calling_thread_vm_only() {
        f(&vm.mem.helper_panics)
    } else {
        let mut f = Some(f);
        VMLESS_HELPER_PANICS
            .try_with(|state| f.take().unwrap()(state))
            .unwrap_or_else(|_| f.take().unwrap()(&HelperPanicState::default()))
    }
}

fn with_panic_state<R>(on_panic: OnPanic, f: impl FnOnce(&HelperPanicState) -> R) -> R {
    if let OnPanic::Throw { vm_ptr }
    | OnPanic::RecordFor { vm_ptr }
    | OnPanic::DeoptFor { vm_ptr } = on_panic
    {
        if vm_ptr != 0 {
            // SAFETY: the guarded helper's own SharedVm argument; the body
            // has unwound but its compiled caller still owns this VM.
            return f(unsafe { &(*(vm_ptr as *const crate::vm::SharedVm)).mem.helper_panics });
        }
    }
    with_current_panic_state(f)
}

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
    with_panic_state(on_panic, |state| {
        state.count.fetch_add(1, Ordering::Relaxed);
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
            if record_helper_panic(state, name) {
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
                OnPanic::Deopt | OnPanic::DeoptFor { .. } => {
                    crate::jit::helpers::set_jit_deopt_pending()
                }
                OnPanic::Record | OnPanic::RecordFor { .. } => {}
            }
        });
        // A payload whose destructor panics must not escape either.
        let _ = cratonvm_jit::tiered::contain_compile_panic(move || drop(payload));
    });
}

/// Record one panic in `name`. Returns `true` the first time `name` is seen,
/// which is when its stderr line is owed.
fn record_helper_panic(state: &HelperPanicState, name: &'static str) -> bool {
    let mut log = state
        .log
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

/// Panics attributed to the calling thread's VM, or its VM-less scope.
pub fn jit_helper_panic_count() -> u64 {
    with_current_panic_state(|state| state.count.load(Ordering::Relaxed))
}

/// One line naming the calling VM's contained panics, or `None` when it
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
    with_current_panic_state(panic_summary_in)
}

pub fn jit_helper_panic_summary_for(vm: &crate::vm::SharedVm) -> Option<String> {
    panic_summary_in(&vm.mem.helper_panics)
}

fn panic_summary_in(state: &HelperPanicState) -> Option<String> {
    let count = state.count.load(Ordering::Relaxed);
    if count == 0 {
        return None;
    }
    let names: Vec<&'static str> = match state.log.try_lock() {
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
    with_current_panic_state(|state| {
        state
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last
    })
}

/// Up to the last eight helpers that panicked, oldest first. A name repeats
/// when that helper panicked more than once.
///
/// Never blocks: the crash report reads this, possibly on a thread that
/// faulted while holding the log's lock, so a contended lock yields an empty
/// list instead.
pub fn jit_helper_recent_panics() -> Vec<&'static str> {
    with_current_panic_state(|state| {
        let log = match state.log.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Vec::new(),
        };
        (0..RECENT_CAPACITY)
            .filter_map(|i| log.recent[(log.next + i) % RECENT_CAPACITY])
            .collect()
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod vm_isolation_tests;
