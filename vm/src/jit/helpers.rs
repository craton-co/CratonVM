// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//!
//! These functions need access to `SharedVm` and other VM internals, so they
//! live in the VM crate rather than the standalone JIT crate.

use std::cell::Cell;

use cratonvm_jit::{DescriptorParamIter, JitInvokeInfo, JitMICSlot, JitPICSlot, JitRuntimeHelpers};
use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_types::{
    ArrayElementType, ClassId, ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};

use crate::memory::vm_heap::VmHeap;
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};

// These two gate the RUNTIME dispatch helper's OWN direct-entry cache
// (`jit_invoke_dispatch`'s `DISPATCH_CACHE`/`jit_cache` lookups and
// `jit_invoke_virtual_mic`'s cached `entry_ptr` fast path) — NOT the same
// thing as `cratonvm_jit::direct_jit_callee_calls_enabled` (which gates
// compile-time eager-callee-compile in `jit/src/lib.rs` and the inline
// MIC/PIC codegen in `jit/src/x64.rs`, and is default-ON: see that
// function's doc comment for the closed RBP-mirror race and the
// general-throughput regression its default avoids).
//
// `statically_bound` (this function's only caller-side gate, besides this
// flag) is `matches!(info.invoke_kind, 1 | 3)` — invokespecial/invokestatic,
// whose target is fully determined by the constant-pool entry under JVM
// semantics, with no receiver-class ambiguity. Default-ON: measured on
// `bench/BenchSuite.java` bintrees16 (self-recursive `static Node make(int)`/
// `static long check(Node)`, the classic allocation+recursion micro-
// benchmark) at 43.7s with this flag off vs. 2.8s on — every invokestatic/
// invokespecial call site was paying the full `jit_invoke_dispatch` helper
// round trip instead of a cached direct entry. invokestatic/invokespecial
// are among the most common call forms in ordinary Java (constructors,
// private/static helpers, `super` calls), so leaving this off by default is
// a severe, general JIT throughput regression, not a narrow one.
//
// Formerly known residual, now FIXED: the static CP-owner cache was not a
// sound target resolver for every invokespecial/static BRIDGE specifically
// (Lucene DataOutput's `writeByte` bridge resolved as `Object.writeByte` was
// the originally-observed case) — a synthetic-bridge target-resolution gap,
// not a receiver-ambiguity one. Root cause: `info.class_name` (and
// `try_jit_compile_callee_slow`'s `class_name` parameter more generally) is
// the literal constant-pool-referenced class for an `invokespecial` site,
// but that is NOT always the class JVMS §6.5 says method *selection* should
// start searching from — for a genuine `super.m(...)` call (ACC_SUPER set on
// the calling class, target not `<init>`, CP-referenced class a genuine
// superclass of the caller), selection restarts at the CALLING class's own
// direct superclass instead. A class between the caller and the far-off
// CP-referenced ancestor that overrides the method (a compiler-generated
// bridge, or an ordinary override) was walked straight past, landing on a
// much-less-specific declaration higher up the chain. Fixed by computing the
// JVMS-correct selection-start class at JIT-compile time (before either
// `DISPATCH_CACHE` or `jit_cache` ever see the site) via
// `classloading::invokespecial_selection_start`, applied identically by both
// the interpreter (`interpreter::invokespecial_owner_class_name`) and the
// JIT compiler (`jit::try_compile_with_invokespecial_resolver`'s
// `cp_invokespecial_owner_resolver`), so the two execution modes agree and
// neither the per-callsite `DISPATCH_CACHE` nor the global `jit_cache` can
// ever cache a target resolved from the wrong starting class.
#[inline]
fn direct_static_compiled_callee_entry_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

// The virtual-call counterpart of the flag above. Default-ON now that every
// compilation path owns MIC/PIC slots, cache publication rejects class-only
// profile seeds, both entry ABIs are lowered, and the generated caller
// republishes its active frame after a raw call. Opt out for diagnosis with
// `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`.
//
// What the default-OFF period cost, measured independently on 2026-07-27 (H2
// `org/h2/` ban residuals): this flag gates the ONLY write of
// `mic.cached_entry_ptr`, so with it off the inline MIC/PIC cascade the codegen
// emits can never open and every virtual call out of compiled code falls back
// through `invoke_or_native` into the INTERPRETER. Compiling a method therefore
// made its callees slower, and compiling more of a program made it slower
// overall. On H2 `TestFreeSpace` -- `org/h2/mvstore/FreeSpaceBitSet.toString`
// compiled, its `java/util/BitSet.nextClearBit` callee compiled too -- the
// `toString` scan loop cost 5582 ms per 2000 calls with this off and 169 ms
// with it on, and the off-cost grew with the callee's working set while the
// on-cost did not.
//
// Consequence for anything verified while it was off: a JIT ban whose mechanism
// is compiled-to-compiled virtual dispatch could not reproduce during a
// default-OFF run, because the dispatch it guards was inert. JASPER-JDT.2/.3
// (`org/eclipse/jdt/internal/compiler/parser/` and `ast/`) were removed on
// 2026-07-26 on exactly such runs and had to be restored -- see their entry in
// `skip_list.rs`. Re-verify any similar removal with this ON.
#[inline]
fn direct_virtual_compiled_callee_entry_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(
        || match cratonvm_types::flags::runtime_var(
            "CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY",
        ) {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

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
        *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIC_PROF").is_some())
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
            self.ctr
                .fetch_add(now().wrapping_sub(self.t0), Ordering::Relaxed);
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

    /// ALL out-of-band JIT→interpreter signals for this thread, consolidated
    /// in ONE thread-local. `execute_jit_call`'s post-return drain used to
    /// pay SIX separate thread-local accesses (pending exception, AIOOBE,
    /// arithmetic, NPE, NPE action, deopt flag) on EVERY JIT invocation —
    /// ~50-60ns, the single largest constant in the interpreter→JIT entry
    /// overhead on short-callee shapes. One struct = one TLS address
    /// computation for the whole drain (`take_all_jit_signals`).
    ///
    /// Field semantics (formerly the individual statics):
    /// * `exception` — pending Java exception from JIT dispatch; the
    ///   interpreter routes it through exception tables after JIT returns.
    /// * `aioobe` — pending AIOOBE `(index, length)` from a JIT bounds
    ///   check (`jit_throw_aioobe`), consumed on the `i64::MIN` sentinel.
    /// * `arithmetic` — pending `ArithmeticException` ("/ by zero") from
    ///   the integer-division zero-divisor guard; drained like `aioobe`
    ///   (throwing through the exception table instead of re-running the
    ///   method, which double-executed prior side effects).
    /// * `npe` — pending NullPointerException from a JIT array helper on a
    ///   null array reference; drained like `aioobe`.
    /// * `npe_action` — JEP 358 (partial): the *operation kind* that raised
    ///   the pending JIT NPE (`helpful_npe::jit_action` code, `0` = none),
    ///   set in lockstep with `npe` so the drain can attach an action-only
    ///   message; reset by every bare `set_jit_pending_npe()` so a stale
    ///   code never leaks onto an unrelated NPE.
    /// * `deopt` — the out-of-band deopt/exception signal (`i64::MIN`
    ///   sentinel collision disambiguation): every JIT path producing the
    ///   `i64::MIN` deopt sentinel ALSO sets this, so a method legitimately
    ///   returning `Long.MIN_VALUE` is not mistaken for a deopt (which
    ///   would spuriously re-run it, double-executing side effects).
    static JIT_SIGNALS: JitSignals = const {
        JitSignals {
            exception: Cell::new(None),
            athrow_bci: Cell::new(-1),
            aioobe: Cell::new(None),
            arithmetic: Cell::new(false),
            npe: Cell::new(false),
            npe_action: Cell::new(0),
            deopt: Cell::new(false),
        }
    };

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

/// Return a replacement JIT argument buffer when a safepoint has forwarded an
/// object argument.  The JIT ABI carries raw addresses, so canonicalizing only
/// the receiver is insufficient for signatures such as `([BII)V`: a moved
/// byte array can otherwise be decoded from its recycled pre-GC address.
///
/// The common case allocates nothing.  `num_jit_args` counts compact native
/// values (including an instance receiver), so each parsed descriptor parameter
/// advances one buffer position even for category-2 Java values.
fn forward_jit_reference_args(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<Vec<i64>> {
    let mut replacement: Option<Vec<i64>> = None;
    let mut arg_index = if info.invoke_kind == 3 { 0 } else { 1 };

    // Instance receivers are object references regardless of their descriptor.
    if info.invoke_kind != 3 {
        if let Some(&raw) = args.first() {
            forward_jit_arg_at(vm, args, &mut replacement, 0, raw);
        }
    }

    let bytes = info.descriptor.as_bytes();
    let mut p = match bytes.iter().position(|&b| b == b'(') {
        Some(i) => i + 1,
        None => return replacement,
    };
    while p < bytes.len() && bytes[p] != b')' && arg_index < args.len() {
        let is_ref = matches!(bytes[p], b'L' | b'[');
        if is_ref {
            let raw = args[arg_index];
            forward_jit_arg_at(vm, args, &mut replacement, arg_index, raw);
        }
        match bytes[p] {
            b'L' => {
                while p < bytes.len() && bytes[p] != b';' {
                    p += 1;
                }
                p = p.saturating_add(1);
            }
            b'[' => {
                while p < bytes.len() && bytes[p] == b'[' {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'L' {
                    while p < bytes.len() && bytes[p] != b';' {
                        p += 1;
                    }
                    p = p.saturating_add(1);
                } else {
                    p = p.saturating_add(1);
                }
            }
            _ => p += 1,
        }
        arg_index += 1;
    }
    replacement
}

#[inline]
fn forward_jit_arg_at(
    vm: &SharedVm,
    original: &[i64],
    replacement: &mut Option<Vec<i64>>,
    index: usize,
    raw: i64,
) {
    if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
        return;
    }
    // SAFETY: the JIT calling convention guarantees that descriptor-declared
    // references are live object pointers at this boundary; the canonicality
    // checks above reject immediate/tagged values before constructing ObjectRef.
    let object = unsafe { ObjectRef::from_raw(raw as usize as *mut u8) };
    let forwarded = vm.mem.heap.load_and_forward(object).as_ptr() as i64;
    if forwarded != raw {
        let args = replacement.get_or_insert_with(|| original.to_vec());
        args[index] = forwarded;
    }
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
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SHADOW").is_some() {
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

/// The consolidated out-of-band JIT→interpreter signal block — see the
/// [`JIT_SIGNALS`] thread-local for field semantics.
struct JitSignals {
    exception: Cell<Option<ObjectRef>>,
    /// RBC.6 correctness fix — the bytecode pc of the `athrow` that produced
    /// `exception`, when statically known at JIT-compile time (`-1` = unknown,
    /// e.g. an exception propagated up from a dispatched callee, which this
    /// method has no bci for). Set ONLY by `jit_throw_exception` alongside
    /// `exception`; every other site that stashes a general exception (no
    /// known local throw site) leaves/resets this to `-1`. Consumed by
    /// `execute_jit_call` to give `route_jit_exception_through_method` a real
    /// `throw_pc` instead of the `usize::MAX`-means-unknown fallback, which
    /// cannot range-check a *typed* handler against the entry it actually
    /// belongs to — with 2+ exception-table entries whose catch types are in
    /// a subtype relationship (e.g. one entry catches `RuntimeException`, a
    /// LATER, unrelated entry catches `IllegalStateException`), the
    /// declaration-order type-only match picks whichever entry comes first
    /// regardless of which try-region actually threw, silently running the
    /// wrong handler. Confirmed via a two-sequential-try/catch differential
    /// repro (`AthrowCountBisect.twoThrowsSequential`,
    /// `vm/tests/jit_local_exception_handler_tests.rs`) before this fix.
    athrow_bci: Cell<i64>,
    aioobe: Cell<Option<(i64, i64)>>,
    arithmetic: Cell<bool>,
    npe: Cell<bool>,
    npe_action: Cell<u8>,
    deopt: Cell<bool>,
}

/// One-shot snapshot-and-clear of EVERY out-of-band JIT signal, produced by
/// [`take_all_jit_signals`] in a single thread-local access. The
/// interpreter's post-JIT-return drain consumes this instead of six separate
/// `take_*` calls.
pub(crate) struct DrainedJitSignals {
    pub exception: Option<ObjectRef>,
    /// See `JitSignals::athrow_bci`. `-1` iff unknown (matches the field's
    /// own sentinel, so callers can pass it straight through as `usize::MAX`
    /// when negative without an extra branch).
    pub athrow_bci: i64,
    pub aioobe: Option<(i64, i64)>,
    pub arithmetic: bool,
    pub npe: bool,
    /// Drained alongside `npe` for hygiene (a stale action code must not
    /// outlive its NPE), but not yet consumed by the JIT-return drains —
    /// they throw the bare NPE exactly as before this consolidation
    /// (attaching the JEP-358 action message here is a follow-up).
    #[allow(dead_code)]
    pub npe_action: u8,
    pub deopt: bool,
}

/// Snapshot-and-clear ALL JIT signals in ONE thread-local access. Draining
/// everything unconditionally is deliberate: a signal surviving into the
/// next unrelated JIT call was the recurring Round-8..11 leak-bug class, and
/// clearing a flag nobody set is free.
#[inline]
pub(crate) fn take_all_jit_signals() -> DrainedJitSignals {
    JIT_SIGNALS.with(|s| DrainedJitSignals {
        exception: s.exception.take(),
        athrow_bci: s.athrow_bci.replace(-1),
        aioobe: s.aioobe.take(),
        arithmetic: s.arithmetic.take(),
        npe: s.npe.take(),
        npe_action: s.npe_action.take(),
        deopt: s.deopt.take(),
    })
}

/// Store a pending Java exception from JIT dispatch. Called when
/// `jit_invoke_dispatch` encounters an `ExceptionThrown` error.
///
/// Always resets `athrow_bci` to `-1` (unknown) — this is the general,
/// origin-agnostic setter (a dispatched callee threw, or a re-stash), never
/// the direct-local-athrow path. Only `jit_throw_exception`'s dedicated
/// `set_jit_pending_exception_with_bci` may set a real bci, and only for the
/// exception it is stashing in that same call.
fn set_jit_pending_exception(exc: ObjectRef) {
    JIT_SIGNALS.with(|s| {
        s.exception.set(Some(exc));
        s.athrow_bci.set(-1);
    });
}

/// RBC.6 correctness fix — sibling of `set_jit_pending_exception` for the ONE
/// call site (`jit_throw_exception`) that knows the exact bytecode pc of the
/// `athrow` producing this exception at JIT-compile time. See
/// `JitSignals::athrow_bci` for why this matters (typed-handler routing
/// correctness with 2+ exception-table entries when `throw_pc` would
/// otherwise be `usize::MAX`).
fn set_jit_pending_exception_with_bci(exc: ObjectRef, bci: i64) {
    JIT_SIGNALS.with(|s| {
        s.exception.set(Some(exc));
        s.athrow_bci.set(bci);
    });
}

/// Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): re-stash a previously
/// taken pending Java exception. Used by `try_osr` when the OSR return path
/// drained the flag but cannot return an error from its own signature — the
/// exception must be re-posted so the interpreter dispatch loop drains it on
/// the next iteration via `take_jit_pending_exception`. Crate-pub because
/// only the OSR entry path should use it; ordinary JIT helpers set the flag
/// directly via the private `set_jit_pending_exception` above.
///
/// Deliberately loses any `athrow_bci` the exception may have carried before
/// being taken (this is the general re-stash path, not the direct-athrow
/// one) — always falls back to the pre-existing `usize::MAX`-means-unknown
/// behavior for the re-stashed exception, never a regression, just not the
/// newly-precise case.
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
    JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
}

/// Take (consume) any pending Java exception set by JIT dispatch.
/// Returns `Some(ObjectRef)` if an exception was pending, `None` otherwise.
pub fn take_jit_pending_exception() -> Option<ObjectRef> {
    JIT_SIGNALS.with(|s| s.exception.take())
}

/// Non-consuming peek: returns `true` if a pending Java exception is set.
///
/// Used by `jit_invoke_dispatch` (and its bail/cache paths) to decide
/// whether to return the `i64::MIN` deopt sentinel — so the JIT caller's
/// post-invoke exception guard fires and the interpreter routes the
/// stashed exception through the method's exception table — instead of
/// returning a bogus `0` that the JIT would keep computing with.
pub(crate) fn jit_pending_exception_is_set() -> bool {
    JIT_SIGNALS.with(|s| {
        let v = s.exception.take();
        let present = v.is_some();
        s.exception.set(v);
        present
    })
}

/// Take (consume) a pending AIOOBE from JIT bounds check.
/// Returns `Some((index, length))` if an AIOOBE was pending.
pub fn take_jit_pending_aioobe() -> Option<(i64, i64)> {
    JIT_SIGNALS.with(|s| s.aioobe.take())
}

/// Take (consume) a pending `ArithmeticException` ("/ by zero") set by the JIT
/// integer-division zero-divisor guard. Returns `true` if one was pending.
/// Mirrors [`take_jit_pending_aioobe`]; the interpreter's post-JIT drain throws
/// a real `ArithmeticException` through the method's exception table.
pub fn take_jit_pending_arithmetic() -> bool {
    JIT_SIGNALS.with(|s| s.arithmetic.take())
}

/// Re-stash a previously taken pending-arithmetic flag. Mirrors
/// [`stash_jit_pending_aioobe`] for the OSR drain-without-route path, so a
/// div-by-zero raised in OSR-compiled code with no in-frame handler survives the
/// OSR→interpreter handoff and is surfaced by the next JIT-return drain.
pub(crate) fn stash_jit_pending_arithmetic() {
    JIT_SIGNALS.with(|s| s.arithmetic.set(true));
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
    JIT_SIGNALS.with(|s| s.npe.take())
}

/// Take (consume) the JEP-358 *action code* recorded alongside a pending JIT
/// NPE (`helpful_npe::jit_action::*`; `0` when none was recorded — e.g. the NPE
/// came from a helper that doesn't carry its operation kind). The interpreter
/// drain calls this right after [`take_jit_pending_npe`] to build the
/// action-only message.
pub fn take_jit_pending_npe_action() -> u8 {
    JIT_SIGNALS.with(|s| s.npe_action.take())
}

/// Internal: set the pending-NPE flag with no action code (the existing bare
/// signal — yields an unmessaged NPE). Resets the action cell to `0` so a stale
/// code from a prior op can never leak onto this NPE.
#[inline]
fn set_jit_pending_npe() {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(0);
    });
}

/// Internal: set the pending-NPE flag *with* a JEP-358 action code
/// (`helpful_npe::jit_action::*`). Called from the array/length helpers that
/// know their operation kind, so the interpreter can attach an action-only
/// JEP-358 message to the JIT-originated NPE.
#[inline]
fn set_jit_pending_npe_action(code: u8) {
    JIT_SIGNALS.with(|s| {
        s.npe.set(true);
        s.npe_action.set(code);
    });
}

/// Re-stash a previously-taken JIT NPE action code (OSR drain-without-route
/// path, mirroring [`stash_jit_pending_npe`]). Preserves the action so a
/// re-surfaced NPE keeps its JEP-358 message.
pub(crate) fn stash_jit_pending_npe_action(code: u8) {
    set_jit_pending_npe_action(code);
}

/// Set the out-of-band deopt/exception signal (MEDIUM fix: `i64::MIN` sentinel
/// collision). MUST be called on EVERY path that produces the `i64::MIN` deopt
/// sentinel, so the interpreter can tell a genuine deopt/exception apart from a
/// method that legitimately returns `Long.MIN_VALUE`. Idempotent; cheap.
///
/// Crate-public so the out-of-line JIT stubs in `jit/src/x64.rs` can route
/// through a tiny `extern "C"` helper that sets it (see [`jit_set_deopt_pending`]).
#[inline]
pub(crate) fn set_jit_deopt_pending() {
    JIT_SIGNALS.with(|s| s.deopt.set(true));
}

/// Read+clear the out-of-band deopt/exception signal. The interpreter's
/// post-invoke check calls this when it observes an `i64::MIN` return: `true`
/// means the JIT took the exception/deopt path and the `i64::MIN` is the
/// sentinel (route/re-run); `false` means `i64::MIN` is a real returned value
/// and must be pushed verbatim. See [`set_jit_deopt_pending`].
#[inline]
pub fn take_jit_deopt_pending() -> bool {
    JIT_SIGNALS.with(|s| s.deopt.take())
}

/// `extern "C"` trampoline for the out-of-line deopt/exception stubs emitted in
/// `jit/src/x64.rs`. Those stubs already `CALL` a helper (e.g. `jit_bastore`,
/// `jit_throw_aioobe`, the dispatch helper) that sets one of the pending-exception
/// flags AND `JIT_DEOPT_PENDING` before producing `i64::MIN`. This dedicated,
/// side-effect-free trampoline lets a stub that does NOT otherwise call such a
/// helper (or whose helper predates this flag) set the deopt signal with a
/// single `CALL` and no argument marshalling.
///
/// SAFETY: no pointer arguments; only touches a thread-local. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel.
pub extern "C" fn jit_set_deopt_pending() {
    set_jit_deopt_pending();
}

/// `i64::MIN`-sentinel disambiguation for `J`/`D` (long/double) call returns —
/// PEEK (non-clearing) of every out-of-band exception/deopt signal.
///
/// A compiled caller signals a callee exception/deopt by the dispatch helpers'
/// `i64::MIN` return in RAX. For `int`/ref/void returns that is unambiguous (no
/// such legitimate value), so the post-invoke check is a plain
/// `CMP RAX, i64::MIN; JE bail`. But a callee that *legitimately* returns
/// `Long.MIN_VALUE` (a `J`/`D` whose bits equal `i64::MIN`) returns the SAME
/// value with NO pending-signal flag set — so a `J`/`D` call site cannot tell the
/// two apart from RAX alone.
///
/// At a `J`/`D` call site the backend therefore emits, ONLY on the (rare)
/// `RAX == i64::MIN` branch, a `CALL` here. We peek (do not clear) every signal a
/// dispatch helper or its callee could have raised before returning `i64::MIN`:
///
///   * `JIT_PENDING_EXCEPTION`  — an explicit/native throwable (athrow, NPE-on
///     -receiver, re-stashed callee exception),
///   * `JIT_PENDING_NPE`        — a void-return-store / null-receiver NPE,
///   * `JIT_PENDING_AIOOBE`     — a bounds-check failure,
///   * `JIT_DEOPT_PENDING`      — the generic out-of-band deopt flag (set by
///     `jit_throw_aioobe` / `jit_uncommon_trap` / the x64 stubs), and
///   * a stashed IR-deopt frame (`cratonvm_jit::deopt::has_last_deopt`) — an
///     IR-path deopt of a dispatched callee returns `i64::MIN` WITHOUT the VM
///     flag (the stash lives in the jit crate).
///
/// Returns `1` iff ANY is pending (the `i64::MIN` is a genuine sentinel — the
/// caller bails through its epilogue, and the interpreter's post-JIT path drains
/// the still-set flag and routes/resumes), `0` iff none is pending (the
/// `i64::MIN` is a real `Long.MIN_VALUE` return — the caller keeps it). The peek
/// is non-destructive so the outer interpreter drain still observes the flag.
///
/// SAFETY: no pointer arguments; only reads thread-locals. Safe to call from
/// JIT-compiled code immediately after a dispatch returns `i64::MIN`.
pub extern "C" fn jit_dispatch_threw() -> i64 {
    let pending = JIT_SIGNALS.with(|s| {
        // Non-destructive peek across the whole signal block in ONE
        // thread-local access (the former per-flag statics cost four).
        let exc = s.exception.take();
        let exc_set = exc.is_some();
        s.exception.set(exc);
        exc_set || s.npe.get() || s.aioobe.get().is_some() || s.deopt.get()
    }) || cratonvm_jit::deopt::has_last_deopt();
    if pending {
        1
    } else {
        0
    }
}

/// JEP 358 (helpful NPE), inline-codegen path — `extern "C"` trampoline for the
/// per-action inline null-check failure stubs emitted in
/// `jit/src/x64.rs::emit_null_check_store_stubs`.
///
/// Each inline array load/store/`arraylength` null-check site is grouped by its
/// JEP-358 [`cratonvm_jit_api::npe_action`] code, and the per-action stub passes
/// that code here. We set the pending-NPE flag *with* the code (so the
/// interpreter's post-JIT NPE drain attaches the right action-only message —
/// "Cannot load from int array", …, gated behind
/// `-XX:+ShowCodeDetailsInExceptionMessages`) and the out-of-band deopt signal
/// (the stub loads `i64::MIN` as the method return value, so the interpreter
/// must not mistake it for a real `Long.MIN_VALUE`).
///
/// This replaces the old single shared stub, which called `jit_bastore(0)` and
/// therefore fabricated `ASTORE_BYTE` ("Cannot store to byte array") for *every*
/// inline array opcode regardless of its real element type or load/store
/// direction. `code == NONE` (0) sets a bare (unmessaged) NPE — the same shape
/// as today's default path — which is what the non-array intrinsic null-check
/// sites use.
///
/// SAFETY: no pointer arguments; only touches thread-locals. Safe to call from
/// JIT-compiled code at any point before loading the `i64::MIN` sentinel. The
/// `code` is truncated to `u8`; an out-of-range value maps to no message
/// (`jit_action_message` returns `None`), never a panic.
pub extern "C" fn jit_npe_with_action(code: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache,
    // matching every other array/field helper's entry (the next GC must
    // re-scan after we deopt back out to the interpreter).
    crate::jit::conservative_roots::note_jit_boundary();
    set_jit_pending_npe_action(code as u8);
    // Out-of-band deopt signal: the stub loads `i64::MIN` as the return value
    // (same invariant as `jit_bastore`'s null arm did before).
    set_jit_deopt_pending();
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
// SAFETY: invoked on the thread that installed `JIT_THREAD`, so no other
// `&mut JvmThread` is live; the returned exclusive borrow is unique for the
// guard's lifetime.
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
// SAFETY: `entry` is a live JIT-compiled `extern "C"` code pointer produced by
// the compiler; it is transmuted to a fn signature matching the (with/without
// NativeContext) arity actually called below.
unsafe fn try_call_compiled_entry(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    // The compiled callee's prologue publishes ITS rbp into the precise-maps
    // innermost-RBP mirror; nothing on this Rust path republishes the JIT
    // caller's rbp after the callee returns, so a later GC would walk the
    // dead callee frame (see `top_rbp_mirror_write`). Snapshot + restore the
    // mirror around the raw entry call.
    let saved_top_rbp = crate::jit::conservative_roots::top_rbp_mirror_read();
    let r = try_call_compiled_entry_inner(entry, needs_ctx, vm_ptr, args_slice);
    crate::jit::conservative_roots::top_rbp_mirror_write(saved_top_rbp);
    r
}

// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
unsafe fn try_call_compiled_entry_inner(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    let n = args_slice.len();
    if needs_ctx {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg with-ctx callee takes exactly (vm_ptr) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr)
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg with-ctx
                // callee takes (vm_ptr, arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg with-ctx
                // callee takes (vm_ptr, arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(vm_ptr, args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                )
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    vm_ptr,
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            _ => return None,
        })
    } else {
        Some(match n {
            0 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry (caller contract);
                // the 0-arg no-ctx callee takes no arguments per the JIT ABI.
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(entry);
                f()
            }
            1 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 1-arg no-ctx
                // callee takes (arg0) per the JIT ABI.
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0])
            }
            2 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 2-arg no-ctx
                // callee takes (arg0, arg1) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1])
            }
            3 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 3-arg no-ctx
                // callee takes (arg0, arg1, arg2) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2])
            }
            4 => {
                // SAFETY: `entry` is a live JIT-compiled extern "C" entry; the 4-arg no-ctx
                // callee takes (arg0, arg1, arg2, arg3) per the JIT ABI.
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(entry);
                f(args_slice[0], args_slice[1], args_slice[2], args_slice[3])
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                )
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                )
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                )
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(entry);
                f(
                    args_slice[0],
                    args_slice[1],
                    args_slice[2],
                    args_slice[3],
                    args_slice[4],
                    args_slice[5],
                    args_slice[6],
                    args_slice[7],
                )
            }
            _ => return None,
        })
    }
}

/// Call a compiled entry from a JIT dispatch helper while suspending the
/// debug borrow tracker for the outer JIT frame.
///
/// A compiled callee can immediately re-enter JIT dispatch and borrow the
/// same `JvmThread`. That nested borrow is a child reborrow of the outer JIT
/// frame, but the direct fast paths do not go through `set_jit_thread`, so the
/// debug tracker must be suspended explicitly around the raw call.
#[inline]
// SAFETY: same entry ABI contract as `try_call_compiled_entry`.
unsafe fn try_call_compiled_entry_reentrant(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
) -> Option<i64> {
    // This helper is itself called from compiled dispatch code.  Its raw ABI
    // call used to enter the nested compiled method without registering a
    // `JitEntryGuard`, so a GC triggered by that callee found a JIT return
    // address above an empty guard chain.  The moving-young collector then
    // had to fall back to conservative/non-moving collection and retained
    // the repeated Hibernate bootstrap graphs until OOM.  Resolve the entry
    // back to its live CompiledMethod and register the precise frame for the
    // full duration of the nested call.
    let mut needs_ctx = needs_ctx;
    let jit_root_guard = cratonvm_jit::lookup_jit_code_range(entry).map(|cm_ptr| {
        // SAFETY: the JIT code-range registry owns this CompiledMethod while
        // its entry remains callable; the guard is dropped before this helper
        // returns to the caller that holds the corresponding code cache entry.
        let compiled = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
        // cceres2 (WildFly SIGSEGV cores SF2/SF3/SM): the caller-supplied ABI
        // flag can come from a cache whose (entry, needs_context) pair was
        // read non-atomically across a concurrent inline-cache retarget or
        // C1->C2 supersede. Marshalling with the wrong flag shifts every
        // argument by one inside the callee (its `this` reads leftover
        // register junk — the receiver=0x2d0-style PIC-dispatch SIGSEGV).
        // The resolved CompiledMethod is the entry's OWN record; its flag is
        // definitive. Override and count/log the mismatch so the lying cache
        // can be identified.
        let own = compiled.needs_context();
        if own != needs_ctx {
            static MISMATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 16 || n.is_power_of_two() {
                tracing::warn!(
                    "JIT ABI flag mismatch #{n}: cached needs_ctx={} but compiled entry 0x{:x} \
                     says {} — using the compiled method's own flag",
                    needs_ctx,
                    entry,
                    own,
                );
            }
            needs_ctx = own;
        }
        crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(compiled)
    });
    #[cfg(debug_assertions)]
    let borrow = suspend_jit_borrow();
    let result = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice);
    #[cfg(debug_assertions)]
    restore_jit_borrow(borrow);
    drop(jit_root_guard);
    result
}

