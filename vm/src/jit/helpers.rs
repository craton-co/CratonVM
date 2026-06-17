// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//!
//! These functions need access to `SharedVm` and other VM internals, so they
//! live in the VM crate rather than the standalone JIT crate.

use std::cell::Cell;

use cratonvm_jit::{
    DescriptorParamIter, JitInvokeInfo, JitMICSlot, JitPICSlot, JitRuntimeHelpers,
};
use cratonvm_types::{
    ArrayElementType, ClassId, ObjectRef, Value,
    ARRAY_LENGTH_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};

use crate::memory::vm_heap::VmHeap;
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

// ---------------------------------------------------------------------------
// WS1 diagnostic profiling for the JIT dispatch helpers
// (env-gated: CRATONVM_DBG_MIC_PROF=1; zero-cost when off beyond one cached
// bool branch). Dumps cumulative path counts + rdtsc cycle totals to stderr
// every 2^24 `jit_invoke_virtual_mic` entries, so the last dump before exit
// approximates the run's totals.
// ---------------------------------------------------------------------------

pub mod mic_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static MIC_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static MIC_HIT_ENTRY: AtomicU64 = AtomicU64::new(0);
    pub static MIC_HIT_NOENTRY: AtomicU64 = AtomicU64::new(0);
    pub static MIC_MISS: AtomicU64 = AtomicU64::new(0);
    pub static MIC_LAMBDA: AtomicU64 = AtomicU64::new(0);
    pub static CYC_MIC_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static CYC_HIT_ENTRY_CALL: AtomicU64 = AtomicU64::new(0);
    pub static CYC_INVOKE: AtomicU64 = AtomicU64::new(0);
    pub static CYC_COMPILE_PROBE: AtomicU64 = AtomicU64::new(0);
    pub static DISP_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static CYC_DISP_TOTAL: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("CRATONVM_DBG_MIC_PROF").is_some())
    }

    #[inline]
    pub fn now() -> u64 {
        // SAFETY: rdtsc is unprivileged on x86-64.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// Cycle accumulator that survives early returns.
    pub struct CycGuard {
        t0: u64,
        ctr: &'static AtomicU64,
    }
    impl CycGuard {
        pub fn new(ctr: &'static AtomicU64) -> Option<Self> {
            if enabled() {
                Some(Self { t0: now(), ctr })
            } else {
                None
            }
        }
    }
    impl Drop for CycGuard {
        fn drop(&mut self) {
            self.ctr.fetch_add(now().wrapping_sub(self.t0), Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn bump(ctr: &'static AtomicU64) {
        if enabled() {
            ctr.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counterpart of [`dump_maybe`] driven from `jit_invoke_dispatch`, so a
    /// workload that's hot through the dispatch helper but cold through the
    /// MIC helper still produces dumps.
    pub fn dump_maybe_disp() {
        if !enabled() {
            return;
        }
        let calls = DISP_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        if calls & ((1 << 20) - 1) != 0 {
            return;
        }
        dump_now();
    }

    pub fn dump_maybe() {
        if !enabled() {
            return;
        }
        let calls = MIC_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        if calls & ((1 << 20) - 1) != 0 {
            return;
        }
        dump_now();
    }

    pub fn dump_now() {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        eprintln!(
            "[MIC_PROF] quiesce_depth={} mic_calls={} hit_entry={} hit_noentry={} miss={} lambda={} \
             cyc_mic_total={} cyc_hit_entry_call={} cyc_invoke={} cyc_compile_probe={} \
             disp_calls={} cyc_disp_total={}",
            cratonvm_gc::gc_quiescence::depth(),
            g(&MIC_CALLS),
            g(&MIC_HIT_ENTRY),
            g(&MIC_HIT_NOENTRY),
            g(&MIC_MISS),
            g(&MIC_LAMBDA),
            g(&CYC_MIC_TOTAL),
            g(&CYC_HIT_ENTRY_CALL),
            g(&CYC_INVOKE),
            g(&CYC_COMPILE_PROBE),
            g(&DISP_CALLS),
            g(&CYC_DISP_TOTAL),
        );
    }
}

// ---------------------------------------------------------------------------
// Thread-local JvmThread pointer for JIT helper access
// ---------------------------------------------------------------------------

thread_local! {
    /// DIAGNOSTIC: name of the most recently dispatched JIT callee on this
    /// thread (`class.method desc`). Set at the top of `jit_invoke_dispatch` /
    /// `jit_invoke_virtual_mic`. Read by `jit_putfield_int` when it sees a
    /// non-canonical receiver, to name the miscompiled method. Only touched on
    /// dispatch (cheap) and never in release-critical inner loops.
    static CURRENT_JIT_CALLEE: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };

    /// Stores a raw pointer to the current thread's JvmThread.
    /// Safety invariant: only ONE `&mut JvmThread` is derived from this at a time,
    /// and only within a single JIT helper call scope. The pointer is set before
    /// entering JIT code and cleared immediately after.
    static JIT_THREAD: Cell<*mut JvmThread> = const { Cell::new(std::ptr::null_mut()) };

    /// Pending Java exception from JIT dispatch. When `jit_invoke_dispatch` calls
    /// a method that throws, we store the exception here instead of swallowing it.
    /// The interpreter checks this after JIT code returns and propagates it through
    /// the normal exception handling path (exception tables, frame unwinding).
    static JIT_PENDING_EXCEPTION: Cell<Option<ObjectRef>> = const { Cell::new(None) };

    /// Pending AIOOBE from JIT bounds check.  Set by `jit_throw_aioobe`,
    /// consumed by the interpreter after JIT code returns `i64::MIN`.
    static JIT_PENDING_AIOOBE: Cell<Option<(i64, i64)>> = const { Cell::new(None) };

    /// Pending NullPointerException from a JIT array helper (`jit_iaload`,
    /// `jit_aaload`, `jit_arraylength` called with a null array reference).
    /// Consumed by the interpreter post-JIT-return path the same way as
    /// `JIT_PENDING_AIOOBE`. The helper returns `i64::MIN` to signal deopt;
    /// the interpreter detects the sentinel, takes this flag, and throws a
    /// real `NullPointerException` through the method's exception table.
    static JIT_PENDING_NPE: Cell<bool> = const { Cell::new(false) };

    /// Debug-only reentrancy guard for [`jit_thread_mut`]. Set while a
    /// `&mut JvmThread` handed out by `jit_thread_mut` is considered live, and
    /// cleared when the [`JitThreadGuard`] returned alongside it is dropped.
    /// A nested/aliasing `jit_thread_mut` call observes the set flag and trips
    /// the `debug_assert!`. Compiled out entirely in release builds, so release
    /// behaviour is unchanged.
    #[cfg(debug_assertions)]
    static JIT_THREAD_BORROWED: Cell<bool> = const { Cell::new(false) };
}

/// Debug-only: snapshot the borrow flag and clear it, so a nested JIT entry
/// (the interpreter re-entering JIT from inside a bail) starts a fresh borrow
/// level. Returns the previous value for [`restore_jit_borrow`]. No-op in
/// release builds.
#[cfg(debug_assertions)]
fn suspend_jit_borrow() -> bool {
    JIT_THREAD_BORROWED.with(|b| {
        let prev = b.get();
        b.set(false);
        prev
    })
}

/// Debug-only: restore the borrow flag suspended by [`suspend_jit_borrow`]
/// once the nested JIT call has returned. No-op in release builds.
#[cfg(debug_assertions)]
fn restore_jit_borrow(prev: bool) {
    JIT_THREAD_BORROWED.with(|b| b.set(prev));
}

/// Debug-only RAII guard that marks the `jit_thread_mut` borrow as released
/// when dropped. In release builds this is a zero-sized no-op.
pub(crate) struct JitThreadGuard {
    #[cfg(debug_assertions)]
    _private: (),
}

impl Drop for JitThreadGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| b.set(false));
    }
}

/// Opaque token returned by [`set_jit_thread`] and consumed by
/// [`restore_jit_thread`]. Bundles the previously-stored raw thread pointer
/// with the suspended debug borrow flag, so a strictly-nested JIT re-entry
/// (outer `jit_invoke_dispatch` → interpreter bail → inner
/// `jit_invoke_dispatch`) gets a clean borrow level while the outer borrow is
/// frozen on the call stack for the nested call's duration.
pub struct JitThreadScope {
    prev_ptr: *mut JvmThread,
    /// Shadow-stack `top` watermark captured at JIT entry (follow-up §3). On exit
    /// [`restore_jit_thread`] resets `top` to this value, healing any push an
    /// abnormal JIT exit (exception/deopt that skipped a method epilogue) left
    /// unbalanced. `None` when the shadow-stack gate is off (no-op). On a normal
    /// exit the per-method epilogues already restored `top`, so the reset is a
    /// no-op there.
    saved_shadow_top: Option<usize>,
    #[cfg(debug_assertions)]
    prev_borrow: bool,
}

/// DIAGNOSTIC: read the current dispatched JIT callee name.
fn current_jit_callee() -> String {
    CURRENT_JIT_CALLEE.with(|c| c.borrow().clone())
}

/// DIAGNOSTIC: crash-handler-safe read of the current JIT callee name.
/// Uses `try_with`/`try_borrow` so it never panics if the TLS is being
/// destroyed or the `RefCell` is already borrowed when a fault lands here.
/// Returns an empty string if unavailable. Only meaningful when
/// `CRATONVM_DBG_JIT_PUTFIELD=1` (that gate is what populates the cell).
pub fn current_jit_callee_for_crash() -> String {
    CURRENT_JIT_CALLEE
        .try_with(|c| c.try_borrow().map(|s| s.clone()).unwrap_or_default())
        .unwrap_or_default()
}

/// DIAGNOSTIC RAII guard: records `info` as the current callee, and on drop
/// restores the PREVIOUS value. This makes `current_jit_callee()` name the
/// method whose body is *currently executing inline* (the one containing a
/// faulting putfield), rather than a sub-call it dispatched and returned from.
struct JitCalleeGuard(String);
impl JitCalleeGuard {
    fn new(info: &JitInvokeInfo) -> Self {
        let prev = CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            let prev = s.clone();
            s.clear();
            s.push_str(info.class_name);
            s.push('.');
            s.push_str(info.method_name);
            s.push_str(info.descriptor);
            prev
        });
        JitCalleeGuard(prev)
    }
}
impl Drop for JitCalleeGuard {
    fn drop(&mut self) {
        CURRENT_JIT_CALLEE.with(|c| {
            let mut s = c.borrow_mut();
            s.clear();
            s.push_str(&self.0);
        });
    }
}

/// Set the current thread's JvmThread pointer for JIT helper access.
/// Returns a [`JitThreadScope`] capturing the previously-stored pointer (and,
/// in debug builds, the suspended borrow level) so callers can restore both
/// later via [`restore_jit_thread`]. Suspending the borrow level here is what
/// makes a re-entrant JIT call (interpreter::execute inside
/// jit_invoke_dispatch) a *nested child reborrow* rather than a false-positive
/// *aliasing sibling* under the `jit_thread_mut` debug check.
///
/// # Safety contract
/// The caller must ensure that no other `&mut JvmThread` reference exists for the
/// duration of JIT execution. The pointer is only dereferenced inside JIT helpers
/// which execute on the same thread that set it.
pub fn set_jit_thread(thread: &mut JvmThread) -> JitThreadScope {
    // Shadow-stack precise roots (CRATONVM_SHADOW_STACK): ensure this thread's
    // shadow stack is allocated before any JIT code that may push to it runs.
    // Cheap `base != 0` check after the first entry; gated, no-op otherwise.
    // Done here (with a legitimate `&mut JvmThread`) rather than in the extern-C
    // `jit_get_current_thread` getter to avoid deriving an aliasing `&mut`.
    let mut saved_shadow_top: Option<usize> = None;
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.ensure_allocated();
        // §3 (unwind safety): snapshot the boundary `top` watermark so
        // `restore_jit_thread` can reset it on exit, healing any push that an
        // abnormal JIT exit (exception/deopt skipping a method epilogue) left
        // unbalanced. Captured after `ensure_allocated` so `top` is valid.
        saved_shadow_top = Some(thread.shadow_stack.top);
        if std::env::var_os("CRATONVM_DBG_SHADOW").is_some() {
            use std::sync::atomic::{AtomicBool, Ordering};
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[SHADOW] set_jit_thread: thread={:p} shadow base={:#x} top={:#x} end={:#x} ss_off={}",
                    thread as *mut JvmThread,
                    thread.shadow_stack.base,
                    thread.shadow_stack.top,
                    thread.shadow_stack.end,
                    JvmThread::shadow_stack_offset(),
                );
            }
        }
    }
    let prev_ptr = JIT_THREAD.with(|t| {
        let old = t.get();
        t.set(thread as *mut JvmThread);
        old
    });
    // Suspend any borrow held by an outer JIT level: the nested JIT call about
    // to run is a child reborrow of `thread`, not an aliasing sibling, so it
    // must start its own borrow level. The outer borrow is frozen on the call
    // stack and is provably unused until this nested call returns and
    // `restore_jit_thread` un-suspends it.
    #[cfg(debug_assertions)]
    let prev_borrow = suspend_jit_borrow();
    JitThreadScope {
        prev_ptr,
        saved_shadow_top,
        #[cfg(debug_assertions)]
        prev_borrow,
    }
}

/// Check if the JIT thread pointer is already set.
#[inline(always)]
pub fn is_jit_thread_set() -> bool {
    JIT_THREAD.with(|t| !t.get().is_null())
}

/// Restore a previously saved JIT thread scope. Used to support re-entrant
/// JIT calls (e.g. JIT put() → jit_invoke_dispatch → interpreter::execute hash()
/// which may JIT-compile hash() and call set_jit_thread again). Re-installs the
/// prior thread pointer and un-suspends the outer level's debug borrow flag.
pub fn restore_jit_thread(scope: JitThreadScope) {
    // §3 (unwind safety): reset the shadow `top` to the watermark captured at the
    // matching `set_jit_thread`, healing any push that an abnormal JIT exit
    // (exception/deopt that skipped a method epilogue) left unbalanced. On a
    // normal return the per-method epilogues already restored `top`, so this is a
    // no-op (`set_top` is idempotent at the boundary value). `JIT_THREAD` still
    // names the thread whose JIT call just returned — it is reset to `prev_ptr`
    // just below — so it identifies the correct shadow stack to heal.
    if let Some(saved_top) = scope.saved_shadow_top {
        let cur = JIT_THREAD.with(|t| t.get());
        if !cur.is_null() {
            // SAFETY: execution is back on `cur`'s own thread and its JIT call has
            // returned, so no aliasing `&mut` to its shadow stack is live; the
            // backing buffer address is stable for the thread's lifetime, and
            // `set_top` clamps into `[base, end]` so a stale value can't widen the
            // scan range.
            unsafe { (*cur).shadow_stack.set_top(saved_top) };
        }
    }
    JIT_THREAD.with(|t| t.set(scope.prev_ptr));
    #[cfg(debug_assertions)]
    restore_jit_borrow(scope.prev_borrow);
}

/// Clear the JIT thread pointer after JIT execution completes.
pub fn clear_jit_thread() {
    JIT_THREAD.with(|t| t.set(std::ptr::null_mut()));
}

/// Store a pending Java exception from JIT dispatch. Called when
/// `jit_invoke_dispatch` encounters an `ExceptionThrown` error.
fn set_jit_pending_exception(exc: ObjectRef) {
    JIT_PENDING_EXCEPTION.with(|e| e.set(Some(exc)));
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending Java exception. Used by `try_osr` when the OSR return path
/// drained the flag but cannot return an error from its own signature — the
/// exception must be re-posted so the interpreter dispatch loop drains it on
/// the next iteration via `take_jit_pending_exception`. Crate-pub because
/// only the OSR entry path should use it; ordinary JIT helpers set the flag
/// directly via the private `set_jit_pending_exception` above.
pub(crate) fn stash_jit_pending_exception(exc: ObjectRef) {
    set_jit_pending_exception(exc);
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-NPE flag. See `stash_jit_pending_exception` for the OSR
/// drain-without-route rationale.
pub(crate) fn stash_jit_pending_npe() {
    set_jit_pending_npe();
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending-AIOOBE payload. See `stash_jit_pending_exception` for the
/// OSR drain-without-route rationale.
pub(crate) fn stash_jit_pending_aioobe(index: i64, length: i64) {
    JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
}

/// Take (consume) any pending Java exception set by JIT dispatch.
/// Returns `Some(ObjectRef)` if an exception was pending, `None` otherwise.
pub fn take_jit_pending_exception() -> Option<ObjectRef> {
    JIT_PENDING_EXCEPTION.with(|e| e.take())
}

/// Non-consuming peek: returns `true` if a pending Java exception is set.
///
/// Used by `jit_invoke_dispatch` (and its bail/cache paths) to decide
/// whether to return the `i64::MIN` deopt sentinel — so the JIT caller's
/// post-invoke exception guard fires and the interpreter routes the
/// stashed exception through the method's exception table — instead of
/// returning a bogus `0` that the JIT would keep computing with.
pub(crate) fn jit_pending_exception_is_set() -> bool {
    JIT_PENDING_EXCEPTION.with(|e| {
        let v = e.take();
        let present = v.is_some();
        e.set(v);
        present
    })
}

/// Take (consume) a pending AIOOBE from JIT bounds check.
/// Returns `Some((index, length))` if an AIOOBE was pending.
pub fn take_jit_pending_aioobe() -> Option<(i64, i64)> {
    JIT_PENDING_AIOOBE.with(|e| e.take())
}

/// Take (consume) a pending NPE from a JIT array helper (`jit_iaload`,
/// `jit_aaload`, `jit_arraylength`). Returns `true` if an NPE was pending.
///
/// The JVM specifies that all three opcodes throw `NullPointerException`
/// when their array reference is null; the JIT array helpers previously
/// swallowed the null silently (returning 0 / -1), which let JIT'd
/// Java code continue with corrupt state. Now they set this flag, return
/// `i64::MIN`, and the interpreter post-JIT path constructs the real
/// `java/lang/NullPointerException` and routes it through the method's
/// exception table — same pattern as `take_jit_pending_aioobe`.
pub fn take_jit_pending_npe() -> bool {
    JIT_PENDING_NPE.with(|e| e.take())
}

/// Internal: set the pending-NPE flag. Called from the array helpers.
#[inline]
fn set_jit_pending_npe() {
    JIT_PENDING_NPE.with(|e| e.set(true));
}

/// Obtain an exclusive reference to the JIT thread. Returns `None` if not set,
/// otherwise the `&mut JvmThread` paired with a [`JitThreadGuard`] RAII token.
///
/// The caller MUST keep the guard alive for as long as it uses the returned
/// reference (binding it to `_guard` is sufficient). When the guard drops it
/// clears the debug reentrancy flag; a nested/aliasing `jit_thread_mut` call
/// made while a prior guard is still live trips a `debug_assert!`.
///
/// # Safety
/// Caller must ensure this is only called from JIT helper functions on the same
/// thread that called `set_jit_thread`, and that no other reference to the
/// JvmThread is live.
// SAFETY: Caller must ensure this is only called from JIT helper functions on the
// same thread that called `set_jit_thread`, and that no other `&mut JvmThread`
// reference is live. The pointer was set by `set_jit_thread` from a valid `&mut JvmThread`.
// The `JIT_THREAD_BORROWED` flag + `JitThreadGuard` enforce the "no aliasing
// borrow" half of this invariant in debug builds; release builds are unaffected.
#[inline]
unsafe fn jit_thread_mut() -> Option<(&'static mut JvmThread, JitThreadGuard)> {
    let ptr = JIT_THREAD.with(|t| t.get());
    if ptr.is_null() {
        None
    } else {
        #[cfg(debug_assertions)]
        JIT_THREAD_BORROWED.with(|b| {
            // This now fires ONLY for a genuine *same-level* aliasing
            // fabrication: two live `jit_thread_mut` borrows that did NOT cross
            // a `set_jit_thread` re-entry boundary. The common case — an outer
            // `jit_invoke_dispatch` bailing into the interpreter, which
            // re-enters JIT and recurses into a second `jit_invoke_dispatch` —
            // is a *strictly nested* reborrow (the inner `&mut *ptr` descends
            // from the outer's `&mut` via the `set_jit_thread(thread)` cast),
            // and `set_jit_thread`/`restore_jit_thread` suspend+restore this
            // flag around that boundary so the legitimate nesting does NOT trip
            // here. (Empirically verified: DaCapo avrora drives ~1100 such
            // nested borrows and completes cleanly with no UB.)
            debug_assert!(
                !b.get(),
                "jit_thread_mut: aliasing &mut JvmThread borrow detected \
                 (a prior JitThreadGuard is still live at the SAME JIT nesting \
                 level — this is a genuine sibling fabrication, not a re-entry)"
            );
            b.set(true);
        });
        Some((
            &mut *ptr,
            JitThreadGuard {
                #[cfg(debug_assertions)]
                _private: (),
            },
        ))
    }
}

/// Attempt to call a JIT-compiled entry through the register-only
/// transmute tables. Returns `Some(rc)` on success, `None` when the arg
/// count exceeds the table coverage (4 with no-ctx, 3 with-ctx). The
/// caller is expected to fall through to the interpreter slow-path in
/// `jit_invoke_dispatch` (which decodes args into `Value`s and bails)
/// rather than returning the previous silent `0` — that was the round-5
/// MED follow-up to the round-4 wave-2 fix for the helper dispatch path.
///
/// CRIT (round-5 review): three sibling sites in `jit_invoke_dispatch`
/// (the thread-local DISPATCH_CACHE fast-path, the JIT cache fast-path,
/// and the post-compile fast-path) each carried `_ => 0` arms that
/// silently dropped 5+-arg callees. Sharing this helper keeps the
/// register-table dispatch in one place and removes those drop sites.
///
/// SAFETY: `entry` must be a live JIT-compiled extern "C" entry point
/// whose calling convention matches `needs_ctx` (with-ctx prepends an
/// `i64` VM pointer to the Java arg slots). `args_slice` must contain
/// exactly `args_slice.len()` valid i64 arg slots; on overflow we don't
/// dereference the table at all.
#[inline]
unsafe fn try_call_compiled_entry(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let n = args_slice.len();
    if needs_ctx {
        Some(match n {
            0 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr)
            }
            1 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0])
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1])
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
            }
            // TODO(round-6-wave-2): extend register-table coverage or
            // emit stack-arg setup so 4+-arg with-ctx callees stay on
            // the JIT fast-path. Until then return None so the caller
            // bails to the interpreter (correct semantics, slower).
            _ => return None,
        })
    } else {
        Some(match n {
            0 => {
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                f()
            }
            1 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0])
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1])
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
            }
            // TODO(round-6-wave-2): see with-ctx branch above.
            _ => return None,
        })
    }
}