/// Bail a JIT-dispatched call out to the interpreter when the compiled
/// callee has more arguments than `call_jit_compiled_method_entry`'s
/// register-arg dispatch tables can pass. Issues `invoke_or_native` with
/// the full `Value` argument vector and converts the result back to the
/// `i64` register-ABI return value expected by the JIT caller. Exceptions
/// are stashed via `handle_jit_dispatch_error` so the interpreter post-JIT
/// path can route them through the caller's exception table.
#[inline(never)]
// SAFETY: called from a JIT helper with a live `SharedVm` and the current
// `JvmThread` set up, so resuming interpretation / routing through the caller's
// exception table operates on valid VM state.
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

struct VirtualDispatchTarget {
    class_name: std::sync::Arc<str>,
    cacheable_receiver: bool,
}

/// Resolve the runtime dispatch class for a virtual/interface helper call.
/// `invoke_or_native` binds to the class name it is handed rather than
/// re-dispatching on the receiver, so the helper must hand it the receiver's
/// *runtime* class when that class is trustworthy, or the CP-resolved
/// call-site class when the receiver carries only an unusable/synthetic id.
///
/// `cacheable_receiver` is true only when `class_name` is the receiver class
/// named by the MIC/PIC key. If the receiver is `ClassId(0)`, an array, or a
/// bare/interface fallback to the CP class, publishing that result under the
/// receiver id would poison later inline-cache hits.
///
/// SAFETY: `vm` must be live; `receiver` must be a valid heap reference.
/// Residual-6 diagnosis (env-gated, `CRATONVM_TRACE_CLASSVALUE`): true when
/// the `ClassValue.get(Class)` dispatch-trace probes should fire. Cached so
/// the hot dispatch path pays two slice compares + one bool load.
pub(crate) fn cv_trace_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_TRACE_CLASSVALUE").is_some())
}

/// Residual-6 diagnosis: does this invoke-info describe the
/// `get(Ljava/lang/Class;)Ljava/lang/Object;` signature the `ClassValue`
/// native answers? (Class-blind on purpose — the probe wants every route.)
fn cv_trace_match(info: &JitInvokeInfo) -> bool {
    info.method_name == "get"
        && info.descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
        && cv_trace_enabled()
}

unsafe fn virtual_dispatch_target_for_receiver(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
) -> VirtualDispatchTarget {
    if vm.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return VirtualDispatchTarget {
            class_name: std::sync::Arc::from("java/lang/Object"),
            cacheable_receiver: false,
        };
    }

    let cid = vm.mem.heap.class_id_of(receiver);
    if cid == ClassId::new(0) {
        return VirtualDispatchTarget {
            class_name: if crate::vm::is_object_member(info.method_name, info.descriptor) {
                std::sync::Arc::from("java/lang/Object")
            } else {
                std::sync::Arc::from(info.class_name)
            },
            cacheable_receiver: false,
        };
    }

    let cm = vm.classes.class_manager.read();
    let Some(recv_class) = cm.get_class(cid) else {
        return VirtualDispatchTarget {
            class_name: std::sync::Arc::from(info.class_name),
            cacheable_receiver: false,
        };
    };
    let recv_name = recv_class.name.clone();
    let recv_is_iface = recv_class.is_interface();
    let recv_is_bare_object = recv_name.as_ref() == "java/lang/Object";
    let cp_is_not_object = info.class_name != "java/lang/Object";
    if (recv_is_iface || recv_is_bare_object)
        && cp_is_not_object
        && !crate::vm::is_object_member(info.method_name, info.descriptor)
        && recv_name.as_ref() != info.class_name
    {
        VirtualDispatchTarget {
            class_name: std::sync::Arc::from(info.class_name),
            cacheable_receiver: false,
        }
    } else {
        VirtualDispatchTarget {
            class_name: recv_name,
            cacheable_receiver: true,
        }
    }
}

/// Resolve the dispatch class for a virtual/interface bail
/// (`bail_to_interpreter`, kinds 0/2). Null/non-object receivers fall back to
/// the static call-site class so we never dispatch on an empty name.
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
    virtual_dispatch_target_for_receiver(vm, receiver, info).class_name
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
    let cm = vm.classes.class_manager.read();
    let class_id = if info.declaring_class_id == 0 {
        cm.find_bootstrap_class_by_name(info.class_name)
    } else {
        cm.find_class_by_name_for_class(
            info.class_name,
            ClassId::new(info.declaring_class_id),
        )
    };
    let Some(class_id) = class_id else {
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
    let cm = vm.classes.class_manager.read();
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
    if rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_implicit_exc_through_callee ENTER {}.{}{} has_last_deopt={} pending_exc={} pending_npe_or_aioobe_unread=?",
            info.class_name,
            info.method_name,
            info.descriptor,
            cratonvm_jit::deopt::has_last_deopt(),
            jit_pending_exception_is_set(),
        );
    }
    // jit-invokedynamic-groovy-regression fix — FIRST chance: a frame-stashing
    // deopt (the unconditional invokedynamic reason-8 trap, or a precise guard
    // bail) in the compiled callee THIS helper just invoked. Resume the callee
    // precisely at its trapping bci and hand the real result back to the
    // compiled caller — no side-effect re-run, no sentinel escape. Refusals
    // (identity mismatch / unmappable / no stash) leave the stash + sentinel
    // to propagate exactly as before. The cheap `has_last_deopt` pre-check
    // keeps the common exception path free of the thread-guard acquire.
    if cratonvm_jit::deopt::has_last_deopt() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(v) = try_resume_trapped_callee(vm, thread, info) {
                return v;
            }
        }
    }
    // Did the callee raise an *implicit* runtime exception?
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
        // KCFULL-13 — *general* pending exception (an explicit `athrow`, or a
        // native-raised throwable, originating in this compiled callee's own
        // callee chain). The original bug-H assumption — "methods-with-tables
        // that `athrow` are never compiled, so a general `JIT_PENDING_EXCEPTION`
        // can't reach a direct compiled call" — only holds for a method that
        // throws *itself*. A method that merely *catches* (declares an
        // exception table but contains no `athrow`) passes the `has_athrow`
        // compile gate and IS compiled; yet the JIT still cannot dispatch to
        // its in-method handler, so an exception propagating up from an
        // (interpreted) sub-callee silently skips its `catch` and escapes to
        // the outermost frame. Canonical victim: keycloak's
        // `CryptoIntegration` chain — JIT'd `getSelectedProvider` wraps an
        // interpreted `CryptoIntegration.getProvider()` (kept interpreted by
        // the string-ldc gate) in `try { ... } catch (IllegalStateException)`,
        // and the ISE escaped the catch under JIT.
        //
        // Mirror the implicit path: if the compiled callee declares a handler,
        // re-execute it in the interpreter so its exception table runs. The
        // interpreter re-run regenerates and routes the exception, so the
        // stale stashed copy is dropped first to avoid a double-drain.
        if jit_pending_exception_is_set() && callee_has_exception_table(vm, info) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let _ = take_jit_pending_exception();
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                if rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_implicit_exc_through_callee KCFULL-13 bail_to_interpreter {}.{}{}",
                        info.class_name, info.method_name, info.descriptor
                    );
                }
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
        }
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

/// jit-invokedynamic-groovy-regression fix — does the JIT cache hold a
/// compiled artifact for `(class, method, descriptor)` whose body contains an
/// unconditional `invokedynamic` uncommon trap (`has_indy_trap`)?
///
/// Entry-publication gates consult this: such an artifact must NEVER be
/// published where MACHINE CODE calls it directly (a baked JIT→JIT direct
/// call, a MIC/PIC inline-cache entry), because the trap's `i64::MIN`
/// sentinel + stashed frame would bail through the compiled CALLER's
/// epilogue, past the only point (the dispatch helper) that can resume the
/// callee precisely. Keyed exactly like the compile probes at each gate site
/// (the receiver class for MIC/PIC, the CP class for static direct calls) so
/// the lookup hits the same cache row `try_jit_compile_callee` filled.
pub(crate) fn compiled_entry_has_indy_trap(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // `&str`-only API (mirrors the compile-probe gate sites this consults —
    // see `JitKey::declaring_class_id`'s doc comment); resolving the class
    // globally by name preserves this helper's existing, not loader-aware,
    // behavior rather than threading a `ClassId` through its 3 call sites.
    let class_id = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let jit_cache = vm.jit.jit_cache.read();
    jit_cache
        .get(class_name, method_name, descriptor, class_id)
        .map_or(false, |c| c.has_indy_trap)
}

/// jit-invokedynamic-groovy-regression fix — precise, in-place resolution of a
/// compiled callee that deopted with a stashed frame, at the dispatch-helper
/// call site that invoked it.
///
/// When a compiled callee returns the `i64::MIN` sentinel because it hit a
/// frame-stashing deopt (the unconditional `invokedynamic` reason-8 trap, or a
/// precise guard bail), the stash describes the CALLEE's own frame at the
/// trapping bci. THIS helper — sitting directly between the compiled caller
/// and the callee — is the only place the callee can be resumed without
/// losing any caller's continuation: rebuild the callee's interpreter frame
/// from the stash, run it to completion, and hand the REAL result back to the
/// compiled caller as if the callee had returned normally. No side effect is
/// re-run, no sentinel escapes.
///
/// Refusal (returns `None`, stash left in place / restored) when:
///   * no stash, or the superseded-artifact sentinel (`bci == u32::MAX`);
///   * the stash's baked `method_key` does not name the SAME method this
///     helper invoked (`info`'s name+descriptor; the declaring class may be a
///     supertype of the call-site class for virtual dispatch, so the class is
///     taken from the key and verified to declare the method) — e.g. a
///     deeper machine-called method's guard deopt propagating through;
///   * the method is `ACC_SYNCHRONIZED` (conservative — monitor accounting
///     for the compiled prologue/epilogue vs the interpreter continuation is
///     not audited for this path);
///   * the frame is unmappable (`build_deopt_frame_inner` bails).
/// On refusal the sentinel propagates exactly as before — the outer sinks'
/// identity checks then despeculate + fall back to the safe re-run.
///
/// SAFETY: same contract as the surrounding dispatch helpers — `vm` live,
/// `info` a live `JitInvokeInfo`, `thread` the current thread's exclusive
/// borrow (passed in, NOT re-acquired via `jit_thread_mut`, because some call
/// sites — `jit_invoke_virtual_mic` — hold the thread guard for their whole
/// body and a nested acquire would alias the `&mut`).
unsafe fn try_resume_trapped_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> Option<i64> {
    let (key, bci) = cratonvm_jit::deopt::peek_last_deopt_identity()?;
    if key.is_empty() || bci == u32::MAX {
        return None;
    }
    // After a class redefinition the stash (from a pre-redefine artifact) may
    // describe bytecode that no longer matches the class store — refuse and
    // let the conservative re-run handle it (mirrors `try_osr`'s guard).
    if crate::classloading::any_class_redefined() {
        return None;
    }
    let (rest, key_desc) = key.rsplit_once(':')?;
    let (key_class, key_method) = rest.rsplit_once('.')?;
    if key_method != info.method_name || key_desc != info.descriptor {
        return None;
    }

    // Resolve the trapping method from ITS OWN declaring class (baked in the
    // key) — mirrors the `callee_compiler` resolution recipe.
    let cached = {
        let cm = vm.classes.class_manager.read();
        let class_id = if info.declaring_class_id == 0 {
            cm.find_bootstrap_class_by_name(key_class)?
        } else {
            cm.find_class_by_name_for_class(
                key_class,
                ClassId::new(info.declaring_class_id),
            )?
        };
        let store = cm.class_store();
        let (method, declaring_id) =
            crate::classloading::find_method_recursive(class_id, key_method, key_desc, store)?;
        let code_attr = method.code()?;
        let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
        // The key must name the method's OWN declaring class — a mismatch
        // means the name resolution drifted (e.g. class redefinition);
        // refuse rather than resume against different bytecode.
        if declaring_class_name != key_class {
            return None;
        }
        if method.is_synchronized() {
            return None;
        }
        let num_params = crate::runtime::interpreter::count_method_params(key_desc);
        std::sync::Arc::new(crate::classloading::resolution::CachedBytecodeMethod {
            declaring_class_id: declaring_id,
            class_name: std::sync::Arc::from(declaring_class_name),
            method_name: std::sync::Arc::from(key_method),
            method_descriptor: std::sync::Arc::from(key_desc),
            source_file: store
                .get(declaring_id)
                .and_then(|c| c.source_file.as_deref())
                .map(std::sync::Arc::from),
            code: crate::runtime::frame::padded_bytecode(&code_attr.code),
            exception_table: std::sync::Arc::from(code_attr.exception_table.as_slice()),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            num_params: num_params as u16,
            is_synchronized: method.is_synchronized(),
            is_static: method.is_static(),
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        })
    };
    if bci as usize >= cached.code.len() {
        return None;
    }

    // Commit: take the stash. From here every path either resumes or restores
    // it so nothing is silently dropped.
    let rframe = cratonvm_jit::deopt::take_last_deopt()?;

    let pin_base = thread.native_pin_roots.len();
    let frame = match crate::runtime::interpreter::build_deopt_frame_inner(
        vm, thread, &cached, &rframe, false,
    ) {
        Some(f) => f,
        None => {
            // Unmappable — release partial pins, restore the stash, and let
            // the sentinel propagate to the outer (identity-checked) sinks.
            thread.native_pin_roots.truncate(pin_base);
            cratonvm_jit::deopt::restash_last_deopt(rframe);
            return None;
        }
    };

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
        eprintln!("[cratonvm-deopt] helper precise-resume of trapped callee {key} at bci={bci}");
    }

    // De-speculate the trapping method FIRST (record + evict + escalate), so
    // repeated traps blacklist it and future calls interpret it outright.
    // Reason: `UnreachedCode` — the one-shot "give up immediately" policy
    // (`recommend_action`), so the trapping method is made not-compilable on
    // the FIRST resolution and the tiered manager stops re-queuing recompiles
    // that would just trap again. (A guard-bail stash reaching this arm is
    // over-blacklisted by this — acceptable: it reverts to the interpreter,
    // which is always correct.)
    DeoptimizationController::deoptimize(
        vm,
        key_class,
        key_method,
        key_desc,
        cratonvm_jit::deopt::DeoptReason::UnreachedCode,
        bci,
    );

    // Run the reconstructed frame to completion. The pins stay installed for
    // the duration (they root the reconstructed oops; the pushed frame roots
    // them too — over-rooting is harmless), released after.
    let res = crate::runtime::interpreter::execute_prebuilt_frame(vm, thread, frame);
    thread.native_pin_roots.truncate(pin_base);

    Some(match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    })
}

/// Inline capacity shared with `safe_native_call_impl`. It covers receiver
/// plus the full x64 register envelope and keeps ordinary JIT-to-native
/// dispatch free of temporary heap allocations.
pub(crate) const INLINE_JIT_NATIVE_ARGS: usize = 8;
type JitDecodedArgs = smallvec::SmallVec<[Value; INLINE_JIT_NATIVE_ARGS]>;

/// Decode a JIT dispatch helper's raw `i64` argument slice into the
/// `Value` slice the interpreter expects. Centralised so that the slow
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
) -> JitDecodedArgs {
    let mut values = JitDecodedArgs::with_capacity(args_slice.len());
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
                    vm.mem.heap.is_object_address(bits as usize)
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
                        vm.mem.heap.is_object_address(bits as usize)
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
    &(*(vm_ptr as *const SharedVm)).mem.heap
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
// SAFETY: every caller is a JIT helper invoked from compiled code; `vm_ptr` is
// either 0 (handled by the early return) or a live `SharedVm` pointer per the
// universal JIT-helper caller contract.
#[inline]
unsafe fn jit_safepoint_flush_satb(vm_ptr: i64) {
    if vm_ptr == 0 {
        return;
    }
    // SAFETY: caller contract for every JIT helper — vm_ptr is a live
    // SharedVm pointer.
    let vm = &*(vm_ptr as *const SharedVm);
    vm.mem.heap.flush_thread_satb();
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
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if length < 0 {
        // Negative length → NegativeArraySizeException (JLS). Stash it in the
        // pending-exception channel and return the 0/null sentinel; the
        // `newarray` codegen's `emit_post_alloc_oom_check` bail then routes it
        // through the method's exception table (catchable), matching the
        // interpreter / HotSpot. The cast-to-usize below would otherwise turn
        // a negative length into a huge allocation request.
        return jit_negative_array_size(vm, length);
    }
    let heap = &vm.mem.heap;
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
    // GC-overhead limit: if forced GCs keep freeing almost nothing, the heap is
    // full of live objects — surface OOM now instead of limping on slivers
    // (death-spiral). Mirrors the interpreter's alloc paths.
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm) {
        return jit_newarray_oom(vm, length as usize);
    }
    // Retry after GC. On a second failure, try a G1 last-ditch full mark
    // cycle (dead humongous spans need cleanup, not a young pause) and retry
    // once more; then the heap is genuinely exhausted — surface a catchable
    // `java/lang/OutOfMemoryError` exactly as the interpreter's
    // `gc_alloc_array` does, instead of the old non-fallible `alloc_array`
    // (which would abort the process on a real OOM).
    let obj_ref = match heap.try_alloc_array(ClassId::new(0), elem_type, length as usize) {
        Some(o) => o,
        None => {
            if !jit_g1_last_ditch_full_cycle(vm) {
                return jit_newarray_oom(vm, length as usize);
            }
            match heap.try_alloc_array(ClassId::new(0), elem_type, length as usize) {
                Some(o) => o,
                None => return jit_newarray_oom(vm, length as usize),
            }
        }
    };
    jit_newarray_finish(obj_ref, atype, length)
}

/// W1-vm: surface an allocation failure from `jit_newarray` as a catchable
/// `java/lang/OutOfMemoryError`, mirroring the interpreter's `gc_alloc_array`
/// (runtime/interpreter.rs:853) OOM arm.
///
/// The `newarray` codegen site (`emit_post_alloc_oom_check` in `jit/src/x64.rs`)
/// null-checks RAX and, on the `0`/null OOM sentinel, bails to the shared
/// exception stub (returning the `i64::MIN` deopt sentinel + running the
/// epilogue). We stash the throwable in `JIT_PENDING_EXCEPTION` (the same
/// channel the void-return store helpers use for null-array NPEs) and the
/// interpreter's post-JIT general-exception drain
/// (`take_jit_pending_exception()` on the dispatch-aware return path) routes the
/// OOME through the JIT'd method's own exception table, giving a JIT'd
/// `newarray` identical catchable-OOM semantics to the interpreter. The bail
/// also forces the method `has_dispatch` (`emitted_alloc_oom_check` in `x64.rs`)
/// so that drain runs AND so `jit_thread_mut()` below is non-null.
///
/// If the OOME object itself cannot be constructed (e.g. the heap is too
/// exhausted to even allocate the throwable), we fall back to leaving the
/// flag unset and returning `0` — the legacy behaviour — so this change is
/// purely additive and never makes a previously-handled case worse.
#[cold]
fn jit_newarray_oom(vm: &SharedVm, length: usize) -> i64 {
    jit_alloc_oom(
        vm,
        &format!("Java heap space (alloc_array length {})", length),
    )
}

/// G1 last-ditch full mark cycle on allocation failure — see the
/// interpreter's `g1_force_full_cycle`: young pauses cannot reclaim dead
/// Old/humongous spans, only a completed mark cycle's cleanup can. Returns
/// `true` when the cycle was attempted (caller should retry the allocation
/// once before surfacing OOM).
#[cold]
fn jit_g1_last_ditch_full_cycle(vm: &SharedVm) -> bool {
    if !vm.mem.heap.is_g1() {
        return false;
    }
    // SAFETY: called only from the JIT allocation slow-path helpers, on a
    // mutator thread that entered compiled code through the JIT entry
    // trampoline — the same contract as the surrounding `jit_thread_mut`
    // calls in those helpers.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        crate::runtime::interpreter::g1_force_full_cycle(vm, thread);
        true
    } else {
        false
    }
}

/// Shared OOM signal for the fallible JIT allocation helpers — `jit_newarray`
/// (via `try_alloc_array`), `jit_anewarray_object` (via `try_alloc_array_full`),
/// and `jit_new_object` (via `try_alloc_object_full`). On heap exhaustion the
/// helper stashes a `java/lang/OutOfMemoryError` in `JIT_PENDING_EXCEPTION` and
/// returns the `0`/null sentinel; the alloc codegen site null-checks the result
/// and bails to the shared exception stub (`emit_post_alloc_oom_check` in
/// `x64.rs`, which also forces the method `has_dispatch`), after which the
/// interpreter's general-exception drain on the dispatch-aware return path routes
/// the OOME through the method's exception table — giving a JIT'd allocation the
/// same catchable-OOM semantics as the interpreter's `gc_alloc_array` /
/// `gc_alloc_object`, instead of the old SIGSEGV (null deref of the result) or
/// hard `alloc_young` abort. The object/array helpers go through the
/// `*_full` fallible paths so the old-generation spill of the non-fallible
/// `alloc_object` / `alloc_array` is preserved before OOM is reported.
///
/// If a fresh OOME object cannot be constructed (heap too exhausted to even
/// allocate the throwable / its message), fall back to the pre-allocated
/// singleton `OutOfMemoryError` (`SharedVm::singleton_oom`) so the OOM stays
/// catchable on a 100%-full heap instead of the materialization hard-aborting.
/// If the singleton is also absent (pre-allocation hasn't run yet), the flag is
/// left unset and `0` returned — the legacy behaviour, never worse.
#[cold]
fn jit_alloc_oom(vm: &SharedVm, msg: &str) -> i64 {
    // SAFETY: called only from a JIT alloc helper on the thread that installed the
    // JIT thread pointer; no other `&mut JvmThread` borrow is live here.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/OutOfMemoryError",
            Some(msg),
        ) {
            set_jit_pending_exception(exc);
            return 0;
        }
    }
    // Fresh creation failed (or no JIT thread) — use the pre-allocated singleton.
    if let Some(oom) = *vm.mem.singleton_oom.read() {
        set_jit_pending_exception(oom);
    }
    0
}

/// Negative-array-length signal for the fallible JIT array helpers
/// (`jit_newarray` / `jit_anewarray_object`). JLS requires
/// `java.lang.NegativeArraySizeException` (message = the offending length) for
/// `new T[n]` with `n < 0`. Mirroring `jit_alloc_oom`, this stashes the
/// exception in `JIT_PENDING_EXCEPTION` and returns the `0`/null sentinel; the
/// shared `emit_post_alloc_oom_check` bail then routes it through the method's
/// exception table (catchable), matching HotSpot and the interpreter. Reusing
/// the same channel + bail means no extra codegen is needed for the negative
/// case. As with OOM, if the exception object cannot be built the flag is left
/// unset and `0` returned.
#[cold]
fn jit_negative_array_size(vm: &SharedVm, length: i64) -> i64 {
    // SAFETY: called only from a JIT array helper on the thread that installed the
    // JIT thread pointer; no other `&mut JvmThread` borrow is live here.
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/NegativeArraySizeException",
            Some(&length.to_string()),
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
// SAFETY: `obj_ref` is a freshly-allocated, non-null array object whose header
// the allocator initialized; the function only writes that array's own
// length/element slots within bounds.
unsafe fn jit_newarray_finish(obj_ref: ObjectRef, atype: i64, length: i64) -> i64 {
    let raw = obj_ref.as_ptr();
    if crate::runtime::env_cache::jit_newarray_trace() {
        let class_id_raw = std::ptr::read(raw as *const u32);
        let kind_byte = *raw.add(4);
        let elem_byte = *raw.add(5);
        let stored_len = std::ptr::read(raw.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32);
        let num_slots = std::ptr::read(raw.add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
        eprintln!(
            "[JIT-NA] ptr={:p} atype={} len={} cid={} kind={} elem={} arrlen={} num_slots={}",
            raw, atype, length, class_id_raw, kind_byte, elem_byte, stored_len, num_slots
        );
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
    //   off 12: shape / full num_slots (4)
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
    // Compact reference-field layout: array_length (off 12) carries the body
    // size in bytes and gc_flags (off 21) gets GC_FLAG_COMPACT — matching the
    // inline header the JIT already wrote (idempotent), and required on the
    // non-skip path so the helper does not clobber them back to legacy.
    let compact_body = if cratonvm_types::compact_ref_fields_enabled() {
        cratonvm_types::class_layout(class_id_raw as u32)
            .filter(|l| l.field_count() == num_fields as usize)
            .map(|l| l.body_size)
    } else {
        None
    };
    let shape = if compact_body.is_some() {
        *(raw_ptr.add(cratonvm_types::GC_FLAGS_OFFSET) as *mut u8) =
            cratonvm_types::GC_FLAG_COMPACT;
        num_fields as u32
    } else {
        num_fields as u32
    };
    *(raw_ptr.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32) = shape;
    let hash = vm.mem.heap.next_identity_hash();
    *(raw_ptr.add(8) as *mut i32) = hash;

    // Family-A forensics (CRATONVM_DBG_A2, default-inert): record the
    // JIT-inline allocation into the a2dbg breadcrumb ring, exactly like the
    // interpreter TLAB fast path (`init_object_header`) and the gen_heap
    // allocators already do. Without this, the sweep's desync forensics
    // report "NO young alloc record" for every JIT-allocated object, which
    // makes the corrupt-header attribution ambiguous (allocated-then-clobbered
    // vs never-allocated-here).
    {
        let total = if let Some(body) = compact_body {
            HEADER_SIZE + body as usize
        } else {
            HEADER_SIZE + num_fields as usize * SLOT_SIZE
        };
        cratonvm_gc::a2dbg::record(
            raw_ptr as usize,
            class_id_raw as u32,
            0, // kind = Object
            0, // element_type = Reference
            compact_body.unwrap_or(0),
            num_fields as u32,
            total,
        );
    }

    // Reconstruct the typed handle and finish init.
    let obj_ref = cratonvm_types::ObjectRef::from_raw(raw_ptr);

    // Primitive-typed default values + JLS §12.6 finalizer registration.
    // Kept here (rather than inlined) because the JIT cannot synthesise
    // per-field descriptor reads without pre-resolving the full layout at
    // compile time; the per-class recipe cache makes the steady state
    // lock-free (see `jit_post_alloc_init`).
    jit_post_alloc_init(vm, obj_ref, class_id);

    if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
        eprintln!(
            "[JIT_ALLOC] post_tlab_init class_id={} obj=0x{:x}",
            class_id_raw, obj_ptr
        );
    }
    obj_ptr
}

/// Cached `CRATONVM_DBG_JIT_ALLOC` class-id filter (`None` = unset or
/// unparseable). PERF: the previous per-call `cratonvm_types::flags::runtime_var` in the two
/// allocation helpers was ~13% of binarytrees-18 wall time — getenv does a
/// linear scan of `environ` on every call.
fn dbg_jit_alloc_filter() -> Option<u32> {
    use std::sync::OnceLock;
    static F: OnceLock<Option<u32>> = OnceLock::new();
    *F.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_ALLOC")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
    })
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and num_fields must match the class metadata resolved at compile time.
// The returned i64 is a raw heap pointer to the newly allocated object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// DBG (CRATONVM_DBG_TLABMISS): count why `jit_new_object` fell past the
/// TLAB arm — the bimodal-bt18 discriminator. reason: 0=oversized,
/// 1=guarded-refill returned None, 2=JIT_THREAD TLS null. Prints a running
/// breakdown every 2^20 misses.
#[inline]
fn dbg_tlabmiss(reason: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TLABMISS").is_some()) {
        return;
    }
    static COUNTS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    let n = COUNTS[reason].fetch_add(1, Ordering::Relaxed) + 1;
    let total = COUNTS[0].load(Ordering::Relaxed)
        + COUNTS[1].load(Ordering::Relaxed)
        + COUNTS[2].load(Ordering::Relaxed);
    if total & 0xFFFFF == 0 || (n == 1 && reason == 2) {
        eprintln!(
            "[tlabmiss] oversized={} refill_none={} jit_thread_null={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
        );
    }
}