/// Bail a JIT-dispatched call out to the interpreter when the compiled
/// callee has more arguments than `call_jit_compiled_method_entry`'s
/// register-arg dispatch tables can pass. Issues `invoke_or_native` with
/// the full `Value` argument vector and converts the result back to the
/// `i64` register-ABI return value expected by the JIT caller. Exceptions
/// are stashed via `handle_jit_dispatch_error` so the interpreter post-JIT
/// path can route them through the caller's exception table.
#[inline(never)]
unsafe fn bail_to_interpreter(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args: &[Value],
) -> i64 {
    // invokespecial (kind=1) must NOT virtually re-target onto the receiver's
    // runtime class — same rationale as the kind=1 arm of the dispatch slow
    // path below. `invoke_or_native` → `invoke_on_class_shared` applies the
    // abstract→receiver retarget, which turns a compiled super-call bridge
    // into a self-call loop: H2's `ValueVarchar.compareTypeSafe` is
    // `invokespecial ValueStringBase.compareTypeSafe` (4 args + ctx, so
    // `try_call_compiled_entry`'s register table always bails it here);
    // retargeting the abstract `ValueStringBase` back to the `ValueVarchar`
    // receiver re-enters the bridge → dispatch → bail → ∞, surfacing as the
    // GROUP BY `StackOverflowError` from the JIT depth guard (every H2
    // TreeMap<ValueRow> comparator walk dies). `invoke_special_shared`
    // preserves super-call semantics with native-override priority.
    let res = if info.invoke_kind == 1 {
        crate::vm::invoke_special_shared(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    } else if matches!(info.invoke_kind, 0 | 2) {
        // VIRTUAL DISPATCH FIX (bug #2 — regex zero-width corruption under
        // `CRATONVM_JIT_VIRTUAL_TIERUP`). For virtual/interface kinds,
        // `info.class_name` is the *static* call-site type, not the receiver's
        // runtime class. `invoke_or_native` resolves the callee against the
        // class name it is handed (it does NOT re-dispatch on `args[0]`), so a
        // static type that declares a *concrete* base method shadows the
        // receiver's override. The regex engine is the canonical victim: the
        // `root.match(...)` call site is statically typed `Pattern$Node`, whose
        // base `match` is an unconditional zero-width "accept" (`matcher.last =
        // i; return true`). Bailing on the static type ran that accept node at
        // every position instead of `Pattern$Start.match`, so `find()` reported
        // a zero-width match before every character and
        // `"org.x".replaceAll("[.]","/")` produced "/o/r/g/...". The compiled-
        // entry MIC path already resolves on the receiver class (see
        // `jit_invoke_virtual_mic`); this register-overflow / exception-reroute
        // bail must do the same. Mirrors the array→Object and synthetic-id
        // fallbacks used there.
        let dispatch_class = virtual_dispatch_class(vm, args, info);
        crate::vm::invoke_or_native(
            vm,
            thread,
            &dispatch_class,
            info.method_name,
            info.descriptor,
            args,
        )
    } else {
        crate::vm::invoke_or_native(
            vm,
            thread,
            info.class_name,
            info.method_name,
            info.descriptor,
            args,
        )
    };
    match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    }
}

/// Resolve the runtime dispatch class for a virtual/interface bail
/// (`bail_to_interpreter`, kinds 0/2). `invoke_or_native` binds to the class
/// name it is handed rather than re-dispatching on the receiver, so the bail
/// must hand it the receiver's *runtime* class — not the static call-site type
/// in `info.class_name`. Mirrors `jit_invoke_virtual_mic`'s receiver
/// resolution: array receivers dispatch through `java/lang/Object` (JVMS
/// §4.4.1); a null/non-object receiver or a class id absent from the store
/// (synthetic alloc, which would derive an empty name) falls back to the
/// static call-site class so we never dispatch on `""`.
///
/// SAFETY: `vm` must be a live `SharedVm`; `args[0]` (when present) is the
/// receiver `Value` decoded by `decode_dispatch_values`/`decode_values`.
unsafe fn virtual_dispatch_class(
    vm: &SharedVm,
    args: &[Value],
    info: &JitInvokeInfo,
) -> std::sync::Arc<str> {
    let receiver = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return std::sync::Arc::from(info.class_name),
    };
    if vm.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return std::sync::Arc::from("java/lang/Object");
    }
    let cid = vm.heap.class_id_of(receiver);
    let cm = vm.class_manager.read();
    cm.get_class(cid)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| std::sync::Arc::from(info.class_name))
}

/// BUG-H: does the statically-bound callee declare a non-empty exception
/// table? Resolved from the class manager by `(class, method, descriptor)`.
///
/// Only meaningful for the statically-bound kinds (invokestatic/invokespecial),
/// where `info.class_name`/`method_name`/`descriptor` name the exact callee —
/// the virtual/interface kinds resolve on the runtime receiver type elsewhere.
///
/// SAFETY: `vm` must be a live `SharedVm`; `info` must point to a valid
/// `JitInvokeInfo` whose name fields are live `&str`s.
unsafe fn callee_has_exception_table(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    let cm = vm.class_manager.read();
    let Some(class_id) = cm.find_class_by_name(info.class_name) else {
        return false;
    };
    let store = cm.class_store();
    let Some((method, _declaring_id)) = crate::classloading::find_method_recursive(
        class_id,
        info.method_name,
        info.descriptor,
        store,
    ) else {
        return false;
    };
    method
        .code()
        .map_or(false, |c| !c.exception_table.is_empty())
}

/// BUG-H (virtual sibling of [`callee_has_exception_table`]): does the
/// receiver-resolved virtual/interface callee declare a non-empty exception
/// table? The MIC hit path knows the callee only by `(info.method_name,
/// descriptor)` + the runtime receiver class, so resolution starts from the
/// receiver's class id (the real override site).
///
/// SAFETY: as [`callee_has_exception_table`]; `receiver_class_id` must be a
/// live class id obtained from the receiver object.
unsafe fn mic_callee_has_exception_table(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    let cm = vm.class_manager.read();
    let store = cm.class_store();
    let Some((method, _decl)) = crate::classloading::find_method_recursive(
        receiver_class_id,
        info.method_name,
        info.descriptor,
        store,
    ) else {
        return false;
    };
    method
        .code()
        .map_or(false, |c| !c.exception_table.is_empty())
}

/// BUG-H fix: route an *implicit* runtime exception thrown by a directly
/// invoked compiled callee through the CALLEE's own exception table.
///
/// When a JIT-compiled callee `B` is invoked directly — either via a
/// machine-code `CALL` to its entry or through one of this helper's
/// `try_call_compiled_entry` fast paths — and it throws an implicit runtime
/// exception (`ArrayIndexOutOfBoundsException` / `NullPointerException`), `B`'s
/// own in-method `catch` is never consulted: `B` returns the `i64::MIN` deopt
/// sentinel with the thread-local pending-exception flag set, and that flag is
/// only drained at the *outermost* interpreter↔JIT boundary — which routes it
/// through the wrong method's exception table (the `TestHexUtils` /
/// `HexUtils.getDec` escape: `T[i-'0']` inside `catch (AIOOBE)` returning -1).
///
/// If `rc` is the deopt sentinel, an implicit AIOOBE/NPE is pending, and the
/// callee declares a non-empty exception table, re-execute the callee in the
/// interpreter with the same args. The interpreter re-throws the same
/// exception at the same bytecode and routes it through the callee's table —
/// running its handler (which may swallow the exception and return a normal
/// value) or re-propagating it to the caller via `handle_jit_dispatch_error`.
/// Re-execution from the start mirrors the existing whole-method deopt
/// semantics; it is confined to the (cold) exceptional path of a method that
/// actually declares a handler region, so straight-line callees are untouched.
///
/// SAFETY: same contract as the surrounding `jit_invoke_dispatch` fast paths —
/// `vm`/`info`/`args_slice` are the live values passed by the JIT caller, and
/// no `jit_thread_mut` borrow is live at the call site.
#[inline]
unsafe fn route_implicit_exc_through_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    rc: i64,
) -> i64 {
    if rc != i64::MIN {
        return rc;
    }
    // Did the callee raise an *implicit* runtime exception? (A general
    // `JIT_PENDING_EXCEPTION` comes from `athrow`, whose methods-with-tables are
    // not compiled — see the `try_compile_inner` gate — so it never reaches a
    // direct compiled call and needs no re-route here.)
    let aioobe = take_jit_pending_aioobe();
    let npe = if aioobe.is_none() {
        take_jit_pending_npe()
    } else {
        false
    };
    // Re-stash the consumed flag and propagate the sentinel unchanged. Used for
    // the pure-deopt case (no flag) and the no-local-handler case (the callee
    // cannot catch it, so existing outward propagation is already correct).
    let restash_and_return = || {
        if let Some((idx, len)) = aioobe {
            stash_jit_pending_aioobe(idx, len);
        } else if npe {
            stash_jit_pending_npe();
        }
        rc
    };
    if aioobe.is_none() && !npe {
        return rc;
    }
    if !callee_has_exception_table(vm, info) {
        return restash_and_return();
    }
    // Re-execute the callee in the interpreter so the implicit exception routes
    // through the callee's own exception table.
    if let Some((thread, _guard)) = jit_thread_mut() {
        let bail_args = decode_dispatch_values(vm, info, args_slice);
        return bail_to_interpreter(vm, thread, info, &bail_args);
    }
    restash_and_return()
}

/// Decode a JIT dispatch helper's raw `i64` argument slice into the
/// `Vec<Value>` the interpreter expects.  Centralised so that the slow
/// path in `jit_invoke_dispatch` and the three `try_call_compiled_entry`
/// overflow bailouts (DISPATCH_CACHE hit, JIT-cache hit, post-compile)
/// all reconstruct args the same way — round-5 CRIT-1 fix.
///
/// `invoke_kind` matches the JIT calling-convention encoding: 0/1/2 are
/// virtual/static/special with an explicit receiver in `args_slice[0]`;
/// 3 is the no-receiver form (used for static and indy callees that the
/// JIT emits without a leading `this` slot).
///
/// SAFETY: `args_slice` must be a slice of valid `i64` arg slots produced
/// by the JIT caller. `vm` must be a live `SharedVm`; the heap is queried
/// to validate any potential object pointers before round-tripping them
/// through `ObjectRef`.
#[inline]
unsafe fn decode_dispatch_values(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> Vec<Value> {
    let mut values = Vec::with_capacity(args_slice.len());
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);

    if info.invoke_kind != 3 {
        if !args_slice.is_empty() {
            let ptr = args_slice[0];
            if ptr == 0 {
                values.push(Value::Object(None));
            } else {
                // Defensive: tagged-long bits in an L-typed receiver slot
                // are downgraded to null instead of being treated as a
                // heap pointer (else GC SEGVs walking a bogus oop).
                let bits = ptr as u64;
                let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                    vm.heap.is_object_address(bits as usize)
                } else {
                    None
                };
                match validated {
                    Some(obj) => values.push(Value::Object(Some(obj))),
                    None => values.push(Value::Object(None)),
                }
            }
        }
    }

    let start_idx = if info.invoke_kind != 3 { 1 } else { 0 };
    for &raw in &args_slice[start_idx..] {
        let val = match desc_iter.next() {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                Value::Int(raw as i32)
            }
            Some(b'J') => Value::Long(raw),
            Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
            Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
            Some(b'L') | Some(b'[') => {
                if raw == 0 {
                    Value::Object(None)
                } else {
                    let bits = raw as u64;
                    let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                        vm.heap.is_object_address(bits as usize)
                    } else {
                        None
                    };
                    match validated {
                        Some(obj) => Value::Object(Some(obj)),
                        None => Value::Object(None),
                    }
                }
            }
            _ => Value::Int(raw as i32),
        };
        values.push(val);
    }
    values
}

// ---------------------------------------------------------------------------
// Helper: extract heap from SharedVm pointer
// ---------------------------------------------------------------------------

// SAFETY: Caller must ensure vm_ptr is a valid pointer to a live SharedVm instance.
// The SharedVm is heap-allocated and outlives all JIT helper calls.
#[inline]
unsafe fn heap_from_vm(vm_ptr: i64) -> &'static VmHeap {
    debug_assert!(vm_ptr != 0, "heap_from_vm called with null VM pointer");
    // SAFETY: vm_ptr was passed from JIT-compiled code which received it from the
    // interpreter's SharedVm reference, so it points to a valid SharedVm.
    &(*(vm_ptr as *const SharedVm)).heap
}

// ---------------------------------------------------------------------------
// JIT safepoint SATB flush — Round-7 fix (CRIT, audit §3)
// ---------------------------------------------------------------------------
//
// The interpreter drains its per-thread SATB buffer at every safepoint
// arrival (`runtime/interpreter.rs::safepoint_check`, line 899). JIT-
// running threads have no such poll — they only return through one of
// the runtime helpers below. If any of those helpers participates in an
// STW pause (directly via `collect_garbage` or transitively via the
// shared barrier on a concurrent GC trigger) without first draining the
// per-thread SATB buffer, up to `DEFAULT_SATB_CAPACITY` (256) overwritten
// references stay invisible to the marker. The next mixed evacuation
// then turns the classic SATB lost-object scenario into a use-after-
// free (audit: docs/round7-gc.md §3).
//
// `flush_thread_satb` itself is a cheap inline call when `is_active() ==
// false`: a single Acquire load and an early return. We invoke it
// unconditionally at the top of every GC-triggering JIT helper so the
// invariant holds without a separate JIT-emitted safepoint stub.
#[inline]
unsafe fn jit_safepoint_flush_satb(vm_ptr: i64) {
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: caller contract for every JIT helper — vm_ptr is a live
    // SharedVm pointer.
    let vm = &*(vm_ptr as *const SharedVm);
    vm.heap.flush_thread_satb();
}

// ---------------------------------------------------------------------------
// Array allocation helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer
// passed through from the interpreter. atype encodes a JVM array element type (T_BOOLEAN..T_LONG).
// length is the requested array size. The returned i64 is a raw heap pointer to the new array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_newarray(vm_ptr: i64, atype: i64, length: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): drain THIS thread's per-thread SATB
    // buffer before any path that may park at the GC barrier or trigger
    // collection. Mirrors interpreter::safepoint_check (line 899).
    jit_safepoint_flush_satb(vm_ptr);
    let elem_type = match atype as u8 {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => return 0,
    };
    // BUGFIX: The JIT may pass length as a NaN-boxed CompactValue raw bit pattern
    // (e.g. 0xFFFC_0000_0000_000B for int 11) when reading from operand-stack slots
    // that were populated via mechanisms that store CompactValue raw bits rather
    // than untagged primitive bits. Defensive: narrow `length` to the int payload,
    // then sign-extend to i64. JLS only allows `int` array lengths, so the upper
    // 32 bits of a valid length are always 0 (or all-1 for negative, which becomes
    // a NegativeArraySizeException — JIT codegen ensures bounds-checked path).
    let length = length as i32 as i64;
    if length < 0 {
        // Negative length — would-be NegativeArraySizeException. JIT codegen
        // is responsible for the proper throw; here we return 0 to prevent
        // the GC abort from a huge cast-to-usize.
        return 0;
    }
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.heap;
    // Try allocation; if young gen exhausted, run GC and retry.
    //
    // Task #43 (HIGH soundness — deferred from #25/#26): route the
    // allocation-failure GC through the real STW handshake instead of
    // calling `heap.collect_garbage` directly. The direct call was a
    // long-standing FIXME because in a multi-threaded VM it bypasses
    // `gc_barrier.request_stw()` / `wait_for_all()` / `complete_gc()` and
    // every mutator's `safepoint_check` — meaning a JIT thread could
    // start rewriting object addresses while another thread is still
    // running, producing the classic mid-flight pointer-tearing UAF.
    // `maybe_gc_forced` (the interpreter's allocation-failure GC entry
    // point — `runtime/interpreter.rs:225`) is the model: it drains
    // per-thread SATB, requests STW through `gc_barrier`, waits for all
    // mutators to park, runs collection, signals completion, and updates
    // roots from the pointer map. Reusing it here keeps the JIT helper
    // on the orchestrated STW path with zero JIT-specific divergence.
    // W1-vm (MED, audit §3): the probe size MUST match the real allocation
    // size. `array_data_size` returns `None` when `HEADER_SIZE + length *
    // elem_size` overflows `usize` — an allocation that can never succeed.
    // The previous `.unwrap_or(0)` silently probed with size 0, which always
    // passes `try_alloc_young_probe` and then hands the impossible length to
    // the non-fallible `alloc_array` (abort / UB on the cast-to-usize). Treat
    // the overflow case as an immediate, catchable OutOfMemoryError instead —
    // the same outcome the interpreter reaches via `gc_alloc_array`'s
    // `try_alloc_array` returning `None`.
    let Ok(data_size) = cratonvm_types::array_data_size(length as usize, elem_type) else {
        return jit_newarray_oom(vm, length as usize);
    };
    let total_size = cratonvm_types::HEADER_SIZE + data_size;
    // Fast path: probe the young gen with the REAL allocation size and, on
    // success, allocate without the fallible retry dance. This preserves the
    // common-case cost of the original helper.
    if heap.try_alloc_young_probe(total_size).is_some() {
        if let Some(obj_ref) = heap.try_alloc_array(ClassId::new(0), elem_type, length as usize) {
            return jit_newarray_finish(obj_ref, atype, length);
        }
    }
    // Slow path: young gen full (or the probe-then-alloc race lost the slot).
    // Mirror the interpreter's `gc_alloc_array` (runtime/interpreter.rs:840):
    // retire the TLAB, run an orchestrated STW GC, then retry the fallible
    // `try_alloc_array`. CRIT (jit/gc audit, 2026-05): MUST retire the
    // calling thread's TLAB before kicking off GC. The retire installs a
    // synthetic `int[]` filler at the cursor so the heap walker can stride
    // over the unused TLAB tail in O(1); without it, the walker mis-decodes
    // the tail's zeroed bytes (or a half-init JIT object) and aborts with
    // "implausible object size" / corrupts old gen when promote-on-pressure
    // copies stale pointers. Mirrors `alloc_object_shared` in the interpreter
    // (runtime/interpreter.rs line ~782).
    if let Some((thread, _guard)) = jit_thread_mut() {
        thread.tlab.retire();
        // Route allocation-failure GC through the interpreter's orchestrated
        // STW path (`maybe_gc_forced` -> `gc_barrier.request_stw()` +
        // `wait_for_all()`), so other mutator threads are parked before the
        // moving collector rewrites object addresses. (Resolves the prior
        // FIXME that called `heap.collect_garbage` with an unchecked
        // StopTheWorldToken.)
        crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
    }
    // Retry after GC. On a second failure the heap is genuinely exhausted —
    // surface a catchable `java/lang/OutOfMemoryError` exactly as the
    // interpreter's `gc_alloc_array` does, instead of the old non-fallible
    // `alloc_array` (which would abort the process on a real OOM).
    let Some(obj_ref) = heap.try_alloc_array(ClassId::new(0), elem_type, length as usize) else {
        return jit_newarray_oom(vm, length as usize);
    };
    jit_newarray_finish(obj_ref, atype, length)
}

/// W1-vm: surface an allocation failure from `jit_newarray` as a catchable
/// `java/lang/OutOfMemoryError`, mirroring the interpreter's `gc_alloc_array`
/// (runtime/interpreter.rs:853) OOM arm.
///
/// The `newarray` codegen site (`jit/src/x64.rs` ~16349) has no `i64::MIN`
/// deopt guard — it pushes RAX straight onto the operand stack — so unlike
/// the invoke-dispatch helpers we cannot signal via the deopt sentinel.
/// Instead we use the same channel the void-return store helpers
/// (`jit_iastore` etc.) use for null-array NPEs: stash the throwable in
/// `JIT_PENDING_EXCEPTION` and return the `0`/null sentinel. The interpreter's
/// post-JIT drain (runtime/interpreter.rs:14079) calls
/// `take_jit_pending_exception()` on *every* JIT return path and routes the
/// OOME through the JIT'd method's own exception table, giving a JIT'd
/// `newarray` identical catchable-OOM semantics to the interpreter.
///
/// If the OOME object itself cannot be constructed (e.g. the heap is too
/// exhausted to even allocate the throwable), we fall back to leaving the
/// flag unset and returning `0` — the legacy behaviour — so this change is
/// purely additive and never makes a previously-handled case worse.
#[cold]
fn jit_newarray_oom(vm: &SharedVm, length: usize) -> i64 {
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        let msg = format!("Java heap space (alloc_array length {})", length);
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/OutOfMemoryError",
            Some(&msg),
        ) {
            set_jit_pending_exception(exc);
        }
    }
    0
}