pub unsafe extern "C" fn jit_new_object(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.mem.heap;
    let class_id = ClassId::new(class_id_raw as u32);

    // JVMS §5.5 / §new: `new` must initialize its class before the object
    // is allocated -- same missing check, same shape of bug as the
    // `jit_getstatic`/`jit_putstatic_*` fixes above (see `jit_getstatic`'s
    // comment for the full hazard writeup). The interpreter's own `new`
    // handler (`runtime/interpreter.rs`, opcode `0xbb`) already calls
    // `ensure_class_initialized_shared` before it ever looks at
    // `num_total_fields`; this JIT slow-path helper never did, so a
    // JIT-compiled `new` that happens to be the first-ever touch of its
    // class (e.g. a rarely-taken `new`, reached only after the surrounding
    // method already tiered up to JIT via other branches) could allocate
    // and construct an instance of a class whose `<clinit>` — including any
    // static state the constructor itself reads — had not yet run.
    //
    // Run the check BEFORE any allocation-capacity probing below: a
    // `<clinit>` failure must be surfaced as the ordinary
    // `ExceptionInInitializerError` through the caller's exception table,
    // not after this helper has already committed heap state for an object
    // that will never be returned. Mirrors the existing OOM/negative-length
    // convention on this same helper family (`jit_alloc_oom`,
    // `jit_negative_array_size`): stash the exception via
    // `set_jit_pending_exception` and return the `0`/null sentinel, which
    // the codegen's EXISTING `emit_post_alloc_oom_check()` guard (already
    // emitted after every call site of this helper in `jit/src/x64.rs`,
    // 0xbb arm) already recognizes and routes through the exception table —
    // no codegen change needed for this fix, unlike `getstatic`/`putstatic`,
    // whose fallible-call convention had no existing post-call check at all.
    //
    // NOTE (scope): this only covers the ordinary allocation path. A `new`
    // site proven non-escaping by the JIT's scalar-replacement optimizer
    // (`self.scalar_replaced` in `jit/src/x64.rs`'s 0xbb codegen) elides the
    // call to this helper entirely and is NOT covered by this fix -- flagged
    // as a residual in docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md.
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
            use crate::error::MethodCallFailed;
            match err {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(exc);
                }
                MethodCallFailed::InternalError(vm_err) => {
                    let msg =
                        format!("JIT new class_id {class_id_raw} failed to initialize: {vm_err}");
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(exc);
                    }
                }
            }
            return 0;
        }
    }

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

    // GC-overhead limit (see jit_newarray): bail to catchable OOM if forced GCs
    // keep freeing almost nothing, instead of death-spiralling on slivers.
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm) {
        return jit_alloc_oom(
            vm,
            &format!("Java heap space (new_object class_id {class_id_raw} fields {num_fields})"),
        );
    }
    // JIT TLAB refill, round 2 (round 1 was reverted the same day: the
    // unconditional/naively-gated refill collapsed bt18 10.6s→40-460s from
    // young churn + per-allocation O(free-list) probes). This round routes
    // the slow path through `tlab_alloc_object_guarded_refill`, whose refill
    // arm only fires when young can supply the chunk WITHOUT a GC — an O(1)
    // bump-tail check plus the amortized-O(1) cached-bound early-exit
    // reclaimed-span probe (`young_has_free_block`) that round 1 lacked.
    // Post-sweep+coalesce young is a handful of big spans, so refills flow
    // at bump speed and the JIT's INLINE bump fast path comes back to life
    // for the following allocations; a genuinely-full young answers `false`
    // in O(1) and falls through to the historical old-gen spill below.
    if total_size <= cratonvm_gc::tlab::tlab_max_alloc() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(obj_ref) = crate::runtime::interpreter::tlab_alloc_object_guarded_refill(
                thread,
                vm,
                class_id,
                num_fields as usize,
                total_size,
            ) {
                jit_post_alloc_init(vm, obj_ref, class_id);
                if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
                    eprintln!(
                        "[JIT_ALLOC] new_object(tlab) class_id={} obj=0x{:x}",
                        class_id_raw,
                        obj_ref.as_ptr() as usize
                    );
                }
                return obj_ref.as_ptr() as i64;
            }
            dbg_tlabmiss(1); // guarded refill returned None
        } else {
            dbg_tlabmiss(2); // JIT_THREAD TLS pointer is null
        }
    } else {
        dbg_tlabmiss(0); // oversized for TLAB
    }
    // Fallible young → old-gen alloc (preserves alloc_object's old-gen spill).
    // A successful young-space probe above does not guarantee this allocation
    // will succeed: the TLAB refill may need a larger contiguous span than the
    // object itself, so it can fall through to an already-full old generation.
    // In that case force one orchestrated collection and retry before reporting
    // OOM.  This is essential for the JIT-active non-moving collector: its
    // in-place old sweep runs during that collection and can reclaim dead
    // promoted objects without relocating conservative JIT roots.
    let obj_ref = match heap.try_alloc_object_full(class_id, num_fields as usize) {
        Some(o) => o,
        None => {
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.tlab.retire();
                crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
            }
            if !crate::runtime::interpreter::gc_overhead_limit_exceeded(vm) {
                if let Some(obj_ref) = heap.try_alloc_object_full(class_id, num_fields as usize) {
                    obj_ref
                } else if !jit_g1_last_ditch_full_cycle(vm) {
                    return jit_alloc_oom(
                        vm,
                        &format!(
                            "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                        ),
                    );
                } else {
                    match heap.try_alloc_object_full(class_id, num_fields as usize) {
                        Some(o) => o,
                        None => {
                            return jit_alloc_oom(
                                vm,
                                &format!(
                                    "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                                ),
                            )
                        }
                    }
                }
            } else {
                return jit_alloc_oom(
                    vm,
                    &format!(
                        "Java heap space (new_object class_id {class_id_raw} fields {num_fields})"
                    ),
                );
            }
        }
    };
    // Initialize primitive-typed fields to proper JVM default values (zero
    // memory reads as Object(None) which is wrong for int/long/float/double
    // fields) + JLS §12.6 finalizer registration.
    jit_post_alloc_init(vm, obj_ref, class_id);
    if dbg_jit_alloc_filter() == Some(class_id_raw as u32) {
        eprintln!(
            "[JIT_ALLOC] new_object class_id={} obj=0x{:x}",
            class_id_raw,
            obj_ref.as_ptr() as usize
        );
    }
    obj_ref.as_ptr() as i64
}

/// Post-allocation init shared by the three JIT slow-path allocation arms:
/// primitive-field default values + JLS §12.6 finalizer registration.
///
/// Fast path: a lock-free per-class recipe from `SharedVm::jit_alloc_class_cache`
/// — no `class_manager.read()`, no hierarchy walk. First allocation of a class
/// computes the recipe under the read lock and publishes it; the legacy
/// two-lookup path remains as the fallback (cache opt-out via
/// `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE`, dense-range overflow, or a class not
/// yet in the store).
fn jit_post_alloc_init(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo, PrimKind};

    let cached = if alloc_class_cache_enabled() {
        match vm.jit.jit_alloc_class_cache.get(class_id.as_u32()) {
            Some(info) => Some(info),
            None => {
                // First slow-path allocation of this class: build the recipe
                // with the SAME walk as `jit_init_primitive_fields` so cached
                // and uncached behavior are bit-identical. Publish only a
                // complete recipe — bail to the legacy path if any class in
                // the hierarchy is missing from the store.
                let recipe = {
                    let cm = vm.classes.class_manager.read();
                    let store = &cm.class_store;
                    store.get(class_id).and_then(|root| {
                        let has_finalizer = root.has_finalizer;
                        let mut prim_inits: Vec<(u32, PrimKind)> = Vec::new();
                        let mut cid = Some(class_id);
                        while let Some(current_id) = cid {
                            let Some(class) = store.get(current_id) else {
                                return None;
                            };
                            let mut inst_idx = class.first_field_index;
                            for f in &class.fields {
                                if f.is_static() {
                                    continue;
                                }
                                let desc_first =
                                    f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                                let kind = match desc_first {
                                    b'I' | b'B' | b'C' | b'S' | b'Z' => Some(PrimKind::Int),
                                    b'J' => Some(PrimKind::Long),
                                    b'F' => Some(PrimKind::Float),
                                    b'D' => Some(PrimKind::Double),
                                    _ => None,
                                };
                                if let Some(kind) = kind {
                                    // Truncation-checked: field indices are bounded
                                    // by num_total_fields, far below u32::MAX.
                                    prim_inits.push((inst_idx as u32, kind));
                                }
                                inst_idx += 1;
                            }
                            cid = class.superclass;
                        }
                        Some(ClassAllocInfo {
                            has_finalizer,
                            prim_inits: prim_inits.into_boxed_slice(),
                        })
                    })
                };
                recipe.and_then(|r| vm.jit.jit_alloc_class_cache.insert(class_id.as_u32(), r))
            }
        }
    } else {
        None
    };

    if let Some(info) = cached {
        for &(inst_idx, kind) in info.prim_inits.iter() {
            let val = match kind {
                PrimKind::Int => Value::Int(0),
                PrimKind::Long => Value::Long(0),
                PrimKind::Float => Value::Float(0.0),
                PrimKind::Double => Value::Double(0.0),
            };
            vm.mem.heap.set_field(obj, inst_idx as usize, val);
        }
        if info.has_finalizer {
            vm.register_finalizable(obj.as_ptr() as usize);
        }
        return;
    }

    // Legacy fallback: per-allocation class-manager lookups.
    jit_init_primitive_fields(vm, obj, class_id);
    let has_fin = vm
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        vm.register_finalizable(obj.as_ptr() as usize);
    }
}

/// Initialize primitive-typed fields of a newly allocated object (JIT version).
fn jit_init_primitive_fields(vm: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    let cm = vm.classes.class_manager.read();
    let store = &cm.class_store;
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                let default = match desc_first {
                    b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
                    b'J' => Some(Value::Long(0)),
                    b'F' => Some(Value::Float(0.0)),
                    b'D' => Some(Value::Double(0.0)),
                    _ => None,
                };
                if let Some(val) = default {
                    vm.mem.heap.set_field(obj, inst_idx, val);
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
/// JIT monitorenter helper. The common path is the mark-word thin-lock CAS in
/// `MonitorTable::enter_or_contend`; only actual contention enters the
/// GC-blocked parking protocol. Returning the possibly remapped receiver gives
/// generated code an unambiguous non-sentinel success value.
pub unsafe extern "C" fn jit_monitor_enter(vm_ptr: i64, obj_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 {
        set_jit_pending_npe();
        return i64::MIN;
    }
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let Some((thread, _guard)) = jit_thread_mut() else {
        return i64::MIN;
    };
    let obj = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    crate::vm::vm_exec::monitor_enter_blocking(vm, thread, obj).as_ptr() as i64
}

#[cold]
fn stash_jit_monitor_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
) {
    use crate::error::{MethodCallFailed, VmError};
    match err {
        MethodCallFailed::ExceptionThrown(exc) => set_jit_pending_exception(exc),
        MethodCallFailed::InternalError(VmError::Runtime(runtime)) => {
            if let MethodCallFailed::ExceptionThrown(exc) =
                crate::runtime::exceptions::throw_runtime_error(vm, thread, runtime)
            {
                set_jit_pending_exception(exc);
            }
        }
        MethodCallFailed::InternalError(other) => {
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/InternalError",
                Some(&format!("JIT monitor operation failed: {other}")),
            ) {
                set_jit_pending_exception(exc);
            }
        }
    }
}

/// JIT monitorexit helper. A successful thin unlock is one release CAS. An
/// ownership failure becomes the ordinary catchable
/// `IllegalMonitorStateException` and is reported with the common JIT sentinel.
pub unsafe extern "C" fn jit_monitor_exit(vm_ptr: i64, obj_ptr: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if obj_ptr == 0 {
        set_jit_pending_npe();
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let Some((thread, _guard)) = jit_thread_mut() else {
        return i64::MIN;
    };
    let obj = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    match vm.threads.monitors.exit(obj, thread.thread_id) {
        Ok(()) => 1,
        Err(err) => {
            stash_jit_monitor_error(vm, thread, err);
            i64::MIN
        }
    }
}

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
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is a valid SharedVm pointer per the caller contract.
    let vm = &*(vm_ptr as *const SharedVm);
    if length < 0 {
        // Negative length → NegativeArraySizeException (JLS). See `jit_newarray`:
        // stash it in the pending-exception channel + return the 0/null sentinel
        // so the `anewarray` codegen's emit_post_alloc_oom_check bail routes it
        // through the method's exception table (catchable).
        return jit_negative_array_size(vm, length);
    }
    let heap = &vm.mem.heap;
    let class_id = ClassId::new(component_class_id_raw as u32);

    // CRIT (jit/gc audit, 2026-05): probe young-gen capacity and retire
    // the calling thread's TLAB before triggering GC. See
    // `jit_new_object` / `jit_newarray` for the full rationale —
    // without the retire, the heap walker steps into TLAB tail bytes
    // and mis-decodes them as object headers when GC fires from this
    // slow path.
    let data_size =
        cratonvm_types::array_data_size(length as usize, ArrayElementType::Reference).unwrap_or(0);
    let total_size = cratonvm_types::HEADER_SIZE + data_size;
    if heap.try_alloc_young_probe(total_size).is_none() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
        }
    }

    // GC-overhead limit (see jit_newarray): bail to catchable OOM if forced GCs
    // keep freeing almost nothing, instead of death-spiralling on slivers.
    if crate::runtime::interpreter::gc_overhead_limit_exceeded(vm) {
        return jit_alloc_oom(
            vm,
            &format!(
                "Java heap space (anewarray component {component_class_id_raw} length {length})"
            ),
        );
    }
    // Fallible young → humongous/old-gen alloc (preserves alloc_array's spill);
    // on exhaustion surface a catchable OutOfMemoryError instead of the hard
    // abort in alloc_young. The `anewarray` codegen's emit_post_alloc_oom_check
    // bails on the 0/null sentinel and routes the OOME through the method's
    // exception table (matching the interpreter's gc_alloc_array).
    let arr = match heap.try_alloc_array_full(
        class_id,
        ArrayElementType::Reference,
        length as usize,
    ) {
        Some(a) => a,
        None => {
            // G1 last-ditch full cycle + one retry (see jit_newarray).
            if !jit_g1_last_ditch_full_cycle(vm) {
                return jit_alloc_oom(
                    vm,
                    &format!(
                        "Java heap space (anewarray component {component_class_id_raw} length {length})"
                    ),
                );
            }
            match heap.try_alloc_array_full(class_id, ArrayElementType::Reference, length as usize)
            {
                Some(a) => a,
                None => {
                    return jit_alloc_oom(
                        vm,
                        &format!(
                            "Java heap space (anewarray component {component_class_id_raw} length {length})"
                        ),
                    )
                }
            }
        }
    };
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §baload: throw NullPointerException on null array reference.
        // Previously returned 0, which silently fabricated a zero byte and
        // masked real null-deref bugs in user code. Match the iaload/aaload
        // protocol: flag the pending NPE and return the deopt sentinel so the
        // post-JIT interpreter path throws on resume.
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_BYTE);
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr as i8 as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null aborts the process — see comment.
// Out-of-bounds is handled gracefully.
//
// JEP 358 inline-NPE-path update (2026-06): the inline null-check failure stub
// in `jit/src/x64.rs::emit_null_check_store_stubs` NO LONGER calls this helper —
// it now calls the dedicated `jit_npe_with_action(code)` so it can attach the
// correct per-element-type JEP-358 action (the old shared stub called
// `bastore(0)` and therefore fabricated `ASTORE_BYTE` for every array opcode).
// The `array_ptr == 0` null guard below is retained as honest defense-in-depth
// for any *direct* compiled call to this helper (it stays registered in
// `JitRuntimeHelpers` for ABI stability), and still must not read `index`/`val`
// on the null path. The fragile "zeroed-by-happenstance argument register" ABI
// coupling the previous comment described is gone with the shared-stub call.
pub unsafe extern "C" fn jit_bastore(array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
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
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_BYTE,
        );
        // Out-of-band deopt signal: the x64 `emit_null_check_store_stubs` stub
        // calls this helper with `array_ptr == 0` and then loads `i64::MIN` as
        // the method's return value, so flag the deopt to keep the interpreter
        // from mistaking a legitimate `Long.MIN_VALUE` return for one. (The
        // interpreter's NPE drain runs first and clears it; this preserves the
        // "every `i64::MIN` method-return sets the flag" invariant regardless.)
        set_jit_deopt_pending();
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §iaload: throw NullPointerException on null array reference.
        // Signal the interpreter via the pending-NPE flag + `i64::MIN` deopt
        // sentinel (same protocol as `jit_throw_aioobe`).
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_INT);
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §iastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_INT);
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §aaload: throw NullPointerException on null array reference.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_OBJECT,
        );
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * ref_element_size());
    // Degrade an implausible element reference to null instead of returning bits
    // the JIT will deref → SIGSEGV (the `0x8D8D..`-class stale ref). Mirrors
    // `read_prim_element`'s Reference arm; valid refs (or 0=null) pass through.
    let raw = read_ref_slot(elem_ptr);
    if cratonvm_types::plausible_heap_pointer(raw) {
        raw as i64
    } else {
        0
    }
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §aastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_OBJECT,
        );
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
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        return;
    }
    // JVMS §aastore covariance check: a non-null element whose runtime type is
    // NOT assignment-compatible with the array's component type must throw
    // ArrayStoreException. Mirror the interpreter `aastore` opcode so JIT and
    // interpreter agree. `aastore_element_assignable` fails open on imprecise
    // type info, so this is additive (never a false ArrayStoreException) — the
    // store still proceeds below for null elements and assignable references.
    if val != 0 {
        let vm = &*(vm_ptr as *const SharedVm);
        let array_ref = ObjectRef::from_raw(array_ptr as usize as *mut u8);
        let value_ref = ObjectRef::from_raw(val as usize as *mut u8);
        if vm.mem.heap.element_type_of(array_ref) == ArrayElementType::Reference
            && !crate::runtime::interpreter::aastore_element_assignable(vm, array_ref, value_ref)
        {
            // Build a real ArrayStoreException and stash it via the pending-
            // exception channel; the void return cannot carry the `i64::MIN`
            // deopt sentinel, so the interpreter's post-JIT-return drain
            // (`take_jit_pending_exception`) routes it through this method's
            // exception table. Skip the store (no element written on the
            // exception path). If we cannot obtain a thread or build the
            // throwable, fall through and perform the store rather than
            // corrupting VM state (degrades to the pre-fix behaviour only in
            // that rare construction-failure case).
            let elem_cls = vm
                .classes
                .class_manager
                .read()
                .get_class(vm.mem.heap.class_id_of(value_ref))
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "?".to_string());
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                    vm,
                    thread,
                    "java/lang/ArrayStoreException",
                    Some(&elem_cls),
                ) {
                    set_jit_pending_exception(exc);
                    return;
                }
            }
        }
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * ref_element_size()) as *mut u8;
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter's
    // `shared.mem.heap.satb_barrier(old_value)` at runtime/interpreter.rs:4228
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
    let old_raw = read_ref_slot(elem_ptr);
    if old_raw != 0 {
        let heap = heap_from_vm(vm_ptr);
        let old_obj = ObjectRef::from_raw(old_raw as usize as *mut u8);
        heap.satb_barrier(Value::Object(Some(old_obj)));
    }
    write_ref_slot(elem_ptr, val as u64);

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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §arraylength: throw NullPointerException on null array reference.
        // Previously returned -1, which JIT'd Java would happily compare against
        // and use as an array bound — masking real null-deref bugs in user code.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ARRAY_LENGTH,
        );
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