/// W1-vm: shared tail for the `jit_newarray` success paths — runs the optional
/// allocation trace and converts the `ObjectRef` into the raw `i64` pointer the
/// JIT caller expects. Factored out so the fast and slow paths stay identical.
#[inline]
unsafe fn jit_newarray_finish(obj_ref: ObjectRef, atype: i64, length: i64) -> i64 {
    let raw = obj_ref.as_ptr();
    if crate::runtime::env_cache::jit_newarray_trace() {
        let class_id_raw = std::ptr::read(raw as *const u32);
        let kind_byte = *raw.add(4);
        let elem_byte = *raw.add(5);
        let stored_len = std::ptr::read(raw.add(12) as *const u32);
        let num_slots = std::ptr::read(raw.add(16) as *const u32);
        eprintln!("[JIT-NA] ptr={:p} atype={} len={} cid={} kind={} elem={} arrlen={} num_slots={}",
            raw, atype, length, class_id_raw, kind_byte, elem_byte, stored_len, num_slots);
    }
    raw as i64
}

/// JIT inline-TLAB completion helper.
///
/// Called from JIT-emitted code AFTER the inline TLAB bump has already
/// claimed `HEADER_SIZE + num_fields * SLOT_SIZE` bytes at `obj_ptr`
/// and written only the `class_id` field at offset 0. This helper
/// finishes the header (kind = Object, identity_hash_code, num_slots —
/// the surrounding bytes are TLAB-zeroed so `mark_word`, `forwarding_ptr`,
/// `gc_age`, `gc_flags`, etc. are already correctly initialized),
/// installs primitive-field typed-zero defaults, and registers the
/// object with the finalizer queue when its class overrides `finalize()`.
///
/// Separating this from `jit_new_object` lets the JIT emit the cheap
/// bump-pointer prologue inline (~5-7 instructions) and pay a single
/// call only for the header-completion + primitive-defaults work that
/// touches the class-metadata `RwLock`.
///
/// # Safety
/// `vm_ptr` must be a valid `SharedVm` pointer; `obj_ptr` must be a
/// freshly-bumped TLAB allocation of at least `HEADER_SIZE + num_fields
/// * SLOT_SIZE` zeroed bytes with `class_id` already written at offset 0.
/// `num_fields` must match the class metadata.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_post_tlab_init(
    vm_ptr: i64,
    obj_ptr: i64,
    class_id_raw: i64,
    num_fields: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    let raw_ptr = obj_ptr as *mut u8;

    // Finish header: identity_hash_code, num_slots.
    //
    // Everything else (kind, element_type, padding, mark_word,
    // forwarding_ptr, gc_age/flags, array_length) is correctly zero
    // already from the TLAB refill: `ObjectKind::Object` discriminant
    // is 0, `ArrayElementType::Reference` is 0, `MARK_NEUTRAL` is 0,
    // `gc_age=0`/`gc_flags=0`/`array_length=0` match a fresh object.
    //
    // Layout reminder (see `types/src/heap_types.rs`):
    //   off  0: class_id (4 bytes)       — written inline by JIT
    //   off  4: kind (1)                 — already zero == Object
    //   off  5: element_type (1)         — already zero == Reference
    //   off  6: padding (2)              — already zero
    //   off  8: identity_hash_code (4)
    //   off 12: array_length (4)         — already zero
    //   off 16: num_slots (4)
    //   off 20: gc_age + gc_flags + _gc_reserved — already zero
    //   off 24: forwarding_ptr (8)       — already zero
    //   off 32: mark_word (8)            — already zero == MARK_NEUTRAL
    // CRIT (#23, BinTrees-18): the documented assumption "TLAB-refill
    // leaves everything zeroed" is empirically violated on long runs.
    // Defensively zero the four header bytes at offset 4 (kind=Object=0,
    // elem=Reference=0, padding=0) and the array_length at offset 12.
    // Without this, a kind=Object header can ship with a non-zero
    // array_length (observed: 0x01010101 from prior byte[] data), which
    // causes the GC walker to mis-decode the object as an array and step
    // into the next object's payload — surfacing as ECJ's
    // HashtableOfInt.put `/by zero` on a zero-length keyTable.
    *(raw_ptr.add(4) as *mut u32) = 0;
    *(raw_ptr.add(12) as *mut u32) = 0;
    let hash = vm.heap.next_identity_hash();
    *(raw_ptr.add(8) as *mut i32) = hash;
    *(raw_ptr.add(16) as *mut u32) = num_fields as u32;

    // Reconstruct the typed handle and finish init.
    let obj_ref = cratonvm_types::ObjectRef::from_raw(raw_ptr);

    // Primitive-typed default values walk the class hierarchy under the
    // class_manager RwLock. Kept here (rather than inlined) because the
    // JIT cannot synthesise per-field descriptor reads without
    // pre-resolving the full layout at compile time.
    jit_init_primitive_fields(vm, obj_ref, class_id);

    // JLS §12.6 finalizer registration. Cold path — most classes do not
    // override finalize().
    let has_fin = vm
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        vm.register_finalizable(obj_ref.as_ptr() as usize);
    }

    if let Ok(filter) = std::env::var("CRATONVM_DBG_JIT_ALLOC") {
        if let Ok(want) = filter.parse::<u32>() {
            if class_id_raw as u32 == want {
                eprintln!(
                    "[JIT_ALLOC] post_tlab_init class_id={} obj=0x{:x}",
                    class_id_raw, obj_ptr
                );
            }
        }
    }
    obj_ptr
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and num_fields must match the class metadata resolved at compile time.
// The returned i64 is a raw heap pointer to the newly allocated object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_new_object(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.heap;
    let class_id = ClassId::new(class_id_raw as u32);

    // CRIT (jit/gc audit, 2026-05): probe young-gen capacity BEFORE
    // allocating. If young gen would overflow, retire the calling
    // thread's TLAB and trigger an orchestrated STW GC so the next
    // allocation has room — mirrors `alloc_object_shared` in
    // `runtime/interpreter.rs:782`. The TLAB retire is critical: it
    // installs a synthetic `int[]` filler at the cursor so the heap
    // walker can stride over the unused tail in O(1) without
    // mis-decoding it.
    //
    // This is the slow-path entry — we're here because the inline-TLAB
    // bump in `emit_inline_tlab_new` failed (TLAB full / null thread)
    // OR because the caller went straight to the helper for an
    // over-sized object. In all cases the inline bump did not commit a
    // half-initialized object: the TLAB cursor in memory is the
    // last-allocated-object's end, so `retire()` here is safe.
    let total_size = cratonvm_types::HEADER_SIZE
        + (num_fields as usize).saturating_mul(cratonvm_types::SLOT_SIZE);
    if heap.try_alloc_young_probe(total_size).is_none() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
        }
    }

    let obj_ref = heap.alloc_object(class_id, num_fields as usize);
    // Initialize primitive-typed fields to proper JVM default values.
    // Zero memory reads as Object(None) which is wrong for int/long/float/double fields.
    jit_init_primitive_fields(vm, obj_ref, class_id);
    // Register with GC finalizer support if the class overrides finalize() (JLS §12.6).
    let has_fin = vm
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        vm.register_finalizable(obj_ref.as_ptr() as usize);
    }
    if let Ok(filter) = std::env::var("CRATONVM_DBG_JIT_ALLOC") {
        if let Ok(want) = filter.parse::<u32>() {
            if class_id_raw as u32 == want {
                eprintln!(
                    "[JIT_ALLOC] new_object class_id={} obj=0x{:x}",
                    class_id_raw,
                    obj_ref.as_ptr() as usize
                );
            }
        }
    }
    obj_ref.as_ptr() as i64
}

/// Initialize primitive-typed fields of a newly allocated object (JIT version).
fn jit_init_primitive_fields(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    let cm = vm.class_manager.read();
    let store = &cm.class_store;
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() { continue; }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                let default = match desc_first {
                    b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
                    b'J' => Some(Value::Long(0)),
                    b'F' => Some(Value::Float(0.0)),
                    b'D' => Some(Value::Double(0.0)),
                    _ => None,
                };
                if let Some(val) = default {
                    vm.heap.set_field(obj, inst_idx, val);
                }
                inst_idx += 1;
            }
            cid = class.superclass;
        } else {
            break;
        }
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// component_class_id_raw is the ClassId of the array's component type. length is non-negative.
// Returns a raw heap pointer to a newly allocated reference array.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_anewarray_object(
    vm_ptr: i64,
    component_class_id_raw: i64,
    length: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    // BUGFIX: see `jit_newarray` — narrow length to int payload and sign-extend.
    // JLS only allows `int` array lengths; defensive against JIT slot patterns
    // that carry stale upper bits (e.g. NaN-boxed CompactValue raw bits).
    let length = length as i32 as i64;
    if length < 0 {
        // Negative length — would-be NegativeArraySizeException. JIT codegen
        // is responsible for the proper throw; here we return 0 to prevent
        // the GC abort from a huge cast-to-usize.
        return 0;
    }
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is a valid SharedVm pointer per the caller contract.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.heap;
    let class_id = ClassId::new(component_class_id_raw as u32);

    // CRIT (jit/gc audit, 2026-05): probe young-gen capacity and retire
    // the calling thread's TLAB before triggering GC. See
    // `jit_new_object` / `jit_newarray` for the full rationale —
    // without the retire, the heap walker steps into TLAB tail bytes
    // and mis-decodes them as object headers when GC fires from this
    // slow path.
    let data_size = cratonvm_types::array_data_size(length as usize, ArrayElementType::Reference)
        .unwrap_or(0);
    let total_size = cratonvm_types::HEADER_SIZE + data_size;
    if heap.try_alloc_young_probe(total_size).is_none() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
        }
    }

    let arr = heap.alloc_array(class_id, ArrayElementType::Reference, length as usize);
    arr.as_ptr() as i64
}

// ---------------------------------------------------------------------------
// Array element access helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null triggers a pending NPE + `i64::MIN`
// deopt sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_baload(array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §baload: throw NullPointerException on null array reference.
        // Previously returned 0, which silently fabricated a zero byte and
        // masked real null-deref bugs in user code. Match the iaload/aaload
        // protocol: flag the pending NPE and return the deopt sentinel so the
        // post-JIT interpreter path throws on resume.
        set_jit_pending_npe();
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the length field in the array header.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §baload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously returned 0, which silently fabricated a zero byte and
        // masked real OOB bugs in user code — the same silent-fabrication class the
        // null arm above was fixed for. Mirror `jit_throw_aioobe`: flag the pending
        // AIOOBE and return the `i64::MIN` deopt sentinel so the post-JIT interpreter
        // path constructs the real exception and routes it through the method's
        // exception table.
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr as i8 as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null aborts the process — see comment.
// Out-of-bounds is handled gracefully.
//
// Round-9 jit HIGH fix (audit `round9-jit.md`, fragile-ABI item): this helper
// is part of an undocumented ABI contract relied on by the inline null-check
// failure stub in `jit/src/x64.rs::emit_null_check_store_stubs`. That stub
// calls `helpers.bastore(0, 0, 0)` after zeroing only the `array_ptr` argument
// register — `index` and `val` are left undefined / zeroed only by happenstance
// of the calling convention's volatile-register set. THIS HELPER MUST handle
// `array_ptr == 0` by setting the pending-NPE flag and returning WITHOUT
// reading `index` or `val`, regardless of their content. The null-guard short
// circuit below is therefore load-bearing for that codegen path; do not move
// any read of `index` or `val` above the null check, do not "optimize" the
// null check away even if profiling shows nulls are rare, and do not change
// the signature without also updating `emit_null_check_store_stubs` to match.
pub unsafe extern "C" fn jit_bastore(array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §bastore: throw NullPointerException on null array reference.
        //
        // Round-8 CRIT fix (audit `round8-jit.md`, "false promise" item):
        // previously this called `std::process::abort()` with a comment
        // claiming the helper was unreachable from inlined codegen, but
        // (a) the helper is still registered in `JitRuntimeHelpers` and
        // therefore reachable from any future codegen path that uses it,
        // and (b) the hardware-page-fault NPE path through
        // `emit_bounds_check` is *also* a false promise — the signal
        // handler dumps an hs_err and re-raises, killing the VM. We now
        // set the pending-NPE flag and return; the void return cannot
        // carry a sentinel, but the interpreter's post-JIT path drains
        // `JIT_PENDING_NPE` on EVERY return (not just the i64::MIN
        // sentinel arm — fixed in the same round) so the NPE surfaces
        // at the right method instead of leaking across calls. The
        // inline-store codegen also emits an explicit `TEST receiver,
        // receiver; JZ deopt_npe` guard before the bounds check, so
        // this helper is the second line of defense.
        set_jit_pending_npe();
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §bastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped, masking real OOB bugs.
        // The void return cannot carry the `i64::MIN` deopt sentinel, so — exactly
        // like the void-return null arm above — we set the pending-AIOOBE flag and
        // return; the interpreter's post-JIT drain surfaces the exception at this
        // method (see `take_jit_pending_aioobe` in runtime/interpreter.rs).
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr = val as u8;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null triggers a pending NPE + `i64::MIN` deopt
// sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_iaload(array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §iaload: throw NullPointerException on null array reference.
        // Signal the interpreter via the pending-NPE flag + `i64::MIN` deopt
        // sentinel (same protocol as `jit_throw_aioobe`).
        set_jit_pending_npe();
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    // The element at HEADER_SIZE + index*4 is within bounds (checked below).
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §iaload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index (same protocol as `jit_throw_aioobe`). Previously returned 0,
        // silently fabricating a zero element and masking real OOB bugs.
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *const i32;
    *elem_ptr as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null aborts the process — see `jit_bastore` for
// the rationale. Out-of-bounds is handled gracefully.
pub unsafe extern "C" fn jit_iastore(array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §iastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe();
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §iastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped. Set the pending-AIOOBE
        // flag and return — the void return cannot carry the deopt sentinel, so the
        // interpreter's post-JIT drain surfaces the exception (same void-arm protocol
        // as the null case above).
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *mut i32;
    *elem_ptr = val as i32;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a reference array object. Null triggers a pending NPE + `i64::MIN`
// deopt sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_aaload(array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §aaload: throw NullPointerException on null array reference.
        set_jit_pending_npe();
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    // ptr::read is used because Value::Object may contain non-Copy ObjectRef.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §aaload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index (same protocol as `jit_throw_aioobe`). Previously returned 0 (null),
        // silently fabricating a null element and masking real OOB bugs.
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * REF_ELEMENT_SIZE) as *const u64;
    std::ptr::read(elem_ptr) as i64
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// array_ptr must be 0 (null) or a valid heap pointer to a reference array.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC card table invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_aastore(vm_ptr: i64, array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §aastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe();
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §aastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped. Set the pending-AIOOBE
        // flag and return BEFORE the SATB barrier / write below (no element is read
        // or written on the OOB path). The void return cannot carry the deopt
        // sentinel, so the interpreter's post-JIT drain surfaces the exception.
        JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * REF_ELEMENT_SIZE) as *mut u64;
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter's
    // `shared.heap.satb_barrier(old_value)` at runtime/interpreter.rs:4228
    // (aastore) and :5349 (aastore via set_array_element). Read the OLD
    // reference *before* the store so concurrent marking still sees a
    // path to the about-to-be-overwritten target (snapshot-at-the-
    // beginning). Without this the marker loses the only path to a
    // still-live object on every JIT-overwritten aastore, and the next
    // mixed evacuation turns the missed live into a use-after-free.
    //
    // `satb_barrier` is the inherent name for the pre-write barrier on
    // `VmHeap` in this codebase (the `GarbageCollector::write_barrier_pre`
    // trait alias is planned but not yet landed here — when it does, this
    // call should migrate to it for triad-pairing under the
    // `vm_heap.rs` debug-build assertion).
    let old_raw = std::ptr::read(elem_ptr);
    if old_raw != 0 {
        let heap = heap_from_vm(vm_ptr);
        let old_obj = ObjectRef::from_raw(old_raw as usize as *mut u8);
        heap.satb_barrier(Value::Object(Some(old_obj)));
    }
    std::ptr::write(elem_ptr, val as u64);

    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        let obj_ref = ObjectRef::from_raw(ptr);
        let value = Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)));
        heap.write_barrier(obj_ref, value);
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// leaf_et encodes the inner array's element type. dim1 and dim2 are the two dimension sizes.
// Returns a raw heap pointer to the outer reference array whose elements are inner arrays.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_multianewarray_2d(
    vm_ptr: i64,
    leaf_et: i64,
    dim1: i64,
    dim2: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    let heap = heap_from_vm(vm_ptr);
    let elem_type = match leaf_et as u8 {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => ArrayElementType::Reference,
    };

    // BUGFIX (mirrors jit_newarray / jit_anewarray_object): narrow dimensions to
    // int payload and sign-extend, defending against NaN-boxed CompactValue raw
    // bits leaking from JIT operand-stack slots.
    let dim1 = dim1 as i32 as i64;
    let dim2 = dim2 as i32 as i64;
    if dim1 < 0 || dim2 < 0 {
        return 0;
    }
    let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, dim1 as usize);
    for i in 0..dim1 as usize {
        let inner = heap.alloc_array(ClassId::new(0), elem_type, dim2 as usize);
        let _ = heap.set_array_element(outer, i, Value::Object(Some(inner)));
    }
    outer.as_ptr() as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to any array object. Null triggers a pending NPE + `i64::MIN` deopt
// sentinel (JVMS §arraylength requires NullPointerException on null).
pub unsafe extern "C" fn jit_arraylength(array_ptr: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if array_ptr == 0 {
        // JVMS §arraylength: throw NullPointerException on null array reference.
        // Previously returned -1, which JIT'd Java would happily compare against
        // and use as an array bound — masking real null-deref bugs in user code.
        set_jit_pending_npe();
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live array on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the u32 length field.
    let ptr = array_ptr as *const u8;
    let length_ptr = ptr.add(ARRAY_LENGTH_OFFSET) as *const u32;
    (*length_ptr) as i64
}

// ---------------------------------------------------------------------------
// Field access helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index is the resolved field slot index within the object layout.
// ptr::read is used because Value may contain non-Copy variants (ObjectRef).
pub unsafe extern "C" fn jit_getfield(obj_ptr: i64, field_index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 {
        // JVMS §getfield: throw NullPointerException on a null receiver.
        // Previously returned 0, which silently fabricated a zero/null field
        // value and masked real null-deref bugs in user code — the same
        // silent-fabrication class the array-load helpers were fixed for.
        // Flag the pending NPE (drained on every JIT method return — see
        // `take_jit_pending_npe` in runtime/interpreter.rs) and return the
        // `i64::MIN` deopt sentinel, mirroring `jit_arraylength`.
        set_jit_pending_npe();
        return i64::MIN;
    }
    // B2 fix (audit `vm-runtime.md`): bounds-check the field slot against the
    // receiver's declared `num_slots` BEFORE the raw read, mirroring the
    // symmetric `jit_putfield_slot_in_bounds` guard on every `jit_putfield_*`
    // helper. Without this, a stale `field_index` (synthetic/real-JDK layout
    // drift) or an operand-stack miscompile reads `obj + HEADER + index*SLOT`
    // out of the object and into the *neighbouring* heap object, leaking its
    // bytes back to JIT'd Java as an i64/object pointer (info leak + potential
    // follow-on UAF if interpreted as a ref). The interpreter (`get_field`)
    // returns a default for an out-of-range slot rather than reading past the
    // object; match that by returning 0 without dereferencing.
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return 0;
    }
    // SAFETY: obj_ptr is non-null and points to a live object, and field_index is now
    // verified < num_slots, so HEADER_SIZE + field_index * SLOT_SIZE is within the
    // object's allocated region.
    let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    let val: Value = std::ptr::read(ptr as *const Value);
    let result = match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => r.as_ptr() as i64,
        Value::Object(None) => 0,
        _ => 0,
    };
    result
}