/// `(byte_offset, is_ref)` for a field of a **compact** object (one allocated
/// under `CRATONVM_COMPACT_REF_FIELDS`, marked `GC_FLAG_COMPACT` @ header byte
/// 21), else `None` for a legacy object. The JIT field helpers only get the raw
/// object pointer + resolved field index, so they read `class_id` (header
/// offset 0) and consult the per-class layout registry directly.
///
/// # Safety
/// `obj_ptr` must be non-null and point at a valid object header.
#[inline]
// SAFETY: callers validate that `obj_ptr` names a live object before using
// the compact-layout metadata derived from its header.
unsafe fn jit_compact_field_slot(
    obj_ptr: i64,
    field_index: i64,
) -> Option<(usize, cratonvm_types::FieldStorageKind)> {
    if field_index < 0 {
        return None;
    }
    // GC_FLAG_COMPACT is in the exported gc_flags byte.
    let gc_flags = std::ptr::read((obj_ptr as *const u8).add(cratonvm_types::GC_FLAGS_OFFSET));
    if gc_flags & cratonvm_types::GC_FLAG_COMPACT == 0 {
        return None;
    }
    let class_id = std::ptr::read(obj_ptr as *const u32); // class_id @ offset 0
    let field_count =
        std::ptr::read((obj_ptr as *const u8).add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
    let layout = cratonvm_types::class_layout_for_fields(class_id, field_count)?;
    Some((
        layout.field_offset(field_index as usize)? as usize,
        layout.field_storage(field_index as usize)?,
    ))
}

/// Byte pointer to a **primitive** field's 16-byte cell, honouring the compact
/// layout (primitive cells stay 16 bytes, only at a packed offset). Legacy
/// objects use the uniform `index * SLOT_SIZE`.
///
/// # Safety
/// `obj_ptr` must be non-null, point at a valid object header, and `field_index`
/// must be within the object's slot count (callers bounds-check first).
#[inline]
// SAFETY: callers bounds-check `field_index`, so the returned address remains
// within the live object's allocated field storage.
unsafe fn jit_field_cell_ptr(
    obj_ptr: i64,
    field_index: i64,
) -> (*mut u8, Option<cratonvm_types::FieldStorageKind>) {
    let (off, storage) = match jit_compact_field_slot(obj_ptr, field_index) {
        Some((o, storage)) => (o, Some(storage)),
        None => (field_index as usize * SLOT_SIZE, None),
    };
    ((obj_ptr as *mut u8).add(HEADER_SIZE + off), storage)
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr may be 0 (null), a valid heap pointer, or stale/corrupt raw bits from
// a miscompiled JIT frame; this helper validates it against the live heap before
// reading any object header. field_index is the resolved field slot index within
// the object layout. ptr::read is used because Value may contain non-Copy
// variants (ObjectRef).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_getfield(vm_ptr: i64, obj_ptr: i64, field_index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
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
    let vm = &*(vm_ptr as *const SharedVm);
    if vm.mem.heap.is_object_address(obj_ptr as usize).is_none() {
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
    // Compact layout: a reference field is a bare 8-byte pointer (0 = null,
    // matching the legacy `Object(Some(r)) => r.as_ptr()` / `Object(None) => 0`
    // result); a primitive field stays a 16-byte cell at its packed offset.
    if let Some((off, storage)) = jit_compact_field_slot(obj_ptr, field_index) {
        // SAFETY: off is within the object body (field_index < num_slots).
        let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + off);
        if storage.is_reference() {
            // Degrade an implausible reference (stale/garbage from a GC
            // root-coverage gap) to null instead of handing the JIT bits it
            // will later deref → SIGSEGV. Mirrors `read_prim_element`'s
            // Reference arm so interpreter and JIT decode a stale ref slot
            // identically. Valid refs (or 0=null) always pass through.
            let raw = read_ref_slot(ptr);
            return if cratonvm_types::plausible_heap_pointer(raw) {
                raw as i64
            } else {
                0
            };
        }
        // PLAIN-SLOT TEARING FIX (2026-07-06): was `std::ptr::read(ptr as
        // *const Value)` -- a non-atomic 16-byte copy that can tear against a
        // concurrent plain putfield on the SAME slot from another mutator
        // thread (interpreted OR JIT-compiled -- `jit_putfield_*` already
        // uses `write_value_atomic`, see commit 4e6b560f, but this read side
        // was never updated to match). Real JDK library code legally relies
        // on a plain field read/write being tear-free (e.g.
        // `ReentrantReadWriteLock$Sync`'s plain `firstReader`/
        // `firstReaderHoldCount`) -- see
        // docs/known-issues/elasticsearch-lucene-binary-docvalues-range-hangs.md
        // #3 for the interpreter-side counterpart of this same gap.
        let val: Value =
            cratonvm_types::read_compact_field(ptr, storage, std::sync::atomic::Ordering::Relaxed);
        return match val {
            Value::Int(i) => i as i64,
            Value::Long(l) => l,
            Value::Float(f) => f.to_bits() as i64,
            Value::Double(d) => d.to_bits() as i64,
            Value::Object(Some(r)) => {
                // Degrade an implausible (stale/garbage) object pointer to null
                // rather than handing the JIT bits it will deref → SIGSEGV.
                // Valid refs always pass the plausibility gate.
                let raw = r.as_ptr() as u64;
                if cratonvm_types::plausible_heap_pointer(raw) {
                    raw as i64
                } else {
                    0
                }
            }
            Value::Object(None) => 0,
            _ => 0,
        };
    }
    // SAFETY: obj_ptr is non-null and points to a live object, and field_index is now
    // verified < num_slots, so HEADER_SIZE + field_index * SLOT_SIZE is within the
    // object's allocated region.
    let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    // PLAIN-SLOT TEARING FIX (2026-07-06): see the matching note on the
    // compact-layout branch above -- was `std::ptr::read(ptr as *const
    // Value)`, non-atomic, tearable against a concurrent plain putfield.
    let val: Value = cratonvm_types::read_value_atomic(ptr as *const Value);
    let result = match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => {
            let raw = r.as_ptr() as u64;
            if cratonvm_types::plausible_heap_pointer(raw) {
                raw as i64
            } else {
                0
            }
        }
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
// SAFETY: `obj_ptr` must be non-null and canonical (caller's null check + the
// JIT's receiver discipline guarantee this); only the object's `num_slots`
// header word at offset 16 is read to bounds-check `field_index`.
#[inline]
unsafe fn jit_putfield_slot_in_bounds(obj_ptr: i64, field_index: i64) -> bool {
    if field_index < 0 {
        return false;
    }
    // shape/num_slots is a u32 at the exported NUM_SLOTS_OFFSET.
    let num_slots =
        std::ptr::read((obj_ptr as *const u8).add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
    (field_index as u64) < num_slots as u64
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
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
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    if crate::runtime::env_cache::jit_pfi_trace() {
        // Read existing value to see if we're overwriting a ref with an int
        let existing = if let Some(storage) = storage {
            cratonvm_types::read_compact_field(ptr, storage, std::sync::atomic::Ordering::Relaxed)
        } else {
            cratonvm_types::read_value_atomic(ptr as *const Value)
        };
        let cid_off = obj_ptr as *const u8;
        let cid: u32 = std::ptr::read(cid_off as *const u32);
        eprintln!("[JIT-PFI] obj=0x{:x} class_id={} field_index={} val=0x{:x} (val_as_i32={}) prev_value={:?}",
            obj_ptr as usize, cid, field_index, val as u64, val as i32, existing);
    }
    // Atomic per-word store: the concurrent GC marker may read this 16-byte
    // slot at the same time (it scans object fields concurrently). See
    // `cratonvm_types::write_value_atomic`.
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            Value::Int(val as i32),
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, Value::Int(val as i32));
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            Value::Long(val),
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, Value::Long(val));
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    let value = Value::Float(f32::from_bits(val as u32));
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            value,
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, value);
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    let value = Value::Double(f64::from_bits(val as u64));
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            value,
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, value);
    }
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
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
        } else {
            0
        };
        let val_kind = if val != 0 {
            let v_kind_ptr = (val as *const u8).add(4);
            std::ptr::read(v_kind_ptr)
        } else {
            0
        };
        let val_arrlen = if val != 0 {
            let len_ptr = (val as *const u8).add(12);
            std::ptr::read(len_ptr as *const u32)
        } else {
            0
        };
        if val != 0 && (val_kind > 1 || val_arrlen > 1_000_000) {
            eprintln!("[JIT-PFO] obj=0x{:x} obj_cid={} field_index={} val=0x{:x} val_cid={} val_kind={} val_arrlen=0x{:x}",
                obj_ptr as usize, obj_cid, field_index, val as u64, val_class_id, val_kind, val_arrlen);
        }
    }
    // Compact layout: reference fields are bare 8-byte pointers. Preserve the
    // SATB pre-barrier on the old 8-byte ref, then route through the heap's
    // compact-aware `set_field` (8-byte store + generational card barrier).
    if let Some((off, storage)) = jit_compact_field_slot(obj_ptr, field_index) {
        if !storage.is_reference() {
            heap_from_vm(vm_ptr).set_field(obj_ref, field_index as usize, value);
            return;
        }
        let heap = heap_from_vm(vm_ptr);
        // SAFETY: `off` is within the object body (slot bounds-checked above).
        let old_raw: u64 = read_ref_slot((obj_ptr as *const u8).add(HEADER_SIZE + off));
        if old_raw != 0 {
            heap.satb_barrier(Value::Object(Some(ObjectRef::from_raw(
                old_raw as usize as *mut u8,
            ))));
        }
        heap.set_field(obj_ref, field_index as usize, value);
        return;
    }
    let ptr = obj_ref
        .as_ptr()
        .add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter putfield's
    // `shared.mem.heap.satb_barrier(old_value)` at runtime/interpreter.rs:6391.
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
    // Atomic per-word read/write: the slot is read concurrently by the GC
    // marker and (possibly) written by another mutator thread; pair both ends
    // through the atomic helpers so the access is well-defined and tear-free.
    let old_value: Value = cratonvm_types::read_value_atomic(ptr as *const Value);
    if let Value::Object(Some(_)) = old_value {
        let heap = heap_from_vm(vm_ptr);
        heap.satb_barrier(old_value);
    }
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
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
// The interpreter calls `shared.mem.heap.satb_barrier(old_value)` at every
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
    vm.mem.heap.satb_barrier(Value::Object(Some(old_obj)));
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

    // JVMS §5.5 / §getstatic: the declaring class must be initialized
    // before its static storage is read. The interpreter's `getstatic`
    // handler (`runtime/interpreter.rs`) already calls
    // `ensure_class_initialized_shared` first; this JIT helper never did.
    // A JIT-compiled `getstatic` that happens to be the FIRST-EVER access
    // to that class's statics (e.g. a rarely-taken branch, such as
    // `LineWrapper.append`'s `shouldWrap ? FlushType.WRAP : nextFlush`
    // ternary, whose WRAP arm is only exercised once a line actually needs
    // wrapping -- well after the surrounding method has already tiered up
    // to JIT) silently read the zero-initialized placeholder
    // (`get_static_shared` returns `Value::Int(0)` for a class with no
    // `statics` entry yet) instead of running `<clinit>` first. Decoded as
    // a reference by the caller, that `Int(0)` becomes a null pointer --
    // `LineWrapper$FlushType.ordinal()` NPE, JIT-only (the interpreter
    // path always initializes the class on its own earlier `getstatic`,
    // masking this gap; see docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md,
    // "JavaPoet LineWrapper$FlushType NPE" JIT-only residual).
    //
    // Mirror the interpreter: ensure init before reading. On failure
    // (`<clinit>` threw -- JVMS wraps this as ExceptionInInitializerError),
    // stash the exception and return the `i64::MIN` deopt sentinel so the
    // caller's `emit_post_invoke_exception_check` (added alongside this
    // fix in `jit/src/x64.rs`'s `0xb2` getstatic codegen) routes it through
    // the method's exception table instead of pushing a bogus value.
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
            use crate::error::MethodCallFailed;
            match err {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(exc);
                }
                MethodCallFailed::InternalError(vm_err) => {
                    let msg = format!(
                        "JIT getstatic class_id {class_id_raw} field_index {field_index} failed to initialize: {vm_err}"
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(exc);
                    }
                }
            }
            return i64::MIN;
        }
    }

    // Bootstrap intercept: mirror the interpreter's System.out/err/in intercept.
    // The real JDK System.<clinit> isn't fully bootable; the interpreter returns
    // pre-built synthetic streams for these three fields. The JIT must do the same,
    // or jit_getstatic falls through to get_static_shared → Object(None) → null
    // receiver → println silently no-ops (arg0=0x0 in jit_invoke_dispatch).
    let field_name = vm
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .and_then(|c| {
            if &*c.name == "java/lang/System" {
                c.fields
                    .get(field_index as usize)
                    .map(|f| f.name.to_string())
            } else {
                None
            }
        });
    if let Some(ref fname) = field_name {
        if fname == "out" || fname == "err" {
            // Honor System.setOut/setErr: if the static field was explicitly set
            // (via setOut0/setErr0), use that value; otherwise fall back to the
            // canonical synthetic stream (same logic as the interpreter intercept).
            let overridden = match crate::vm::get_static_shared(vm, class_id, field_index as usize)
            {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            };
            let stream = match overridden {
                Some(s) => s,
                None => {
                    let (out, err) = vm.ensure_system_streams();
                    if fname == "out" {
                        out
                    } else {
                        err
                    }
                }
            };
            return stream.as_ptr() as i64;
        } else if fname == "in" {
            // System.in: same pattern — use the pre-built InputStream object.
            // `ensure_system_stdin_object` requires a mutable JvmThread which we
            // don't have here; fall back to the static field (set during init).
            let val = crate::vm::get_static_shared(vm, class_id, field_index as usize);
            return match val {
                Value::Object(Some(r)) => {
                    let raw = r.as_ptr() as u64;
                    if cratonvm_types::plausible_heap_pointer(raw) {
                        raw as i64
                    } else {
                        0
                    }
                }
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
        Value::Object(Some(r)) => {
            let raw = r.as_ptr() as u64;
            if cratonvm_types::plausible_heap_pointer(raw) {
                raw as i64
            } else {
                0
            }
        }
        Value::Object(None) => 0,
        _ => 0,
    }
}

// JVMS §5.5 / §putstatic: the declaring class must be initialized before
// its static storage is written, exactly like §getstatic (see the long
// comment on `jit_getstatic`'s fix for the full hazard writeup — this is
// the sibling bytecode, same missing check, same shape of bug). Beyond the
// generic "read/write before <clinit> ran" hazard, `putstatic` has its own
// sharper failure mode: `set_static_shared` (`vm/src/vm/vm_object.rs`)
// lazily creates the class's static-field `Vec` on FIRST write, sized from
// `class.fields.len()`, with no awareness of whether `<clinit>` has run.
// If a JIT `putstatic` writes first, a *later* `<clinit>` run (triggered by
// some other, unrelated access to the class) reaches the very same
// `set_static_shared` and unconditionally overwrites that slot with its own
// initializer value — silently discarding the JIT's write. Ordering
// `<clinit>` before the write (mirroring the interpreter's own `putstatic`
// handler, `runtime/interpreter.rs`, which calls
// `ensure_class_initialized_shared` before ever touching the operand
// stack/value) is the only sound fix.
//
// `putstatic` is `void`-returning, so unlike `jit_getstatic` there is no
// legitimate return value that could collide with the `i64::MIN` deopt
// sentinel — but the JIT call site still needs *some* signal to route a
// `<clinit>` failure through the method's exception table instead of
// silently proceeding to the (now-skipped) write. Mirrors the existing
// void-helper convention used by the `invokestatic` arraycopy dispatch call
// (`jit/src/x64.rs`, `emit_post_invoke_exception_check(b'V')` after
// `self.helpers.invoke_dispatch`): the helper always returns `i64`, `0` on
// the ordinary/no-exception path and `i64::MIN` when it stashed a pending
// exception via `set_jit_pending_exception`. The matching codegen change
// (`emit_post_invoke_exception_check(b'V')` after each `putstatic_*` call)
// lives in `jit/src/x64.rs`'s two `0xb3` arms.
//
// Shared by all five `jit_putstatic_*` helpers. Returns `Some(i64::MIN)`
// (the deopt sentinel, after stashing a Java exception) if `<clinit>`
// failed; the caller must return that value immediately without writing
// the field. Returns `None` when initialization already succeeded (or the
// per-thread JIT context isn't available — mirrors `jit_getstatic`'s same
// defensive fallback), meaning the caller should proceed with the write.
//
// `unsafe` purely to match the calling-convention contract of its `pub
// unsafe extern "C"` callers (the five `jit_putstatic_*` helpers, e.g.
// `jit_putstatic_int` just below) — every argument here is a safe
// reference/value already validated at that boundary.
#[inline]
// SAFETY: must run on the JIT-execution thread, which every
// `jit_putstatic_*` caller guarantees — that's the only precondition
// `jit_thread_mut()`'s per-thread context lookup and
// `set_jit_pending_exception` depend on here.
unsafe fn jit_putstatic_class_init_guard(vm: &SharedVm, class_id_raw: i64) -> Option<i64> {
    let class_id = ClassId::new(class_id_raw as u32);
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
            use crate::error::MethodCallFailed;
            match err {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(exc);
                }
                MethodCallFailed::InternalError(vm_err) => {
                    let msg = format!(
                        "JIT putstatic class_id {class_id_raw} failed to initialize: {vm_err}"
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(exc);
                    }
                }
            }
            return Some(i64::MIN);
        }
    }
    None
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_int(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is non-null and points to a valid SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Int(val as i32));
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_long(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Long(val));
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_float(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(
        vm,
        class_id,
        field_index as usize,
        Value::Float(f32::from_bits(val as u32)),
    );
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_double(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(
        vm,
        class_id,
        field_index as usize,
        Value::Double(f64::from_bits(val as u64)),
    );
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time. val is 0 (null) or a raw
// pointer to a live heap object, converted to Value::Object for storage in the static field table.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_object(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier — log the
    // OLD static value before overwriting. Mirrors interpreter putstatic
    // at runtime/interpreter.rs:5961.
    let old_static = crate::vm::get_static_shared(vm, class_id, field_index as usize);
    if let Value::Object(Some(_)) = old_static {
        vm.mem.heap.satb_barrier(old_static);
    }
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    crate::vm::set_static_shared(vm, class_id, field_index as usize, value);
    0
}

// ---------------------------------------------------------------------------
// Type check helpers
// ---------------------------------------------------------------------------

thread_local! {
    /// Resolved JIT type-check targets on this mutator. Compiled loops
    /// repeatedly execute the same checkcast/instanceof sites, whose class-name
    /// bytes live in the immutable JIT string table. Class IDs are stable for
    /// a VM, so `(vm, ptr, len)` is a complete cache key.
    ///
    /// This holds SEVERAL entries, not one. It used to be a single `Cell`, and
    /// because the key is the class-name *pointer*, two distinct type-check
    /// sites — even two `checkcast`s to the very same class, which get separate
    /// string-table entries — evicted each other on every iteration. Every
    /// execution then missed and re-ran `find_unique_class_by_name`, which
    /// allocates two host `String`s (`to_string()` plus a `'/'`→`'.'`
    /// `replace`) before it even looks anything up. Measured: in a loop whose
    /// body holds one type-check site the site costs ~64ns, and a second site
    /// in the same body costs ~1750ns *each* — a 27x cliff that any
    /// `instanceof` ladder or twice-casting method falls off. A short linear
    /// scan cannot thrash that way.
    static JIT_TYPECHECK_TARGET_CACHE:
        std::cell::RefCell<Vec<(usize, usize, usize, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// Memoized *positive* answers from `ClassManager::is_subclass_of` for the
    /// JIT type-check path, keyed by `(vm, child_class_id, parent_class_id)`.
    ///
    /// `is_subclass_of` is not cheap: it takes the process-wide
    /// `class_manager` read lock, allocates an `FxHashSet` visited set, and
    /// then DFS-walks the superclass chain *and* every transitively implemented
    /// interface. `jit_typecheck_resolve` calls it on every `checkcast` /
    /// `instanceof` whose receiver class is not *identical* to the target —
    /// i.e. on every genuinely polymorphic type check, which is the common
    /// case. A compiled loop doing `checkcast` on the result of a map lookup
    /// (`(Charset) cache.get(name)`, the shape
    /// `org.apache.tomcat.util.buf.CharsetCache.getCharset` has) therefore paid
    /// a lock acquisition, a heap allocation and a hierarchy walk per
    /// iteration. With several mutator threads in that loop the contended read
    /// lock dominated everything else.
    ///
    /// Only `true` answers are memoized, and that is deliberate. A class's
    /// superclass and interface lists are fixed when it is defined, so once a
    /// subtype relation holds it holds for the life of the `SharedVm` — a
    /// cached `true` can never go stale. A `false`, in contrast, can be
    /// observed while the hierarchy is still being populated (a supertype not
    /// yet in the class store makes that DFS branch stop early), so caching it
    /// could pin a wrong answer; those keep paying the full walk exactly as
    /// before. The cache is also bypassed entirely while any class redefine is
    /// in flight, matching `jit_hashmap_receiver_is_exact`.
    static JIT_SUBTYPE_POSITIVE_CACHE:
        std::cell::RefCell<Vec<(usize, u32, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Entry cap for [`JIT_SUBTYPE_POSITIVE_CACHE`]. Scanned linearly (the same
/// shape `jit_hashmap_string_node_cache` uses) rather than direct-mapped: a
/// direct-mapped table silently collides two hot pairs into one slot, and a
/// loop alternating between them then misses *every* time — measured at a 40%
/// miss rate over just five distinct receiver classes. A linear scan over this
/// many `(usize, u32, u32)` entries cannot collide and is a handful of
/// compares.
const JIT_SUBTYPE_CACHE_CAP: usize = 32;

/// Entry cap for [`JIT_TYPECHECK_TARGET_CACHE`]. One entry per distinct
/// type-check *site* reached on this thread; 64 covers a long `instanceof`
/// ladder plus the sites around it.
const JIT_TYPECHECK_TARGET_CACHE_CAP: usize = 64;

/// Record `(site key) -> resolved target class id`, evicting the oldest entry
/// once the cache is full.
fn jit_typecheck_target_cache_put(cache_key: (usize, usize, usize), target: ClassId) {
    JIT_TYPECHECK_TARGET_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let entry = (cache_key.0, cache_key.1, cache_key.2, target.as_u32());
        if let Some(slot) = cache.iter_mut().find(|(vm, ptr, len, _)| {
            *vm == cache_key.0 && *ptr == cache_key.1 && *len == cache_key.2
        }) {
            *slot = entry;
            return;
        }
        if cache.len() >= JIT_TYPECHECK_TARGET_CACHE_CAP {
            cache.remove(0);
        }
        cache.push(entry);
    });
}

/// `ClassManager::is_subclass_of` with the positive-answer memo described on
/// [`JIT_SUBTYPE_POSITIVE_CACHE`]. Behaviour-identical to calling
/// `is_subclass_of` directly: a hit can only ever replace a call that would
/// have returned `true` with `true`.
fn jit_is_subclass_of_cached(vm: &SharedVm, child: ClassId, parent: ClassId) -> bool {
    let vm_key = vm as *const SharedVm as usize;
    let key = (vm_key, child.as_u32(), parent.as_u32());
    let redefined = crate::classloading::any_class_redefined();
    if !redefined {
        let hit =
            JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| cache.borrow().iter().any(|e| *e == key));
        if hit {
            return true;
        }
    }
    let is_subclass = {
        vm.classes
            .class_manager
            .read()
            .is_subclass_of(child, parent)
    };
    if is_subclass && !redefined {
        JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= JIT_SUBTYPE_CACHE_CAP {
                cache.remove(0);
            }
            cache.push(key);
        });
    }
    is_subclass
}

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
    // GC-SAFETY (FMT-JIT-CCE): taken `&mut` so the slow path below can
    // refresh the caller's copy after a GC-triggering class load — see the
    // comment on that branch. Every read of `*obj_ref` in this function
    // after that point observes the refreshed address; `jit_checkcast`'s
    // caller reads it back too so its own return value isn't stale.
    obj_ref: &mut ObjectRef,
    class_name: &str,
    // SBR-03: `checkcast` is lenient (preserve native `Object[]`→`T[]` casts),
    // `instanceof` is strict (a genuine `Object[]` is not an instance of `I[]`).
    lenient: bool,
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
    if vm.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
        if let Some(src_desc) = crate::runtime::interpreter::array_descriptor_of(vm, *obj_ref) {
            // AUTHORITATIVE for an array receiver — do NOT fall through on a
            // negative answer.
            //
            // BUG-JIT-ARRAY-INSTANCEOF-20260726: this used to `return true`
            // only on success and otherwise drop into the class-hierarchy path
            // below, which compares `obj_class_id` against the target. For a
            // reference array `obj_class_id` is the header's *component* class
            // id (see the paragraph above), so `String[]` arrived at
            // `obj_class_id == target_class_id` with both sides equal to
            // `java/lang/String` and `instanceof` answered **true** for
            // `String[] instanceof String`. Same for `Integer[] instanceof
            // Integer`, and `is_subclass_of` extended it to interfaces, so
            // `String[] instanceof CharSequence` was true as well. Every one of
            // those is false in the interpreter, whose `Checkcast`/`InstanceOf`
            // handlers dispatch on array-ness and never reach a hierarchy
            // comparison — which is why this only reproduced JIT-on, and only
            // after warm-up.
            //
            // H2's `ObjectDataType.getTypeId` is a 15-arm `instanceof` ladder
            // over `Object`; once compiled it classified a `String[]` as
            // `TYPE_STRING`, so `StringType`'s generic bridge ran `checkcast
            // java/lang/String` on the array and `TestObjectDataType` died with
            // `ClassCastException: java.lang.String cannot be cast to
            // java.lang.String` (the message renders an array receiver by its
            // component name — a separate cosmetic defect that made this look
            // like a class-identity split for far longer than it should have).
            //
            // `array_is_assignable_to_impl` is complete for an array source: it
            // handles `Object`/`Serializable`/`Cloneable`, rejects every
            // non-array target, recurses on components, and implements the
            // `lenient` native-`Object[]`→`T[]` carve-out itself. The
            // hierarchy path below can only add the component-id collapse, so
            // there is nothing to fall through FOR.
            return if lenient {
                crate::runtime::interpreter::array_is_assignable_to(vm, &src_desc, class_name)
            } else {
                crate::runtime::interpreter::array_is_instance_of(vm, &src_desc, class_name)
            };
        }
        // No descriptor (a synthetic/incomplete array header): keep the
        // historical best-effort fall-through rather than hard-failing.
    }

    // Fast path: target already loaded. Most call sites hit this.
    //
    // IMPORTANT: bind the result to a local so the `RwLockReadGuard` temporary
    // from `.read()` is dropped at the semicolon. Using `if let Some(x) =
    // rwlock.read().method()` would extend the guard's lifetime to the entire
    // `if let` block (including the `else` branch), deadlocking any path that
    // later calls `load_class_concurrent` (which needs a write lock).
    let cache_key = (
        vm as *const SharedVm as usize,
        class_name.as_ptr() as usize,
        class_name.len(),
    );
    let cached_target = JIT_TYPECHECK_TARGET_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(cached_vm, cached_ptr, cached_len, _)| {
                *cached_vm == cache_key.0 && *cached_ptr == cache_key.1 && *cached_len == cache_key.2
            })
            .map(|(_, _, _, raw)| ClassId::new(*raw))
    });
    let target_class_id_opt = if cached_target.is_some() {
        cached_target
    } else {
        let resolved = vm
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(class_name);
        if let Some(target) = resolved {
            jit_typecheck_target_cache_put(cache_key, target);
        }
        resolved
    };
    if let Some(target_class_id) = target_class_id_opt {
        if obj_class_id == target_class_id {
            return true;
        }
        if jit_is_subclass_of_cached(vm, obj_class_id, target_class_id) {
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
        //
        // GC-SAFETY (FMT-JIT-CCE): `load_class_concurrent` parses/defines the
        // target class — allocating its `Class` mirror, constant-pool
        // strings, and method/field metadata, and potentially running a
        // user classloader's `loadClass`/`findClass` bytecode — any of which
        // can trigger a moving GC that relocates `*obj_ref`. Unlike the
        // interpreter's `Checkcast` handler (which pins the receiver in
        // `thread.native_pin_roots` around this exact class-resolution call
        // — see `runtime/interpreter.rs`), this JIT helper used to carry
        // `obj_ref` across the call unpinned and unrefreshed, so a GC landing
        // here left every later use of `*obj_ref` — the fallback checks
        // below, and critically `jit_checkcast`'s own returned pointer —
        // pointing at memory the collector had already reused. Observed live
        // as `ClassCastException: java.lang.Object cannot be cast to
        // org.jboss.as.controller.AttributeDefinition` (and other targets)
        // during WildFly's highly concurrent `parallel-extension-add` boot
        // step, where ~30 extension threads allocate/classload
        // simultaneously and a target interface like `AttributeDefinition`
        // is commonly resolved here for the first time under heavy GC
        // pressure. Pin `*obj_ref` in the current thread's
        // `native_pin_roots` (same mechanism, same pattern) across the call
        // and refresh it from the pin afterward.
        let current_thread = jit_get_current_thread();
        // Explicit `&mut *current_thread` reborrows (rather than letting
        // `.push()`/indexing implicitly autoref the raw pointer) — each is
        // scoped to a single statement and dropped well before
        // `load_class_concurrent` runs, so there's no borrow held across
        // that call (which may itself reborrow the same TLS thread pointer,
        // e.g. while running a classloader's bytecode).
        let pin_idx = if !current_thread.is_null() {
            let thread_ref: &mut JvmThread = &mut *current_thread;
            let idx = thread_ref.native_pin_roots.len();
            thread_ref.native_pin_roots.push(*obj_ref);
            Some(idx)
        } else {
            None
        };
        let load_result = vm.load_class_concurrent(class_name);
        if let Some(idx) = pin_idx {
            let thread_ref: &mut JvmThread = &mut *current_thread;
            *obj_ref = thread_ref.native_pin_roots[idx];
            thread_ref.native_pin_roots.truncate(idx);
        }
        if let Ok(target_class_id) = load_result {
            jit_typecheck_target_cache_put(cache_key, target_class_id);
            if obj_class_id == target_class_id {
                return true;
            }
            if jit_is_subclass_of_cached(vm, obj_class_id, target_class_id) {
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

    // Loader-duplication fallback (Residual 6,
    // `SpringBootContextLoaderAotTests`): this helper resolves the target by
    // NAME through the flat global `find_class_by_name`, which returns ONE
    // winner even when the same class was defined twice by two loaders (e.g.
    // Spring's AOT-processing child loader re-defining Groovy's `ClassInfo`).
    // The receiver's `ClassId` then never equals the resolved target's and the
    // id-based checks above wrongly refuse a cast the interpreter's
    // loader-faithful CP resolution would pass — under `checkcast` that
    // surfaced as a SILENT null (see `jit_checkcast`), observed live as
    // `ClassInfo.getClassInfo()` returning null only under `-Jit on`. Fall
    // back to a name-based hierarchy walk (supers + interfaces), mirroring
    // the accepted `is_subclass_of_by_name` tradeoff used for exception
    // catch_type resolution.
    if vm
        .classes
        .class_manager
        .read()
        .is_assignable_to_name(obj_class_id, class_name)
    {
        return true;
    }

    // Name-based fallback for synthetic classes whose interface relationships
    // are encoded in `synthetic_implements` rather than in the class hierarchy.
    if crate::runtime::interpreter::synthetic_implements_public(vm, obj_class_id, class_name) {
        return true;
    }
    // Instance-aware annotation-proxy admission (the proxy's real annotation
    // type lives on the heap object, not its shared ClassId). Reads
    // `*obj_ref`, which by this point reflects the refresh above if the slow
    // path ran.
    if crate::runtime::interpreter::annotation_proxy_satisfies_target(vm, *obj_ref, class_name) {
        return true;
    }

    // Array fallback: arrays with class_id 0 (e.g. from Array.newInstance via JIT)
    // lack class hierarchy entries.  EVERY array — primitive or reference — is an
    // Object and implements Serializable + Cloneable.
    //
    // The `[Ljava/lang/Object;` case is the subtle one. Under `instanceof`
    // (strict, `lenient == false`) only a *reference* array is an instance of
    // `Object[]`: a primitive array such as `byte[]` is NOT — `byte[] instanceof
    // Object[]` is `false`. The old code returned `true` here for *any* array,
    // which made JIT-compiled `Arrays.deepHashCode` take its `instanceof Object[]`
    // branch on a `byte[]` element, recurse `deepHashCode((Object[]) byteArray)`,
    // and dereference the raw byte payload as object pointers → SIGSEGV during
    // Hibernate jar/class scanning (BUG-scanning-crashes: JarVisitorTest /
    // ScannerTest / PackagedEntityManagerTest / SimpleTests). Gating the strict
    // path on the element kind fixes it. The lenient (`checkcast`) leniency is
    // preserved unchanged (SBR-03 keeps native `Object[]`→`T[]` casts working).
    if vm.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
        if class_name == "java/lang/Object"
            || class_name == "java/io/Serializable"
            || class_name == "java/lang/Cloneable"
        {
            return true;
        }
        if class_name == "[Ljava/lang/Object;"
            && (lenient || vm.mem.heap.element_type_of(*obj_ref) == ArrayElementType::Reference)
        {
            return true;
        }
    }

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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        if obj_ptr != 0 && cv_trace_enabled() {
            eprintln!(
                "[cv-checkcast-fail] implausible obj {:#x} -> silent null",
                obj_ptr
            );
        }
        return 0;
    }
    // Defensive: an unresolved typecheck site (no class_name attached) must
    // not silently allow the cast. Return 0 so the JIT-compiled code observes
    // a "failed cast" and falls back to the interpreter exception path.
    if class_name_len <= 0 || class_name_ptr.is_null() {
        if cv_trace_enabled() {
            eprintln!(
                "[cv-checkcast-fail] unresolved site, obj {:#x} -> silent null",
                obj_ptr
            );
        }
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let mut obj_ref = match vm.mem.heap.is_object_address(obj_ptr as usize) {
        Some(r) => r,
        None => {
            if cv_trace_enabled() {
                eprintln!(
                    "[cv-checkcast-fail] obj {:#x} FAILED is_object_address -> silent null",
                    obj_ptr
                );
            }
            return 0;
        }
    };
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let obj_class_id = vm.mem.heap.class_id_of(obj_ref);
    // checkcast: lenient (SBR-03).
    //
    // GC-SAFETY (FMT-JIT-CCE): `jit_typecheck_resolve` can trigger a moving
    // GC (via `load_class_concurrent` on a not-yet-loaded target) that
    // relocates `obj_ref`; it refreshes our local `obj_ref` in place through
    // the `&mut` when that happens. Return the (possibly refreshed)
    // `obj_ref.as_ptr()` on success, NOT the original `obj_ptr` argument —
    // returning the stale `obj_ptr` would hand the JIT-compiled caller a
    // dangling pointer into memory the collector already reused, which is
    // exactly what surfaced as `ClassCastException: java.lang.Object cannot
    // be cast to org.jboss.as.controller.AttributeDefinition` (and other
    // targets) during WildFly's concurrent `parallel-extension-add` boot.
    if jit_typecheck_resolve(vm, obj_class_id, &mut obj_ref, class_name, true) {
        obj_ref.as_ptr() as i64
    } else {
        if cv_trace_enabled() {
            let cm = vm.classes.class_manager.read();
            let obj_cls_name = cm
                .get_class(obj_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<none>".into());
            let target_cid = cm.find_unique_class_by_name(class_name);
            eprintln!(
                "[cv-checkcast-fail] typecheck REFUSED: obj={:#x} obj_cid={} obj_cls={} target_name={} target_cid={:?}",
                obj_ptr,
                obj_class_id.as_u32(),
                obj_cls_name,
                class_name,
                target_cid.map(|c| c.as_u32())
            );
        }
        // A definitive refusal — the object's class is known and provably not
        // assignable — must throw ClassCastException per JVMS §6.5.checkcast,
        // NOT silently hand the compiled caller a null (the old behavior,
        // which converted every genuine type error into downstream data
        // corruption — and is what kept Residual 6 invisible for seven
        // sessions). Stash the CCE and return the i64::MIN sentinel; the
        // codegen's post-helper check (`emit_post_invoke_exception_check`)
        // routes it through the standard pending-exception drain. The
        // stale/implausible-pointer and unresolved-site branches above keep
        // the old fail-soft `0` — there the object's type is UNKNOWABLE, and
        // throwing would turn tolerated stale-reference reads into new
        // failures.
        if let Some((thread, _jit_thread_guard)) = jit_thread_mut() {
            // Render an array receiver by its own descriptor. The header of a
            // reference array carries the COMPONENT class id, so the plain
            // `get_class(obj_class_id).name` lookup reports `java.lang.String`
            // for a `String[]` -- the self-cast text
            // `java.lang.String cannot be cast to java.lang.String` that cost
            // a session of misdiagnosis on TestObjectDataType. HotSpot prints
            // `[Ljava.lang.String;`.
            let obj_cls_name = match crate::runtime::interpreter::array_descriptor_of(vm, obj_ref)
            {
                Some(desc) => desc.replace('/', "."),
                None => vm
                    .classes
                    .class_manager
                    .read()
                    .get_class(obj_class_id)
                    .map(|c| c.name.replace('/', "."))
                    .unwrap_or_else(|| "<unknown>".into()),
            };
            let msg = format!(
                "class {} cannot be cast to class {}",
                obj_cls_name,
                class_name.replace('/', ".")
            );
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/ClassCastException",
                Some(&msg),
            ) {
                set_jit_pending_exception(exc);
                return i64::MIN;
            }
        }
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
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives. This is a
    // cheap, vm-independent pre-filter — kept ahead of the `vm_ptr` dereference
    // below so it alone guards the existing unit tests, which pass `vm_ptr = 0`.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return 0;
    }
    if class_name_len <= 0 || class_name_ptr.is_null() {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // Defense-in-depth beyond the bit-pattern-only `plausible_heap_pointer`
    // check above: that check has no view of the heap's actual mapped
    // extent, only whether the bits LOOK like a pointer (aligned,
    // in-range). A stale `ObjectRef` into memory the heap has since
    // reclaimed/reused past a GC root-coverage gap still passes it, and
    // the unchecked `ObjectRef::from_raw` + `class_id_of` this used to do
    // next would then read through a dangling pointer — observed live as
    // a SIGSEGV inside this function under concurrent executor load
    // (WildFly `EEConcurrencyExecutorShutdownTestCase`, see
    // docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md).
    // `is_object_address` additionally validates the address falls inside
    // a live heap region (and looks like a real header) before ever
    // dereferencing it, degrading a dangling reference to "not an
    // instance" instead of crashing — the same fallback every other stale-
    // reference guard in this codebase uses.
    let mut obj_ref = match vm.mem.heap.is_object_address(obj_ptr as usize) {
        Some(r) => r,
        None => return 0,
    };
    // SAFETY: class_name_ptr is non-null (checked above) and class_name_len > 0.
    // The pointer comes from the JIT string table which outlives this call.
    let class_name = match std::str::from_utf8(std::slice::from_raw_parts(
        class_name_ptr,
        class_name_len as usize,
    )) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let obj_class_id = vm.mem.heap.class_id_of(obj_ref);
    // instanceof: strict (SBR-03). `obj_ref` is passed `&mut` — see the
    // GC-SAFETY comment in `jit_checkcast` — so any GC triggered by
    // resolving a not-yet-loaded target class inside `jit_typecheck_resolve`
    // doesn't leave the fallback checks in that function reading through a
    // stale pointer. `instanceof` returns a bool, not a pointer, so there is
    // no analogous stale-return-value fix needed here.
    if jit_typecheck_resolve(vm, obj_class_id, &mut obj_ref, class_name, false) {
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
// Only stores two i64 values in a thread-local. The raw pointer is read solely
// by the explicitly-enabled diagnostic block below.
pub unsafe extern "C" fn jit_throw_aioobe(
    index: i64,
    length: i64,
    array_ptr: i64,
    bytecode_pc: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // TEMP DIAGNOSTIC (BigInteger.smallToString AIOOBE investigation,
    // 2026-07-17, `CRATONVM_DBG_AIOOBE3`): dump the raw ObjectHeader at the
    // pointer the JIT's bounds check actually compared against, to
    // distinguish "array genuinely under-allocated" (header's own
    // `array_length` field matches the reported `length`, and `forwarding_ptr`
    // is null) from "stale/forwarded pointer" (non-null `forwarding_ptr`, or
    // a header that doesn't look like a live long[] at all — the classic
    // unrooted-local-across-allocating-call signature this codebase has hit
    // repeatedly elsewhere). Best-effort raw read: the pointer came from a
    // live JIT register moments ago, so even if stale it should still point
    // at mapped (recycled, not unmapped) heap memory.
    if aioobe3_dbg() {
        if array_ptr != 0 {
            let base = array_ptr as usize as *const u8;
            let class_id = std::ptr::read_unaligned(base as *const u32);
            let kind = std::ptr::read_unaligned(base.add(4) as *const u8);
            let elem_ty = std::ptr::read_unaligned(base.add(5) as *const u8);
            let ident_hash = std::ptr::read_unaligned(base.add(8) as *const i32);
            let arr_len_hdr = std::ptr::read_unaligned(
                base.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32,
            );
            let num_slots =
                std::ptr::read_unaligned(base.add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
            let gc_age =
                std::ptr::read_unaligned(base.add(cratonvm_types::GC_AGE_OFFSET) as *const u8);
            let gc_flags =
                std::ptr::read_unaligned(base.add(cratonvm_types::GC_FLAGS_OFFSET) as *const u8);
            let fwd_ptr = std::ptr::read_unaligned(
                base.add(cratonvm_types::FORWARDING_PTR_OFFSET) as *const usize
            );
            eprintln!(
                "[AIOOBE3-DIAG] bci={bytecode_pc} jit-reported index={index} length={length} array_ptr={array_ptr:#x} \
header: class_id={class_id} kind={kind} elem_ty={elem_ty} ident_hash={ident_hash} \
array_length_field={arr_len_hdr} num_slots={num_slots} gc_age={gc_age} gc_flags={gc_flags} \
forwarding_ptr={fwd_ptr:#x}"
            );
        } else {
            eprintln!(
                "[AIOOBE3-DIAG] bci={bytecode_pc} jit-reported index={index} length={length} array_ptr=NULL"
            );
        }
    }
    JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
    // Out-of-band deopt signal: this `i64::MIN` IS the bounds-check stub's
    // method return value, so flag it as a genuine deopt so the interpreter
    // doesn't mistake a method legitimately returning `Long.MIN_VALUE` for one.
    set_jit_deopt_pending();
    i64::MIN // deopt sentinel — interpreter will detect and throw AIOOBE
}

/// Cached `CRATONVM_DBG_AIOOBE3` gate: temp diagnostic companion to
/// [`jit_throw_aioobe`] for the BigInteger.smallToString AIOOBE
/// investigation (2026-07-17) — see that function's doc comment.
#[inline]
fn aioobe3_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_AIOOBE3").is_some())
}

/// RBC.6 — trace exception routing through `route_implicit_exc_through_callee`
/// / `route_jit_exception_through_method` (which entry each takes, resolved
/// handler pc). Cached read-once like the other `*_dbg()` gates in this file.
#[inline]
pub(crate) fn rbc6_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_RBC6").is_some())
}

/// Direct-throw for `ArithmeticException` ("/ by zero") — the div-by-zero
/// sibling of [`jit_throw_aioobe`]. The x64 `idiv`/`irem`/`ldiv`/`lrem`
/// zero-divisor guard jumps to a stub that calls this and immediately runs the
/// method epilogue. We set the pending-arithmetic flag and the out-of-band deopt
/// signal, then return the `i64::MIN` sentinel; the interpreter's JIT-return
/// drain throws a real `ArithmeticException` through the method's exception table
/// WITHOUT re-running the method from entry. Re-running (the previous
/// `uncommon_trap` path) double-executed any side effect that preceded the trap,
/// diverging from HotSpot.
///
/// Same Windows platform rationale as [`jit_throw_aioobe`]: JIT frames have no
/// SEH unwind tables, so a Rust panic/unwind here would terminate the process;
/// thread-local stashing sidesteps that.
// SAFETY: Called from JIT-compiled code at a div-by-zero guard. Sets two
// thread-locals and returns a sentinel; no pointer dereferences.
pub unsafe extern "C" fn jit_throw_arithmetic() -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_SIGNALS.with(|s| s.arithmetic.set(true));
    set_jit_deopt_pending();
    i64::MIN // deopt sentinel — interpreter will detect and throw ArithmeticException
}

/// RBC.6 (athrow codegen) — stash the thrown exception object as the
/// pending JIT exception and return the `i64::MIN` deopt sentinel. The
/// x64 `athrow` arm calls this and immediately runs the method epilogue;
/// the interpreter's JIT-return drains (`take_jit_pending_exception` on
/// every dispatch-aware return path) route the exception through the
/// caller's handling. `exc_ptr == 0` (athrow on a null reference) sets
/// the pending-NPE flag instead, per JVMS athrow semantics.
///
/// `bci` is the bytecode pc of this `athrow` instruction — a compile-time
/// immediate the x64 codegen bakes into the call site (RBC.6 correctness
/// fix, see `JitSignals::athrow_bci`). Stashed alongside the exception so
/// `execute_jit_call` can give `route_jit_exception_through_method` a real
/// `throw_pc` instead of `usize::MAX`, which — for a method with 2+
/// exception-table entries whose catch types are in a subtype relationship —
/// can match the WRONG entry (declaration-order, type-only) regardless of
/// which try-region actually threw. `exc_ptr == 0` still routes to the NPE
/// flag (no bci needed there — `jit_pending_npe` has no such ambiguity path
/// yet).
///
/// Same platform rationale as [`jit_throw_aioobe`]: JIT frames have no
/// SEH unwind tables on Windows, so a Rust panic/unwind here would
/// terminate the process; thread-local stashing sidesteps that.
// SAFETY: Called from JIT-compiled code at an athrow site. `exc_ptr` is
// either 0 or the heap pointer the JIT popped from the operand stack;
// no dereference happens here — it is only wrapped and stored in a TLS.
// `bci` is a bare immediate (no pointer semantics).
pub unsafe extern "C" fn jit_throw_exception(exc_ptr: i64, bci: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if exc_ptr == 0 {
        stash_jit_pending_npe();
    } else {
        set_jit_pending_exception_with_bci(ObjectRef::from_raw(exc_ptr as usize as *mut u8), bci);
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
    /// Owns JIT code while this thread-local raw entry remains published.
    _owner: Option<std::sync::Arc<cratonvm_jit::CompiledMethod>>,
}

#[derive(Clone, Copy)]
struct NativeDispatchCache {
    receiver_class_id: u32,
    callback: cratonvm_native_api::NativeCallback,
    kind: ObjectNativeKind,
}

#[derive(Clone, Copy)]
enum ObjectNativeKind {
    HashMap,
    Matcher,
    /// Registered `java/lang/StringBuilder` natives (the `append(I)/(C)/
    /// (String)` family + `toString`/`length`). Exact-receiver-guarded like
    /// the other kinds; resolution consults the native registry ONCE at
    /// cache-fill time instead of a 3-string hash per call.
    StringBuilder,
}

#[derive(Clone, Copy)]
enum IntegerNativeKind {
    ValueOf,
    IntValue,
}

#[derive(Clone, Copy)]
struct IntegerNativeDispatchCache {
    kind: IntegerNativeKind,
    callback: cratonvm_native_api::NativeCallback,
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
    static OBJECT_NATIVE_DISPATCH_CACHE:
        std::cell::RefCell<rustc_hash::FxHashMap<usize, NativeDispatchCache>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    static INTEGER_NATIVE_DISPATCH_CACHE:
        std::cell::RefCell<rustc_hash::FxHashMap<usize, Option<IntegerNativeDispatchCache>>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Real `java/lang/Integer` class discovered from the first ordinary
    /// `valueOf` result in each VM. A new VM pointer invalidates the entry.
    static INTEGER_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// Exact real-JDK `java/util/regex/Matcher` class id, discovered once per
    /// VM for the virtual-MIC native fast path.
    static MATCHER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    // Virtual/interface call sites are keyed by their JIT metadata pointer AND
    // the receiver's actual class id. A static CP owner is not sound here:
    // an interface method may resolve to a receiver override.
    static VIRTUAL_DISPATCH_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<(usize, u32), DispatchCache>>
        = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    static VIRTUAL_DISPATCH_COUNTER: std::cell::RefCell<rustc_hash::FxHashMap<(usize, u32), u32>>
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

    /// Last C1→C2 supersede epoch this thread's DISPATCH_CACHE was flushed
    /// at — see the flush in `jit_invoke_dispatch`.
    static DISPATCH_CACHE_SUPERSEDE_EPOCH: Cell<u32> = const { Cell::new(0) };
    static DISPATCH_CACHE_JIT_GENERATION: Cell<u64> = const { Cell::new(0) };
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

#[cold]
fn throwable_class_name(vm: &SharedVm, obj: ObjectRef) -> Option<String> {
    let cid = vm.mem.heap.class_id_of(obj);
    vm.classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
}

#[cold]
fn throwable_detail_message(vm: &SharedVm, obj: ObjectRef) -> Option<String> {
    let cid = vm.mem.heap.class_id_of(obj);
    let msg_ref = {
        let cm = vm.classes.class_manager.read();
        let idx =
            crate::vm::resolve_field_index_in_hierarchy(cid, "detailMessage", &cm.class_store)?;
        match vm.mem.heap.get_field(obj, idx) {
            Value::Object(Some(s)) => s,
            _ => return None,
        }
    };
    if throwable_class_name(vm, msg_ref).as_deref() != Some("java/lang/String") {
        return None;
    }
    crate::vm::read_java_string(&vm.mem.heap, msg_ref)
}

#[cold]
fn is_dispatch_no_such_method_miss(
    vm: &SharedVm,
    err: &crate::error::MethodCallFailed,
    dispatch_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    use crate::error::{LinkageError, MethodCallFailed, VmError};

    match err {
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoSuchMethodError {
            class_name,
            method_name: err_method,
            method_descriptor,
        })) => {
            class_name == dispatch_class
                && err_method == method_name
                && method_descriptor == descriptor
        }
        MethodCallFailed::ExceptionThrown(exc)
            if throwable_class_name(vm, *exc).as_deref() == Some("java/lang/NoSuchMethodError") =>
        {
            let expected = format!("{dispatch_class}.{method_name}{descriptor}");
            throwable_detail_message(vm, *exc).as_deref() == Some(expected.as_str())
        }
        _ => false,
    }
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
    use crate::error::{ClassFileError, MethodCallFailed, RuntimeError, VmError};
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
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
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
            let converted =
                crate::runtime::exceptions::throw_linkage_error(vm, thread, linkage_err);
            match converted {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(exc);
                    }
                }
            }
        }
        // A class-resolution miss in the resolver surfaces as
        // `VmError::ClassFile(ClassNotFound)`. The interpreter maps this
        // to `NoClassDefFoundError` (see `raise_no_class_def_found`); do
        // the same here so JIT-dispatched callees behave identically.
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            ref class_name,
        })) => {
            // `CRATONVM_DBG_LINKAGE_BT=1` -- the third place a
            // `NoClassDefFoundError` reaches Java (see the matching hooks in
            // `runtime::exceptions`). Only the Rust backtrace names the JIT
            // dispatch site that could not resolve the class.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LINKAGE_BT").is_some() {
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("[DBG_LINKAGE_BT] jit NoClassDefFoundError {class_name}\n{bt}");
            }
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/NoClassDefFoundError",
                Some(class_name),
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
                vm,
                thread,
                "java/lang/InternalError",
                Some(&msg),
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
                // SAFETY: `p` is non-null (checked above) and `i < num_args`, so
                // `p.add(i)` stays inside the JIT-provided args slice (debug path only).
                let v = unsafe { *p.add(i) };
                buf.push_str(&format!(" arg{}=0x{:x}", i, v));
            }
        }
        eprintln!(
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{} info_ptr=0x{:x}",
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            num_args,
            buf,
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

    // The SATB flush above can participate in a moving collection before the
    // helper reads the JIT caller's raw argument array.  Forward every
    // descriptor-declared reference, not only `this`: a compiled callee must
    // not receive a stale byte-array/object argument after a moving GC.
    let forwarded_args = forward_jit_reference_args(vm, info, args_slice);
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);

    // JIT dispatch normally calls a custom loader's inherited bytecode
    // directly. ClassLoader's resource methods must throw NPE for a null name
    // before that bytecode runs; the sentinel routes it through Java handlers.
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }

    let info_key = info_ptr as usize;
    // Cached exact-receiver native fast path — the FIRST per-callsite probe. The
    // resolution/insertion slow path stays further down (after the compile
    // probes); this early block only serves sites the cache has already
    // resolved. Rationale: `HashMap.put/get` and the real-layout Matcher
    // operations are registered natives with no
    // bytecode, so neither the Integer cache below nor the virtual-dispatch
    // machinery can ever serve them — yet every map call paid those probes
    // first. Probing the map cache first costs the (now direct-called on
    // x64, hence rarely dispatched) Integer sites one extra hash lookup and
    // saves one on every cached native operation. The receiver class-id equality
    // check preserves the exact-receiver guard; the `any_class_redefined`
    // gate matches the resolution site below. Runs ahead of the recursion
    // depth guard like the Integer block: the cached callbacks are native
    // leaves that never re-enter JIT code.
    if matches!(info.invoke_kind, 0 | 2)
        && !args_slice.is_empty()
        && !crate::classloading::any_class_redefined()
    {
        let cached =
            OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        if let Some(entry) = cached {
            let receiver_raw = args_slice[0] as u64;
            if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
                if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                    if vm.mem.heap.class_id_of(receiver).as_u32() == entry.receiver_class_id {
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            if let Some(result) = call_object_native_raw(
                                vm, thread, info, receiver, args_slice, entry,
                            ) {
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }
    if !crate::classloading::any_class_redefined() {
        let cached =
            INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        let integer_native = match cached {
            Some(entry) => entry,
            None => {
                let entry = match (info.class_name, info.method_name, info.descriptor) {
                    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => {
                        Some(IntegerNativeDispatchCache {
                            kind: IntegerNativeKind::ValueOf,
                            callback: cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
                        })
                    }
                    ("java/lang/Integer", "intValue", "()I") => {
                        Some(IntegerNativeDispatchCache {
                            kind: IntegerNativeKind::IntValue,
                            callback: cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_int_value,
                        })
                    }
                    _ => None,
                };
                INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| {
                    cache.borrow_mut().insert(info_key, entry);
                });
                entry
            }
        };
        if let Some(entry) = integer_native {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(result) = call_integer_native_raw(vm, thread, info, args_slice, entry) {
                    return result;
                }
            }
        }
    }
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
    let redefine_jit_quiesced = crate::classloading::any_class_redefined();
    if redefine_jit_quiesced {
        DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
        VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
    }
    // Every compiled publication/invalidation advances this generation. Flush
    // raw-entry dispatch caches before probing them, both to pick up tier
    // replacements and to release their code owners after invalidation.
    {
        let generation = cratonvm_jit::jit_cache_generation();
        DISPATCH_CACHE_JIT_GENERATION.with(|seen| {
            if seen.get() != generation {
                seen.set(generation);
                DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
                VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
            }
        });
    }
    // Retain the older supersede epoch as a compatibility signal for tiering
    // paths that may advance it independently of a cache replacement. The
    // general JIT generation above is the lifetime-safety mechanism.
    {
        let epoch = crate::classloading::jit_supersede_epoch();
        DISPATCH_CACHE_SUPERSEDE_EPOCH.with(|e| {
            if e.get() != epoch {
                e.set(epoch);
                DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
                VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
            }
        });
    }

    // A JIT call-site's CP owner is often an interface. Resolve once on the
    // actual receiver class, then cache and directly enter the compiled
    // concrete body. The generic helper otherwise re-enters invoke_virtual on
    // every element access, rebuilding conservative JIT roots each time.
    if direct_virtual_compiled_callee_entry_enabled()
        && !statically_bound
        && !redefine_jit_quiesced
        && !args_slice.is_empty()
    {
        let raw = args_slice[0] as u64;
        if raw != 0 && (raw & 7) == 0 && raw < (1u64 << 48) {
            let receiver = ObjectRef::from_raw(raw as usize as *mut u8);
            let receiver_cid = vm.mem.heap.class_id_of(receiver);
            let target = virtual_dispatch_target_for_receiver(vm, receiver, info);
            let globally_named = target.cacheable_receiver
                && vm
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(&target.class_name)
                    == Some(receiver_cid);
            // RBC.6 perf follow-up (docs/feature-designs/jit-local-exception-handlers.md)
            // — this used to also require `!mic_callee_has_exception_table(...)`,
            // excluding ANY callee that declares a local exception table from
            // this cache entirely and forcing every such call through the
            // "generic helper" fallback this comment block warns is expensive
            // ("re-enters invoke_virtual on every element access, rebuilding
            // conservative JIT roots each time"). Unlike the INLINE machine-code
            // MIC/PIC cascade (`jit/src/x64.rs`, guarded by the SAME check at its
            // own publish sites — see `BUG-H` comments there — which really does
            // bypass Rust-level exception routing since it CALLs the raw entry
            // pointer directly from compiled machine code), THIS cache is a plain
            // Rust `HashMap` consulted from inside this same Rust function — a hit
            // still calls `try_call_compiled_entry_reentrant` and then
            // `route_implicit_exc_through_callee` below EXACTLY as a cache miss
            // would, so the callee's own exception table is routed identically
            // either way. Excluding it here bought no correctness and cost a real
            // ~450s/round regression for `Response.toAbsolute()` once RBC.6 let it
            // compile (confirmed via the Tomcat suite's `TestResponsePerformance`
            // on the Linux build host).
            if globally_named {
                let key = (info_key, receiver_cid.as_u32());
                if let Some(cached) = VIRTUAL_DISPATCH_CACHE
                    .with(|dc| dc.borrow().get(&key).map(|c| (c.entry, c.needs_context)))
                {
                    if let Some(rc) =
                        try_call_compiled_entry_reentrant(cached.0, cached.1, vm_ptr, args_slice)
                    {
                        return route_implicit_exc_through_callee(vm, info, args_slice, rc);
                    }
                } else {
                    let should_compile = VIRTUAL_DISPATCH_COUNTER.with(|dc| {
                        let mut counts = dc.borrow_mut();
                        let count = counts.entry(key).or_insert(0);
                        *count += 1;
                        *count == crate::runtime::env_cache::jit_invocation_threshold()
                    });
                    if should_compile {
                        if let Some((_callee_pin, entry, needs_context)) =
                            crate::runtime::interpreter::try_jit_compile_callee(
                                vm,
                                &target.class_name,
                                info.method_name,
                                info.descriptor,
                                true,
                            )
                        {
                            // `_callee_pin` keeps the callee mapped across the
                            // cache publication and the direct call below.
                            // Only publish a raw entry we can keep alive:
                            // `_owner` is the sole keep-alive for this
                            // thread-local pointer, so caching with `None`
                            // would let a later tier-up `put` unmap the body
                            // under it.
                            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                                VIRTUAL_DISPATCH_CACHE.with(|dc| {
                                    dc.borrow_mut().insert(
                                        key,
                                        DispatchCache {
                                            entry,
                                            needs_context,
                                            _owner: Some(owner),
                                        },
                                    );
                                });
                            }
                            if let Some(rc) = try_call_compiled_entry_reentrant(
                                entry,
                                needs_context,
                                vm_ptr,
                                args_slice,
                            ) {
                                return route_implicit_exc_through_callee(vm, info, args_slice, rc);
                            }
                        }
                    }
                }
            }
        }
    }

    let cached_entry = if direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !redefine_jit_quiesced
    {
        DISPATCH_CACHE.with(|dc| {
            dc.borrow()
                .get(&info_key)
                .map(|c| (c.entry, c.needs_context))
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
        if let Some(rc) = try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice) {
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
    if direct_static_compiled_callee_entry_enabled() && statically_bound && !redefine_jit_quiesced {
        // `JitInvokeInfo` carries only the static-CP class NAME baked into this
        // compiled call site at codegen time, not a loader-scoped `ClassId` —
        // extending the raw `extern "C"` JIT ABI to also pass the caller's
        // `ClassId` here would require touching the x64 codegen call-site
        // emission itself. Resolving the name globally preserves this path's
        // existing (already name-based, not loader-aware) behavior unchanged;
        // it does not newly introduce the multi-loader-same-name collision —
        // see `JitKey::declaring_class_id`'s doc comment for the interpreter-
        // side fix this mirrors.
        let info_class_id = vm
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(info.class_name)
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let jit_cache = vm.jit.jit_cache.read();
        if let Some(compiled) = jit_cache.get(
            info.class_name,
            info.method_name,
            info.descriptor,
            info_class_id,
        ) {
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
                dc.borrow_mut().insert(
                    info_key,
                    DispatchCache {
                        entry,
                        needs_context: needs_ctx,
                        _owner: Some(compiled.clone()),
                    },
                );
            });
            // SAFETY: entry was obtained from a CompiledMethod in the JIT cache, whose
            // entry_ptr points to executable memory with the correct extern "C" ABI.
            // CRIT round-5 fix: on >ARG_REGS args, route directly to the interpreter
            // via `bail_to_interpreter` rather than silently returning 0 (the original
            // wave-2 fall-through was already correct; this just makes the bail
            // explicit at the call site to match the MIC fast-path).
            if let Some(rc) =
                try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice)
            {
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
    let should_compile = direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !redefine_jit_quiesced
        && DISPATCH_COUNTER.with(|dc| {
            let mut map = dc.borrow_mut();
            let count = map.entry(info_key).or_insert(0);
            *count += 1;
            *count == crate::runtime::env_cache::jit_invocation_threshold()
        });
    if should_compile {
        // Try to compile the callee and cache it
        if let Some((_callee_pin, entry, needs_ctx)) = try_compile_callee(vm, info) {
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/compile] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // See the virtual-dispatch sibling above: an unowned raw entry
            // must not be cached.
            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                DISPATCH_CACHE.with(|dc| {
                    dc.borrow_mut().insert(
                        info_key,
                        DispatchCache {
                            entry,
                            needs_context: needs_ctx,
                            _owner: Some(owner),
                        },
                    );
                });
            }
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. CRIT round-5 fix: bail explicitly to the interpreter on
            // >ARG_REGS args via `bail_to_interpreter` (matches the MIC fast-path).
            if let Some(rc) =
                try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice)
            {
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

    // Registered natives cannot populate DISPATCH_CACHE because they have no
    // compiled entry pointer. Cache the callback, decoder kind, and exact
    // receiver ClassId for the hot monomorphic HashMap and Matcher sites
    // instead of repeating virtual resolution and a failed compile probe on
    // every iteration.
    let object_native_kind = hashmap_native_arg_count(info)
        .map(|_| ObjectNativeKind::HashMap)
        .or_else(|| matcher_native_arg_count(info).map(|_| ObjectNativeKind::Matcher))
        .or_else(|| stringbuilder_native_arg_count(info).map(|_| ObjectNativeKind::StringBuilder));
    if matches!(info.invoke_kind, 0 | 2)
        && !crate::classloading::any_class_redefined()
        && object_native_kind.is_some()
        && !args_slice.is_empty()
    {
        let kind = object_native_kind.expect("checked above");
        let receiver_raw = args_slice[0] as u64;
        if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
            if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                let receiver_class_id = vm.mem.heap.class_id_of(receiver).as_u32();
                let cached = OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                    cache
                        .borrow()
                        .get(&info_key)
                        .copied()
                        .filter(|entry| entry.receiver_class_id == receiver_class_id)
                });
                if let Some(entry) = cached {
                    if let Some(result) =
                        call_object_native_raw(vm, thread, info, receiver, args_slice, entry)
                    {
                        return result;
                    }
                } else {
                    let expected_class = match kind {
                        ObjectNativeKind::HashMap => "java/util/HashMap",
                        ObjectNativeKind::Matcher => "java/util/regex/Matcher",
                        ObjectNativeKind::StringBuilder => "java/lang/StringBuilder",
                    };
                    let is_exact_receiver = {
                        let classes = vm.classes.class_manager.read();
                        classes
                            .get_class(ClassId::new(receiver_class_id))
                            .map(|class| class.name.as_ref() == expected_class)
                            .unwrap_or(false)
                    };
                    if is_exact_receiver {
                        let callback = match kind {
                            ObjectNativeKind::HashMap => hashmap_native_callback(info),
                            ObjectNativeKind::Matcher => matcher_native_callback(info),
                            ObjectNativeKind::StringBuilder => {
                                stringbuilder_native_callback(vm, info)
                            }
                        };
                        if let Some(callback) = callback {
                            let entry = NativeDispatchCache {
                                receiver_class_id,
                                callback,
                                kind,
                            };
                            OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                                cache.borrow_mut().insert(info_key, entry);
                            });
                            if let Some(result) = call_object_native_raw(
                                vm, thread, info, receiver, args_slice, entry,
                            ) {
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }

    // Lambda proxies are synthetic and therefore cannot participate in the
    // class-store MIC. Give the erased primitive adapter its own receiver-guarded
    // direct path before allocating decoded Values for the generic fallback.
    if matches!(info.invoke_kind, 0 | 2) && args_slice.len() == 2 {
        if let Some(proxy) = vm.mem.heap.is_object_address(args_slice[0] as usize) {
            let proxy_class_id = vm.mem.heap.class_id_of(proxy);
            if vm
                .classes
                .lambda_proxies
                .read()
                .contains_key(&proxy_class_id)
            {
                match try_fast_lambda_int_to_double_apply(
                    vm,
                    thread,
                    proxy,
                    proxy_class_id,
                    info,
                    args_slice,
                ) {
                    Ok(Some(result)) => return result,
                    Ok(None) => {}
                    Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
                }
            }
        }
    }

    // Round-5 CRIT-1 fix: share arg-decoding with the three cache-hit
    // overflow bailouts above via `decode_dispatch_values`.
    let values = decode_dispatch_values(vm, info, args_slice);

    let result: Option<Value> = match info.invoke_kind {
        0 | 2 => {
            if values.is_empty() {
                return 0;
            }
            let receiver_ref = match values[0] {
                Value::Object(Some(obj)) => obj,
                // JVM semantics: invokevirtual/invokeinterface on a null
                // receiver throws NullPointerException. Signal it like the
                // array helpers — set the pending-NPE flag and return the
                // i64::MIN sentinel so the interpreter's post-JIT drain builds
                // the real NPE and routes it through the method's exception
                // table. Previously this returned `0`, silently SWALLOWING the
                // NPE: a null deref inside a JIT-compiled method (e.g. a
                // once-called lambda body that reaches this cold dispatch
                // helper before any inline null-check / MIC warm-up) completed
                // normally instead of throwing, so `() -> nullRef.foo()` ran
                // as a no-op under the JIT (the bug only reproduced JIT-on).
                Value::Object(None) => {
                    set_jit_pending_npe();
                    return i64::MIN;
                }
                // A non-object receiver slot is a miscompile, not a legitimate
                // null — keep the defensive 0 bail (does not mask a real NPE).
                _ => return 0,
            };
            // Match the register-overflow bail path: `NativeContext::invoke_virtual`
            // resolves solely from the heap object's class id. That is insufficient
            // for a synthetic/ClassId(0) receiver (common for Lucene iterator
            // adapters): it turns `Iterator.hasNext()` into `Object.hasNext()`.
            // `virtual_dispatch_target_for_receiver` preserves the real receiver
            // class when available and otherwise supplies the CP-resolved class;
            // `invoke_or_native` then applies the VM's interface/abstract retarget.
            let dispatch_class =
                virtual_dispatch_target_for_receiver(vm, receiver_ref, info).class_name;
            let virt_result = crate::vm::invoke_or_native(
                vm,
                thread,
                &dispatch_class,
                info.method_name,
                info.descriptor,
                &values,
            );
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
                    if !info.class_name.is_empty() {
                        let recv_cid = vm.mem.heap.class_id_of(receiver_ref);
                        let recv_name_opt = {
                            let cm = vm.classes.class_manager.read();
                            cm.get_class(recv_cid).map(|c| c.name.to_string())
                        };
                        let cp_differs = recv_name_opt
                            .as_deref()
                            .map(|n| n != info.class_name)
                            .unwrap_or(true);
                        if cp_differs
                            && recv_name_opt.as_deref().is_some_and(|recv_name| {
                                is_dispatch_no_such_method_miss(
                                    vm,
                                    &e,
                                    recv_name,
                                    info.method_name,
                                    info.descriptor,
                                )
                            })
                        {
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
                                    return handle_jit_dispatch_error(vm, thread, e2, info);
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
            //
            // BUG-JIT-INVOKESPECIAL-LOADER-20260726: resolve the CP class
            // through the CALLER's loader before dispatching. `info.class_name`
            // is constant-pool text; with two loaders defining the same binary
            // name, `invoke_special_shared`'s own `load_class_concurrent` picks
            // whichever copy the global map holds — see
            // `JitInvokeInfo::declaring_class_id`. `resolve_class_loader_aware`
            // is the same resolver the interpreter's invokespecial uses, and it
            // short-circuits to the global answer whenever no user-defined
            // loader has ever defined a class, so single-loader processes pay
            // one relaxed atomic load.
            let resolved_owner = if info.declaring_class_id != 0 {
                crate::runtime::interpreter::resolve_class_loader_aware(
                    vm,
                    thread,
                    ClassId::new(info.declaring_class_id),
                    info.class_name,
                )
                .ok()
            } else {
                None
            };
            let r = match resolved_owner {
                Some(owner) => crate::vm::invoke_special_shared_on_class(
                    vm,
                    thread,
                    owner,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
                None => crate::vm::invoke_special_shared(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
            };
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
#[allow(clippy::type_complexity)]
unsafe fn try_compile_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    use crate::runtime::interpreter::try_jit_compile_callee;
    // JIT-dispatch callee compile — optimized (C2-equivalent) tier. The
    // artifact comes back with the entry so the caller can keep it mapped for
    // as long as it calls or caches that address.
    try_jit_compile_callee(vm, info.class_name, info.method_name, info.descriptor, true)
}

/// Synthetic call-site info for [`jit_integer_value_of_direct`]'s error
/// path: `handle_jit_dispatch_error` only reads the name triple for
/// diagnostics/exception context, and this helper serves exactly one callee.
static INTEGER_VALUE_OF_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Integer",
    method_name: "valueOf",
    descriptor: "(I)Ljava/lang/Integer;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
};

/// Thin direct-call target for JIT `invokestatic Integer.valueOf(I)` sites
/// (registered into `cratonvm_jit::INTEGER_VALUE_OF_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// Semantics are identical to the `IntegerNativeKind::ValueOf` arm of
/// [`call_integer_native_raw`], minus the generic `jit_invoke_dispatch`
/// round trip (info decode, per-call thread-local cache probes, argument
/// buffer build):
///  * out-of-range values allocate a fresh wrapper through the mutator's
///    normal native-context TLAB path and publish it via
///    `native_pending_return` (the established JIT→native object-return
///    handoff root);
///  * `-128..=127` (and the cold pre-discovery case, and any class-redefine
///    window) route through `safe_native_call` to the canonical native
///    callback, preserving the JLS identity-cache contract;
///  * errors (OOM) route through `handle_jit_dispatch_error` exactly like
///    the dispatch helper, so the returned sentinel carries properly
///    stashed exception state for the caller's post-invoke check.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub unsafe extern "C" fn jit_integer_value_of_direct(vm_ptr: i64, value: i64) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as `jit_invoke_dispatch`: the
    // per-thread conservative-scan cache must be invalidated, and a callee
    // that allocates may enter the GC barrier, so the SATB queue is flushed.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let value = value as i32;
    if !(-128..=127).contains(&value) && !crate::classloading::any_class_redefined() {
        let vm_key = vm as *const SharedVm as usize;
        let cached_class = INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
            cache
                .get()
                .filter(|(cached_vm, _)| *cached_vm == vm_key)
                .map(|(_, raw)| ClassId::new(raw))
        });
        if let Some(class_id) = cached_class {
            if let Some((thread, _guard)) = jit_thread_mut() {
                // Direct TLAB bump — the SAME allocation function
                // `NativeContextImpl::alloc_object`'s TLAB arm uses (full
                // header init, fresh identity hash), minus that method's
                // per-call clamp-cache scan, alloc-pool probes and context
                // plumbing. The slot count is the class's real declared
                // field count, resolved once per (vm, class) below — so the
                // undersized-layout clamp is honored, not skipped. TLAB
                // exhaustion (or an oversized layout) falls back to the full
                // allocator with its old-gen spill/batch behavior.
                thread_local! {
                    // (vm_key, class_id, slots) — invalidated implicitly by
                    // the enclosing `any_class_redefined` gate (field counts
                    // only change through redefinition).
                    static INTEGER_ALLOC_SLOTS: std::cell::Cell<Option<(usize, u32, u32)>> =
                        const { std::cell::Cell::new(None) };
                }
                let slots = INTEGER_ALLOC_SLOTS.with(|cache| {
                    if let Some((vk, cid, slots)) = cache.get() {
                        if vk == vm_key && cid == class_id.as_u32() {
                            return slots as usize;
                        }
                    }
                    let resolved = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1);
                    // Cast: field counts are far below u32::MAX.
                    cache.set(Some((vm_key, class_id.as_u32(), resolved as u32)));
                    resolved
                });
                use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
                let requested_size = HEADER_SIZE + slots.saturating_mul(SLOT_SIZE);
                let tlab_object = if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
                    crate::runtime::interpreter::tlab_alloc_object(
                        thread,
                        vm,
                        class_id,
                        slots,
                        requested_size,
                    )
                } else {
                    None
                };
                if let Some(object) = tlab_object {
                    // Raw primitive-cell write: this arm JUST allocated
                    // `object` through the legacy TLAB path
                    // (`init_object_header`, zeroed 16-byte Value cells), so
                    // field 0 is the Value cell at HEADER_SIZE — the exact
                    // bytes `set_field_as(.., b'I')` would store, minus that
                    // path's per-call header read + layout dispatch. A
                    // primitive store takes no write barrier.
                    // SAFETY: `object` is a live legacy-layout allocation
                    // with >= 1 slot (`slots.max(1)` above); the cell is
                    // exclusively ours until published below.
                    unsafe {
                        std::ptr::write(
                            object.as_ptr().add(cratonvm_gc::heap::HEADER_SIZE) as *mut Value,
                            Value::Int(value),
                        );
                    }
                    // Object-return handoff root (see `call_integer_native_raw`).
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                use cratonvm_native_api::NativeContext as _;
                let object = {
                    // Reborrow: `thread` is used again after this arm for
                    // the pending-return publication.
                    let mut ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    ctx.alloc_object(class_id, 1)
                };
                // Descriptor-typed write (`Integer.value`, field 0, `I`) —
                // this cold arm's allocator may pick a non-legacy layout, so
                // keep the layout-aware store.
                vm.mem.heap.set_field_as(object, 0, Value::Int(value), b'I');
                // Object-return handoff root (see `call_integer_native_raw`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
        }
    }
    // Cold / in-range / redefine-window path: canonical native callback via
    // the full safe-native-call wrapper (identity cache; also discovers the
    // real wrapper ClassId for the fast path above).
    let Some((thread, _guard)) = jit_thread_mut() else {
        // No JIT thread context — cannot safely run the native. Signal the
        // deopt sentinel; the caller's post-invoke check bails to the
        // interpreter, which re-dispatches through the normal path.
        return i64::MIN;
    };
    let arg = Value::Int(value);
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return handle_jit_dispatch_error(vm, thread, error, &INTEGER_VALUE_OF_INFO),
    };
    match result {
        Some(Value::Object(Some(object))) => {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm as *const SharedVm as usize,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
            object.as_ptr() as i64
        }
        _ => 0,
    }
}

/// Synthetic call-site info for [`jit_integer_int_value_direct`]'s
/// generic-dispatch fallback (invalid non-null receiver — a shape the
/// verifier rules out for a `final`-class receiver, kept for defensive
/// parity with `call_integer_native_raw`'s bail-to-dispatch behavior).
static INTEGER_INT_VALUE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Integer",
    method_name: "intValue",
    descriptor: "()I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
};

/// Thin direct-call target for JIT `invokevirtual Integer.intValue()` sites
/// whose constant-pool class is exactly `java/lang/Integer` (a `final`
/// class, so the site is statically monomorphic — no receiver guard
/// needed; only `null` remains, which throws NPE per JVMS).
///
/// Mirrors the `IntegerNativeKind::IntValue` arm of
/// [`call_integer_native_raw`]: a heap-validated field-0 read that cannot
/// allocate or safepoint. Null receiver → pending-NPE + deopt sentinel
/// (the canonical implicit-NPE signal). A non-null receiver that fails
/// heap validation falls back to the full generic dispatcher, exactly like
/// the existing arm's `None` return.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub unsafe extern "C" fn jit_integer_int_value_direct(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let raw = receiver as u64;
    if raw == 0 {
        set_jit_pending_npe();
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            return match vm.mem.heap.get_field(object, 0) {
                Value::Int(value) => value as i64,
                _ => 0,
            };
        }
    }
    // Defensive fallback: hand the call to the generic dispatcher (same
    // machinery the non-direct site would have used).
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &INTEGER_INT_VALUE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site infos for the exact-HashMap thin direct-call helpers'
/// fallback/dispatch paths (perf/halfgap-20260717).
static HASHMAP_PUT_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "put",
    descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
};
static HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/HashMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
};
static CONCURRENT_HASHMAP_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/concurrent/ConcurrentMap",
    method_name: "get",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 2,
    declaring_class_id: 0,
};

static STRING_LATIN1_LOWER_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/StringLatin1",
    method_name: "toLowerCase",
    descriptor: "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
};

thread_local! {
    static CONCURRENT_HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// `(vm_key, class_id)` of the EXACT `java/util/HashMap` class, learned
    /// on the first direct-call fallback resolution — same pattern as
    /// `MATCHER_CLASS_CACHE`/`INTEGER_WRAPPER_CLASS_CACHE`.
    static HASHMAP_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Shared receiver/key screening for the two HashMap thin helpers. Returns
/// the validated `(thread-independent)` pieces or `None` → caller falls back
/// to the full dispatcher. The receiver must be EXACTLY `java/util/HashMap`
/// (a LinkedHashMap receiver at a HashMap-declared site takes the fallback,
/// which performs real virtual dispatch), and no class redefine may be in
/// flight (the overlay + class-id cache assume stable identity).
///
/// SAFETY: `receiver` must be non-null and 8-aligned below 2^48 (checked by
/// callers before the raw header read).
unsafe fn jit_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm as *const SharedVm as usize;
    if HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !crate::classloading::any_class_redefined();
    }
    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/HashMap")
        .unwrap_or(false);
    if is_exact {
        HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
        return !crate::classloading::any_class_redefined();
    }
    false
}

unsafe fn jit_concurrent_hashmap_receiver_is_exact(vm: &SharedVm, receiver: i64) -> bool {
    let cid = std::ptr::read(receiver as usize as *const u32);
    let vm_key = vm as *const SharedVm as usize;
    if CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.get() == Some((vm_key, cid))) {
        return !crate::classloading::any_class_redefined();
    }
    let exact = vm
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(cid))
        .map(|class| class.name.as_ref() == "java/util/concurrent/ConcurrentHashMap")
        .unwrap_or(false);
    if exact {
        CONCURRENT_HASHMAP_CLASS_CACHE.with(|c| c.set(Some((vm_key, cid))));
    }
    exact && !crate::classloading::any_class_redefined()
}