/// Bounds-check a JIT putfield slot against the receiver's declared
/// `num_slots` (read directly from the object header at offset 16 — the same
/// `num_slots` field `VmHeap::num_fields` returns; see the header layout in
/// `types/src/heap_types.rs` and the existing offset-16 read at the array
/// diagnostics above). Mirrors the guard in `jit_putfield_object`: under
/// synthetic/real-JDK layout drift a stale `field_index` would otherwise
/// overflow the raw `obj + HEADER + index*SLOT` write into the *neighbouring*
/// heap object — silent corruption surfacing as a delayed SIGSEGV far from the
/// offending putfield. The interpreter (`set_field`) silently drops such
/// writes; match it. `obj_ptr` must be non-null and canonical (the caller's
/// null check + the JIT's receiver discipline guarantee this — a non-canonical
/// receiver would fault on the header read exactly as the raw write would).
#[inline]
unsafe fn jit_putfield_slot_in_bounds(obj_ptr: i64, field_index: i64) -> bool {
    if field_index < 0 {
        return false;
    }
    // off 16: num_slots (u32).
    let num_slots = std::ptr::read((obj_ptr as *const u8).add(16) as *const u32);
    (field_index as u64) < num_slots as u64
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    // DIAGNOSTIC (gated by CRATONVM_DBG_JIT_PUTFIELD, one-shot, zero release
    // cost when off): a JIT operand-stack miscompile can hand this helper a
    // non-canonical receiver (e.g. the int/boolean `1` instead of an object
    // pointer), and the unchecked write below then faults through ~null. When
    // enabled, report obj_ptr/field_index + the containing JIT method BEFORE
    // dereferencing, so the miscompiled method/bytecode can be pinned. (Pinned
    // case: avrora real-RAF -> LegacyInstrVisitor.visit(CPI), obj_ptr=0x1.)
    if crate::runtime::env_cache::jit_putfield_diag() {
        let bits = obj_ptr as u64;
        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[JIT-PFI-BAD] non-canonical receiver obj_ptr=0x{:x} field_index={} val=0x{:x} (write would target 0x{:x}) current_jit_callee={}",
                    bits, field_index, val as u64,
                    bits.wrapping_add((HEADER_SIZE + field_index as usize * SLOT_SIZE) as u64),
                    current_jit_callee(),
                );
            }
        }
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    if crate::runtime::env_cache::jit_pfi_trace() {
        // Read existing value to see if we're overwriting a ref with an int
        let existing = std::ptr::read(ptr as *const Value);
        let cid_off = obj_ptr as *const u8;
        let cid: u32 = std::ptr::read(cid_off as *const u32);
        eprintln!("[JIT-PFI] obj=0x{:x} class_id={} field_index={} val=0x{:x} (val_as_i32={}) prev_value={:?}",
            obj_ptr as usize, cid, field_index, val as u64, val as i32, existing);
    }
    std::ptr::write(ptr as *mut Value, Value::Int(val as i32));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Double(f64::from_bits(val as u64)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr must be 0 (null) or a valid heap pointer to a live object.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putfield_object(
    vm_ptr: i64,
    obj_ptr: i64,
    field_index: i64,
    val: i64,
) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    // Bounds check against the object's declared slot count. The
    // interpreter's `GenHeap::set_field` silently drops a write whose
    // index falls past the object's layout (synthetic/real-JDK layout
    // drift); the JIT helper previously did a raw unchecked
    // `obj + HEADER + index*SLOT` write, so an out-of-range `field_index`
    // overflowed into the *neighbouring* heap object — silent corruption
    // that surfaced as a delayed SIGSEGV far from the offending putfield
    // (observed in Tomcat: JIT-compiled `Catalina.setParentClassLoader`).
    // Match the interpreter: drop the write instead of corrupting the heap.
    // Reads num_slots straight from the object header (same check
    // `jit_putfield_int` uses) — the `heap.num_fields` virtual-dispatch
    // route lands on the same header word at several times the cost, and
    // this helper runs once per reference field store.
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return;
    }
    if crate::runtime::env_cache::jit_pfo_trace() {
        let cid_off = obj_ptr as *const u8;
        let obj_cid: u32 = std::ptr::read(cid_off as *const u32);
        // Surface only the suspect bit-patterns: invalid kind byte or
        // implausibly-large array_length. Real refs to live objects pass
        // through silently.
        let val_class_id = if val != 0 {
            let v_cid_ptr = val as *const u8;
            std::ptr::read(v_cid_ptr as *const u32)
        } else { 0 };
        let val_kind = if val != 0 {
            let v_kind_ptr = (val as *const u8).add(4);
            std::ptr::read(v_kind_ptr)
        } else { 0 };
        let val_arrlen = if val != 0 {
            let len_ptr = (val as *const u8).add(12);
            std::ptr::read(len_ptr as *const u32)
        } else { 0 };
        if val != 0 && (val_kind > 1 || val_arrlen > 1_000_000) {
            eprintln!("[JIT-PFO] obj=0x{:x} obj_cid={} field_index={} val=0x{:x} val_cid={} val_kind={} val_arrlen=0x{:x}",
                obj_ptr as usize, obj_cid, field_index, val as u64, val_class_id, val_kind, val_arrlen);
        }
    }
    let ptr = obj_ref
        .as_ptr()
        .add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter putfield's
    // `shared.heap.satb_barrier(old_value)` at runtime/interpreter.rs:6391.
    // Read the OLD reference before overwriting it so concurrent marking
    // preserves the snapshot-at-the-beginning invariant. The post-store
    // `write_barrier` (card-table dirty) below is necessary but not
    // sufficient on its own — without this pre-barrier the marker can
    // lose any still-live ref reachable only through this slot.
    //
    // `satb_barrier` is the inherent name for the pre-write barrier on
    // `VmHeap` in this codebase. When/if the planned
    // `GarbageCollector::write_barrier_pre` trait alias lands, this call
    // should migrate to it so the debug-build (pre, store, post) triad
    // assertion in `gc/src/vm_heap.rs` can validate slot-identity pairing.
    let old_value: Value = std::ptr::read(ptr as *const Value);
    if let Value::Object(Some(_)) = old_value {
        let heap = heap_from_vm(vm_ptr);
        heap.satb_barrier(old_value);
    }
    std::ptr::write(ptr as *mut Value, value);
    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        heap.write_barrier(obj_ref, value);
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr and val_ptr must be 0 (null) or valid heap pointers to live objects.
// Records a generational write barrier so the GC tracks old-to-young references.
pub unsafe extern "C" fn jit_write_barrier(vm_ptr: i64, obj_ptr: i64, val_ptr: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 { return; }
    if val_ptr == 0 {
        return;
    }
    let heap = heap_from_vm(vm_ptr);
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let val_ref = ObjectRef::from_raw(val_ptr as usize as *mut u8);
    let value = Value::Object(Some(val_ref));
    heap.write_barrier(obj_ref, value);
}

// Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier helper.
//
// Logs the OLD reference value to the per-thread SATB buffer before the
// JIT-compiled aastore / putfield / putstatic actually overwrites the
// reference slot. This preserves the snapshot-at-the-beginning invariant
// the concurrent marker relies on; without it, JIT-overwritten still-live
// references silently disappear from the mark closure and become UAF on
// the next mixed evacuation.
//
// The interpreter calls `shared.heap.satb_barrier(old_value)` at every
// ref-store site (interpreter.rs lines 4093, 5164, 5961, 6206). This
// helper is the JIT-callable equivalent.
//
// Fast path: when concurrent marking is idle (`SatbQueue::is_active() ==
// false`), the helper performs a single Acquire load and returns — no
// lock taken, no buffer touched. In steady state the cost is two
// register operations and a not-taken branch.
//
// SAFETY: Called from JIT-compiled code. `vm_ptr` must be a valid
// SharedVm pointer. `old_ref` is 0 (null) or the raw address of the
// reference value that was about to be overwritten; null is filtered out
// inside `satb_barrier` and `satb_thread_local_log`, so passing it is
// safe but wasteful.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_satb_pre_write_barrier(vm_ptr: i64, old_ref: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 || old_ref == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let old_obj = ObjectRef::from_raw(old_ref as usize as *mut u8);
    vm.heap.satb_barrier(Value::Object(Some(old_obj)));
}

// ---------------------------------------------------------------------------
// Static field helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_getstatic(vm_ptr: i64, class_id_raw: i64, field_index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);

    // Bootstrap intercept: mirror the interpreter's System.out/err/in intercept.
    // The real JDK System.<clinit> isn't fully bootable; the interpreter returns
    // pre-built synthetic streams for these three fields. The JIT must do the same,
    // or jit_getstatic falls through to get_static_shared → Object(None) → null
    // receiver → println silently no-ops (arg0=0x0 in jit_invoke_dispatch).
    let field_name = vm.class_manager.read().get_class(class_id).and_then(|c| {
        if &*c.name == "java/lang/System" {
            c.fields.get(field_index as usize).map(|f| f.name.to_string())
        } else {
            None
        }
    });
    if let Some(ref fname) = field_name {
        if fname == "out" || fname == "err" {
            // Honor System.setOut/setErr: if the static field was explicitly set
            // (via setOut0/setErr0), use that value; otherwise fall back to the
            // canonical synthetic stream (same logic as the interpreter intercept).
            let overridden = match crate::vm::get_static_shared(vm, class_id, field_index as usize) {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            };
            let stream = match overridden {
                Some(s) => s,
                None => {
                    let (out, err) = vm.ensure_system_streams();
                    if fname == "out" { out } else { err }
                }
            };
            return stream.as_ptr() as i64;
        } else if fname == "in" {
            // System.in: same pattern — use the pre-built InputStream object.
            // `ensure_system_stdin_object` requires a mutable JvmThread which we
            // don't have here; fall back to the static field (set during init).
            let val = crate::vm::get_static_shared(vm, class_id, field_index as usize);
            return match val {
                Value::Object(Some(r)) => r.as_ptr() as i64,
                _ => 0,
            };
        }
    }

    let val = crate::vm::get_static_shared(vm, class_id, field_index as usize);
    match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => r.as_ptr() as i64,
        Value::Object(None) => 0,
        _ => 0,
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_int(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: vm_ptr is non-null and points to a valid SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Int(val as i32));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_long(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_float(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_double(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Double(f64::from_bits(val as u64)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time. val is 0 (null) or a raw
// pointer to a live heap object, converted to Value::Object for storage in the static field table.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_object(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier — log the
    // OLD static value before overwriting. Mirrors interpreter putstatic
    // at runtime/interpreter.rs:5961.
    let old_static = crate::vm::get_static_shared(vm, class_id, field_index as usize);
    if let Value::Object(Some(_)) = old_static {
        vm.heap.satb_barrier(old_static);
    }
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    crate::vm::set_static_shared(vm, class_id, field_index as usize, value);
}

// ---------------------------------------------------------------------------
// Type check helpers
// ---------------------------------------------------------------------------

/// Common type-check resolution shared by `jit_checkcast` and `jit_instanceof`.
///
/// Returns `true` if `obj_ref` (which must be non-null and live) is an instance
/// of the class named `class_name`. Mirrors the interpreter's `Instanceof` /
/// `Checkcast` semantics exactly:
///
/// 1. Resolve the target class via `load_class_concurrent` so a not-yet-loaded
///    target class is loaded on demand. This is the fix for the long-standing
///    JIT instanceof miscompile that returned `false` whenever the target class
///    happened to be loaded only after the JIT call site warmed up.
/// 2. Fall back to `lambda_proxy_satisfies` for objects whose class id is a
///    synthetic lambda proxy (>= 0x8000_0000 — never present in `class_store`).
/// 3. Fall back to `synthetic_implements` for the hand-built collection helper
///    classes whose interface relationships live in `synthetic_implements`
///    rather than in the loaded class hierarchy.
///
/// # Safety
/// Caller must ensure `vm_ptr` is a valid `SharedVm` pointer and `obj_ref` is
/// derived from a live heap object (or that the caller has already short-circuited
/// the null case). The function holds only short-lived `class_manager.read()` /
/// `class_manager.write()` locks and never reborrows the heap.
// SAFETY: Caller must ensure vm_ptr (via `vm`) is a valid SharedVm reference and obj_ref is
// derived from a live heap object. Only short-lived class_manager read/write locks are held;
// the heap is never reborrowed. The null case must be handled by the caller before entry.
unsafe fn jit_typecheck_resolve(
    vm: &SharedVm,
    obj_class_id: ClassId,
    obj_ref: ObjectRef,
    class_name: &str,
) -> bool {
    // KC26 array.clone() bug — descriptor-based array assignability.
    //
    // When the receiver is an array, falling through to the class-hierarchy
    // `is_subclass_of` path misses every legitimate case: primitive arrays
    // carry `class_id == 0` (no class entry), and reference arrays store
    // their *component* class id in the header (which is never a subclass
    // of the array class). The interpreter's `Checkcast` handler
    // (`runtime/interpreter.rs::6876`) computes the array's descriptor
    // and runs `array_is_assignable_to` — mirror that here so JIT-compiled
    // checkcast/instanceof on arrays returns the same result.
    //
    // Reproducer: `() -> SRC.clone()` on an `int[]` field returns null in
    // the JIT'd lambda body because `checkcast [I` after the clone() return
    // hit the false branch below and zeroed the result. With this branch
    // in place, the cast succeeds and the array round-trips correctly.
    if vm.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
        if let Some(src_desc) =
            crate::runtime::interpreter::array_descriptor_of(vm, obj_ref)
        {
            if crate::runtime::interpreter::array_is_assignable_to(
                vm, &src_desc, class_name,
            ) {
                return true;
            }
        }
    }

    // Fast path: target already loaded. Most call sites hit this.
    //
    // IMPORTANT: bind the result to a local so the `RwLockReadGuard` temporary
    // from `.read()` is dropped at the semicolon. Using `if let Some(x) =
    // rwlock.read().method()` would extend the guard's lifetime to the entire
    // `if let` block (including the `else` branch), deadlocking any path that
    // later calls `load_class_concurrent` (which needs a write lock).
    let target_class_id_opt = vm.class_manager.read().find_class_by_name(class_name);
    if let Some(target_class_id) = target_class_id_opt {
        let is_subclass = vm
            .class_manager
            .read()
            .is_subclass_of(obj_class_id, target_class_id);
        if is_subclass {
            return true;
        }
        // Lambda proxy fallback uses the *already-resolved* target id.
        if crate::runtime::interpreter::lambda_proxy_satisfies_public(
            vm,
            obj_class_id,
            target_class_id,
        ) {
            return true;
        }
    } else {
        // Slow path: target not yet loaded. Load it on demand using the
        // concurrent loader so we don't deadlock if another thread is racing
        // the same load. Failure is silently treated as "not assignable",
        // matching what HotSpot does for unresolvable targets in instanceof
        // (instanceof on an unresolvable target returns false; checkcast
        // would have been linked earlier and is a different failure mode).
        if let Ok(target_class_id) = vm.load_class_concurrent(class_name) {
            let is_subclass = vm
                .class_manager
                .read()
                .is_subclass_of(obj_class_id, target_class_id);
            if is_subclass {
                return true;
            }
            if crate::runtime::interpreter::lambda_proxy_satisfies_public(
                vm,
                obj_class_id,
                target_class_id,
            ) {
                return true;
            }
        }
    }

    // Name-based fallback for synthetic classes whose interface relationships
    // are encoded in `synthetic_implements` rather than in the class hierarchy.
    if crate::runtime::interpreter::synthetic_implements_public(vm, obj_class_id, class_name) {
        return true;
    }
    // Instance-aware annotation-proxy admission (the proxy's real annotation
    // type lives on the heap object, not its shared ClassId).
    if crate::runtime::interpreter::annotation_proxy_satisfies_target(vm, obj_ref, class_name) {
        return true;
    }

    // Array fallback: arrays with class_id 0 (e.g. from Array.newInstance via JIT)
    // lack class hierarchy entries.  Any reference array is assignable to
    // [Ljava/lang/Object; and any array is assignable to java/lang/Object,
    // java/io/Serializable, or java/lang/Cloneable.
    if vm.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
        if class_name == "[Ljava/lang/Object;"
            || class_name == "java/lang/Object"
            || class_name == "java/io/Serializable"
            || class_name == "java/lang/Cloneable"
        {
            return true;
        }
    }

    let _ = obj_ref;
    false
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT-compiled code's string table (or are null/<=0
// for an unresolved site, which fails closed). Returns obj_ptr on success, 0 on failure.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_checkcast(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Null reference is always a valid cast (matches JVMS §6.5.checkcast).
    if obj_ptr == 0 {
        return 0;
    }
    // Defensive: an unresolved typecheck site (no class_name attached) must
    // not silently allow the cast. Return 0 so the JIT-compiled code observes
    // a "failed cast" and falls back to the interpreter exception path.
    if class_name_len <= 0 || class_name_ptr.is_null() {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    // SAFETY: obj_ptr is non-null (checked above) and points to a live heap object.
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let obj_class_id = vm.heap.class_id_of(obj_ref);
    if jit_typecheck_resolve(vm, obj_class_id, obj_ref, class_name) {
        obj_ptr
    } else {
        0
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT string table (or are null/<=0 for unresolved,
// which returns 0). Returns 1 if obj is an instance, 0 otherwise.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_instanceof(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Null reference is never an instance of anything (JVMS §6.5.instanceof).
    if obj_ptr == 0 {
        return 0;
    }
    if class_name_len <= 0 || class_name_ptr.is_null() {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    // SAFETY: obj_ptr is non-null (checked above) and points to a live heap object.
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let obj_class_id = vm.heap.class_id_of(obj_ref);
    if jit_typecheck_resolve(vm, obj_class_id, obj_ref, class_name) {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Bounds check helper
// ---------------------------------------------------------------------------

/// JIT bounds-check helper — sets a pending AIOOBE flag and returns `i64::MIN`
/// (the deopt sentinel) to signal the interpreter that a bounds check failed.
///
/// On Windows, JIT frames have no SEH unwind tables, so panicking here would
/// terminate the process instead of unwinding to the `catch_unwind` in the
/// interpreter.  Using a thread-local flag sidesteps this platform limitation.
// SAFETY: Called from JIT-compiled code when an array bounds check fails.
// Only stores two i64 values in a thread-local; no pointer dereferences.
pub unsafe extern "C" fn jit_throw_aioobe(index: i64, length: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
    i64::MIN // deopt sentinel — interpreter will detect and throw AIOOBE
}

/// RBC.6 (athrow codegen) — stash the thrown exception object as the
/// pending JIT exception and return the `i64::MIN` deopt sentinel. The
/// x64 `athrow` arm calls this and immediately runs the method epilogue;
/// the interpreter's JIT-return drains (`take_jit_pending_exception` on
/// every dispatch-aware return path) route the exception through the
/// caller's handling. `exc_ptr == 0` (athrow on a null reference) sets
/// the pending-NPE flag instead, per JVMS athrow semantics.
///
/// Same platform rationale as [`jit_throw_aioobe`]: JIT frames have no
/// SEH unwind tables on Windows, so a Rust panic/unwind here would
/// terminate the process; thread-local stashing sidesteps that.
// SAFETY: Called from JIT-compiled code at an athrow site. `exc_ptr` is
// either 0 or the heap pointer the JIT popped from the operand stack;
// no dereference happens here — it is only wrapped and stored in a TLS.
pub unsafe extern "C" fn jit_throw_exception(exc_ptr: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if exc_ptr == 0 {
        stash_jit_pending_npe();
    } else {
        stash_jit_pending_exception(ObjectRef::from_raw(exc_ptr as usize as *mut u8));
    }
    i64::MIN // deopt sentinel — interpreter drains the pending exception
}

// ---------------------------------------------------------------------------
// Invoke dispatch helpers
// ---------------------------------------------------------------------------

/// Per-info-pointer JIT dispatch state: caches a compiled callee's entry point
/// so that repeated calls from the same JIT call site skip the JIT cache lookup.
struct DispatchCache {
    entry: usize,
    needs_context: bool,
}

// Thread-local map from JitInvokeInfo pointer -> cached JIT entry.
// Using a thread-local avoids synchronization on the hot path.
// T10.9.B: FxHashMap — pointer values are internal; this is touched on every
// JIT-dispatched invoke.
thread_local! {
    static DISPATCH_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<usize, DispatchCache>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    static DISPATCH_COUNTER: std::cell::RefCell<rustc_hash::FxHashMap<usize, u32>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}


// ===========================================================================
// BUG-1 fix: native-stack recursion guard for the JIT→JIT dispatch path.
//
// A recursive Java method that has been JIT-compiled (e.g. `binaryTrees(18)`'s
// deep `make(int)` self-recursion) never re-enters `interpreter::execute`, so
// the interpreter's `EXEC_DEPTH` / `EXEC_DEPTH_CEILING` guard (which throws a
// *catchable* `StackOverflowError`) never fires. Each compiled recursion level
// instead stacks a large native Rust frame through
// `jit_invoke_dispatch` / `jit_invoke_virtual_mic` → compiled entry → … with
// nothing checking remaining OS stack, so deep recursion overflows the guard
// page → uncatchable rc=127 abort.
//
// We mirror the interpreter's guard with a dedicated thread-local JIT-dispatch
// depth counter, incremented at the top of BOTH dispatch helpers under an RAII
// Drop-decrement, compared against the per-thread ceiling
// `interpreter::jit_dispatch_depth_ceiling()` (derived from the thread's REAL
// native stack size, using a generous per-level budget because the JIT
// dispatch frame is much larger than an interpreter level). On overflow we do
// NOT recurse: we stash a catchable `java/lang/StackOverflowError` in
// `JIT_PENDING_EXCEPTION` and return the `i64::MIN` deopt sentinel, exactly
// like the array-NPE / AIOOBE helpers, so the interpreter's post-JIT drain
// routes it through the method's exception table.
// ===========================================================================
thread_local! {
    /// Re-entrant JIT-dispatch depth for the current thread. Incremented on
    /// entry to `jit_invoke_dispatch` / `jit_invoke_virtual_mic` and
    /// decremented (RAII) on return. Compared against
    /// `interpreter::jit_dispatch_depth_ceiling()`.
    static JIT_DISPATCH_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard that decrements [`JIT_DISPATCH_DEPTH`] when dropped. Constructed
/// by [`enter_jit_dispatch`] only AFTER the depth has been incremented and the
/// ceiling check passed, so every successful entry has exactly one matching
/// decrement on every return path (including the compiled callee unwinding).
struct JitDispatchDepthGuard;
impl Drop for JitDispatchDepthGuard {
    fn drop(&mut self) {
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Enter a JIT dispatch level: bump the thread-local depth and check it against
/// the per-thread native-stack ceiling.
///
/// On success returns `Ok(guard)` — the caller binds it (e.g. `let _g = ...`)
/// so the level is released on return. On overflow returns `Err(sentinel)`:
/// the depth has already been rolled back, a catchable
/// `java/lang/StackOverflowError` has been stashed in `JIT_PENDING_EXCEPTION`
/// (when constructible), and the caller must immediately `return` the contained
/// `i64::MIN` deopt sentinel WITHOUT recursing further. If the throwable cannot
/// be constructed (heap too exhausted) the sentinel is still `i64::MIN` so the
/// JIT caller deopts rather than continuing with corrupt state.
#[must_use]
fn enter_jit_dispatch(vm: &SharedVm) -> Result<JitDispatchDepthGuard, i64> {
    let ceiling = crate::runtime::interpreter::jit_dispatch_depth_ceiling();
    let depth = JIT_DISPATCH_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v + 1
    });
    if depth > ceiling {
        // Roll back the increment we just made — we are NOT entering a level.
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        return Err(raise_jit_stack_overflow(vm));
    }
    Ok(JitDispatchDepthGuard)
}

/// Stash a catchable `java/lang/StackOverflowError` for the JIT caller to route
/// through its exception table, and return the `i64::MIN` deopt sentinel.
///
/// Mirrors `jit_newarray_oom`: obtain the current `&mut JvmThread`, construct
/// the throwable via `create_exception_object`, and `set_jit_pending_exception`.
/// Cold path — only hit at pathological recursion depth.
#[cold]
fn raise_jit_stack_overflow(vm: &SharedVm) -> i64 {
    // SAFETY: called from inside a JIT dispatch helper, on the thread that set
    // the JIT thread pointer; no other `&mut JvmThread` is live at this point
    // (we are above any compiled-callee invocation).
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/StackOverflowError",
            None,
        ) {
            set_jit_pending_exception(exc);
        }
    }
    i64::MIN
}

/// S112r9 — JIT dispatch error handler. When a JIT-dispatched callee returns
/// an error, route it through `JIT_PENDING_EXCEPTION` so the interpreter's
/// post-JIT exception-routing path can find a handler (or propagate to the
/// top of the JVM with a printable message).
///
/// Previously only `MethodCallFailed::ExceptionThrown` was captured, and
/// `MethodCallFailed::InternalError` was silently dropped — the JIT helper
/// returned 0/null to the JIT caller, which would proceed as if the call
/// returned a benign null. That was the root cause of Spring Boot 3 fat-jars
/// exiting silently with rc=0 between `prepareEnvironment` and `printBanner`:
/// some downstream invoke produced an `InternalError` ("method has no Code
/// attribute" or similar linkage gap), the JIT swallowed it, the JIT'd
/// `prepareEnvironment` continued with corrupt state and returned, then the
/// caller `run()` returned cleanly without ever reaching `printBanner`.
///
/// Wrapping the InternalError in a Java `java/lang/InternalError` gives the
/// VM a real Throwable to walk through exception tables. If the heap is
/// exhausted or the class can't be loaded, we fall back to leaving the
/// error unstored — the original "swallow and return 0" behaviour. That
/// keeps this purely additive: it never makes a previously-working scenario
/// worse, only converts silent rc=0 into a visible stack trace.
/// Route a failed JIT dispatch into the thread-local pending-exception
/// slot and return the value the dispatch helper should hand back to its
/// JIT caller: `i64::MIN` (the deopt sentinel) when a pending Java
/// exception was successfully stashed, or `0` if the failure could not be
/// turned into a throwable (legacy silent-drop fallback).
///
/// Returning `i64::MIN` makes the JIT caller's post-invoke exception guard
/// (`emit_post_invoke_exception_check` in `jit/src/x64.rs`) fire and deopt
/// out, so the interpreter routes the real exception through the method's
/// exception table — instead of the JIT running on with a bogus `0` and
/// masking the true failure with a downstream secondary error.
#[must_use]
fn handle_jit_dispatch_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> i64 {
    use crate::error::{ClassFileError, LinkageError, MethodCallFailed, RuntimeError, VmError};
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            set_jit_pending_exception(exc);
        }
        // A native callee that returns `Err(RuntimeError::X)` is, by the
        // exception model, asking the VM to throw the Java exception that
        // `X` maps to (e.g. `NoSuchMethodException`, `NullPointerException`,
        // `ClassCastException`). The `From<RuntimeError>` conversion wraps
        // these as `InternalError(VmError::Runtime(..))`, which is *not*
        // an internal VM bug — it is a catchable Java throwable.
        //
        // The interpreter's per-instruction post-processing already does
        // this conversion (`interpreter.rs`: "Convert RuntimeErrors from
        // native methods into catchable Java exceptions"), but the JIT
        // dispatch path previously skipped it and wrapped the runtime
        // error in a *fatal* `java/lang/InternalError`. That turned an
        // ordinary catchable exception into an uncatchable abort — e.g.
        // Netty's `Class.getDeclaredConstructor(...)` probe for the
        // legacy `DirectByteBuffer(long,int)` constructor (absent on
        // JDK 25) threw `NoSuchMethodException`, which Netty catches and
        // falls back from; under the JIT it surfaced as a fatal
        // `InternalError: ... NoSuchMethodException: <init>`.
        //
        // Mirror the interpreter: route `VmError::Runtime` through
        // `throw_runtime_error` so the proper Java exception object is
        // built and caught by the caller's exception table. Exclude
        // `NotImplemented` / `StackOverflowError` for parity with the
        // interpreter's exclusion list (those stay as hard errors).
        MethodCallFailed::InternalError(VmError::Runtime(rt_err))
            if !matches!(
                rt_err,
                RuntimeError::NotImplemented { .. } | RuntimeError::StackOverflowError
            ) =>
        {
            match crate::runtime::exceptions::throw_runtime_error(vm, thread, rt_err) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    // Exception-object construction failed — fall back to
                    // the legacy `InternalError` wrap so the failure is
                    // still visible rather than silently dropped.
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm, thread, "java/lang/InternalError", Some(&msg),
                    ) {
                        set_jit_pending_exception(exc);
                    }
                }
            }
        }
        // `VmError::Linkage` variants are catchable Java errors in the
        // `java.lang.LinkageError` hierarchy (NoClassDefFoundError,
        // NoSuchFieldError, NoSuchMethodError, …). The interpreter
        // already converts them at opcode boundaries via
        // `convert_class_not_found` (`runtime/exceptions.rs`); the JIT
        // dispatch path previously fell through to the catch-all and
        // wrapped them in `java/lang/InternalError`, breaking
        // `catch (NoClassDefFoundError)` / `catch (LinkageError)` in
        // application code — notably JUnit Platform's test-execution
        // error handler in `EngineExecutionOrchestrator.execute`, which
        // expects `NoClassDefFoundError` from a JIT-compiled callee to
        // propagate as-is so the orchestrator can record the test as
        // failed instead of aborting the whole JVM.
        //
        // Mirror the interpreter: build the matching Java throwable so
        // the caller's exception table can find a handler.
        MethodCallFailed::InternalError(VmError::Linkage(linkage_err)) => {
            let (exc_class, detail) = match &linkage_err {
                LinkageError::NoClassDefFoundError { class_name } => {
                    ("java/lang/NoClassDefFoundError", class_name.clone())
                }
                LinkageError::NoSuchFieldError { class_name, field_name } => {
                    ("java/lang/NoSuchFieldError", format!("{}.{}", class_name, field_name))
                }
                LinkageError::NoSuchMethodError {
                    class_name, method_name, method_descriptor,
                } => (
                    "java/lang/NoSuchMethodError",
                    format!("{}.{}{}", class_name, method_name, method_descriptor),
                ),
                LinkageError::IncompatibleClassChangeError { message } => {
                    ("java/lang/IncompatibleClassChangeError", message.clone())
                }
                LinkageError::AbstractMethodError { class_name, method_name } => {
                    ("java/lang/AbstractMethodError", format!("{}.{}", class_name, method_name))
                }
                LinkageError::IllegalAccessError { message } => {
                    ("java/lang/IllegalAccessError", message.clone())
                }
                LinkageError::VerifyError { class_name, method_name, message } => (
                    "java/lang/VerifyError",
                    format!("{}.{}: {}", class_name, method_name, message),
                ),
                LinkageError::ClassFormatError { class_name, message } => {
                    ("java/lang/ClassFormatError", format!("{}: {}", class_name, message))
                }
                LinkageError::UnsupportedClassRedefinitionError { class_name, message } => (
                    "java/lang/UnsupportedOperationException",
                    format!("{}: {}", class_name, message),
                ),
            };
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm, thread, exc_class, Some(&detail),
            ) {
                set_jit_pending_exception(exc);
            } else {
                // Throwable construction failed (heap / rt.jar gap) — fall
                // back to the legacy InternalError wrap so the failure is
                // still visible rather than silently dropped.
                let msg = format!(
                    "JIT dispatch into {}.{}{} failed: {}",
                    info.class_name, info.method_name, info.descriptor, linkage_err,
                );
                if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                    vm, thread, "java/lang/InternalError", Some(&msg),
                ) {
                    set_jit_pending_exception(exc);
                }
            }
        }
        // A class-resolution miss in the resolver surfaces as
        // `VmError::ClassFile(ClassNotFound)`. The interpreter maps this
        // to `NoClassDefFoundError` (see `raise_no_class_def_found`); do
        // the same here so JIT-dispatched callees behave identically.
        MethodCallFailed::InternalError(VmError::ClassFile(
            ClassFileError::ClassNotFound { ref class_name },
        )) => {
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm, thread, "java/lang/NoClassDefFoundError", Some(class_name),
            ) {
                set_jit_pending_exception(exc);
            }
        }
        MethodCallFailed::InternalError(vm_err) => {
            // Format a message that points at the failing dispatch site so
            // the user can see WHICH callee blew up. This is the difference
            // between a silent rc=0 and a visible "Exception in thread main"
            // for Spring Boot.
            let msg = format!(
                "JIT dispatch into {}.{}{} failed: {}",
                info.class_name, info.method_name, info.descriptor, vm_err,
            );
            // Try to wrap in a Java `InternalError`; on any allocation /
            // load failure, fall through to the legacy silent drop so we
            // never make things worse than before this fix.
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm, thread, "java/lang/InternalError", Some(&msg),
            ) {
                set_jit_pending_exception(exc);
            }
        }
    }
    // Return the deopt sentinel iff a pending exception was actually
    // stashed; otherwise `0` (legacy silent-drop — exception construction
    // itself failed, nothing for the caller to route).
    if jit_pending_exception_is_set() {
        i64::MIN
    } else {
        0
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo (heap-allocated, outlives this call).
// args_ptr/num_args form a valid i64 slice of JIT-encoded arguments.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// with known signatures matching the compiled method's calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_dispatch(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush. A dispatched
    // call may transitively enter the GC barrier through callee
    // allocations or `Object.wait` paths.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe_disp();
    let _cyc_disp = mic_prof::CycGuard::new(&mic_prof::CYC_DISP_TOTAL);
    // WS1 diagnostic: per-call dispatch trace (the dispatch helper is cold
    // enough on the kafka repro — ~15 calls — that an eprintln per call is
    // affordable and names the callee that encloses the lost wall time).
    let _disp_trace = if mic_prof::enabled() {
        let info = &*(info_ptr as *const JitInvokeInfo);
        struct DispTrace {
            label: String,
            t0: u64,
        }
        impl Drop for DispTrace {
            fn drop(&mut self) {
                eprintln!(
                    "[DISP_TRACE] {} cycles={}",
                    self.label,
                    mic_prof::now().wrapping_sub(self.t0)
                );
            }
        }
        Some(DispTrace {
            label: format!(
                "{}.{}{} kind={}",
                info.class_name, info.method_name, info.descriptor, info.invoke_kind
            ),
            t0: mic_prof::now(),
        })
    } else {
        None
    };
    // SAFETY: vm_ptr and info_ptr originate from JIT code; both point to valid, live objects.
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    // DIAGNOSTIC (gated): record the dispatched callee (restored on return) so
    // a downstream jit_putfield_int miscompile can name the offending method.
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    // Defensive gate: when the user-facing CRATONVM_DISABLE_JIT kill-switch is set,
    // no JIT code should be executing — so this dispatch helper must never run.
    // Reaching it means a JIT entry point bypassed the flag (a real bug). Returning
    // 0 here is preferable to UB from a stale compiled callsite; emit a one-shot
    // warning so the bypass is visible during bisection.
    if crate::runtime::env_cache::disable_jit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[cratonvm] WARN: jit_invoke_dispatch reached with CRATONVM_DISABLE_JIT=1 \
                 (callee {}.{}{}). A JIT entry-point bypassed the kill-switch — \
                 returning 0 to avoid undefined behavior.",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return 0;
    }
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        let p = args_ptr as *const i64;
        let mut buf = String::new();
        if !p.is_null() && num_args > 0 {
            for i in 0..(num_args as usize).min(4) {
                let v = unsafe { *p.add(i) };
                buf.push_str(&format!(" arg{}=0x{:x}", i, v));
            }
        }
        eprintln!(
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{} info_ptr=0x{:x}",
            info.class_name, info.method_name, info.descriptor, info.invoke_kind, num_args, buf,
            info_ptr,
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    // SAFETY: args_ptr is non-null (checked above) and num_args >= 0.
    // The JIT caller allocated this array on its own stack frame.
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // BUG-1: native-stack recursion guard for the JIT→JIT dispatch path. Bump
    // the per-thread JIT-dispatch depth and check it against the native-stack
    // ceiling BEFORE we recurse into any compiled callee. On overflow this
    // stashes a catchable `StackOverflowError` and returns the `i64::MIN`
    // deopt sentinel instead of overflowing the OS stack. The guard is held
    // for the rest of this call so the level is released on every return path.
    let _jit_dispatch_depth_guard = match enter_jit_dispatch(vm) {
        Ok(g) => g,
        Err(sentinel) => return sentinel,
    };

    // Fast path: check thread-local dispatch cache for a previously-compiled callee.
    // This avoids the JIT cache lock on every call.
    let info_key = info_ptr as usize;
    // Virtual/interface dispatch (invoke_kind 0/2) must resolve on the RUNTIME
    // receiver type. The callsite-keyed entry cache below and the
    // `info.class_name` (static CP class) JIT-cache lookup both assume static
    // binding, so reusing them at a polymorphic call site dispatches a
    // SUPERTYPE method — e.g. `Object.equals` (identity `==`) run on boxed
    // `Integer` receivers, so two equal Integers compare unequal and junit
    // `assertEquals` fails on identical values. Only the statically-bound kinds
    // (invokespecial=1, invokestatic=3) may use these fast paths; virtual /
    // interface fall through to the receiver-resolving slow path
    // (`ctx.invoke_virtual`). Hot monomorphic virtual sites are already served
    // by the receiver-guarded MIC helper (`jit_invoke_virtual_mic`).
    let statically_bound = matches!(info.invoke_kind, 1 | 3);
    let cached_entry = if statically_bound {
        DISPATCH_CACHE.with(|dc| {
            dc.borrow().get(&info_key).map(|c| (c.entry, c.needs_context))
        })
    } else {
        None
    };
    if let Some((entry, needs_ctx)) = cached_entry {
        if crate::runtime::env_cache::jit_dispatch_dbg() {
            eprintln!(
                "[JIT_DISPATCH_ARM/dcache] {}.{} entry=0x{:x}",
                info.class_name, info.method_name, entry,
            );
        }
        // SAFETY: entry is a JIT-compiled function pointer cached from a previous successful
        // compilation. `try_call_compiled_entry` selects the correct extern "C" fn signature
        // based on arg count; on overflow it returns None and we bail to the interpreter.
        // CRIT round-5 fix: the previous `_ => 0` arm silently dropped 5+-arg callees;
        // wave-2 changed it to fall through to the slow path, and this wave goes one
        // step further by routing directly through `bail_to_interpreter` so the bail is
        // explicit at the call site (matches the MIC fast-path at `:1722`).
        if let Some(rc) = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice) {
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_RET/dcache] {}.{}{} ret=0x{:x}",
                    info.class_name, info.method_name, info.descriptor, rc,
                );
            }
            // BUG-H: if the callee threw an implicit exception its own `catch`
            // should handle, re-run it in the interpreter to route through its
            // exception table.
            return route_implicit_exc_through_callee(vm, info, args_slice, rc);
        }
        // Overflow: decode args once and hand off to the interpreter.
        if let Some((thread, _guard)) = jit_thread_mut() {
            let bail_args = decode_dispatch_values(vm, info, args_slice);
            return bail_to_interpreter(vm, thread, info, &bail_args);
        }
        return 0;
    }

    // Check JIT cache for a compiled version of this callee.
    // PERF: `JitCache::get` takes `&str`, so we can pass the static literals from
    // `info` directly. Earlier code wrapped each in `Arc::from(...)` which
    // allocated a fresh heap buffer + atomic header on every dispatch — three
    // wasted allocations per hot call. Deref coercion handles the conversion.
    // Gated on `statically_bound`: the lookup key is the static CP class, which
    // is only the correct dispatch target for invokespecial/invokestatic.
    if statically_bound {
        let jit_cache = vm.jit_cache.read();
        if let Some(compiled) = jit_cache.get(info.class_name, info.method_name, info.descriptor) {
            let entry = compiled.entry_ptr() as usize;
            let needs_ctx = compiled.needs_context();
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/jcache] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // Cache for future calls
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(info_key, DispatchCache { entry, needs_context: needs_ctx });
            });
            drop(jit_cache);
            // SAFETY: entry was obtained from a CompiledMethod in the JIT cache, whose
            // entry_ptr points to executable memory with the correct extern "C" ABI.
            // CRIT round-5 fix: on >ARG_REGS args, route directly to the interpreter
            // via `bail_to_interpreter` rather than silently returning 0 (the original
            // wave-2 fall-through was already correct; this just makes the bail
            // explicit at the call site to match the MIC fast-path).
            if let Some(rc) = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice) {
                if crate::runtime::env_cache::jit_dispatch_dbg() {
                    eprintln!(
                        "[JIT_DISPATCH_RET/jcache] {}.{}{} ret=0x{:x}",
                        info.class_name, info.method_name, info.descriptor, rc,
                    );
                }
                // BUG-H: route a callee-thrown implicit exception through the
                // callee's own exception table (see dcache site above).
                return route_implicit_exc_through_callee(vm, info, args_slice, rc);
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }

    // Invocation counting — trigger compilation for hot callees. Gated on
    // `statically_bound`: compiling `info` (the static CP-class method) and
    // caching it under the callsite key would re-introduce the supertype
    // miscompile for a virtual/interface site.
    let should_compile = statically_bound && DISPATCH_COUNTER.with(|dc| {
        let mut map = dc.borrow_mut();
        let count = map.entry(info_key).or_insert(0);
        *count += 1;
        *count == crate::runtime::env_cache::jit_invocation_threshold()
    });
    if should_compile {
        // Try to compile the callee and cache it
        if let Some((entry, needs_ctx)) = try_compile_callee(vm, info) {
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/compile] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(info_key, DispatchCache { entry, needs_context: needs_ctx });
            });
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. CRIT round-5 fix: bail explicitly to the interpreter on
            // >ARG_REGS args via `bail_to_interpreter` (matches the MIC fast-path).
            if let Some(rc) = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice) {
                // BUG-H: route a callee-thrown implicit exception through the
                // callee's own exception table (see dcache site above).
                return route_implicit_exc_through_callee(vm, info, args_slice, rc);
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }

    // Slow path: interpreter fallback
    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => {
            return 0;
        }
    };

    // Round-5 CRIT-1 fix: share arg-decoding with the three cache-hit
    // overflow bailouts above via `decode_dispatch_values`.
    let values = decode_dispatch_values(vm, info, args_slice);

    let result: Option<Value> = match info.invoke_kind {
        0 | 2 => {
            if values.is_empty() {
                return 0;
            }
            let receiver_ref = match &values[0] {
                Value::Object(Some(obj)) => *obj,
                _ => return 0,
            };
            let method_args: Vec<Value> = values[1..].to_vec();
            let virt_result = {
                let mut ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                use crate::native::registry::NativeContext;
                ctx.invoke_virtual(
                    receiver_ref,
                    info.method_name,
                    info.descriptor,
                    &method_args,
                )
            };
            match virt_result {
                Ok(v) => v,
                Err(e) => {
                    // S111r12 — JIT virtual-dispatch rescue: when the
                    // receiver's `class_id_of` returns a stub class
                    // (e.g. `java/lang/Comparable` for a malformed
                    // ClassLoader instance) that doesn't declare the
                    // CP-resolved method, `invoke_virtual` raises
                    // `NoSuchMethodError`. The CP method-ref class
                    // carried in `info.class_name` (e.g.
                    // `java/lang/ClassLoader`) is the spec-correct
                    // resolution target — retry the dispatch through
                    // it. Mirrors the S111r10 receiver-walk fallback
                    // for invokeinterface and the S111r8 cid=0 →
                    // CP-class fallback in `execute_invoke`.
                    let is_nsme = matches!(
                        &e,
                        crate::error::MethodCallFailed::InternalError(
                            crate::error::VmError::Linkage(
                                crate::error::LinkageError::NoSuchMethodError { .. },
                            ),
                        ),
                    );
                    if is_nsme && !info.class_name.is_empty() {
                        let recv_cid = vm.heap.class_id_of(receiver_ref);
                        let recv_name_opt = {
                            let cm = vm.class_manager.read();
                            cm.get_class(recv_cid)
                                .map(|c| c.name.to_string())
                        };
                        let cp_differs = recv_name_opt
                            .as_deref()
                            .map(|n| n != info.class_name)
                            .unwrap_or(true);
                        if cp_differs {
                            let r = crate::vm::invoke_or_native(
                                vm,
                                thread,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                &values,
                            );
                            match r {
                                Ok(v) => v,
                                Err(e2) => {
                                    return handle_jit_dispatch_error(
                                        vm, thread, e2, info,
                                    );
                                }
                            }
                        } else {
                            return handle_jit_dispatch_error(vm, thread, e, info);
                        }
                    } else {
                        return handle_jit_dispatch_error(vm, thread, e, info);
                    }
                }
            }
        }
        1 => {
            // invokespecial: dispatch must NOT virtually re-target onto the
            // receiver's runtime class. `invoke_or_native` -> `invoke_on_class_shared`
            // applies the iface/abstract -> receiver-class retarget that
            // `invokevirtual` semantics require, which for invokespecial turns
            // a super-call into a self-call and produces unbounded recursion
            // (e.g. `RunLast.execute` invokespecial-calls `AbstractParseResultHandler.execute`;
            // APRH is abstract, so the retarget bounces back to `RunLast.execute`
            // and we recurse forever — surfaced as `StackOverflowError` inside the
            // picocli `execute` chain on neo4j / keycloak). The `<init>` and
            // `<clinit>` carve-outs that protect that path in
            // `invoke_on_class_shared_inner` do NOT cover ordinary super-calls,
            // so we have to take an invokespecial-aware dispatch path here.
            //
            // `invoke_special_shared` matches the interpreter's invokespecial
            // semantics: walk the hierarchy from the CP-resolved class to the
            // declaring class for the requested method, then invoke through
            // `invoke_on_class_shared_no_retarget` so the virtual retarget never
            // fires. Native-override priority is preserved (same as
            // `invoke_or_native`).
            let r = crate::vm::invoke_special_shared(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &values,
            );
            match r {
                Ok(v) => v,
                Err(e) => {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
            }
        }
        3 => {
            // invokestatic: no receiver, no retarget concern. The historical
            // `invoke_or_native` path is correct here (static method lookup
            // by class name with native-override priority and superclass walk).
            let r = crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &values,
            );
            match r {
                Ok(v) => v,
                Err(e) => {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
            }
        }
        _ => None,
    };

    let ret = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    };
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        eprintln!(
            "[JIT_DISPATCH_RET] {}.{}{} ret=0x{:x} ({})",
            info.class_name, info.method_name, info.descriptor, ret, ret,
        );
    }
    ret
}