/// Guarded direct path for `ConcurrentMap.get(Object)` when the runtime
/// receiver is exactly ConcurrentHashMap. All other receivers retain the
/// canonical interface dispatcher.
pub unsafe extern "C" fn jit_concurrent_hashmap_get_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver != 0
        && (receiver as u64 & 0x7) == 0
        && (receiver as u64) < (1u64 << 48)
        && jit_concurrent_hashmap_receiver_is_exact(vm, receiver)
    {
        if let (Some(recv), Some((thread, _guard))) = (
            vm.mem.heap.is_object_address(receiver as usize),
            jit_thread_mut(),
        ) {
            let key = if key == 0 {
                Value::Object(None)
            } else if let Some(key) = vm.mem.heap.is_object_address(key as usize) {
                Value::Object(Some(key))
            } else {
                return 0;
            };
            let values = [Value::Object(Some(recv)), key];
            // `native_chm_get` pins its receiver/key before every operation
            // that can invoke Java or collect. Calling it directly avoids the
            // second, redundant safe-native wrapper/root snapshot on a hot
            // read-only lookup while retaining its canonical error contract.
            let result = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::native_chm_get(&mut ctx, &values)
            };
            match result {
                Ok(Some(Value::Object(Some(object)))) => {
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                Ok(Some(Value::Object(None))) | Ok(None) => return 0,
                Ok(_) => {}
                Err(error) => {
                    return handle_jit_dispatch_error(
                        vm,
                        thread,
                        error,
                        &CONCURRENT_HASHMAP_GET_DIRECT_INFO,
                    )
                }
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &CONCURRENT_HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Thin direct-call target for JIT `invokevirtual HashMap.get(Object)` sites
/// whose constant-pool class is exactly `java/util/HashMap` (recognition in
/// `jit::try_compile`; registered via `set_hashmap_get_direct_fn`).
///
/// Fast path: the Integer-keyed overlay probe (`jit_overlay_hashmap_get`) —
/// no Java-heap allocation, no Java dispatch, no `safe_native_call` wrapper
/// (same contract as `jit_integer_int_value_direct`). Everything else —
/// subclass receiver, non-Integer key, materialized map, redefine window —
/// falls back to the full generic dispatcher, byte-for-byte the semantics
/// the non-direct site had.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub unsafe extern "C" fn jit_hashmap_get_direct(vm_ptr: i64, receiver: i64, key: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver == 0 {
        set_jit_pending_npe();
        return i64::MIN;
    }
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        let key_val = if key == 0 {
            Value::Object(None)
        } else {
            let kraw = key as u64;
            if (kraw & 0x7) != 0 || kraw >= (1u64 << 48) {
                break 'fast;
            }
            match vm.mem.heap.is_object_address(key as usize) {
                Some(object) => Value::Object(Some(object)),
                None => break 'fast,
            }
        };
        let Some(recv_obj) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        // The native HashMap path maintains a GC-remapped, modCount-guarded
        // String-node cache. Probe it directly here so hot dynamic lowercase
        // keys avoid entering the GC-safe native wrapper at all.
        if let Value::Object(Some(key_object)) = key_val {
            let cached = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                ctx.hashmap_string_node_cache_get_object(recv_obj, key_object)
            };
            if let Some(Value::Object(Some(object))) = cached {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            if let Some(Value::Object(None)) = cached {
                return 0;
            }
        }
        let probe = {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_get(&ctx, recv_obj, key_val)
        };
        match probe {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                // Object-return handoff root (see `jit_integer_value_of_direct`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            // The overlay stores whatever Value was put; an object-typed map
            // returning a non-object Value is out-of-contract — take the
            // full dispatcher rather than guessing an encoding.
            Some(Ok(Some(_))) => break 'fast,
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_GET_DIRECT_INFO)
            }
            None => {
                let values = [Value::Object(Some(recv_obj)), key_val];
                match crate::vm::safe_native_call_prevalidated_objects(
                    vm,
                    thread,
                    cratonvm_native_collections::native_hashmap_get_exact,
                    &values,
                ) {
                    Ok(Some(Value::Object(Some(object)))) => {
                        let result = object.as_ptr() as i64;
                        thread.native_pending_return = Some(object);
                        return result;
                    }
                    Ok(Some(Value::Object(None))) | Ok(None) => return 0,
                    Ok(_) => break 'fast,
                    Err(error) => {
                        return handle_jit_dispatch_error(
                            vm,
                            thread,
                            error,
                            &HASHMAP_GET_DIRECT_INFO,
                        )
                    }
                }
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Direct compact-Latin1 lowercase helper. The registered Java helper is
/// correct for interpreter execution, but its generic native-dispatch round
/// trip dominates repeated charset lookups. This preserves the same cached
/// immutable result and pending-return root contracts without that overhead.
/// Direct receiver-typed `String.toLowerCase(Locale)` entry. The ASCII
/// compact helper below owns the implementation; Locale is currently unused
/// by the VM's existing ASCII fast path.
pub unsafe extern "C" fn jit_string_locale_to_lower_direct(
    vm_ptr: i64,
    source: i64,
    locale: i64,
) -> i64 {
    jit_string_latin1_to_lower_direct(vm_ptr, source, 0, locale)
}

pub unsafe extern "C" fn jit_string_latin1_to_lower_direct(
    vm_ptr: i64,
    source: i64,
    _value: i64,
    _locale: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    if source == 0 {
        return 0;
    }
    let Some(source) = vm.mem.heap.is_object_address(source as usize) else {
        return 0;
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        return 0;
    };
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let result = if let Some(cached) = ctx.get_ascii_case_string_cached(source, false) {
        cached
    } else {
        let mut lower = ctx.read_string(source).unwrap_or_default();
        let changed = if lower.is_ascii() {
            let changed = lower.bytes().any(|byte| byte.is_ascii_uppercase());
            lower.make_ascii_lowercase();
            changed
        } else {
            let folded = lower.to_lowercase();
            if folded == lower {
                false
            } else {
                lower = folded;
                true
            }
        };
        if changed {
            ctx.create_ascii_case_string_cached(source, &lower, false)
        } else {
            source
        }
    };
    thread.native_pending_return = Some(result);
    result.as_ptr() as i64
}

/// PUT sibling of [`jit_hashmap_get_direct`] — see its doc for the contract.
/// The overlay insert writes only the Rust-side table (GC-scanned as roots),
/// so the wrapper-free path holds; any non-overlay case (materialization,
/// resize, non-Integer key) falls back to full dispatch.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub unsafe extern "C" fn jit_hashmap_put_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
    value: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if receiver == 0 {
        set_jit_pending_npe();
        return i64::MIN;
    }
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        let mut vals = [Value::Object(None); 2];
        for (slot, raw) in [(0usize, key), (1usize, value)] {
            if raw == 0 {
                continue;
            }
            let bits = raw as u64;
            if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                break 'fast;
            }
            match vm.mem.heap.is_object_address(raw as usize) {
                Some(object) => vals[slot] = Value::Object(Some(object)),
                None => break 'fast,
            }
        }
        let Some(recv_obj) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let probe = {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_put(
                &mut ctx, recv_obj, vals[0], vals[1],
            )
        };
        match probe {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            Some(Ok(Some(_))) => break 'fast,
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_PUT_DIRECT_INFO)
            }
            None => break 'fast,
        }
    }
    let args = [receiver, key, value];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_PUT_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

#[inline]
fn call_integer_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    entry: IntegerNativeDispatchCache,
) -> Option<i64> {
    if args_slice.len() != 1 {
        return None;
    }
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        let value = args_slice[0] as i32;
        if !(-128..=127).contains(&value) {
            let vm_key = vm as *const SharedVm as usize;
            let cached_class = INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache
                    .get()
                    .filter(|(cached_vm, _)| *cached_vm == vm_key)
                    .map(|(_, raw)| ClassId::new(raw))
            });
            if let Some(class_id) = cached_class {
                // Young-pressure relief for this cached fast path, which
                // deliberately bypasses the `safe_native_call` boundary (and
                // its young-pressure GC hook): the only argument here is a
                // primitive `i32`, so initiating the orchestrated GC is
                // exactly as safe as `jit_new_object`'s slow-path GC — no raw
                // object pointers are held across it. Without this, a
                // boxing-dominated compiled loop keeps spilling wrappers into
                // old gen until `alloc_young_initialized` hard-aborts.
                if vm.mem.heap.young_spill_pressure() {
                    if !crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
                        && vm.mem.heap.needs_gc_for_jit_allocation()
                    {
                        crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
                    }
                    vm.mem.heap.clear_young_spill_pressure();
                }
                use cratonvm_native_api::NativeContext as _;
                let mut ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                let object = ctx.alloc_object(class_id, 1);
                ctx.set_field(object, 0, Value::Int(value));
                // Mirror `safe_native_call`'s object-return handoff root. A
                // peer STW cannot collect this active JIT mutator until its
                // next safepoint, but publishing the pending value preserves
                // the existing root contract and diagnostic visibility.
                ctx.thread.native_pending_return = Some(object);
                return Some(object.as_ptr() as i64);
            }
        }
    }
    if matches!(entry.kind, IntegerNativeKind::IntValue) {
        let raw = args_slice[0] as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(raw as usize)?;
        // `intrinsic_integer_int_value` delegates to
        // `native_wrapper_int_value`, whose complete behavior is a read of
        // wrapper field 0 and `Int`-or-zero normalization. The receiver is
        // already heap-validated above and this operation cannot allocate or
        // safepoint, so entering `safe_native_call` adds only rooting/panic/
        // dispatch overhead on every unbox in a compiled loop.
        return Some(match vm.mem.heap.get_field(object, 0) {
            Value::Int(value) => value as i64,
            _ => 0,
        });
    }
    let arg = match entry.kind {
        IntegerNativeKind::ValueOf => Value::Int(args_slice[0] as i32),
        IntegerNativeKind::IntValue => unreachable!("handled above"),
    };
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        entry.callback,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        if let Some(Value::Object(Some(object))) = result {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm as *const SharedVm as usize,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
        }
    }
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

#[inline]
fn hashmap_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => Some(3),
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => Some(2),
        _ => None,
    }
}

#[inline]
fn hashmap_native_callback(info: &JitInvokeInfo) -> Option<cratonvm_native_api::NativeCallback> {
    match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => {
            Some(cratonvm_native_collections::native_hashmap_put_exact)
        }
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => {
            Some(cratonvm_native_collections::native_hashmap_get_exact)
        }
        _ => None,
    }
}

#[inline]
fn matcher_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("find", "()Z") | ("start", "()I") | ("end", "()I") | ("group", "()Ljava/lang/String;") => {
            Some(1)
        }
        ("find", "(I)Z")
        | ("start", "(I)I")
        | ("end", "(I)I")
        | ("group", "(I)Ljava/lang/String;") => Some(2),
        _ => None,
    }
}

#[inline]
fn matcher_native_callback(info: &JitInvokeInfo) -> Option<cratonvm_native_api::NativeCallback> {
    if !crate::runtime::env_cache::native_matcher_find() {
        return None;
    }
    cratonvm_native_builtins::matcher_realjdk_native_callback(info.method_name, info.descriptor)
}

#[inline]
fn stringbuilder_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("append", "(I)Ljava/lang/StringBuilder;")
        | ("append", "(C)Ljava/lang/StringBuilder;")
        | ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;") => Some(2),
        ("toString", "()Ljava/lang/String;") | ("length", "()I") => Some(1),
        _ => None,
    }
}

/// Resolve the registered StringBuilder native for this site ONCE (the
/// registry's 3-string hash) — cached per callsite afterwards, exactly like
/// the HashMap/Matcher kinds. Returns `None` (no caching, generic dispatch)
/// when no native is registered for the triple.
#[inline]
fn stringbuilder_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<cratonvm_native_api::NativeCallback> {
    vm.natives
        .native_methods
        .find("java/lang/StringBuilder", info.method_name, info.descriptor)
}

/// Invoke a cached StringBuilder native from raw JIT argument slots.
#[inline]
fn call_stringbuilder_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = stringbuilder_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }
    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        match info.descriptor.as_bytes().get(1) {
            // int / char parameter — the natives take Value::Int for both.
            Some(b'I') | Some(b'C') => values[1] = Value::Int(args_slice[1] as i32),
            Some(b'L') => {
                let raw = args_slice[1];
                // `append((String) null)` must append "null" — the generic
                // path (appendNull routing) owns that; don't serve it here.
                if raw == 0 {
                    return None;
                }
                let bits = raw as u64;
                if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                    return None;
                }
                let object = vm.mem.heap.is_object_address(bits as usize)?;
                values[1] = Value::Object(Some(object));
            }
            _ => return None,
        }
    }
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

#[inline]
fn is_exact_matcher_class(vm: &SharedVm, class_id: ClassId) -> bool {
    let vm_key = vm as *const SharedVm as usize;
    let raw_class_id = class_id.as_u32();
    if MATCHER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, raw_class_id))) {
        return true;
    }

    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|class| class.name.as_ref() == "java/util/regex/Matcher")
        .unwrap_or(false);
    if is_exact {
        MATCHER_CLASS_CACHE.with(|cache| cache.set(Some((vm_key, raw_class_id))));
    }
    is_exact
}

#[inline]
fn call_object_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    entry: NativeDispatchCache,
) -> Option<i64> {
    match entry.kind {
        ObjectNativeKind::HashMap => {
            call_hashmap_native_raw(vm, thread, info, receiver_ref, args_slice, entry.callback)
        }
        ObjectNativeKind::Matcher => {
            call_matcher_native_raw(vm, thread, info, receiver_ref, args_slice, entry.callback)
        }
        ObjectNativeKind::StringBuilder => call_stringbuilder_native_raw(
            vm,
            thread,
            info,
            receiver_ref,
            args_slice,
            entry.callback,
        ),
    }
}

/// Invoke a cached HashMap bridge directly from raw JIT argument slots.
#[inline]
fn call_hashmap_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = match args_slice.len() {
        len @ (2 | 3) => len,
        _ => return None,
    };

    let mut values = [Value::Object(None); 3];
    values[0] = Value::Object(Some(receiver_ref));
    for (index, &raw) in args_slice[1..].iter().enumerate() {
        if raw == 0 {
            continue;
        }
        let bits = raw as u64;
        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(bits as usize)?;
        values[index + 1] = Value::Object(Some(object));
    }

    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Invoke a cached real-layout Matcher native from raw JIT argument slots.
#[inline]
fn call_matcher_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = matcher_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }

    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        values[1] = Value::Int(args_slice[1] as i32);
    }

    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Specialize a lambda proxy's erased `Function.apply(Object)` call when its
/// implementation is a concrete `(int) -> double` virtual/interface target.
///
/// The generic lambda route allocates vectors, reparses descriptors, and builds
/// an interpreter frame for each application. TDigest invokes this shape in
/// its quantile/cdf numeric kernels, so dispatch directly to the already-JITed
/// receiver method and only retain the Java-mandated boxed Double result.
// SAFETY: called by the JIT peephole for Dist private numeric kernels.
pub unsafe extern "C" fn jit_lambda_int_to_double(vm_ptr: i64, proxy_raw: i64, index: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let Some(proxy) = vm.mem.heap.is_object_address(proxy_raw as usize) else {
        return f64::NAN.to_bits() as i64;
    };
    let proxy_class_id = vm.mem.heap.class_id_of(proxy);
    let call_site = match vm
        .classes
        .lambda_proxies
        .read()
        .get(&proxy_class_id)
        .cloned()
    {
        Some(call_site) => call_site,
        None => return f64::NAN.to_bits() as i64,
    };
    if call_site.functional_interface.as_ref() != "java/util/function/Function"
        || call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        )
    {
        return f64::NAN.to_bits() as i64;
    }
    let receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return f64::NAN.to_bits() as i64,
    };
    let receiver_class_id = vm.mem.heap.class_id_of(receiver);
    let class_name = match vm.classes.class_manager.read().get_class(receiver_class_id) {
        Some(class) => class.name.clone(),
        None => return f64::NAN.to_bits() as i64,
    };
    let mut compiled = {
        let cache = vm.jit.jit_cache.read();
        cache.get(&class_name, "get", "(I)D", receiver_class_id)
    };
    if compiled.is_none() {
        // This direct scalar route bypasses the normal bytecode invocation
        // counter, so publish the receiver-specialized getter once here.
        let _ = crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            &class_name,
            "get",
            "(I)D",
            true,
        );
        compiled = vm
            .jit
            .jit_cache
            .read()
            .get(&class_name, "get", "(I)D", receiver_class_id);
    }
    if let Some(compiled) = compiled {
        let args = [receiver.as_ptr() as i64, index as i32 as i64];
        // The scalar helper is entered by a JIT caller that already owns the
        // active-root chain entry. Mirror the established virtual-dispatch
        // cache and re-enter this leaf directly: a second guard would mutate
        // that chain (and invalidate its scan cache) for every element.
        if let Some(result) = try_call_compiled_entry_reentrant(
            compiled.entry_ptr() as usize,
            compiled.needs_context(),
            vm_ptr,
            &args,
        ) {
            return result;
        }
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return f64::NAN.to_bits() as i64;
    };
    match crate::vm::invoke_on_class_shared(
        vm,
        thread,
        receiver_class_id,
        "get",
        "(I)D",
        &[Value::Object(Some(receiver)), Value::Int(index as i32)],
    ) {
        Ok(Some(Value::Double(value))) => value.to_bits() as i64,
        _ => f64::NAN.to_bits() as i64,
    }
}

// SAFETY: the JIT invoke metadata and the validated proxy receiver originate
// from the active interpreted frame; callers fall back to interpretation on a miss.
unsafe fn try_fast_lambda_int_to_double_apply(
    vm: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    proxy_class_id: ClassId,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Result<Option<i64>, crate::error::MethodCallFailed> {
    if info.method_name != "apply"
        || info.descriptor != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || args.len() != 2
    {
        return Ok(None);
    }
    let call_site = match vm
        .classes
        .lambda_proxies
        .read()
        .get(&proxy_class_id)
        .cloned()
    {
        Some(call_site) => call_site,
        None => return Ok(None),
    };
    if call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        )
    {
        return Ok(None);
    }
    let index = match vm.mem.heap.is_object_address(args[1] as usize) {
        Some(index_box) => match vm.mem.heap.get_field(index_box, 0) {
            Value::Int(index) => index,
            _ => return Ok(None),
        },
        None => return Ok(None),
    };
    let receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return Ok(None),
    };
    let receiver_class_id = vm.mem.heap.class_id_of(receiver);
    let class_name = match vm.classes.class_manager.read().get_class(receiver_class_id) {
        Some(class) => class.name.clone(),
        None => return Ok(None),
    };
    let compiled = {
        let cache = vm.jit.jit_cache.read();
        cache.get(&class_name, "get", "(I)D", receiver_class_id)
    };
    let Some(compiled) = compiled else {
        return Ok(None);
    };
    // A compiled leaf may itself contain a safe dispatch helper. That is not a
    // reason to discard a receiver-guarded entry: JitEntryGuard covers its
    // roots and the nested helper preserves normal Java dispatch semantics.
    let jit_args = [receiver.as_ptr() as i64, index as i64];
    let vm_ptr = vm as *const _ as i64;
    let _guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(&*compiled);
    let bits = if compiled.needs_context() {
        compiled.try_call_with_context(vm_ptr, &jit_args)
    } else {
        compiled.try_call(&jit_args)
    };
    let value = match bits {
        Ok(bits) => f64::from_bits(bits as u64),
        Err(_) => return Ok(None),
    };
    let box_args = [value.to_bits() as i64];
    if let Some((_callee_pin, entry, needs_context)) =
        crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            "java/lang/Double",
            "valueOf",
            "(D)Ljava/lang/Double;",
            true,
        )
    {
        if let Some(boxed) =
            try_call_compiled_entry_reentrant(entry, needs_context, vm_ptr, &box_args)
        {
            return Ok(Some(boxed));
        }
    }
    let boxed = crate::vm::invoke_or_native(
        vm,
        thread,
        "java/lang/Double",
        "valueOf",
        "(D)Ljava/lang/Double;",
        &[Value::Double(value)],
    )?;
    Ok(Some(match boxed {
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => return Ok(None),
    }))
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo. args_ptr/num_args form a valid i64 slice.
// mic_ptr must point to a live JitMICSlot used for monomorphic inline cache dispatch.
// pic_ptr, when non-zero, must point to a live JitPICSlot co-allocated with the MIC at
// the same call site; the helper populates its 4-way entries via `install` so the next
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
    let cv_trace = cv_trace_match(info);
    if cv_trace {
        let mut bits = [0i64; 4];
        if num_args > 0 && !(args_ptr as *const i64).is_null() {
            for (i, b) in bits.iter_mut().enumerate().take((num_args as usize).min(4)) {
                *b = *(args_ptr as *const i64).add(i);
            }
        }
        eprintln!(
            "[cv-mic-entry] cp_class={} num_args={} a0={:#x} a1={:#x}",
            info.class_name, num_args, bits[0], bits[1]
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] bad num_args/args_ptr -> silent 0");
        }
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
        None => {
            if cv_trace {
                eprintln!("[cv-mic-earlyout] no jit thread -> silent 0");
            }
            return 0;
        }
    };

    if args_slice.is_empty() {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] empty args -> silent 0");
        }
        return 0;
    }
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }
    let receiver_raw = args_slice[0];
    if receiver_raw == 0 {
        // Null receiver: throw NullPointerException (JVM semantics), mirroring
        // the `jit_invoke_dispatch` fix. Set the pending-NPE flag + return the
        // i64::MIN sentinel so the interpreter's post-JIT drain builds the real
        // NPE and routes it through the method's exception table. Was `return
        // 0`, which silently swallowed a null-receiver call in hot JIT'd code.
        set_jit_pending_npe();
        return i64::MIN;
    }
    // Defensive: a receiver slot carrying tagged-long bits (low 3 bits set
    // or value above the 48-bit canonical-address ceiling) is not a valid
    // heap pointer.  Bail out with rc=0 (the dispatcher's "no result"
    // path); this mirrors the receiver_raw == 0 short-circuit above and
    // avoids the `ObjectRef::from_raw` alignment panic.
    let receiver_bits = receiver_raw as u64;
    if (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        if cv_trace {
            eprintln!(
                "[cv-mic-earlyout] misaligned/tagged receiver {:#x} -> silent 0",
                receiver_bits
            );
        }
        return 0;
    }
    // SAFETY: receiver_bits is non-zero, 8-byte aligned, and within the
    // 48-bit canonical address space — matches the invariants required by
    // ObjectRef::from_raw for live heap objects.
    // The SATB flush above may have moved either the receiver or a reference
    // parameter.  A MIC hit jumps straight into compiled code with this raw
    // slice, so canonicalize every reference before the class lookup and the
    // eventual direct call.
    let forwarded_args = forward_jit_reference_args(vm, info, args_slice);
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);
    let receiver_raw = args_slice[0];
    let receiver_ref = ObjectRef::from_raw(receiver_raw as usize as *mut u8);

    // WS1 (kafka JIT throughput): the `Value` decode is deferred. The MIC-hit
    // fast path dispatches straight off the raw `args_slice` and never
    // materializes `Value`s; only the lambda, register-overflow-bailout and
    // full-resolution paths pay for the decode. Eagerly building this Vec
    // (one heap alloc + a descriptor parse) on every call — including pure
    // cache hits — was a measured contributor to JIT'd call-heavy code
    // running slower than the interpreter.
    let decode_values = || -> JitDecodedArgs {
        let mut values = JitDecodedArgs::with_capacity(args_slice.len());
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
                        // Tagged-long bits leaked into an L/[ slot (e.g. a
                        // primitive that should have been boxed before this
                        // virtual call) are downgraded to null instead of
                        // being treated as a heap pointer. The alignment +
                        // 48-bit-ceiling check alone is NOT sufficient: a
                        // round-number primitive long (e.g. a millisecond
                        // timeout like 60000/120000) is also 8-byte-aligned
                        // and well under 2^48, so it silently passed as a
                        // "plausible" pointer here and `ObjectRef::from_raw`
                        // fabricated a bogus reference into unmapped memory --
                        // confirmed via a live gdb repro: `XnioWorker$Builder
                        // .set(Option, Object)` receiving an unboxed 120000
                        // (0x1d4c0) segfaulted inside `read_string` when the
                        // bogus ObjectRef's header was dereferenced. Match
                        // `decode_dispatch_values`'s sibling path (used by the
                        // cache-miss/bailout decode) and require actual heap
                        // membership, not just bit-pattern plausibility.
                        let bits = raw as u64;
                        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            vm.mem.heap.is_object_address(bits as usize)
                        } else {
                            None
                        };
                        if cv_trace && validated.is_none() {
                            eprintln!(
                                "[cv-mic-decode] arg bits {:#x} FAILED is_object_address -> downgraded to null",
                                bits
                            );
                        }
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
    };

    // ClassLoader's resource accessors require a non-null resource name. The
    // MIC fast path dispatches on a custom loader's runtime class and can call
    // inherited JDK bytecode without consulting the ClassLoader callback that
    // owns this contract. Handle the null-only case before cache lookup; normal
    // resource calls still use the loader's virtual implementation unchanged.
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        if let Some(callback) = vm.natives.native_methods.find(
            "java/lang/ClassLoader",
            info.method_name,
            info.descriptor,
        ) {
            let values = decode_values();
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    let receiver_class_id = vm.mem.heap.class_id_of(receiver_ref);
    let receiver_cid = receiver_class_id.as_u32();

    // Real-layout Matcher methods are registered natives, so they can never
    // publish a compiled entry into the ordinary MIC/PIC. Without this leaf
    // path every `find`/`start`/`end`/`group` hit decodes a heap-allocated
    // `Vec<Value>`, retries a compile probe, and performs generic virtual/native
    // resolution. Guarding the exact receiver class preserves subclass
    // overrides; the feature gate in `matcher_native_callback` preserves the
    // native opt-out, and class redefinition keeps using the generic resolver.
    if !crate::classloading::any_class_redefined() {
        if let Some(callback) = matcher_native_callback(info) {
            if is_exact_matcher_class(vm, receiver_class_id) {
                if let Some(result) =
                    call_matcher_native_raw(vm, thread, info, receiver_ref, args_slice, callback)
                {
                    return result;
                }
            }
        }
    }

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
    if vm
        .classes
        .lambda_proxies
        .read()
        .contains_key(&receiver_class_id)
    {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        match try_fast_lambda_int_to_double_apply(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info,
            args_slice,
        ) {
            Ok(Some(result)) => return result,
            Ok(None) => {}
            Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
        }
        let values = decode_values();
        match crate::runtime::interpreter::try_lambda_dispatch(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info.method_name,
            info.descriptor,
            &values[1..],
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
    let redefine_jit_quiesced = crate::classloading::any_class_redefined();
    if redefine_jit_quiesced {
        mic.clear_compiled_entry();
        if pic_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            pic.clear_entries();
        }
    }
    let cached_cid = mic
        .cached_class_id
        .load(std::sync::atomic::Ordering::Acquire);

    if crate::runtime::env_cache::jit_mic_dbg() {
        let (pic_classes, pic_entries, pic_contexts) = if pic_ptr == 0 {
            (
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [false; cratonvm_jit::JIT_PIC_ENTRIES],
            )
        } else {
            let pic = &*(pic_ptr as *const JitPICSlot);
            (
                std::array::from_fn(|i| {
                    pic.class_ids[i].load(std::sync::atomic::Ordering::Acquire)
                }),
                std::array::from_fn(|i| {
                    pic.entry_ptrs[i].load(std::sync::atomic::Ordering::Acquire)
                }),
                std::array::from_fn(|i| {
                    pic.needs_context[i].load(std::sync::atomic::Ordering::Acquire)
                }),
            )
        };
        eprintln!(
            "[JIT_MIC] {}.{}{} cached_cid={} recv_cid={} entry={} pic_ptr={:#x} pic_classes={:?} pic_entries={:?} pic_contexts={:?}",
            info.class_name,
            info.method_name,
            info.descriptor,
            cached_cid,
            receiver_cid,
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            pic_ptr,
            pic_classes,
            pic_entries,
            pic_contexts,
        );
    }

    // Megamorphic helper fast path. Generated code probes only the four-entry
    // inline PIC; receiver types beyond that capacity land here. The PIC keeps
    // a bounded secondary target cache so those misses still avoid repeated
    // class hierarchy resolution and compile-cache probing.
    if cached_cid != receiver_cid
        && pic_ptr != 0
        && direct_virtual_compiled_callee_entry_enabled()
        && !redefine_jit_quiesced
    {
        let pic = &*(pic_ptr as *const JitPICSlot);
        if let Some((entry, needs_context)) = pic.lookup_megamorphic(receiver_cid) {
            if entry != 0 {
                mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
                if let Some(result) = try_call_compiled_entry_reentrant(
                    entry as usize,
                    needs_context,
                    vm_ptr,
                    args_slice,
                ) {
                    return result;
                }
            }
        }
    }

    // --- Monomorphic Inline Cache: fast path ---
    // If the receiver ClassId matches the cached value AND we have a cached
    // entry pointer, dispatch directly without any class_manager lookup or
    // method resolution.  This is the zero-overhead dispatch path.
    if cached_cid == receiver_cid && cached_cid != 0 {
        mic.record_hit();

        // Try the cached compiled entry pointer (true inline cache hit)
        let entry = mic
            .cached_entry_ptr
            .load(std::sync::atomic::Ordering::Acquire);
        if direct_virtual_compiled_callee_entry_enabled() && entry != 0 && !redefine_jit_quiesced {
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
                try_call_compiled_entry_reentrant(entry as usize, needs_ctx, vm_ptr, args_slice)
            };
            if let Some(rc) = rc_opt {
                // BUG-H: if the receiver-resolved callee threw an implicit
                // exception (AIOOBE/NPE) its own `catch` should handle, the
                // direct compiled call bypassed its exception table. Re-execute
                // it in the interpreter so the exception routes through the
                // callee's table (e.g. Tomcat `HttpParser.isNotRequestTarget
                // Relaxed`: `IS_NOT_REQUEST_TARGET[c]` in `catch (AIOOBE)`).
                if rc == i64::MIN {
                    // jit-invokedynamic-groovy-regression fix — precise resume
                    // of a frame-stashing deopt in the MIC-dispatched callee
                    // (see `try_resume_trapped_callee`). Must run BEFORE the
                    // implicit-exception drains below (a pure deopt sets no
                    // exception flags).
                    if let Some(v) = try_resume_trapped_callee(vm, thread, info) {
                        return v;
                    }
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
        let dispatch_target = virtual_dispatch_target_for_receiver(vm, receiver_ref, info);
        let cacheable_receiver = dispatch_target.cacheable_receiver;
        // An entryless slot can be retargeted by another thread after the
        // class-id probe above. Its cached class name is therefore not a
        // coherent companion to this receiver; resolve from the receiver
        // itself until there is a callable entry to use.
        let class_name = dispatch_target.class_name;
        if cv_trace {
            eprintln!(
                "[cv-mic-hit-noentry] resolved_class={} recv_cid={} cacheable={}",
                class_name, receiver_cid, cacheable_receiver
            );
        }

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
        let compile_res = if !direct_virtual_compiled_callee_entry_enabled()
            || redefine_jit_quiesced
            || !cacheable_receiver
        {
            None
        } else {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
            crate::runtime::interpreter::try_jit_compile_callee(
                vm,
                &class_name,
                info.method_name,
                info.descriptor,
                // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
                true,
            )
        };
        // BUG-H: as in the cache-miss branch below, do not publish a direct
        // compiled entry for a callee with a local exception table — the inline
        // machine-code cascade would bypass it. Keep dispatch on the
        // `invoke_or_native` path so the exception routes through the callee's
        // own table.
        if let Some((_callee_pin, entry_ptr, needs_ctx)) = compile_res {
            // `_callee_pin` holds the callee artifact across the publications
            // below: `update`/`install` take their own keep-alive by resolving
            // the entry, and that resolution can only succeed while the
            // artifact is alive.
            // jit-invokedynamic-groovy-regression fix: also never publish an
            // artifact containing an unconditional invokedynamic trap — the
            // inline MIC/PIC cascade would machine-CALL it, letting the trap's
            // sentinel + stashed frame bail through the compiled caller's
            // epilogue past the only point able to resume it precisely.
            if !mic_callee_has_exception_table(vm, receiver_class_id, info)
                && !compiled_entry_has_indy_trap(vm, &class_name, info.method_name, info.descriptor)
            {
                // Publish through `update`, never with a raw store: `update` is
                // the only writer that also resolves and RETAINS the callee's
                // `Arc<CompiledMethod>` in the slot's `compiled_owner`.
                //
                // Until 2026-07-27 this branch stored `cached_entry_ptr`
                // directly, so a slot reached through the "class cached, target
                // unresolved" shape (what `prepopulate` seeds and what
                // `clear_compiled_entry` leaves behind after every
                // invalidation) ended up holding a RAW entry pointer with no
                // keep-alive. The next tier-up `put` for that callee replaced
                // its shard snapshot, dropped the last `Arc`, and `munmap`ped
                // the body — while the inline `MOV R11,[mic+8]; CALL R11`
                // cascade emitted by `jit/src/x64.rs` still called it. That is
                // the ElasticSearch `NodeConnectionsServiceTests` SIGSEGV
                // (`rip == r11 ==` first byte of a retired code mapping) and,
                // once the address was recycled by a later allocation, the
                // json-smart "re-parse returned another method's result"
                // corruption.
                mic.update(receiver_cid, &class_name, entry_ptr as u64, needs_ctx);
                // CRIT-1 — also populate the co-allocated PIC so the
                // inline 4-way cascade in `jit/src/x64.rs` hits on the
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
            Err(e)
                if !info.class_name.is_empty()
                    && &*class_name != info.class_name
                    && is_dispatch_no_such_method_miss(
                        vm,
                        &e,
                        &class_name,
                        info.method_name,
                        info.descriptor,
                    ) =>
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

        if cv_trace {
            eprintln!(
                "[cv-mic-hit-result] is_null={}",
                matches!(result, Some(Value::Object(None)) | None)
            );
        }
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

    let dispatch_target = virtual_dispatch_target_for_receiver(vm, receiver_ref, info);
    let cacheable_receiver = dispatch_target.cacheable_receiver;
    let class_name = dispatch_target.class_name;
    if cv_trace {
        eprintln!(
            "[cv-mic-miss] resolved_class={} recv_cid={} cacheable={}",
            class_name, receiver_cid, cacheable_receiver
        );
    }

    mic_prof::bump(&mic_prof::MIC_MISS);
    // Try to compile callee for cached entry. Resolve by the RECEIVER's class
    // (`class_name`), not the static `info.class_name` — see the matching
    // VIRTUAL DISPATCH FIX in the cache-hit branch above. `class_name` here is
    // an `Arc<str>`; deref to `&str` for the resolver.
    let compile_res = if !direct_virtual_compiled_callee_entry_enabled()
        || redefine_jit_quiesced
        || !cacheable_receiver
    {
        None
    } else {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
        crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            &class_name,
            info.method_name,
            info.descriptor,
            // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
            true,
        )
    };
    // `_callee_pin` must outlive the `mic.update` / `pic.install` below — see
    // the matching note in the cache-hit branch.
    let (_callee_pin, entry_ptr, needs_ctx) = match compile_res {
        Some((pin, ptr, nc)) => (Some(pin), ptr as u64, nc),
        None => (None, 0, false),
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
    if cacheable_receiver
        && !mic_callee_has_exception_table(vm, receiver_class_id, info)
        // jit-invokedynamic-groovy-regression fix — see the matching gate in
        // the cache-hit branch above: an indy-trap-bearing artifact must stay
        // on the dispatch helper, never in a machine-called MIC/PIC entry.
        && !compiled_entry_has_indy_trap(vm, &class_name, info.method_name, info.descriptor)
    {
        // Update all MIC fields atomically (needs_ctx must match compiled entry ABI)
        mic.update(receiver_cid, &class_name, entry_ptr, needs_ctx);

        // CRIT-1 — Populate the co-allocated PIC so the inline 4-way
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
        Err(e)
            if !info.class_name.is_empty()
                && &*class_name != info.class_name
                && is_dispatch_no_such_method_miss(
                    vm,
                    &e,
                    &class_name,
                    info.method_name,
                    info.descriptor,
                ) =>
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

    if cv_trace {
        eprintln!(
            "[cv-mic-miss-result] is_null={}",
            matches!(result, Some(Value::Object(None)) | None)
        );
    }
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

        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!(
                "[cratonvm-deopt] {} reason={:?} bci={} action={:?}",
                method_key, reason, bci, action
            );
        }

        // Invalidate the compiled method from the JIT cache. Skipped only
        // for a SOFT OsrExit (action == Reinterpret, i.e. still within the
        // "tolerate a few, they're rare" window in deopt.rs's OsrExit arm):
        // that case means a loop boundary inside an otherwise-good artifact
        // bailed to the interpreter for THIS call (real_frame_deopt_resume_
        // and_despeculate already reconstructs and resumes the frame
        // correctly without needing eviction) -- not that the compiled code
        // is wrong, so recompiling it would be wasted work. Once OsrExit
        // escalates past that window (action == MakeNotCompilable, meaning
        // the SAME bci keeps exiting -- a structural property of the loop,
        // not noise), evict normally like every other reason: keeping a
        // doomed compiled entry alive pays the reconstruct-and-resume tax on
        // literally every future call, which measured net SLOWER than plain
        // interpretation (347s/round vs a 63-72s/round fully-interpreted
        // baseline for the same benchmark -- docs/known-issues/tomcat-08-07/
        // silent-hang-no-signature-cluster.md, TestResponsePerformance).
        let skip_eviction = reason == cratonvm_jit::deopt::DeoptReason::OsrExit
            && action == cratonvm_jit::deopt::DeoptAction::Reinterpret;
        if !skip_eviction {
            // `deoptimize` is a `&str`-keyed public API with ~20 call sites;
            // resolving the class globally by name here preserves this
            // (pre-existing, not loader-aware) eviction behavior unchanged
            // rather than threading a new `ClassId` parameter through every
            // caller. See `JitKey::declaring_class_id`'s doc comment.
            let class_id = vm
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(class_name)
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let mut jit_cache = vm.jit.jit_cache.write();
            jit_cache.remove(class_name, method_name, descriptor, class_id);
        }

        // deopt-osr Step 9 — advance the method's live compilation epoch so that
        // (a) the next compilation is stamped fresh and (b) any frame still
        // executing this now-evicted artifact, when it reaches the
        // real-frame-deopt resume sink, sees `compilation_epoch < live` and
        // re-runs instead of resuming a superseded speculation. Gated on the
        // resume feature: the epoch is only ever *read* under `deopt_real_enabled()`,
        // so production VMs neither bump nor consult it (byte-identical).
        if cratonvm_jit::deopt_real_enabled() {
            vm.bump_compilation_epoch(&method_key);
        }

        // For class-check or receiver-type failures, also check the
        // invalidation manager for dependent methods.
        if matches!(
            reason,
            cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
                | cratonvm_jit::deopt::DeoptReason::ClassCheck
                | cratonvm_jit::deopt::DeoptReason::ClassLoading
        ) {
            let mut inv_mgr = vm.jit.invalidation_manager.lock();
            // Clear stale assumptions for the deoptimized method
            inv_mgr.clear_assumptions(&method_key);
        }

        // If the deopt log recommends giving up, add to the JIT skip set.
        // Also mark it bail-listed in the SEPARATE cratonvm_jit registry
        // (`is_jit_bail_listed`/`mark_jit_bail_listed`, RBC.4) -- `jit_skip_set`
        // alone only gates the interpreter's own per-call hotness/upgrade path
        // (`vm/src/runtime/interpreter.rs`'s `execute()`); a JIT-compiled
        // caller dispatching to this method as a CALLEE goes through
        // `try_jit_compile_callee`/`_slow`, which never consults
        // `jit_skip_set` at all. Without this, a give-up decision reached via
        // the callee-dispatch path (exactly the shape of Tomcat's
        // `Response.toAbsolute()`, called from `TestResponsePerformance`'s
        // JIT-compiled `doHomebrew()` loop) never actually stuck: the method
        // kept getting recompiled every time its deopt count re-crossed the
        // threshold, an unbounded repeat of the same 15KB-recompile thrash
        // this fix exists to stop. See docs/known-issues/tomcat-08-07/
        // silent-hang-no-signature-cluster.md.
        if action == cratonvm_jit::deopt::DeoptAction::MakeNotCompilable {
            let mut skip = vm.jit.jit_skip_set.write();
            skip.insert((class_name.into(), method_name.into(), descriptor.into()));
            cratonvm_jit::mark_jit_bail_listed(class_name, method_name, descriptor);
        }

        // deopt-osr Step 9 follow-up (b): eager recompile re-queue. On a
        // RecompileAndReinterpret action the artifact was just evicted
        // (make-not-entrant); historically the method then had to re-cross the
        // interpreter hotness threshold (up to JIT_RETRY_STRIDE calls) before the
        // optimized body was rebuilt. When a background compiler is active,
        // eagerly enqueue the recompile so it happens off-thread without that
        // delay (`on_deoptimization` above already reset `queued_for_compilation`
        // / `current_tier`, so this enqueue is accepted). Gated on the deopt-
        // resume feature so the production uncommon-trap path is byte-identical;
        // a no-op when no background worker drains the queue — the existing
        // hotness-retry path still recompiles, so behaviour never regresses.
        if cratonvm_jit::deopt_real_enabled()
            && action == cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret
            && vm.jit.tiered_manager.compiler_active()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            vm.jit
                .tiered_manager
                .enqueue_compilation(cratonvm_jit::tiered::CompilationTask {
                    method_key: tiered_key.clone(),
                    target_tier: cratonvm_jit::tiered::CompilationTier::C1,
                    priority: cratonvm_jit::tiered::CompilationPriority::High,
                    enqueue_time_ms: now_ms,
                    osr_bci: None,
                });
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                eprintln!(
                    "[cratonvm-deopt] eager re-queue (RecompileAndReinterpret) {}",
                    method_key
                );
            }
        }

        tracing::debug!(
            "deopt: {} reason={:?} bci={} action={:?}",
            method_key,
            reason,
            bci,
            action
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
                cratonvm_jit::deopt::DeoptReason::OsrExit => "OsrExit",
            };
            let action_static: &'static str = match action {
                cratonvm_jit::deopt::DeoptAction::Reinterpret => "Reinterpret",
                cratonvm_jit::deopt::DeoptAction::RecompileAndReinterpret => {
                    "RecompileAndReinterpret"
                }
                cratonvm_jit::deopt::DeoptAction::MakeNotEntrant => "MakeNotEntrant",
                cratonvm_jit::deopt::DeoptAction::MakeNotCompilable => "MakeNotCompilable",
            };
            let mut jfr = vm.debug.flight_recorder.lock();
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
pub unsafe extern "C" fn jit_uncommon_trap(vm_ptr: i64, reason: i64, bci: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Out-of-band deopt signal: the x64 `emit_deopt_stubs` stub that calls this
    // helper loads `i64::MIN` into RAX and runs the method epilogue immediately
    // afterwards, so this trap ALWAYS precedes an `i64::MIN` method return. Flag
    // it as a genuine deopt so the interpreter doesn't mistake a method
    // legitimately returning `Long.MIN_VALUE` for one.
    set_jit_deopt_pending();
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
        let default = (
            "unknown".to_string(),
            "unknown".to_string(),
            "()V".to_string(),
        );
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
    fn native_dispatch_scratch_stays_inline_through_register_envelope() {
        let mut args = JitDecodedArgs::with_capacity(INLINE_JIT_NATIVE_ARGS);
        args.resize(INLINE_JIT_NATIVE_ARGS, Value::Int(0));
        assert!(!args.spilled(), "eight native arguments must stay inline");
        args.push(Value::Int(0));
        assert!(args.spilled(), "larger descriptors may use heap fallback");
    }

    #[test]
    fn virtual_dispatch_target_uses_cp_class_for_cid0_non_object_method() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let receiver = vm.mem.heap.alloc_object(ClassId::new(0), 0);
        let info = JitInvokeInfo {
            class_name: "java/io/InputStream",
            method_name: "read",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
        };

        let target = unsafe { virtual_dispatch_target_for_receiver(&vm, receiver, &info) };
        assert_eq!(target.class_name.as_ref(), "java/io/InputStream");
        assert!(
            !target.cacheable_receiver,
            "ClassId(0) is the MIC/PIC empty-slot sentinel and must not be cached"
        );
    }

    #[test]
    fn virtual_dispatch_target_keeps_object_members_on_object_for_cid0() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let receiver = vm.mem.heap.alloc_object(ClassId::new(0), 0);
        let info = JitInvokeInfo {
            class_name: "java/lang/Object",
            method_name: "hashCode",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
        };

        let target = unsafe { virtual_dispatch_target_for_receiver(&vm, receiver, &info) };
        assert_eq!(target.class_name.as_ref(), "java/lang/Object");
        assert!(!target.cacheable_receiver);
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_no_ctx_abi() {
        unsafe extern "C" fn ret_seven() -> i64 {
            7
        }

        // SAFETY: the test function has the no-context, zero-arg ABI selected below.
        let result = unsafe {
            try_call_compiled_entry_reentrant(ret_seven as *const () as usize, false, 0, &[])
        };
        assert_eq!(result, Some(7));
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_ctx_abi() {
        unsafe extern "C" fn add_ctx_arg(ctx: i64, arg: i64) -> i64 {
            ctx + arg
        }

        // SAFETY: the test function has the with-context, one-arg ABI selected below.
        let result = unsafe {
            try_call_compiled_entry_reentrant(add_ctx_arg as *const () as usize, true, 5, &[7])
        };
        assert_eq!(result, Some(12));
    }

    #[test]
    fn compiled_entry_reentrant_wrapper_preserves_stack_arg_abi() {
        unsafe extern "C" fn sum_five(a: i64, b: i64, c: i64, d: i64, e: i64) -> i64 {
            a + b + c + d + e
        }
        unsafe extern "C" fn sum_ctx_four(ctx: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
            ctx + a + b + c + d
        }
        // SAFETY: both functions use the C ABI selected by the helper, including
        // the first stack-passed parameter on Windows.
        let no_ctx = unsafe {
            try_call_compiled_entry_reentrant(
                sum_five as *const () as usize,
                false,
                0,
                &[1, 2, 3, 4, 5],
            )
        };
        let with_ctx = unsafe {
            try_call_compiled_entry_reentrant(
                sum_ctx_four as *const () as usize,
                true,
                10,
                &[1, 2, 3, 4],
            )
        };
        assert_eq!(no_ctx, Some(15));
        assert_eq!(with_ctx, Some(20));
    }

    #[test]
    fn jit_checkcast_null_ptr_returns_zero() {
        // SAFETY: Passing all-zero/null arguments exercises the null-object fast path;
        // no heap pointers are dereferenced.
        let result = unsafe { jit_checkcast(0, 0, std::ptr::null(), 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn jit_checkcast_non_heap_ptr_returns_zero() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let vm = SharedVm::new(VmConfig::default());
        let target = "java/lang/Object";
        // SAFETY: vm points to a live SharedVm and target is a valid UTF-8
        // string. The fake receiver is aligned and pointer-shaped, but outside
        // the VM heap; jit_checkcast must reject it before any header read.
        let result = unsafe {
            jit_checkcast(
                &vm as *const SharedVm as i64,
                0x1000,
                target.as_ptr(),
                target.len() as i64,
            )
        };
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
        assert!(
            take_jit_pending_npe(),
            "baload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_iaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §iaload: NPE on null array. Helper returns the i64::MIN deopt sentinel
        // and sets the pending-NPE flag for the interpreter to consume.
        let _ = take_jit_pending_npe(); // clear any prior state
        let result = unsafe { jit_iaload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "iaload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_aaload_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §aaload: NPE on null array.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_aaload(0, 0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "aaload(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_arraylength_null_sets_pending_npe() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        // JVMS §arraylength: NPE on null array. Previously returned -1, which
        // silently corrupted any downstream length-comparison or loop-bound use.
        let _ = take_jit_pending_npe();
        let result = unsafe { jit_arraylength(0) };
        assert_eq!(result, i64::MIN);
        assert!(
            take_jit_pending_npe(),
            "arraylength(null) must set pending NPE flag"
        );
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
        assert!(
            take_jit_pending_npe(),
            "iastore(null) must set pending NPE flag"
        );
    }

    #[test]
    fn jit_bastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        let _ = take_jit_deopt_pending();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences.
        unsafe { jit_bastore(0, 0, 0) };
        assert!(
            take_jit_pending_npe(),
            "bastore(null) must set pending NPE flag"
        );
    }

    // -----------------------------------------------------------------------
    // Out-of-band deopt signal (finding 1): i64::MIN sentinel disambiguation
    // -----------------------------------------------------------------------

    #[test]
    fn deopt_pending_set_take_cycle() {
        let _ = take_jit_deopt_pending(); // clear any prior state
        assert!(!take_jit_deopt_pending(), "flag must start clear");
        set_jit_deopt_pending();
        assert!(take_jit_deopt_pending(), "set then take must observe true");
        assert!(!take_jit_deopt_pending(), "take must clear the flag");
    }

    #[test]
    fn jit_dispatch_threw_peeks_all_signals_nondestructively() {
        // The J/D-call-site disambiguation peek: `1` ⇒ a genuine exception/deopt
        // is pending (caller bails), `0` ⇒ the `i64::MIN` in RAX is a legitimate
        // `Long.MIN_VALUE` return (caller keeps it). It must read EVERY signal a
        // dispatch could leave, and must NOT clear any (the interpreter drain
        // still needs them).
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_npe();
        let _ = take_jit_pending_aioobe();

        // Nothing pending → legitimate value.
        assert_eq!(
            jit_dispatch_threw(),
            0,
            "no pending signal must report 0 (a legitimate Long.MIN_VALUE return)"
        );

        // Out-of-band deopt flag → genuine sentinel, peeked non-destructively.
        set_jit_deopt_pending();
        assert_eq!(
            jit_dispatch_threw(),
            1,
            "a pending deopt must report 1 (bail)"
        );
        assert_eq!(jit_dispatch_threw(), 1, "the peek must be non-clearing");
        assert!(
            take_jit_deopt_pending(),
            "the deopt flag must survive the peek for the interpreter drain"
        );
        assert_eq!(jit_dispatch_threw(), 0, "drained → back to 0");

        // Pending NPE (e.g. a null-receiver dispatch) → 1, non-clearing.
        set_jit_pending_npe();
        assert_eq!(jit_dispatch_threw(), 1, "a pending NPE must report 1");
        assert!(take_jit_pending_npe(), "NPE flag must survive the peek");
        let _ = take_jit_pending_npe_action();
        assert_eq!(jit_dispatch_threw(), 0);

        // Pending AIOOBE (a bounds-check failure) → 1, non-clearing.
        stash_jit_pending_aioobe(5, 3);
        assert_eq!(jit_dispatch_threw(), 1, "a pending AIOOBE must report 1");
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((5, 3)),
            "AIOOBE payload must survive the peek"
        );
        assert_eq!(jit_dispatch_threw(), 0, "fully drained → 0");
    }

    #[test]
    fn jit_throw_aioobe_sets_deopt_pending() {
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_aioobe();
        // SAFETY: only stores into thread-locals; no pointer dereference.
        let r = unsafe { jit_throw_aioobe(5, 3, 0, 17) };
        assert_eq!(r, i64::MIN);
        assert!(
            take_jit_deopt_pending(),
            "jit_throw_aioobe must set the out-of-band deopt signal so the \
             interpreter does not mistake a legitimate Long.MIN_VALUE return"
        );
        // also leaves the AIOOBE payload for the interpreter drain
        assert_eq!(take_jit_pending_aioobe(), Some((5, 3)));
    }

    #[test]
    fn jit_bastore_null_sets_deopt_pending() {
        let _ = take_jit_deopt_pending();
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); null-guard early return, no deref.
        unsafe { jit_bastore(0, 0, 0) };
        assert!(
            take_jit_deopt_pending(),
            "bastore(null) (the null-check-store stub path) must set the deopt signal"
        );
        let _ = take_jit_pending_npe();
    }

    #[test]
    fn jit_aastore_null_sets_pending_npe() {
        let _ = take_jit_pending_npe();
        // SAFETY: array_ptr is 0 (null); the function takes the null-guard
        // early-return path and never dereferences either array_ptr or
        // vm_ptr / val (the null path returns before touching them).
        unsafe { jit_aastore(0, 0, 0, 0) };
        assert!(
            take_jit_pending_npe(),
            "aastore(null) must set pending NPE flag"
        );
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
        let arr = vm_box.mem.heap.alloc_array(ClassId::new(0), et, len);
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
        assert_eq!(
            hi,
            i64::MIN,
            "iaload OOB-high must return the deopt sentinel"
        );
        assert_eq!(
            take_jit_pending_aioobe(),
            Some((4, 4)),
            "iaload OOB-high must set pending AIOOBE (index, length)"
        );
        // SAFETY: `arr_ptr` is a valid test array header built above; `jit_iaload`
        // bounds-checks the index and signals AIOOBE rather than reading OOB.
        let lo = unsafe { jit_iaload(arr_ptr, -1) };
        assert_eq!(
            lo,
            i64::MIN,
            "iaload OOB-low must return the deopt sentinel"
        );
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
        let result = unsafe { jit_getfield(0, 0, 0) };
        assert_eq!(
            result,
            i64::MIN,
            "getfield(null) must return the deopt sentinel"
        );
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
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        // Object with exactly 2 reference fields (num_slots == 2).
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 2);
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot indices 2 and 5 are
        // out of range so the helper takes the bounds-check arm and never
        // dereferences past the object. A negative index is likewise rejected.
        let oob_hi = unsafe { jit_getfield(vm_ptr, obj_ptr, 2) };
        assert_eq!(
            oob_hi, 0,
            "getfield on an out-of-range slot must not read OOB"
        );
        // SAFETY: `obj_ptr` is a valid test object header built above; `jit_getfield`
        // bounds-checks the slot index and returns 0 rather than reading OOB.
        let oob_far = unsafe { jit_getfield(vm_ptr, obj_ptr, 5) };
        assert_eq!(oob_far, 0, "getfield far past num_slots must not read OOB");
        let oob_neg = unsafe { jit_getfield(vm_ptr, obj_ptr, -1) };
        assert_eq!(oob_neg, 0, "getfield on a negative slot must not read OOB");
        assert!(
            !take_jit_pending_npe(),
            "an in-range receiver with an OOB slot must not raise NPE"
        );
    }

    #[test]
    fn jit_getfield_rejects_pointer_shaped_non_heap_receiver() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;

        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        // Aligned, above the null guard, and below 47 bits: it passes the
        // context-free plausibility filter, but it is not a heap address in
        // this VM. This is the Tomcat rc=139 shape where generated code had
        // stale/truncated receiver bits before getfield.
        let mut bad_receiver = 0x1000_i64;
        while vm_box
            .mem
            .heap
            .is_object_address(bad_receiver as usize)
            .is_some()
        {
            bad_receiver += 0x1000;
        }
        assert!(cratonvm_types::plausible_heap_pointer(bad_receiver as u64));
        let result = unsafe { jit_getfield(vm_ptr, bad_receiver, 0) };
        assert_eq!(result, i64::MIN);
        assert!(take_jit_pending_npe());
    }

    #[test]
    fn jit_getfield_in_bounds_reads_stored_int() {
        // The fast in-bounds path must be untouched by the B2 fix: a value
        // written into a valid slot reads back identically.
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        let _ = take_jit_pending_npe();
        let vm_box: Box<SharedVm> = Box::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = (&*vm_box as *const SharedVm) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 2);
        // Write via the interpreter path (the helper read must observe it).
        vm_box.mem.heap.set_field(obj, 1, Value::Int(0x5A5A));
        let obj_ptr = obj.as_ptr() as i64;
        // SAFETY: obj_ptr is a live 2-field object; slot 1 is in bounds.
        let v = unsafe { jit_getfield(vm_ptr, obj_ptr, 1) };
        assert_eq!(
            v, 0x5A5A,
            "in-bounds getfield must read back the stored value"
        );
        assert!(
            !take_jit_pending_npe(),
            "an in-bounds getfield must not raise NPE"
        );
    }

    // Regression test for the PLAIN-SLOT TEARING FIX (2026-07-06, see
    // docs/known-issues/elasticsearch-lucene-binary-docvalues-range-hangs.md
    // #3): `jit_getfield` used to read a 16-byte `Value` slot via a bare,
    // non-atomic `ptr::read`, asymmetric with `jit_putfield_*`'s already-
    // atomic `write_value_atomic` (commit 4e6b560f). Two threads hammering
    // the same field slot -- one via `jit_putfield_int` (mirroring a
    // JIT-compiled `putfield`), one via `jit_getfield` (mirroring a
    // JIT-compiled `getfield`) -- must never observe a torn value.
    #[test]
    fn jit_getfield_never_tears_against_concurrent_jit_putfield_int() {
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let vm_box: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = Arc::as_ptr(&vm_box) as i64;
        let obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 1);
        let obj_ptr = obj.as_ptr() as i64;

        const A: i32 = 0x1111_1111;
        const B: i32 = 0x2222_2222_u32 as i32;
        const ITERATIONS: usize = 2_000_000;
        // Establish A as the slot'''s initial value BEFORE spawning the reader:
        // a freshly-allocated slot is zero-initialized (decodes as Value::Int(0)),
        // and 0 is neither A nor B, so a reader started before the writer'''s
        // first store would see a legitimate-but-unaccounted-for transient
        // value -- a test-harness race, not a torn read.
        unsafe { jit_putfield_int(obj_ptr, 0, A as i64) };
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for i in 0..ITERATIONS {
                    let v = if i % 2 == 0 { A } else { B };
                    // SAFETY: obj_ptr is a live, single-field object; slot 0
                    // is in bounds.
                    unsafe { jit_putfield_int(obj_ptr, 0, v as i64) };
                }
                stop.store(true, Ordering::Release);
            })
        };

        let reader = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut seen_a = 0usize;
                let mut seen_b = 0usize;
                while !stop.load(Ordering::Acquire) {
                    // SAFETY: obj_ptr is a live, single-field object; slot 0
                    // is in bounds.
                    let v = unsafe { jit_getfield(vm_ptr, obj_ptr, 0) } as i32;
                    if v == A {
                        seen_a += 1;
                    } else if v == B {
                        seen_b += 1;
                    } else {
                        panic!(
                            "torn read: observed {v:#x}, neither of the two \
                             legitimate written values ({A:#x}, {B:#x})"
                        );
                    }
                }
                (seen_a, seen_b)
            })
        };

        writer.join().unwrap();
        let (seen_a, seen_b) = reader.join().unwrap();
        assert!(
            seen_a > 0 && seen_b > 0,
            "reader never observed both written values (seen_a={seen_a}, \
             seen_b={seen_b}) -- test may not be exercising real contention"
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
        let result = unsafe {
            jit_newarray(0, 10 /* T_INT */, nan_boxed_11)
        };
        assert_eq!(result, 0, "jit_newarray must not abort on NaN-boxed length");
    }

    #[test]
    fn jit_anewarray_strips_nanbox_tag_from_length() {
        let nan_boxed_11: i64 = 0xFFFC_0000_0000_000B_u64 as i64;
        // SAFETY: vm_ptr=0 hits the explicit null check after length narrowing.
        let result = unsafe { jit_anewarray_object(0, 0, nan_boxed_11) };
        assert_eq!(
            result, 0,
            "jit_anewarray_object must not abort on NaN-boxed length"
        );
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
        vm_box
            .mem
            .heap
            .enable_concurrent_gc(satb.clone(), state.clone());

        // Activate marking. Both `satb.activate()` (so `is_active()`
        // returns true) and `state.set_phase(ConcurrentMark)` (so
        // `is_marking_active()` returns true) are required by the
        // generational `satb_barrier` fast-path gate.
        satb.activate();
        state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);

        // Allocate a container with one reference field plus two payload
        // objects to use as old/new references for the putfield store.
        let container = vm_box.mem.heap.alloc_object(ClassId::new(0), 1);
        let old_obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 0);
        let new_obj = vm_box.mem.heap.alloc_object(ClassId::new(0), 0);

        // Pre-write the old reference into slot 0 (interpreter path —
        // bypasses the SATB barrier we are about to test).
        vm_box
            .mem
            .heap
            .set_field(container, 0, Value::Object(Some(old_obj)));

        // Pre-drain any baggage from this thread's local SATB buffer so
        // the test only observes references logged by the JIT helper.
        vm_box.mem.heap.flush_thread_satb();
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
        vm_box.mem.heap.flush_thread_satb();
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
        let post = vm_box.mem.heap.get_field(container, 0);
        match post {
            Value::Object(Some(obj)) => assert_eq!(
                obj.as_ptr() as usize,
                new_obj.as_ptr() as usize,
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
            let _ = vm_box
                .mem
                .heap
                .try_alloc_array(ClassId::new(0), ArrayElementType::Int, 1024);
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
        // SAFETY: `result` is the non-zero array pointer just returned by the
        // successful `jit_newarray` call above, so it is a live, aligned heap object.
        let arr = unsafe { ObjectRef::from_raw(result as usize as *mut u8) };
        assert_eq!(
            vm_box.mem.heap.array_length(arr),
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
// SAFETY: takes no pointer arguments; only reads the per-thread `JIT_THREAD` TLS
// slot and returns it (null if `set_jit_thread` was never called). Safe to call
// from JIT-compiled code.
#[no_mangle]
pub unsafe extern "C" fn jit_get_current_thread() -> *mut JvmThread {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_THREAD.with(|t| t.get())
}

// ===========================================================================
// BUG-1 companion — native-stack headroom guard for DIRECT self-recursive
// calls.
//
// The dispatch helpers contain the thread-local depth guard (`enter_jit_
// dispatch`) that converts runaway compiled recursion into a catchable
// `StackOverflowError`. Routing every non-tail self-recursive `invokestatic`
// through `jit_invoke_dispatch` purely to reach that guard made each
// recursive call pay the full dispatch round trip (scan-cache invalidation,
// SATB flush, dispatch-cache lookup, borrow bookkeeping, …) — the dominant
// cost of recursive workloads (fib(42) ran ~10x slower than with a direct
// CALL; binarytrees pays it on both `make` and `check`).
//
// This guard is the cheap replacement the backend calls immediately before
// a direct rel32 self-CALL: compare the current native stack pointer against
// a per-thread floor (queried from the OS once per thread and cached in
// TLS). Comfortably above the floor → return 0 and the compiled site
// proceeds with the direct CALL. At exhaustion → stash a catchable
// `java/lang/StackOverflowError` (identical to the dispatch depth guard) and
// return the `i64::MIN` deopt sentinel, which the call site routes through
// its existing post-invoke sentinel check.
// ===========================================================================
thread_local! {
    /// Cached self-call guard floor for the current thread.
    /// `usize::MAX` = not yet computed.
    static JIT_SELF_CALL_STACK_FLOOR: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Query the current thread's native stack bounds `(low, high)` from the OS.
/// `None` when the platform query is unavailable/fails — the caller falls
/// back to a conservative offset from the first observed stack pointer.
#[cfg(windows)]
fn thread_stack_bounds() -> Option<(usize, usize)> {
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let mut low = 0usize;
    let mut high = 0usize;
    // SAFETY: plain out-pointer Win32 call on the current thread.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    if low != 0 && high > low {
        Some((low, high))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn thread_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: standard pthread attr query on the current thread; attr is
    // initialized by pthread_getattr_np on success and destroyed after use.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&mut attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 || addr.is_null() || size == 0 {
            return None;
        }
        Some((addr as usize, addr as usize + size))
    }
}

#[cfg(target_os = "macos")]
fn thread_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: both are infallible pthread queries on the current thread.
    unsafe {
        let top = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
        let size = libc::pthread_get_stacksize_np(libc::pthread_self());
        if top == 0 || size == 0 {
            return None;
        }
        Some((top - size, top))
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn thread_stack_bounds() -> Option<(usize, usize)> {
    None
}

/// Compute the guard floor for this thread: the lowest stack pointer at which
/// a compiled self-recursive site may still CALL one level deeper.
///
/// The headroom below the floor must cover everything that can run once the
/// guard trips: the `StackOverflowError` construction (class load + alloc),
/// the compiled frames' unwind back through the sentinel checks, and any
/// interpreter/exception-table routing above. 1 MiB is generous for all of
/// those; it is clamped to a quarter of the stack (min 64 KiB) so small
/// carrier stacks keep most of their space usable.
#[cold]
fn compute_self_call_stack_floor(sp_now: usize) -> usize {
    const HEADROOM: usize = 1 << 20; // 1 MiB
    match thread_stack_bounds() {
        Some((low, high)) => {
            let size = high - low;
            let margin = HEADROOM.min(size / 4).max(64 * 1024);
            low.saturating_add(margin)
        }
        // No OS query available: assume at least ~4 MiB of stack below the
        // first observed SP (threads here default to 8 MiB). This still
        // converts unbounded recursion into a catchable error well before
        // a typical guard page.
        None => sp_now.saturating_sub(4 << 20),
    }
}

/// Leaf floor query for the INLINE self-recursion check: get-or-compute the
/// current OS thread's native-stack floor (the same TLS value
/// `jit_self_call_stack_guard` consults). Called ONCE from the prologue of a
/// method with direct self-recursive call sites; each site then compares RSP
/// against the frame-cached value inline. Touches no VM state and never GCs
/// (no scan-cache boundary note needed — a leaf like `jit_get_current_thread`).
///
/// SAFETY: no arguments, reads only this thread's TLS + stack bounds.
#[no_mangle]
pub unsafe extern "C" fn jit_native_stack_floor() -> i64 {
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now);
            f.set(computed);
            computed
        }
    }) as i64
}