/// Try to compile a callee method from a JitInvokeInfo.
/// Returns (entry_ptr, needs_context) if compilation succeeds.
// SAFETY: Caller must ensure vm is a valid SharedVm reference and info points to a live
// JitInvokeInfo. Delegates to try_jit_compile_callee which accesses the class manager
// and JIT compiler; no raw pointer dereferences occur within this function itself.
unsafe fn try_compile_callee(vm: &SharedVm, info: &JitInvokeInfo) -> Option<(usize, bool)> {
    use crate::runtime::interpreter::try_jit_compile_callee;
    try_jit_compile_callee(vm, info.class_name, info.method_name, info.descriptor)
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo. args_ptr/num_args form a valid i64 slice.
// mic_ptr must point to a live JitMICSlot used for monomorphic inline cache dispatch.
// pic_ptr, when non-zero, must point to a live JitPICSlot co-allocated with the MIC at
// the same call site; the helper populates its 3-way entries via `install` so the next
// invocation hits the inline cascade emitted in `jit/src/x64.rs`.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// matching the compiled method's extern "C" calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_virtual_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe();
    let _cyc_total = mic_prof::CycGuard::new(&mic_prof::CYC_MIC_TOTAL);
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // BUG-1: native-stack recursion guard for the JIT→JIT virtual dispatch
    // path (the `invokevirtual` sibling of `jit_invoke_dispatch`). Same
    // contract: bump the per-thread JIT-dispatch depth, and on overflow stash
    // a catchable `StackOverflowError` + return the `i64::MIN` deopt sentinel
    // rather than recursing into a compiled callee and blowing the OS stack.
    // Held for the rest of the call so the level releases on every return.
    let _jit_dispatch_depth_guard = match enter_jit_dispatch(vm) {
        Ok(g) => g,
        Err(sentinel) => return sentinel,
    };

    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => return 0,
    };

    if args_slice.is_empty() {
        return 0;
    }
    let receiver_raw = args_slice[0];
    if receiver_raw == 0 {
        return 0;
    }
    // Defensive: a receiver slot carrying tagged-long bits (low 3 bits set
    // or value above the 48-bit canonical-address ceiling) is not a valid
    // heap pointer.  Bail out with rc=0 (the dispatcher's "no result"
    // path); this mirrors the receiver_raw == 0 short-circuit above and
    // avoids the `ObjectRef::from_raw` alignment panic.
    let receiver_bits = receiver_raw as u64;
    if (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        return 0;
    }
    // SAFETY: receiver_bits is non-zero, 8-byte aligned, and within the
    // 48-bit canonical address space — matches the invariants required by
    // ObjectRef::from_raw for live heap objects.
    let receiver_ref = ObjectRef::from_raw(receiver_raw as usize as *mut u8);

    // WS1 (kafka JIT throughput): the `Value` decode is deferred. The MIC-hit
    // fast path dispatches straight off the raw `args_slice` and never
    // materializes `Value`s; only the lambda, register-overflow-bailout and
    // full-resolution paths pay for the decode. Eagerly building this Vec
    // (one heap alloc + a descriptor parse) on every call — including pure
    // cache hits — was a measured contributor to JIT'd call-heavy code
    // running slower than the interpreter.
    let decode_values = || -> Vec<Value> {
        let mut values = Vec::with_capacity(args_slice.len());
        values.push(Value::Object(Some(receiver_ref)));
        let mut desc_iter = DescriptorParamIter::new(info.descriptor);
        for &raw in &args_slice[1..] {
            let val = match desc_iter.next() {
                Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                    Value::Int(raw as i32)
                }
                Some(b'J') => Value::Long(raw),
                Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
                Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
                Some(b'L') | Some(b'[') => {
                    if raw == 0 {
                        Value::Object(None)
                    } else {
                        // Same defensive guard as the receiver decode above.
                        // Tagged-long bits in an L/[ slot are downgraded to
                        // null instead of panicking in ObjectRef::from_raw.
                        let bits = raw as u64;
                        if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            // SAFETY: bits is non-zero, 8-byte aligned, and
                            // within the 48-bit canonical address space.
                            Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
                        } else {
                            Value::Object(None)
                        }
                    }
                }
                _ => Value::Int(raw as i32),
            };
            values.push(val);
        }
        values
    };

    let receiver_class_id = vm.heap.class_id_of(receiver_ref);
    let receiver_cid = receiver_class_id.as_u32();

    // AnnotationProxy receiver: synthetic class with no bytecode methods, so the
    // cache-miss path below would resolve on it, fail the compile-probe, and
    // call `invoke_or_native` only to hit its annotation rescue — every call.
    // Route straight to the shared annotation dispatch here. Uses the lock-free
    // cached cid (warmed by `invoke_or_native`'s fast-path); before it is warmed
    // the hint is `u32::MAX` (never a real cid) so this simply falls through.
    if receiver_cid == crate::vm::annotation_proxy_cid_hint() {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        let values = decode_values();
        match crate::vm::annotation_proxy_invoke_shared(
            vm,
            thread,
            receiver_ref,
            info.method_name,
            &values[1..],
        ) {
            Ok(result) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    // Lambda-proxy receiver: its class id is a synthetic id absent from the
    // class store, so the resolution below derives an EMPTY class name and
    // `invoke_or_native("")` surfaces as a message-less
    // `NoClassDefFoundError` — e.g. `Consumer.accept` inside a JIT-compiled
    // `CollectionUtils.forEachInReverseOrder` (JUnit5 listener notification)
    // died on the first compiled execution. Mirror the interpreter's
    // invokeinterface route: dispatch through the lambda's SAM impl_handle.
    if vm.lambda_proxies.read().contains_key(&receiver_class_id) {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        let values = decode_values();
        let rest: Vec<Value> = values[1..].to_vec();
        match crate::runtime::interpreter::try_lambda_dispatch(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info.method_name,
            &rest,
        ) {
            Ok(Some(result)) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            // SAM arity/name mismatch (e.g. a same-named default method) —
            // fall through; the CP interface fallback below dispatches the
            // default body via the static call-site class.
            Ok(None) => {}
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    let mic = &*(mic_ptr as *const JitMICSlot);
    let cached_cid = mic
        .cached_class_id
        .load(std::sync::atomic::Ordering::Acquire);

    if crate::runtime::env_cache::jit_mic_dbg() {
        eprintln!(
            "[JIT_MIC] {}.{}{} cached_cid={} recv_cid={} entry={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            cached_cid,
            receiver_cid,
            mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire),
        );
    }

    // --- Monomorphic Inline Cache: fast path ---
    // If the receiver ClassId matches the cached value AND we have a cached
    // entry pointer, dispatch directly without any class_manager lookup or
    // method resolution.  This is the zero-overhead dispatch path.
    if cached_cid == receiver_cid && cached_cid != 0 {
        mic.record_hit();

        // Try the cached compiled entry pointer (true inline cache hit)
        let entry = mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire);
        if entry != 0 {
            // Direct call to the compiled callee — same ABI as `jit_invoke_dispatch`
            // uses after a JIT-cache hit (receiver + params in `args_slice`, optional
            // leading `vm_ptr` when `cached_needs_context` is true).  **Do not** pass
            // `(vm_ptr, info_ptr, args_ptr, num_args)` here; that was a mis-invocation
            // that corrupts the stack and surfaces as Windows AV / Linux SIGSEGV.
            let needs_ctx = mic
                .cached_needs_context
                .load(std::sync::atomic::Ordering::Acquire);
            // Register-table call straight off the raw arg slots — no `Value`
            // decode on this hot path. `try_call_compiled_entry` returns
            // `None` exactly when the callee has more args than the register
            // tables cover (4 no-ctx / 3 with-ctx); only then decode the args
            // and route through the interpreter instead of silently returning
            // 0 from a truncated register-arg table.
            mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
            let rc_opt = {
                let _g = mic_prof::CycGuard::new(&mic_prof::CYC_HIT_ENTRY_CALL);
                try_call_compiled_entry(entry as usize, needs_ctx, vm_ptr, args_slice)
            };
            if let Some(rc) = rc_opt {
                // BUG-H: if the receiver-resolved callee threw an implicit
                // exception (AIOOBE/NPE) its own `catch` should handle, the
                // direct compiled call bypassed its exception table. Re-execute
                // it in the interpreter so the exception routes through the
                // callee's table (e.g. Tomcat `HttpParser.isNotRequestTarget
                // Relaxed`: `IS_NOT_REQUEST_TARGET[c]` in `catch (AIOOBE)`).
                if rc == i64::MIN {
                    let aioobe = take_jit_pending_aioobe();
                    let npe = if aioobe.is_none() {
                        take_jit_pending_npe()
                    } else {
                        false
                    };
                    if aioobe.is_some() || npe {
                        if mic_callee_has_exception_table(vm, receiver_class_id, info) {
                            let values = decode_values();
                            return bail_to_interpreter(vm, thread, info, &values);
                        }
                        // No local handler — re-stash the consumed flag and
                        // propagate the sentinel unchanged (existing behavior).
                        if let Some((idx, len)) = aioobe {
                            stash_jit_pending_aioobe(idx, len);
                        } else if npe {
                            stash_jit_pending_npe();
                        }
                    }
                }
                return rc;
            }
            let values = decode_values();
            return bail_to_interpreter(vm, thread, info, &values);
        }

        mic_prof::bump(&mic_prof::MIC_HIT_NOENTRY);
        // Entry not cached yet — use cached class name for fast dispatch.
        //
        // KC26 array.clone() bug: array receivers store the COMPONENT class
        // id in their header (per the documented invariant in
        // `runtime/interpreter.rs`). Falling through to
        // `get_class(receiver_class_id).name` would resolve dispatch on the
        // component (e.g. `OptionCategory`/`Enum`) and surface
        // `Enum.clone() → CloneNotSupportedException` for every array clone
        // of an enum type. Per JVMS §4.4.1, array classes inherit their
        // method table from `Object`; short-circuit accordingly.
        let class_name: std::sync::Arc<str> = if vm.heap.kind_of(receiver_ref)
            == cratonvm_types::ObjectKind::Array
        {
            std::sync::Arc::from("java/lang/Object")
        } else {
            let guard = mic.cached_class_name.lock();
            match &*guard {
                Some(name) => name.clone(),
                None => {
                    drop(guard);
                    let cm = vm.class_manager.read();
                    cm.get_class(receiver_class_id)
                        .map(|c| c.name.clone())
                        // Same fallback as the miss path below: never
                        // dispatch on an empty class name.
                        .unwrap_or_else(|| std::sync::Arc::from(info.class_name))
                }
            }
        };

        // `decode_values` yields exactly `[receiver, args...]` — the full
        // argument vector `invoke_or_native` expects. (This path previously
        // rebuilt it twice via `values[1..].to_vec()` + a fresh prepend.)
        let full_args = decode_values();

        // Try to compile callee for next time (populate cached_entry_ptr + needs_ctx).
        //
        // VIRTUAL DISPATCH FIX: resolve the callee from the RECEIVER's class
        // (`class_name`, derived from `receiver_class_id` above), NOT from
        // `info.class_name` (the *static* call-site type). When the receiver
        // overrides a concrete superclass method — e.g. a `Long` reached
        // through an `Object`-typed `equals(Object)Z` call site — the static
        // type resolves to `Object.equals` (identity `==`) instead of
        // `Long.equals` (value comparison). The MIC then cached that identity
        // entry against the receiver's class id and invoked it on every
        // monomorphic hit, so two distinct but equal-valued boxes compared
        // unequal (bc-java InterleaveTest, junit assertEquals(Object,Object)).
        // `find_method_recursive` (inside `try_jit_compile_callee`) walks up
        // from the receiver class to the real override.
        let compile_res = {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
            crate::runtime::interpreter::try_jit_compile_callee(
                vm,
                &class_name,
                info.method_name,
                info.descriptor,
            )
        };
        // BUG-H: as in the cache-miss branch below, do not publish a direct
        // compiled entry for a callee with a local exception table — the inline
        // machine-code cascade would bypass it. Keep dispatch on the
        // `invoke_or_native` path so the exception routes through the callee's
        // own table.
        if let Some((entry_ptr, needs_ctx)) = compile_res {
            if !mic_callee_has_exception_table(vm, receiver_class_id, info) {
                mic.cached_entry_ptr
                    .store(entry_ptr as u64, std::sync::atomic::Ordering::Release);
                mic.cached_needs_context
                    .store(needs_ctx, std::sync::atomic::Ordering::Release);
                // CRIT-1 — also populate the co-allocated PIC so the
                // inline 3-way cascade in `jit/src/x64.rs` hits on the
                // next invocation. Without this the cascade's empty
                // (class_id == 0) slots always fail and every dispatch
                // pays the full helper cost. We only install when we
                // actually have an entry_ptr to publish; a 0 entry_ptr
                // in a PIC slot would force the inline cascade to call
                // through a null function pointer.
                if pic_ptr != 0 && entry_ptr != 0 {
                    let pic = &*(pic_ptr as *const JitPICSlot);
                    pic.install(receiver_cid, &class_name, entry_ptr as u64, needs_ctx);
                }
            }
        }

        let invoke_res = {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
            crate::vm::invoke_or_native(
                vm,
                thread,
                &class_name,
                info.method_name,
                info.descriptor,
                &full_args,
            )
        };
        // S111r12 — JIT MIC fast-path rescue: same CP-class fallback
        // as the cache-miss branch below (see comment there).
        //
        // Round-fix (Jetty): a thrown exception from the MIC dispatch must
        // be routed through `handle_jit_dispatch_error` (stash + return the
        // `i64::MIN` deopt sentinel) — the old code merely logged it and
        // returned 0, silently swallowing the exception and letting the JIT
        // caller run on with a bogus value.
        let result = match invoke_res {
            Ok(v) => v,
            Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Linkage(
                    crate::error::LinkageError::NoSuchMethodError { .. },
                ),
            )) if !info.class_name.is_empty()
                && &*class_name != info.class_name =>
            {
                match crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &full_args,
                ) {
                    Ok(v) => v,
                    Err(e2) => {
                        return handle_jit_dispatch_error(vm, thread, e2, info);
                    }
                }
            }
            Err(e) => {
                return handle_jit_dispatch_error(vm, thread, e, info);
            }
        };

        return match result {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
            Some(Value::Object(None)) | None => 0,
            _ => 0,
        };
    }

    // --- Cache miss: full resolution + update cache ---
    mic.record_miss();

    // See the matching block in the cache-hit branch above for the rationale
    // — array receivers must dispatch through `java/lang/Object` rather than
    // their component class id, otherwise enum-array `clone()` resolves to
    // `Enum.clone()` (a JDK-deliberate CNSE thrower).
    let class_name: std::sync::Arc<str> = if vm.heap.kind_of(receiver_ref)
        == cratonvm_types::ObjectKind::Array
    {
        std::sync::Arc::from("java/lang/Object")
    } else {
        let cm = vm.class_manager.read();
        cm.get_class(receiver_class_id)
            .map(|c| c.name.clone())
            // Receiver class id not in the class store (synthetic alloc) —
            // dispatching on "" would raise a message-less
            // NoClassDefFoundError; the CP call-site class is the
            // spec-correct resolution target.
            .unwrap_or_else(|| std::sync::Arc::from(info.class_name))
    };

    mic_prof::bump(&mic_prof::MIC_MISS);
    // Try to compile callee for cached entry. Resolve by the RECEIVER's class
    // (`class_name`), not the static `info.class_name` — see the matching
    // VIRTUAL DISPATCH FIX in the cache-hit branch above. `class_name` here is
    // an `Arc<str>`; deref to `&str` for the resolver.
    let compile_res = {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
        crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            &class_name,
            info.method_name,
            info.descriptor,
        )
    };
    let (entry_ptr, needs_ctx) = match compile_res {
        Some((ptr, nc)) => (ptr as u64, nc),
        None => (0, false),
    };

    // BUG-H: never publish a direct compiled entry for a callee that declares a
    // local exception table. The inline machine-code MIC/PIC cascade emitted in
    // `jit/src/x64.rs` would `CALL` it directly, bypassing the callee's own
    // exception table — so an implicit AIOOBE/NPE the callee should catch
    // locally escapes its `catch` (Tomcat `HttpParser.isNotRequestTarget
    // Relaxed`, an *instance* method: `IS_NOT_REQUEST_TARGET[c]` inside
    // `catch (AIOOBE)`). Leaving the cache empty keeps every dispatch on the
    // helper's `invoke_or_native` path below, which routes the exception
    // through the callee's table correctly. (The statically-bound sibling is
    // gated in the `callee_compiler` closure in `interpreter.rs`.)
    if !mic_callee_has_exception_table(vm, receiver_class_id, info) {
        // Update all MIC fields atomically (needs_ctx must match compiled entry ABI)
        mic.update(receiver_cid, &class_name, entry_ptr, needs_ctx);

        // CRIT-1 — Populate the co-allocated PIC so the inline 3-way
        // cascade emitted in `jit/src/x64.rs` actually hits on subsequent
        // dispatches. Eager allocation made `pic_inline` always-true at
        // codegen, so the cascade is always emitted but stays cold until
        // the helper publishes entries here. Mirror the MIC update with
        // a `pic.install(...)` so the next call with the same receiver
        // class takes the inline fast path (5 cycles slot-0 hit vs the
        // full helper call). LFU eviction inside `install` handles
        // megamorphic spillover automatically.
        if pic_ptr != 0 && entry_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            pic.install(receiver_cid, &class_name, entry_ptr, needs_ctx);
        }
    }

    // See the matching note in the cache-hit branch — `decode_values` already
    // yields the `[receiver, args...]` vector `invoke_or_native` expects.
    let full_args = decode_values();

    let invoke_res = {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
        crate::vm::invoke_or_native(
            vm,
            thread,
            &class_name,
            info.method_name,
            info.descriptor,
            &full_args,
        )
    };
    // S111r12 — JIT MIC virtual-dispatch rescue. When the receiver's
    // runtime class (e.g. `java/lang/Comparable` for a malformed
    // ClassLoader instance) does not declare the CP-resolved method,
    // `invoke_or_native` raises `NoSuchMethodError`. The CP method-ref
    // class carried in `info.class_name` (e.g. `java/lang/ClassLoader`)
    // is the spec-correct resolution target — retry through it.
    // Mirrors the S111r10 receiver-walk fallback for invokeinterface
    // and the S111r8 cid=0 → CP-class fallback in `execute_invoke`.
    let result = match invoke_res {
        Ok(v) => v,
        Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Linkage(
                crate::error::LinkageError::NoSuchMethodError { .. },
            ),
        )) if !info.class_name.is_empty()
            && &*class_name != info.class_name =>
        {
            match crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &full_args,
            ) {
                Ok(v) => v,
                Err(e2) => {
                    return handle_jit_dispatch_error(vm, thread, e2, info);
                }
            }
        }
        // Round-fix (Jetty): route a thrown exception through
        // `handle_jit_dispatch_error` (stash + return the `i64::MIN` deopt
        // sentinel) rather than logging and returning a bogus 0.
        Err(e) => {
            return handle_jit_dispatch_error(vm, thread, e, info);
        }
    };

    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Uncommon Trap / Deoptimization
// ---------------------------------------------------------------------------

/// Deopt reason codes passed from JIT-compiled code.
/// These map to `cratonvm_jit::deopt::DeoptReason` variants.
pub const DEOPT_REASON_NULL_CHECK: i64 = 0;
pub const DEOPT_REASON_CLASS_CHECK: i64 = 1;
pub const DEOPT_REASON_BOUNDS_CHECK: i64 = 2;
pub const DEOPT_REASON_DIV_BY_ZERO: i64 = 3;
pub const DEOPT_REASON_RECEIVER_TYPE_CHANGED: i64 = 4;
pub const DEOPT_REASON_CLASS_LOADING: i64 = 5;
pub const DEOPT_REASON_UNCOMMON_TRAP: i64 = 6;
pub const DEOPT_REASON_SPECULATION_FAILED: i64 = 7;
pub const DEOPT_REASON_UNREACHED_CODE: i64 = 8;

/// Deopt action codes returned from `jit_uncommon_trap`.
pub const DEOPT_ACTION_REINTERPRET: i64 = 0;
pub const DEOPT_ACTION_RECOMPILE: i64 = 1;
pub const DEOPT_ACTION_BLACKLIST: i64 = 2;

pub fn reason_code_to_deopt_reason(code: i64) -> cratonvm_jit::deopt::DeoptReason {
    match code {
        DEOPT_REASON_NULL_CHECK => cratonvm_jit::deopt::DeoptReason::NullCheck,
        DEOPT_REASON_CLASS_CHECK => cratonvm_jit::deopt::DeoptReason::ClassCheck,
        DEOPT_REASON_BOUNDS_CHECK => cratonvm_jit::deopt::DeoptReason::BoundsCheck,
        DEOPT_REASON_DIV_BY_ZERO => cratonvm_jit::deopt::DeoptReason::DivByZero,
        DEOPT_REASON_RECEIVER_TYPE_CHANGED => cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged,
        DEOPT_REASON_CLASS_LOADING => cratonvm_jit::deopt::DeoptReason::ClassLoading,
        DEOPT_REASON_UNCOMMON_TRAP => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        DEOPT_REASON_SPECULATION_FAILED => cratonvm_jit::deopt::DeoptReason::SpeculationFailed,
        DEOPT_REASON_UNREACHED_CODE => cratonvm_jit::deopt::DeoptReason::UnreachedCode,
        _ => cratonvm_jit::deopt::DeoptReason::UncommonTrap,
    }
}

/// Orchestrates deoptimization: records the event, invalidates compiled code,
/// and queues recompilation based on the deopt log's recommended action.
pub struct DeoptimizationController;