/// The self-call stack guard baked before every direct self-recursive CALL.
/// Returns `0` (proceed) or the `i64::MIN` deopt sentinel with a catchable
/// `java/lang/StackOverflowError` stashed in `JIT_PENDING_EXCEPTION`.
///
// SAFETY: called from JIT-compiled code; `vm_ptr` is the SharedVm pointer the
// compiled frame received at entry (same contract as `jit_invoke_dispatch`).
#[no_mangle]
pub unsafe extern "C" fn jit_self_call_stack_guard(vm_ptr: i64) -> i64 {
    // Rust<->JIT boundary — invalidate the per-thread JIT-scan cache (same
    // single TLS bump `jit_get_current_thread` performs on the inline-TLAB
    // fast path).
    crate::jit::conservative_roots::note_jit_boundary();
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now);
            f.set(computed);
            computed
        }
    });
    if sp_now > floor {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code and points to the live SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    let rc = raise_jit_stack_overflow(vm);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] self-call stack guard TRIP sp={:#x} floor={:#x} pending={}",
            sp_now,
            floor,
            jit_pending_exception_is_set(),
        );
    }
    rc
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
pub(crate) mod savebase_watcher {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

    pub static ARM_ADDR: AtomicUsize = AtomicUsize::new(0);
    static WORKER_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);

    extern "system" {
        // Returns the Win32 pseudo-HANDLE as `*mut c_void` to stay structurally
        // identical to the `GetCurrentProcess` decl in
        // `runtime::crash_handler::windows_fault` — same symbol, so
        // `clashing_extern_declarations` compares the two and warns if they
        // diverge. The handle is pointer-sized either way; this module treats
        // handles as `isize` (see `DuplicateHandle` below), so the single call
        // site casts the result with `as isize`.
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
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

    // SAFETY: `h` must be a valid, suspended thread HANDLE owned by this watcher;
    // the CONTEXT is a correctly aligned 1232-byte buffer and the offsets written
    // (ContextFlags/Dr0/Dr7) match the Win32 x64 CONTEXT layout passed to
    // SetThreadContext.
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
    // SAFETY: called on the worker thread; the Win32 handle-duplication and thread
    // spawn use only valid pseudo-handles (GetCurrentProcess/GetCurrentThread) and
    // store the duplicated real handle for the watcher to Suspend/Resume.
    pub unsafe fn publish(addr: usize) {
        ARM_ADDR.store(addr, Ordering::Relaxed);
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut h: isize = 0;
        let proc = GetCurrentProcess() as isize;
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
        eprintln!(
            "[WATCH] watcher thread started; worker savebase @0x{:016X}",
            addr
        );
        // SAFETY: `watcher_loop` only Suspend/Resume/SetThreadContext's the
        // duplicated worker HANDLE stored in `WORKER_HANDLE`; no shared Rust state
        // is aliased mutably across threads (all coordination is via atomics).
        std::thread::spawn(|| unsafe { watcher_loop() });
    }

    // SAFETY: runs on the dedicated watcher thread; the only handle it touches is
    // the duplicated worker HANDLE in `WORKER_HANDLE` (valid until process exit),
    // and the Suspend→SetThreadContext→Resume sequence keeps the worker quiesced
    // while its debug registers are written.
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
// SAFETY: naked fn — its body is hand-written asm that reads the on-stack return
// address and tail-jumps to `arm_savebase_watch_inner` with the extern "C" ABI
// preserved (`addr` in RCX/ARG0, the return address placed in RDX/ARG1).
#[cfg(windows)]
#[unsafe(naked)]
pub unsafe extern "C" fn jit_arm_savebase_watch(addr: i64) {
    core::arch::naked_asm!(
        "mov rdx, [rsp]",
        "jmp {inner}",
        inner = sym arm_savebase_watch_inner,
    );
}

// SAFETY: called only from the `jit_arm_savebase_watch` naked trampoline with the
// extern "C" ABI it sets up; `ra` is the JIT return address read off the stack and
// `addr` is a savebase slot address that is validated (non-null, 8-byte aligned)
// before use.
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
// SAFETY: empty body — takes no arguments and dereferences nothing.
#[cfg(windows)]
pub unsafe extern "C" fn jit_disarm_savebase_watch() {}

// SAFETY: non-Windows stub with an empty body — takes no pointer it dereferences.
#[cfg(not(windows))]
pub unsafe extern "C" fn jit_arm_savebase_watch(_addr: i64) {}
// SAFETY: non-Windows stub with an empty body — dereferences nothing.
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
    cratonvm_jit::x64::set_disarm_savebase_watch_fn(
        jit_disarm_savebase_watch as *const () as usize,
    );

    // `Integer.valueOf(I)` / `Integer.intValue()` thin direct-call helpers —
    // same no-ABI-change registration pattern as the savebase watch helpers
    // above. See `jit_integer_value_of_direct` / `jit_integer_int_value_direct`
    // and the recognition in `jit::try_compile`.
    cratonvm_jit::set_integer_value_of_direct_fn(jit_integer_value_of_direct as *const () as usize);
    cratonvm_jit::set_integer_int_value_direct_fn(
        jit_integer_int_value_direct as *const () as usize,
    );
    cratonvm_jit::set_monitor_direct_fns(
        jit_monitor_enter as *const () as usize,
        jit_monitor_exit as *const () as usize,
    );
    cratonvm_jit::set_hashmap_put_direct_fn(jit_hashmap_put_direct as *const () as usize);
    cratonvm_jit::set_hashmap_get_direct_fn(jit_hashmap_get_direct as *const () as usize);
    cratonvm_jit::set_string_latin1_lower_direct_fn(
        jit_string_latin1_to_lower_direct as *const () as usize,
    );
    cratonvm_jit::set_string_locale_lower_direct_fn(
        jit_string_locale_to_lower_direct as *const () as usize,
    );
    cratonvm_jit::set_concurrent_hashmap_get_direct_fn(
        jit_concurrent_hashmap_get_direct as *const () as usize,
    );

    let (jit_card_table_addr, jit_card_old_base, jit_card_old_end) =
        crate::native::jni::process_vm()
            .and_then(|shared| shared.mem.heap.jit_card_table_info())
            .unwrap_or((0, 0, 0));

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
        throw_arithmetic: jit_throw_arithmetic as *const () as usize,
        invoke_dispatch: jit_invoke_dispatch as *const () as usize,
        invoke_virtual_mic: jit_invoke_virtual_mic as *const () as usize,
        lambda_int_to_double: jit_lambda_int_to_double as *const () as usize,
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
        frame_record: if cratonvm_jit::x64::precise_jit_maps_enabled()
            || cratonvm_jit::x64::moving_young_enabled()
        {
            // Step 1 self-check: when inline frame-record is active AND the
            // verify knob is on, wire the verify helper here instead — the
            // prologue calls it right after the inline store to assert the
            // mirror slot it wrote is the one the GC reads.
            if cratonvm_jit::x64::verify_inline_frame_record_enabled()
                && cratonvm_jit::x64::inline_rbp_tls_disp() != 0
            {
                jit_verify_inline_frame_record as *const () as usize
            } else {
                jit_frame_record as *const () as usize
            }
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
        // JEP 358 (helpful NPE), inline-codegen path — per-action null-check
        // failure stub target. Sets the pending NPE *with* its JEP-358 action
        // code so the interpreter drain can attach the right action-only
        // message.
        jit_npe_with_action: jit_npe_with_action as *const () as usize,
        // i64::MIN-sentinel disambiguation for J/D (long/double) call returns —
        // peeked by a compiled caller on the rare `RAX == i64::MIN` branch to
        // tell a genuine callee exception/deopt apart from a legitimate
        // `Long.MIN_VALUE` return.
        dispatch_threw: jit_dispatch_threw as *const () as usize,
        // IR FP tier (Slice A) — fmod-style FP remainder helpers, CALLed by the
        // IR `Op::Rem` Float/Double arms (operands in XMM0/XMM1, result XMM0).
        jit_frem: jit_frem as *const () as usize,
        jit_drem: jit_drem as *const () as usize,
        // BUG-1 companion — native-stack headroom guard enabling direct
        // (non-dispatch) self-recursive CALLs. See `jit_self_call_stack_guard`.
        self_call_stack_guard: jit_self_call_stack_guard as *const () as usize,
        // Guarded inline getfield — address of the GC's process-global region
        // bounds table. Non-zero even under G1/ZGC (the table just stays
        // all-zero there, so every guard falls through to the checked helper).
        region_bounds_addr: cratonvm_gc::jit_region_bounds_addr(),
        // Inline self-recursion check — leaf floor-query helper (see the
        // jit-api field doc; prologue-called once per self-recursive method).
        native_stack_floor_fn: jit_native_stack_floor as *const () as usize,
        ldc_string: jit_ldc_string as *const () as usize,
        // Cooperative JIT safepoint polling (CRATONVM_JIT_SAFEPOINT_POLLS,
        // off by default) — address of the process-global VM's
        // stw_requested flag byte. `process_vm()` is published by
        // `Vm::new()` before any bytecode runs (see its doc comment), which
        // is always before the first `build_helpers()` call a real JIT
        // compile can trigger (compilation only starts once the
        // interpreter is executing bytecode). `None` here (e.g. a unit
        // test that calls `build_helpers()` before any `Vm::new()`) leaves
        // this at `0`, which `emit_safepoint_poll` (the SOLE reader of this
        // field) treats as "not wired" and emits no poll code at all — the
        // same optional-helper contract as `region_bounds_addr`/
        // `frame_record` above.
        safepoint_flag_addr: crate::native::jni::process_vm()
            .map(|shared| shared.mem.gc_barrier.stw_requested_flag_addr() as usize)
            .unwrap_or(0),
        // Slow-path helper for a poll hit. Unconditionally wired (the
        // function always exists in this binary) — `safepoint_flag_addr`
        // above is what actually gates whether the JIT ever emits a CALL
        // to it, so leaving this non-zero when the flag address happens to
        // be unavailable is harmless (dead code, never reached).
        safepoint_slow_path: jit_safepoint_slow_path as *const () as usize,
        jit_card_table_addr,
        jit_card_old_base,
        jit_card_old_end,
    }
}

/// GC-safe materialization for a compiled `ldc "..."` instruction.
///
/// The literal bytes live in the owning `CompiledMethod`; this helper consults
/// the VM string pool on every execution. Keeping the object reference out of
/// generated code is essential: the pool is rewritten after a moving GC, while
/// an immediate object address would become stale on the next invocation.
#[no_mangle]
pub extern "C" fn jit_ldc_string(vm_ptr: i64, bytes: *const u8, len: usize) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 || bytes.is_null() {
        return 0;
    }
    // SAFETY: the JIT compiler owns the literal bytes for the lifetime of its
    // compiled method, and `vm_ptr` is the hidden SharedVm argument installed
    // by the compiled-entry trampoline.
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(bytes, len)) };
    let shared = unsafe { &*(vm_ptr as *const SharedVm) };
    crate::vm::create_java_string(shared, text).as_ptr() as i64
}

/// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`) slow
/// path — called by JIT-compiled code when the inline poll
/// (`jit/src/x64.rs::emit_safepoint_poll`) observes the `stw_requested`
/// flag byte (`helpers.safepoint_flag_addr`, see
/// `GcBarrier::stw_requested_flag_addr`) set.
///
/// Recovers the `SharedVm`/`JvmThread` from process-wide VM publication and
/// `jit_thread_mut()`'s `JIT_THREAD` TLS, then
/// joins the SAME stop-the-world wait the interpreter's own poll hit uses
/// (`crate::runtime::interpreter::safepoint_check` — retire the TLAB, drain
/// SATB, publish a fresh root snapshot, arrive at the GC barrier) so a
/// thread parked here is exactly as GC-visible as an interpreter frame at
/// its poll point.
///
/// The poll site (`emit_safepoint_poll`) always emits
/// `emit_pre_safepoint_spill()` immediately before this CALL, so every
/// register-resident local/oop is already flushed to its canonical frame
/// slot before `safepoint_check` can park this thread — the conservative
/// scanner sees a complete picture of this frame while parked.
///
/// An absent process VM or `JIT_THREAD` TLS entry is a silent no-op. Resolving
/// the VM here instead of passing the hidden context pointer lets pure
/// compiled methods use the same poll sequence as context methods.
// SAFETY: called only from JIT-compiled code at a poll site emitted by
// `emit_safepoint_poll`, which always precedes the CALL with
// `emit_pre_safepoint_spill`.
#[no_mangle]
pub unsafe extern "C" fn jit_safepoint_slow_path() {
    static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    crate::jit::conservative_roots::note_jit_boundary();
    let Some(vm) = crate::native::jni::process_vm() else {
        return;
    };
    if let Some((thread, _guard)) = jit_thread_mut() {
        let hit = HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if hit <= 16 && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_SAFEPOINTS").is_some() {
            eprintln!(
                "[jit-safepoint] cooperative slow-path hit={} thread_id={}",
                hit, thread.thread_id.0
            );
        }
        crate::runtime::interpreter::safepoint_check(vm.as_ref(), thread);
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

/// Step 1 (`docs/feature-designs/precise-jit-maps-default.md`) debug self-check
/// helper (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`).
///
/// When inline frame-record AND the verify knob are both on, the JIT prologue
/// calls this immediately AFTER its inline `mov gs:[disp], rbp` (it is wired
/// into the `frame_record` helper slot for that combination, see
/// `build_helpers`). It reads the innermost-RBP mirror back through the SAME
/// accessor the GC root walk uses and asserts it equals the RBP the inline
/// store should have written — i.e. that the baked `gs:[disp]` slot is exactly
/// the slot the Rust side reads. Logs on mismatch (never panics); pure
/// validation aid with no effect on the mirror value.
extern "C" fn jit_verify_inline_frame_record(rbp: usize) {
    let got = crate::jit::conservative_roots::top_rbp_mirror_read();
    if got != rbp {
        eprintln!(
            "[VERIFY-INLINE-FR] mismatch: inline store rbp={:#x} but mirror reads {:#x}",
            rbp, got
        );
    }
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

/// IR FP tier (Slice A) — `frem` runtime helper.
///
/// Called from JIT code via an absolute `CALL` emitted by the IR `Op::Rem`
/// Float arm (`jit/src/ir_lower.rs`), which loads the two operands into
/// XMM0/XMM1 (the float ABI's first two argument registers on both Win64 and
/// SysV) and reads the result back from XMM0.
///
/// JVMS `frem` is the truncated remainder `a - (a / b rounded toward zero) * b`
/// taking the sign of the dividend — exactly C `fmod` and Rust's `f32 %`. The
/// special cases also match the JVMS table: `frem(x, ±∞) = x`, `frem(±∞, y) =
/// NaN`, `frem(x, ±0) = NaN`, `frem(±0, y) = ±0`, and any NaN operand yields
/// NaN. There is no single SSE instruction for it, hence the helper.
#[no_mangle]
pub extern "C" fn jit_frem(a: f32, b: f32) -> f32 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary), mirroring the FMA helpers.
    crate::jit::conservative_roots::note_jit_boundary();
    a % b
}

/// IR FP tier (Slice A) — `drem` runtime helper. The double analogue of
/// [`jit_frem`]; the IR `Op::Rem` Double arm `CALL`s it with the operands in
/// XMM0/XMM1 and reads the remainder from XMM0. Rust's `f64 %` is `fmod`,
/// matching the JVMS `drem` semantics exactly.
#[no_mangle]
pub extern "C" fn jit_drem(a: f64, b: f64) -> f64 {
    crate::jit::conservative_roots::note_jit_boundary();
    a % b
}