impl DeoptimizationController {
    /// Execute a full deoptimization cycle for a method.
    ///
    /// 1. Record the deopt event in the deopt log
    /// 2. Invalidate the compiled method in the JIT cache
    /// 3. Notify the tiered compilation manager
    /// 4. If receiver type changed, invalidate via the invalidation manager
    /// 5. Return the recommended action
    pub fn deoptimize(
        vm: &SharedVm,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        reason: cratonvm_jit::deopt::DeoptReason,
        bci: u32,
    ) -> cratonvm_jit::deopt::DeoptAction {
        // Build method key for deopt log
        let method_key = format!("{}.{}:{}", class_name, method_name, descriptor);

        // Create the deopt event
        let event = cratonvm_jit::deopt::DeoptEvent {
            reason,
            action: cratonvm_jit::deopt::DeoptAction::Reinterpret, // initial; may be overridden
            bci,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            speculation_id: 0,
        };

        // Record in deopt log and get recommended action
        let tiered_key = cratonvm_jit::tiered::MethodKey {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
        };
        let action = vm.record_deoptimization(&method_key, event, &tiered_key);

        if std::env::var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!(
                "[cratonvm-deopt] {} reason={:?} bci={} action={:?}",
                method_key, reason, bci, action
            );
        }

        // Invalidate the compiled method from the JIT cache
        {
            let mut jit_cache = vm.jit_cache.write();
            jit_cache.remove(class_name, method_name, descriptor);
        }

        // For class-check or receiver-type failures, also check the
        // invalidation manager for dependent methods.
        if matches!(
            reason,
            cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
                | cratonvm_jit::deopt::DeoptReason::ClassCheck
                | cratonvm_jit::deopt::DeoptReason::ClassLoading
        ) {
            let mut inv_mgr = vm.invalidation_manager.lock();
            // Clear stale assumptions for the deoptimized method
            inv_mgr.clear_assumptions(&method_key);
        }

        // If the deopt log recommends giving up, add to the JIT skip set
        if action == cratonvm_jit::deopt::DeoptAction::MakeNotCompilable {
            let mut skip = vm.jit_skip_set.write();
            skip.insert((
                class_name.into(),
                method_name.into(),
                descriptor.into(),
            ));
        }

        tracing::debug!(
            "deopt: {} reason={:?} bci={} action={:?}",
            method_key, reason, bci, action
        );

        // Emit JFR deoptimization event
        // Round-4: emit_deoptimization_event takes `&'static str` for reason
        // and action — both are bounded enums, so map to static literals
        // rather than `format!("{:?}", ...)`-allocating per deopt.
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let reason_static: &'static str = match reason {
                cratonvm_jit::deopt::DeoptReason::NullCheck => "NullCheck",
                cratonvm_jit::deopt::DeoptReason::ClassCheck => "ClassCheck",
                cratonvm_jit::deopt::DeoptReason::BoundsCheck => "BoundsCheck",
                cratonvm_jit::deopt::DeoptReason::DivByZero => "DivByZero",
                cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged => "ReceiverTypeChanged",
                cratonvm_jit::deopt::DeoptReason::ClassLoading => "ClassLoading",
                cratonvm_jit::deopt::DeoptReason::UninitializedAccess => "UninitializedAccess",
                cratonvm_jit::deopt::DeoptReason::TransferToInterpreter => "TransferToInterpreter",
                cratonvm_jit::deopt::DeoptReason::UncommonTrap => "UncommonTrap",
                cratonvm_jit::deopt::DeoptReason::SpeculationFailed => "SpeculationFailed",
                cratonvm_jit::deopt::DeoptReason::NotCompiled => "NotCompiled",
                cratonvm_jit::deopt::DeoptReason::UnreachedCode => "UnreachedCode",
            };
            let action_static: &'static str = match action {
                cratonvm_jit::deopt::DeoptAction::Reinterpret => "Reinterpret",
                cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => "RecompileAndReinterpret",
                cratonvm_jit::deopt::DeoptAction::MakeNotEntrant => "MakeNotEntrant",
                cratonvm_jit::deopt::DeoptAction::MakeNotCompilable => "MakeNotCompilable",
            };
            let mut jfr = vm.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_deoptimization_event(
                &mut jfr,
                &method_key,
                0, // compile_id
                reason_static,
                action_static,
                bci as i32,
                // Round-5 MED-fix (2026-05-17): plumb the real JFR thread id
                // so JMC can attribute the deopt to the thread that triggered
                // it. `current_jfr_thread_id()` is TLS-cached, allocates once
                // per thread, and steady-state cost is a TLS read + branch.
                cratonvm_jfr::builtin::current_jfr_thread_id(),
                now_ns,
            );
        }

        action
    }
}

/// JIT runtime helper: called from compiled code when a speculative
/// optimization fails (uncommon trap).
///
/// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
///
/// The reason parameter encodes a `DeoptReason` variant as an integer.
/// Returns a deopt action code:
///   0 = reinterpret (continue in interpreter)
///   1 = recompile (invalidate and recompile with updated profile)
///   2 = blacklist (never compile again)
///
/// After this returns, the JIT code should return control to the interpreter.
/// The calling convention is that the JIT method returns a sentinel value
/// (i64::MIN) to signal "deoptimized, resume in interpreter".
// SAFETY: Called from JIT-compiled code when a speculative optimization fails.
// vm_ptr must be 0 or a valid SharedVm pointer. reason encodes a DeoptReason variant.
// bci is the bytecode index of the failing instruction. Accesses the JIT thread pointer
// (via jit_thread_mut) and the deoptimization controller to invalidate compiled code.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_uncommon_trap(
    vm_ptr: i64,
    reason: i64,
    bci: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 {
        return DEOPT_ACTION_REINTERPRET;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let deopt_reason = reason_code_to_deopt_reason(reason);

    // Try to determine the method being executed from the JIT thread context.
    // If we can't determine the method, we still record the deopt but with a
    // generic key.
    let (class_name, method_name, descriptor) = {
        // The thread's current frame has the method info
        let default = ("unknown".to_string(), "unknown".to_string(), "()V".to_string());
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(frame) = thread.frames.last() {
                (
                    frame.class_name().to_string(),
                    frame.method_name().to_string(),
                    frame.method_descriptor().to_string(),
                )
            } else {
                default
            }
        } else {
            default
        }
    };

    let action = DeoptimizationController::deoptimize(
        vm,
        &class_name,
        &method_name,
        &descriptor,
        deopt_reason,
        bci as u32,
    );

    match action {
        cratonvm_jit::deopt::DeoptAction::Reinterpret => DEOPT_ACTION_REINTERPRET,
        cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => DEOPT_ACTION_RECOMPILE,
        cratonvm_jit::deopt::DeoptAction::MakeNotEntrant => DEOPT_ACTION_RECOMPILE,
        cratonvm_jit::deopt::DeoptAction::MakeNotCompilable => DEOPT_ACTION_BLACKLIST,
    }
}

// ---------------------------------------------------------------------------
// Helper table construction
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_checkcast_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_checkcast(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_instanceof(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: a typecheck site with a missing class-name pointer
    /// is a JIT-side bug (typecheck_info should always be populated by the
    /// scanner). The safe behavior is to *fail closed* — return 0 from
    /// checkcast, which the JIT-compiled code interprets as a failed cast and
    /// surfaces as a deterministic ClassCastException through the interpreter
    /// fallback. The old behavior silently let the cast through, masking the
    /// underlying scan/compile mismatch.
    #[test]
    fn jit_checkcast_negative_len_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_negative_len_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_len is negative, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 42, "test".as_ptr(), -1) };
        assert_eq!(result, 0);
    }

    /// NEW-1.2 regression: same fail-closed semantics for a null class-name
    /// pointer with a positive length.
    #[test]
    fn jit_checkcast_null_class_name_ptr_fails_closed() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_checkcast(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_instanceof_null_class_name_ptr_returns_zero() {
        // SAFETY: obj_ptr is 0 (null) and class_name_ptr is null, so no pointer
        // dereferences occur; the function returns early on both guards.
        let result = unsafe { jit_instanceof(0, 99, std::ptr::null(), 5) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_baload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §baload: NPE on null array. Like iaload/aaload, the helper returns
        // the i64::MIN deopt sentinel and sets the pending-NPE flag (the old
        // "return 0" silently fabricated a zero byte and masked real null derefs).
        let _ = take_jit_pending_npe(); // clear any prior state
        let result = unsafe { jit_baload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe(), "baload(null) must set pending NPE flag");
    }

    #[test]
    fn jit_iaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §iaload: NPE on null array. Helper returns the i64::MIN deopt sentinel
        // and sets the pending-NPE flag for the interpreter to consume.
        let _ = take_jit_pending_npe(); // clear any prior state
        let result = unsafe { jit_iaload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe(), "iaload(null) must set pending NPE flag");
    }

    #[test]
    fn jit_aaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §aaload: NPE on null array.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_aaload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe(), "aaload(null) must set pending NPE flag");
    }

    #[test]
    fn jit_arraylength_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §arraylength: NPE on null array. Previously returned -1, which
        // silently corrupted any downstream length-comparison or loop-bound use.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_arraylength(0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe(), "arraylength(null) must set pending NPE flag");
    }

    /// Round-8 CRIT fix: store helpers (`jit_iastore` / `jit_bastore` /
    /// `jit_aastore`) on a null array previously called
    /// `std::process::abort()` with a comment claiming the helper was
    /// unreachable; in reality the helpers were registered in
    /// `JitRuntimeHelpers` and reachable. They now set the pending-NPE
    /// flag (drained on every JIT return by the interpreter) so the NPE
    /// surfaces at the right method.
    #[test]
    fn jit_iastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences.
        unsafe { jit_iastore(0, 0, 0) };
        assert!(take_jit_pending_npe(), "iastore(null) must set pending NPE flag");
    }

    #[test]
    fn jit_bastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences.
        unsafe { jit_bastore(0, 0, 0) };
        assert!(take_jit_pending_npe(), "bastore(null) must set pending NPE flag");
    }

    #[test]
    fn jit_aastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences either array_ptr or
        // vm_ptr / val (the null path returns before touching them).
        unsafe { jit_aastore(0, 0, 0, 0) };
        assert!(take_jit_pending_npe(), "aastore(null) must set pending NPE flag");
    }

    // ----------------------------------------------------------------------
    // B1 fix (review `vm-runtime.md`): the array load/store helpers used to
    // silently swallow an out-of-bounds index (load returned a fabricated 0 /
    // null, store dropped the write) instead of raising AIOOBE, diverging from
    // JVMS and masking real bugs. They now mirror `jit_throw_aioobe`: set the
    // pending-AIOOBE payload + return the `i64::MIN` deopt sentinel (loads) /
    // set the flag and return (void stores). The fast in-bounds path is
    // unchanged — verified by the round-trip assertions below.
    // ----------------------------------------------------------------------

    /// Allocate a small real heap and a single array on it. Returns the owning
    /// `SharedVm` box (kept alive by the caller) and the raw array pointer the
    /// JIT helpers consume.
    fn alloc_test_array(et: ArrayElementType, len: usize) -> (Box<crate::vm::SharedVm>, i64) {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let mut config = VmConfig::default();
        config.max_heap_size = 4 * 1024 * 1024; // 4 MB — plenty for one tiny array
        config.initial_heap_size = 4 * 1024 * 1024;
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(config));
        let arr = vm_box.heap.alloc_array(ClassId::new(0), et, len);
        let arr_ptr = arr.as_ptr() as i64;
        (vm_box, arr_ptr)
    }

    #[test]
    fn jit_iaload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe(); // clear any prior state
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4] on the test heap; index 4 is out of
        // bounds so the helper takes the bounds-check arm and never dereferences
        // an element. index -1 is likewise rejected before any element read.
        let hi = unsafe { jit_iaload(arr_ptr, 4) };
        assert_eq!(hi, i64::MIN, "iaload OOB-high must return the deopt sentinel");
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((4, 4)),
            "iaload OOB-high must set pending AIOOBE (index, length)"
        );
        let lo = unsafe { jit_iaload(arr_ptr, -1) };
        assert_eq!(lo, i64::MIN, "iaload OOB-low must return the deopt sentinel");
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((-1, 4)),
            "iaload OOB-low (negative index) must set pending AIOOBE"
        );
    }

    #[test]
    fn jit_iastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4]; index 7 is OOB so the store is not
        // performed and no element pointer is dereferenced.
        unsafe { jit_iastore(arr_ptr, 7, 0x1234) };
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((7, 4)),
            "iastore OOB must set pending AIOOBE and not write past the array"
        );
    }

    #[test]
    fn jit_baload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Byte, 2);
        // SAFETY: arr_ptr is a live byte[2]; index 2 is OOB.
        let r = unsafe { jit_baload(arr_ptr, 2) };
        assert_eq!(r, i64::MIN, "baload OOB must return the deopt sentinel");
        assert_eq!(take_jit_pending_aioobe(), Some((2, 2)));
    }

    #[test]
    fn jit_bastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Byte, 2);
        // SAFETY: arr_ptr is a live byte[2]; index 5 is OOB so no write occurs.
        unsafe { jit_bastore(arr_ptr, 5, 0xFF) };
        assert_eq!(take_jit_pending_aioobe(), Some((5, 2)));
    }

    #[test]
    fn jit_aaload_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 3);
        // SAFETY: arr_ptr is a live Object[3]; index 3 is OOB.
        let r = unsafe { jit_aaload(arr_ptr, 3) };
        assert_eq!(r, i64::MIN, "aaload OOB must return the deopt sentinel");
        assert_eq!(take_jit_pending_aioobe(), Some((3, 3)));
    }

    #[test]
    fn jit_aastore_oob_sets_pending_aioobe() {
        let _ = take_jit_pending_aioobe();
        let (vm, arr_ptr) = alloc_test_array(ArrayElementType::Reference, 3);
        let vm_ptr = &*vm as *const crate::vm::SharedVm as i64;
        // SAFETY: arr_ptr is a live Object[3]; index 9 is OOB so the helper
        // returns BEFORE the SATB barrier / element write and never touches
        // vm_ptr or val. vm_ptr is a live SharedVm regardless.
        unsafe { jit_aastore(vm_ptr, arr_ptr, 9, 0) };
        assert_eq!(take_jit_pending_aioobe(), Some((9, 3)));
    }

    #[test]
    fn jit_int_array_in_bounds_roundtrip_unchanged() {
        // The fast in-bounds path must be untouched by the B1 fix: a value
        // stored at a valid index reads back identically, and no AIOOBE flag
        // is left pending.
        let _ = take_jit_pending_aioobe();
        let (_vm, arr_ptr) = alloc_test_array(ArrayElementType::Int, 4);
        // SAFETY: arr_ptr is a live int[4]; indices 0..4 are all in bounds.
        unsafe {
            jit_iastore(arr_ptr, 0, 11);
            jit_iastore(arr_ptr, 3, -7);
        }
        assert!(
            take_jit_pending_aioobe().is_none(),
            "in-bounds stores must NOT set the AIOOBE flag"
        );
        // SAFETY: in-bounds loads.
        let a = unsafe { jit_iaload(arr_ptr, 0) };
        let b = unsafe { jit_iaload(arr_ptr, 3) };
        assert_eq!(a, 11, "in-bounds iaload(0) must read back the stored value");
        assert_eq!(b, -7, "in-bounds iaload(3) must read back the stored value");
        assert!(
            take_jit_pending_aioobe().is_none(),
            "in-bounds loads must NOT set the AIOOBE flag"
        );
    }

    #[test]
    fn jit_getfield_null_sets_pending_npe() {
        // B2 fix (review `vm-runtime.md`): JVMS §getfield throws NPE on a null
        // receiver. The helper used to return 0, silently fabricating a
        // zero/null field value (same masked-null-deref class as the array-load
        // helpers). It now sets the pending-NPE flag and returns the i64::MIN
        // deopt sentinel, mirroring `jit_arraylength`.
        let _ = take_jit_pending_npe(); // clear any prior state
        // SAFETY: obj_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_getfield(0, 0) };
        assert_eq!(result, i64::MIN, "getfield(null) must return the deopt sentinel");
        assert!(
            take_jit_pending_npe(),
            "getfield(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_getfield_oob_slot_does_not_read_past_object() {
        // B2 fix (review `vm-runtime.md`): a stale/miscompiled `field_index`
        // must NOT read past the object into the neighbouring heap object
        // (info leak / follow-on UAF). The helper now bounds-checks the slot
        // against the receiver's `num_slots` header field — exactly like the
        // symmetric `jit_putfield_slot_in_bounds` guard on the putfield
        // helpers — and returns 0 (mirroring the interpreter's out-of-range
        // `get_field` default) instead of dereferencing.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        // Object with exactly 2 reference fields (num_slots == 2).
        let obj = vm_box.heap.alloc_object(ClassId::new(0), 2);
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot indices 2 and 5 are
        // out of range so the helper takes the bounds-check arm and never
        // dereferences past the object. A negative index is likewise rejected.
        let oob_hi = unsafe { jit_getfield(obj_ptr, 2) };
        assert_eq!(oob_hi, 0, "getfield on an out-of-range slot must not read OOB");
        let oob_far = unsafe { jit_getfield(obj_ptr, 5) };
        assert_eq!(oob_far, 0, "getfield far past num_slots must not read OOB");
        let oob_neg = unsafe { jit_getfield(obj_ptr, -1) };
        assert_eq!(oob_neg, 0, "getfield on a negative slot must not read OOB");
        assert!(
            !take_jit_pending_npe(),
            "an in-range receiver with an OOB slot must not raise NPE"
        );
    }

    #[test]
    fn jit_getfield_in_bounds_reads_stored_int() {
        // The fast in-bounds path must be untouched by the B2 fix: a value
        // written into a valid slot reads back identically.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let obj = vm_box.heap.alloc_object(ClassId::new(0), 2);
        // Write via the interpreter path (the helper read must observe it).
        vm_box.heap.set_field(obj, 1, Value::Int(0x5A5A));
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot 1 is in bounds.
        let v = unsafe { jit_getfield(obj_ptr, 1) };
        assert_eq!(v, 0x5A5A, "in-bounds getfield must read back the stored value");
        assert!(
            !take_jit_pending_npe(),
            "an in-bounds getfield must not raise NPE"
        );
    }

    #[test]
    fn jit_thread_cleared_returns_none() {
        clear_jit_thread();
        // SAFETY: The JIT thread pointer was just cleared above, so jit_thread_mut
        // returns None without dereferencing any pointer.
        let result = unsafe { jit_thread_mut() };
        assert!(result.is_none());
    }

    // Regression: when the JIT JIT passes a NaN-boxed CompactValue raw bit
    // pattern as the array length (e.g. `0xFFFC_0000_0000_000B` for int(11)),
    // the helper must extract the low-32 int payload and NOT treat the upper
    // tag bits as part of the length. Previously, `length as usize` cast the
    // tag bits into an enormous unsigned value, triggering
    // "array data size overflow in gen_heap alloc_array" / "young gen exhausted".
    //
    // Repros: `bench/fannkuch` (n=11 → 0xFFFC_..._000B) and
    // `bench/FullStackBench` phase 5 (`new boolean[100000]`).
    #[test]
    fn jit_newarray_strips_nanbox_tag_from_length() {
        // 0xFFFC_0000_0000_000B is the NaN-boxed CompactValue for int(11)
        // (NANBOX_BITS | SUB_INT << 47 | 11). After narrowing to i32, the
        // value should be 11 — non-negative, so the helper takes the
        // early `vm_ptr == 0` exit rather than the abort path.
        let nan_boxed_11: i64 = 0xFFFC_0000_0000_000B_u64 as i64;
        // SAFETY: vm_ptr=0 hits the explicit null check after length narrowing,
        // so no dereference occurs.
        let result = unsafe { jit_newarray(0, 10 /* T_INT */, nan_boxed_11) };
        assert_eq!(result, 0, "jit_newarray must not abort on NaN-boxed length");
    }

    #[test]
    fn jit_anewarray_strips_nanbox_tag_from_length() {
        let nan_boxed_11: i64 = 0xFFFC_0000_0000_000B_u64 as i64;
        // SAFETY: vm_ptr=0 hits the explicit null check after length narrowing.
        let result = unsafe { jit_anewarray_object(0, 0, nan_boxed_11) };
        assert_eq!(result, 0, "jit_anewarray_object must not abort on NaN-boxed length");
    }

    #[test]
    fn jit_newarray_negative_length_returns_zero_not_abort() {
        // Sign-extended -5 (0xFFFF_FFFF_FFFF_FFFB). After narrowing to i32,
        // value is -5; the helper must return 0 (would-be NegativeArraySize)
        // instead of casting to a huge usize and aborting.
        let neg_5: i64 = -5;
        // SAFETY: negative-length path returns 0 before any dereference.
        let result = unsafe { jit_newarray(0, 10, neg_5) };
        assert_eq!(result, 0);
        // SAFETY: negative-length path returns 0 before any dereference.
        let result2 = unsafe { jit_anewarray_object(0, 0, neg_5) };
        assert_eq!(result2, 0);
    }

    // -----------------------------------------------------------------
    // Task #43 (HIGH soundness): SATB pre-barrier + real-STW newarray
    // -----------------------------------------------------------------
    //
    // The two regression tests below pin the acceptance criteria from
    // task #43 (deferred from #25/#26):
    //
    //   (1) `jit_putfield_object` records the OLD reference in the SATB
    //       queue *before* overwriting the slot. Without this, concurrent
    //       marking loses any still-live ref reachable only through the
    //       overwritten slot, turning the next mixed evacuation into a
    //       use-after-free.
    //
    //   (2) `jit_newarray` under a low-heap-pressure / try_alloc_young_probe
    //       failure correctly drives a GC through the orchestrated STW
    //       path (`maybe_gc_forced_pub`) and successfully completes the
    //       follow-up `alloc_array` call without crashing. Previously the
    //       helper called `heap.collect_garbage` directly, bypassing the
    //       `gc_barrier.request_stw()` handshake — a multi-threaded UAF.

    /// Task #43 acceptance #3 (JIT-compiled putfield ref-store with
    /// non-null old ref correctly enqueues `old` in the SATB log).
    ///
    /// Build a SharedVm, install + activate the SATB queue, allocate a
    /// container object plus two payload objects, write the first
    /// payload into slot 0, then drive `jit_putfield_object` to
    /// overwrite slot 0 with the second payload. The first payload's
    /// raw address must land in the SATB queue after we drain the
    /// per-thread buffer.
    #[test]
    fn jit_putfield_object_satb_pre_barrier_enqueues_old_ref() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use cratonvm_gc::{ConcurrentGcState, SatbQueue};
        use std::sync::Arc;

        // Build the SharedVm via Box so we can take a `&mut` to enable
        // concurrent GC before sharing it. The JIT helper only requires
        // a raw `*const SharedVm` pointer, so no Arc is needed.
        let mut vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));

        // Wire up the SATB queue + concurrent GC state on the heap. The
        // generational backend's `satb_barrier` is a hard no-op until
        // both the queue and the state are present AND the state reports
        // marking active (ConcurrentMark or Remark phase).
        let satb: Arc<SatbQueue> = Arc::new(SatbQueue::new());
        let state: Arc<ConcurrentGcState> = Arc::new(ConcurrentGcState::new());
        vm_box.heap.enable_concurrent_gc(satb.clone(), state.clone());

        // Activate marking. Both `satb.activate()` (so `is_active()`
        // returns true) and `state.set_phase(ConcurrentMark)` (so
        // `is_marking_active()` returns true) are required by the
        // generational `satb_barrier` fast-path gate.
        satb.activate();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);

        // Allocate a container with one reference field plus two payload
        // objects to use as old/new references for the putfield store.
        let container = vm_box.heap.alloc_object(ClassId::new(0), 1);
        let old_obj = vm_box.heap.alloc_object(ClassId::new(0), 0);
        let new_obj = vm_box.heap.alloc_object(ClassId::new(0), 0);

        // Pre-write the old reference into slot 0 (interpreter path —
        // bypasses the SATB barrier we are about to test).
        vm_box
            .heap
            .set_field(container, 0, Value::Object(Some(old_obj)));

        // Pre-drain any baggage from this thread's local SATB buffer so
        // the test only observes references logged by the JIT helper.
        vm_box.heap.flush_thread_satb();
        let _ = satb.drain();

        let vm_ptr = &*vm_box as *const SharedVm as i64;
        let container_ptr = container.as_ptr() as i64;
        let new_obj_ptr = new_obj.as_ptr() as i64;

        // SAFETY: `vm_ptr` points to a live `SharedVm` (the Box we own);
        // `container_ptr` and `new_obj_ptr` are live heap objects; slot
        // 0 is within the container's declared layout (num_fields=1).
        unsafe {
            jit_putfield_object(vm_ptr, container_ptr, 0, new_obj_ptr);
        }

        // Flush this thread's SATB buffer into the global queue so the
        // drain below sees it. The per-thread buffer auto-flushes at
        // 256 entries; with a single store we must drain explicitly.
        vm_box.heap.flush_thread_satb();
        let drained = satb.drain();

        // The SATB pre-barrier must have logged the OLD reference's
        // raw address (NOT the new ref's address). Searching is robust
        // against unrelated heap activity inside `alloc_object` that
        // might happen to log; the precise acceptance check is
        // "old is present".
        let old_addr = old_obj.as_ptr() as usize;
        assert!(
            drained.contains(&old_addr),
            "jit_putfield_object must SATB-log the OLD ref before overwriting; \
             drained={:?} expected_to_contain={:#x}",
            drained,
            old_addr,
        );

        // The new value must be visible in the slot post-store (sanity
        // check that the helper actually performed the write).
        let post = vm_box.heap.get_field(container, 0);
        match post {
            Value::Object(Some(obj)) => assert_eq!(
                obj.as_ptr() as usize, new_obj.as_ptr() as usize,
                "post-store slot must hold the new ref",
            ),
            other => panic!("expected Object(Some) post-store, got {:?}", other),
        }

        // Clean up: deactivate SATB so the box's Drop path doesn't
        // race a marker (none is running in this test, but tidy state
        // is a habit worth keeping).
        let _ = satb.deactivate_and_drain();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
    }

    /// Task #43 acceptance #3 (jit_newarray under low heap pressure
    /// correctly triggers GC and re-attempts allocation).
    ///
    /// Drive `try_alloc_young_probe` into the failure arm by requesting
    /// a length that exceeds the young-gen capacity, then verify the
    /// helper does not crash and ultimately returns a non-zero pointer
    /// (the post-GC `alloc_array` succeeds because the heap can grow
    /// or because the requested length still fits after collection).
    ///
    /// The critical bit being tested is that the GC path goes through
    /// the orchestrated STW handshake (`maybe_gc_forced_pub`) — the
    /// previous direct `heap.collect_garbage` call would deadlock or
    /// UAF when multiple threads were active. Running this test under
    /// `--test-threads=2` exercises that handshake.
    #[test]
    fn jit_newarray_under_pressure_drives_real_stw_gc() {
        use crate::config::VmConfig;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use crate::vm::SharedVm;

        // Shrink the heap to a single-digit-MB size so the burn loop
        // below realistically pushes the young gen near exhaustion and
        // forces `try_alloc_young_probe` into the failure arm. Without
        // this, the default 256 MB heap would let the helper hit the
        // probe-success fast path on every iteration, never exercising
        // the orchestrated-STW code path under test.
        let mut config = VmConfig::default();
        config.max_heap_size = 4 * 1024 * 1024; // 4 MB
        config.initial_heap_size = 4 * 1024 * 1024;
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(config));

        // The JIT helper requires a `jit_thread` set via `set_jit_thread`
        // so `jit_thread_mut()` returns Some(thread) — otherwise the
        // GC-trigger arm silently no-ops and the test would not
        // exercise the STW path.
        let mut thread = JvmThread::new(ThreadId(0), "jit_newarray_test");

        // Burn through most of the young gen so the next allocation
        // probe is overwhelmingly likely to fail and drive the
        // `maybe_gc_forced_pub` arm. We use unrooted allocations so
        // they're immediately dead and the post-GC retry succeeds.
        //
        // CRIT: use the *fallible* `try_alloc_array`, not the panicking
        // `alloc_array`. On a 1 MB young gen this loop intentionally runs
        // the from-space to exhaustion; `alloc_array` would hit
        // `alloc_young`'s hard `std::process::abort()` (surfacing on
        // Windows as STATUS_STACK_BUFFER_OVERRUN / 0xC0000409) the instant
        // the heap filled — killing the test process during *setup*, before
        // `jit_newarray` is ever reached. `try_alloc_array` instead returns
        // `None` once the gen is full, which we harmlessly drop: the gen is
        // now pressured exactly as the test requires.
        for _ in 0..256 {
            let _ = vm_box.heap.try_alloc_array(
                ClassId::new(0),
                ArrayElementType::Int,
                1024,
            );
        }

        // Install the JIT thread pointer so the helper's `jit_thread_mut`
        // returns Some. SAFETY: the thread outlives the helper call;
        // `set_jit_thread` only stashes a `*mut JvmThread` in TLS.
        let prev = set_jit_thread(&mut thread);

        let vm_ptr = &*vm_box as *const SharedVm as i64;
        // T_INT (10) with a moderately large length — large enough that
        // the probe almost certainly fails on the shrunken heap,
        // exercising the GC arm; small enough that the actual
        // `alloc_array` after GC succeeds.
        let len: i64 = 1024;

        // SAFETY: vm_ptr is a live SharedVm; T_INT is a valid atype;
        // length is non-negative. The helper either takes the probe-
        // success fast path or the GC-then-alloc slow path; either way
        // returns a non-zero pointer on success.
        let result = unsafe { jit_newarray(vm_ptr, 10, len) };

        // Restore the prior JIT thread pointer (probably null, but
        // preserve correctness in case the test harness runs in a
        // re-entrant context).
        restore_jit_thread(prev);

        // The post-GC `alloc_array` always runs (no early return on
        // probe-failure), so a non-zero return proves the GC arm did
        // not crash and the heap recovered enough to satisfy the
        // request. A zero return would indicate either an OOM panic
        // turned into None or a regression in the helper's control
        // flow — both of which would surface here.
        assert!(
            result != 0,
            "jit_newarray must return a non-zero ObjectRef pointer after \
             GC-on-pressure (orchestrated STW path); got 0",
        );

        // The returned pointer must reference a live array on this heap
        // with the requested length, confirming the post-GC retry took
        // the regular `alloc_array` path (not some salvage / abort
        // shortcut).
        let arr = unsafe { ObjectRef::from_raw(result as usize as *mut u8) };
        assert_eq!(
            vm_box.heap.array_length(arr),
            len as usize,
            "post-GC alloc_array must produce an int[] of the requested length",
        );
    }
}

/// Return the current thread's `JvmThread` pointer for the JIT inline
/// TLAB bump-pointer fast path.
///
/// Reads the same TLS slot (`JIT_THREAD`) populated by `set_jit_thread`
/// just before JIT-compiled code runs. The pointer is valid for the
/// duration of the JIT invocation and is cleared by `clear_jit_thread`
/// when JIT code returns.
///
/// Returning a raw pointer is intentional — the JIT immediately reads
/// the TLAB cursor/end fields from `[thread + tlab_offset + ..]` and
/// never dereferences anything outside that two-word window during the
/// fast path. The slow-path fallback (`jit_new_object`) reaches the
/// thread via the same TLS slot.
///
/// Returns `null` if invoked from a thread that did not call
/// `set_jit_thread` (defensive — the JIT fast path treats a null thread
/// pointer as "skip the inline bump, fall through to slow path").
#[no_mangle]
pub unsafe extern "C" fn jit_get_current_thread() -> *mut JvmThread {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_THREAD.with(|t| t.get())
}

/// spring-bug-10 watchpoint: arm a hardware data WRITE breakpoint (DR0) on the
/// savebase frame slot at `addr` for the CURRENT thread, so the vectored
/// exception handler can report the RIP that writes the corrupt `0xFFFF…FFFE`.
///
/// Called from the JIT prologue (under `CRATONVM_SHADOW_WATCH`) with
/// `addr = rbp - savebase_off`. Re-arms only when `addr` changes (reset is
/// invoked repeatedly at the same stack depth ⇒ usually a no-op) and stops once
/// the VEH has caught the write. Setting debug registers on the running current
/// thread via `SetThreadContext(GetCurrentThread())` is applied by the kernel on
/// the next ring transition.
/// True if `ra` (a JIT return address) resolves to the no-arg `Matcher.reset()`.
/// Requires `CRATONVM_DBG_JIT_NAMES=1` (populates the name registry).
#[cfg(windows)]
fn ra_is_reset(ra: usize) -> bool {
    match cratonvm_jit::lookup_jit_method_name(ra) {
        Some(n) => n.contains("Matcher.reset()"),
        None => false,
    }
}

/// spring-bug-10 watchpoint via a dedicated WATCHER THREAD. Setting debug
/// registers on the *running current* thread is unreliable (the DRs only reload
/// on a context-switch-IN, which races the write we want to trap). The fix: a
/// background thread that SUSPENDS the worker, `SetThreadContext`s its DR0/DR7,
/// and RESUMES — applied deterministically on resume. The worker's reset prologue
/// just publishes its current savebase address; the watcher keeps DR0 pinned to
/// it (reset is called at a stable stack depth, so this is steady-state idle).
#[cfg(windows)]
mod savebase_watcher {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

    pub static ARM_ADDR: AtomicUsize = AtomicUsize::new(0);
    static WORKER_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);

    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetCurrentThread() -> isize;
        fn DuplicateHandle(
            sp: isize,
            sh: isize,
            tp: isize,
            th: *mut isize,
            access: u32,
            inherit: i32,
            opts: u32,
        ) -> i32;
        fn SuspendThread(t: isize) -> u32;
        fn ResumeThread(t: isize) -> u32;
        fn SetThreadContext(t: isize, ctx: *const u8) -> i32;
    }
    const DUPLICATE_SAME_ACCESS: u32 = 0x2;

    unsafe fn set_dr_on(h: isize, addr: u64, dr7: u64) -> i32 {
        #[repr(C, align(16))]
        struct Ctx([u8; 1232]);
        let mut c = Ctx([0u8; 1232]);
        let p = c.0.as_mut_ptr();
        core::ptr::write_unaligned(p.add(0x30) as *mut u32, 0x0010_0010); // DEBUG_REGISTERS
        core::ptr::write_unaligned(p.add(0x48) as *mut u64, addr); // Dr0
        core::ptr::write_unaligned(p.add(0x70) as *mut u64, dr7); // Dr7
        SetThreadContext(h, p)
    }

    /// Worker-side: publish the current reset savebase address; start the watcher
    /// thread on first call (duplicating the worker's thread handle for it).
    pub unsafe fn publish(addr: usize) {
        ARM_ADDR.store(addr, Ordering::Relaxed);
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut h: isize = 0;
        let proc = GetCurrentProcess();
        DuplicateHandle(
            proc,
            GetCurrentThread(),
            proc,
            &mut h,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        WORKER_HANDLE.store(h, Ordering::SeqCst);
        eprintln!("[WATCH] watcher thread started; worker savebase @0x{:016X}", addr);
        std::thread::spawn(|| unsafe { watcher_loop() });
    }

    unsafe fn watcher_loop() {
        let mut set_addr = 0usize;
        loop {
            let h = WORKER_HANDLE.load(Ordering::SeqCst);
            if crate::runtime::crash_handler::savebase_watch_caught() {
                if h != 0 {
                    SuspendThread(h);
                    set_dr_on(h, 0, 0);
                    ResumeThread(h);
                }
                return;
            }
            let want = ARM_ADDR.load(Ordering::Relaxed);
            if h != 0 && want != 0 && want != set_addr {
                let susp = SuspendThread(h);
                // DR7: L0 + R/W0=01 (write) + LEN0=10 (8-byte).
                let stc = set_dr_on(h, want as u64, 0x0009_0001);
                let res = ResumeThread(h);
                if set_addr == 0 {
                    eprintln!(
                        "[WATCH] watcher armed DR0=0x{:016X} on worker h=0x{:X}: SuspendThread={} SetThreadContext={} ResumeThread={}",
                        want, h, susp as i32, stc, res as i32
                    );
                }
                set_addr = want;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

// Naked trampoline: read `[rsp]` (the true return address into reset's prologue)
// at entry, tail-jump to the inner handler with it in RDX (ARG1).
#[cfg(windows)]
#[unsafe(naked)]
pub unsafe extern "C" fn jit_arm_savebase_watch(addr: i64) {
    core::arch::naked_asm!(
        "mov rdx, [rsp]",
        "jmp {inner}",
        inner = sym arm_savebase_watch_inner,
    );
}

#[cfg(windows)]
unsafe extern "C" fn arm_savebase_watch_inner(addr: i64, ra: usize) {
    if crate::runtime::crash_handler::savebase_watch_caught() {
        return;
    }
    if !ra_is_reset(ra) {
        return;
    }
    let a = addr as usize;
    if a == 0 || a & 0x7 != 0 {
        return;
    }
    savebase_watcher::publish(a);
}

// Disarm is watcher-managed (it disarms on catch), so the epilogue helper is a
// no-op; the cross-frame filter in the VEH separates the live corruptor from a
// coincidental -2 write to the reused stack slot after reset returns.
#[cfg(windows)]
pub unsafe extern "C" fn jit_disarm_savebase_watch() {}

#[cfg(not(windows))]
pub unsafe extern "C" fn jit_arm_savebase_watch(_addr: i64) {}
#[cfg(not(windows))]
pub unsafe extern "C" fn jit_disarm_savebase_watch() {}

/// Build the JIT runtime helpers table with real function pointer addresses.
pub fn build_helpers() -> JitRuntimeHelpers {
    // Compute the inline-TLAB offset triple once at startup so the JIT
    // can bake them as immediates. The runtime tests
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // pin the layout against drift.
    let tlab_off = JvmThread::tlab_offset();
    let cursor_in_thread = tlab_off + cratonvm_gc::Tlab::CURSOR_OFFSET;
    let end_in_thread = tlab_off + cratonvm_gc::Tlab::END_OFFSET;

    // spring-bug-10 watchpoint: register the savebase-watch arm-helper so the JIT
    // prologue (under CRATONVM_SHADOW_WATCH) can bake an absolute call to it.
    cratonvm_jit::x64::set_arm_savebase_watch_fn(jit_arm_savebase_watch as *const () as usize);
    cratonvm_jit::x64::set_disarm_savebase_watch_fn(jit_disarm_savebase_watch as *const () as usize);

    JitRuntimeHelpers {
        newarray: jit_newarray as *const () as usize,
        new_object: jit_new_object as *const () as usize,
        anewarray_object: jit_anewarray_object as *const () as usize,
        baload: jit_baload as *const () as usize,
        bastore: jit_bastore as *const () as usize,
        iaload: jit_iaload as *const () as usize,
        iastore: jit_iastore as *const () as usize,
        aaload: jit_aaload as *const () as usize,
        aastore: jit_aastore as *const () as usize,
        multianewarray_2d: jit_multianewarray_2d as *const () as usize,
        arraylength: jit_arraylength as *const () as usize,
        getfield: jit_getfield as *const () as usize,
        putfield_int: jit_putfield_int as *const () as usize,
        putfield_long: jit_putfield_long as *const () as usize,
        putfield_float: jit_putfield_float as *const () as usize,
        putfield_double: jit_putfield_double as *const () as usize,
        putfield_object: jit_putfield_object as *const () as usize,
        getstatic: jit_getstatic as *const () as usize,
        putstatic_int: jit_putstatic_int as *const () as usize,
        putstatic_long: jit_putstatic_long as *const () as usize,
        putstatic_float: jit_putstatic_float as *const () as usize,
        putstatic_double: jit_putstatic_double as *const () as usize,
        putstatic_object: jit_putstatic_object as *const () as usize,
        checkcast: jit_checkcast as *const () as usize,
        instanceof_check: jit_instanceof as *const () as usize,
        throw_aioobe: jit_throw_aioobe as *const () as usize,
        invoke_dispatch: jit_invoke_dispatch as *const () as usize,
        invoke_virtual_mic: jit_invoke_virtual_mic as *const () as usize,
        write_barrier: jit_write_barrier as *const () as usize,
        // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier so JIT-
        // overwritten references are logged before the concurrent marker
        // loses the only path to them.
        satb_pre_write_barrier: jit_satb_pre_write_barrier as *const () as usize,
        uncommon_trap: jit_uncommon_trap as *const () as usize,
        math_fma_double: jit_math_fma_double as *const () as usize,
        math_fma_float: jit_math_fma_float as *const () as usize,
        // HIGH-6 JIT audit — inline TLAB bump-pointer wiring.
        tlab_cursor_offset_in_thread: cursor_in_thread,
        tlab_end_offset_in_thread: end_in_thread,
        // JIT contract: `ObjectHeader.class_id` is at byte offset 0
        // (enforced by `class_id_remains_at_offset_zero` in
        // `types/src/heap_types.rs`). Exposed here so the JIT does not
        // hardcode the constant in a second place.
        class_id_offset_in_obj: 0,
        get_current_thread: jit_get_current_thread as *const () as usize,
        tlab_post_init: jit_post_tlab_init as *const () as usize,
        // Stage 3 (precise oop maps) — only wire the frame-record helper when
        // the precise gate is on; otherwise leave it 0 so the prologue emits
        // nothing extra. The JIT also gates emission on its own cached flag,
        // but keying the pointer on the same env keeps the default build inert.
        frame_record: if cratonvm_jit::x64::precise_jit_maps_enabled() {
            jit_frame_record as *const () as usize
        } else {
            0
        },
        // Shadow-stack precise roots — byte offset of the `ShadowStack` field
        // from `&JvmThread`. The JIT bakes `[thread + this + ShadowStack::TOP_OFFSET]`
        // as the inline push target. Always wired (harmless when codegen is off,
        // which gates emission on its own cached `CRATONVM_SHADOW_STACK` flag).
        shadow_stack_offset_in_thread: JvmThread::shadow_stack_offset(),
        // RBC.6 — athrow lowering: stash pending exception + sentinel.
        throw_exception: jit_throw_exception as *const () as usize,
    }
}

/// Stage 3 (precise oop maps) — record the EXACT RBP of the JIT frame that is
/// about to run, called once from the JIT prologue (gated on
/// `CRATONVM_PRECISE_JIT_MAPS`). The Rust-side `JitEntryGuard` pushed a chain
/// entry just before transferring control to compiled code, but it could only
/// capture an approximate stack pointer; this fills in the precise frame base
/// so the GC root walker can address oop-map slots as `[rbp - offset]`.
///
/// `extern "C"` with the single `rbp` argument in the platform's first
/// integer-argument register, matching the JIT's `ARG_REGS[0]` load.
extern "C" fn jit_frame_record(rbp: usize) {
    crate::jit::conservative_roots::set_top_frame_base(rbp);
}

/// T1.1.28 — Math.fma(double, double, double) runtime helper.
///
/// Called from JIT code via an absolute CALL emitted by the
/// `MATH_FMA_DOUBLE_INTRINSIC` path in `jit/src/x64.rs`. Delegates to
/// Rust's `f64::mul_add`, which compiles to `VFMADD231SD` on x86-64
/// hosts with FMA3 and to a correctly-rounded software implementation
/// otherwise. Both paths satisfy the JLS `Math.fma` contract of
/// "compute `a*b + c` as if with unlimited intermediate precision,
/// then round once".
#[no_mangle]
pub extern "C" fn jit_math_fma_double(a: f64, b: f64, c: f64) -> f64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    a.mul_add(b, c)
}

/// T1.1.28 — Math.fma(float, float, float) runtime helper.
#[no_mangle]
pub extern "C" fn jit_math_fma_float(a: f32, b: f32, c: f32) -> f32 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    a.mul_add(b, c)
}
