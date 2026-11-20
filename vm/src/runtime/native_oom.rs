// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap-exhaustion unwind channel for the *native* allocation helpers.
//!
//! # The problem
//!
//! `NativeContext::new_array` / `new_ref_array` / `alloc_object` funnel into
//! the **panicking** `GenerationalHeap::alloc_array` / `alloc_object`. Those
//! entry points deliberately cannot GC-and-retry — a native holds raw
//! `ObjectRef`s in Rust locals that are in NO GC root set, so a moving young
//! collection triggered from there would relocate them and dangle the locals
//! (the stale-ref SEGV class). So on young-full they spill the allocation into
//! the old generation, and when old gen is *also* full they
//! `std::process::abort()` the whole process.
//!
//! HotSpot throws a catchable `java.lang.OutOfMemoryError` in exactly this
//! situation. The abort is much worse than a test failure for anything that
//! depends on `OutOfMemoryError` being recoverable — it kills every other test
//! sharing the JVM, and it kills applications (H2, Lucene, Netty) that
//! deliberately allocate to the ceiling and recover. Two members of this family
//! were already fixed one call site at a time by threading the fallible
//! `try_new_array` / `try_new_ref_array` allocators through the offending
//! native (`crash-01-arraylist-capacity-oom-abend.md`,
//! `crash-02-native-capacity-ctor-abort-family.md`); `ByteBuffer.allocate` —
//! the H2 `TestOutOfMemory` abort — was the third. There are ~1000
//! `ctx.new_array(..)` call sites, so converting them one by one does not
//! converge.
//!
//! # The channel
//!
//! A native callback dispatched by `safe_native_call_impl` runs *directly*
//! under that function's `catch_unwind`, with no JIT-compiled frame between
//! the callback and the handler. In that position a Rust unwind is safe and
//! already an established recovery path (any panic out of a native is caught
//! there today and reported as an internal error). So when the allocators
//! genuinely cannot satisfy a request, they unwind with the [`NativeAllocOom`]
//! payload and `safe_native_call_impl` converts it into the catchable
//! `java.lang.OutOfMemoryError` HotSpot would have thrown.
//!
//! # Why the permission is *scoped*, not global
//!
//! JIT-compiled frames carry no unwind information (no SEH tables on Windows,
//! no `.eh_frame` on Linux), so a panic that has to cross one terminates the
//! process instead of unwinding — this is why `jit_throw_aioobe` signals
//! through a thread-local flag rather than panicking. `NativeContextImpl` is
//! also constructed *inside JIT helpers*, where a panic would have to cross
//! the compiled frame that called the helper, and by VM-internal code outside
//! any native call (class initialisation, JVMTI functions an agent's C code
//! calls, service threads), where no handler waits; those open their own
//! scope with [`catch_alloc_oom_on`] and answer a refusal on their own channel
//! (gci i17/na, i18/na).
//!
//! [`UNWIND_OK`] therefore tracks "this thread is inside a native callback
//! with no JIT frame between it and a `catch_unwind`":
//!
//! * `safe_native_call_impl` increments it around the callback, and
//! * `JitEntryGuard` (the guard every single JIT/OSR entry constructs)
//!   suspends it to zero for the duration of the compiled frame.
//!
//! A native invoked *from* JIT code is still covered: the chain is
//! `JIT frame -> jit_invoke_* helper -> safe_native_call_impl (catch_unwind)
//! -> native`, so the unwind stops at the handler without ever reaching the
//! compiled frame. Only an allocation made by a JIT helper *directly* runs
//! with the permission cleared, and there the historical abort is retained.
//!
//! Cost: the JIT-entry side is a single thread-local read when no native call
//! is in flight (the overwhelmingly common case) and writes nothing; the
//! native-call side is one read plus two writes per call.
//!
//! # Collecting before the unwind is the NATIVE's decision
//!
//! This channel reports exhaustion; it never collects, for the reason above.
//! A native that can prove it holds no unpinned `ObjectRef` may instead SEE
//! the refusal and collect first: the fallible doors
//! (`NativeContext::try_new_array`, `try_new_ref_array`, and since gc-common
//! w8-b `try_alloc_object`) answer `None` without unwinding, and
//! `NativeContext::reclaim_before_alloc_retry` runs the interpreter's ladder.
//! `native-builtins`' `new_array_reclaiming` / `new_ref_array_reclaiming` /
//! `try_alloc_concurrent_synthetic_reclaiming` package that with the pins,
//! and the w8-b factories (`Arrays.copyOf*`, `Array.newArray`, the
//! `StringBuilder` growth, `ArrayList(int)`) use them. Everything else still
//! gets one attempt and then this unwind
//! (`docs/feature-designs/real-jdk-mode.md`).

use std::cell::Cell;

/// Panic payload marking an intentional heap-exhaustion unwind raised by
/// [`raise`]. Carries enough detail to build the `OutOfMemoryError` message.
///
/// Public so the process-wide panic hooks can recognise it and stay quiet:
/// this unwind is a *handled* condition, not a crash, and must not write an
/// `hs_err_pid<pid>.log` (which would also latch the crash handler's one-report
/// -per-process guard and suppress the report for a later real crash).
#[derive(Debug, Clone, Copy)]
pub struct NativeAllocOom {
    /// Element/slot count the caller asked for.
    pub length: usize,
    /// Human-readable allocation kind, e.g. `"byte[]"` / `"object"`.
    pub what: &'static str,
}

impl NativeAllocOom {
    /// The `OutOfMemoryError` message this unwind stands for.
    ///
    /// An ARRAY request longer than HotSpot's array limit is refused for its
    /// length, not for the heap, with HotSpot's own text
    /// (`Requested array size exceeds VM limit`), as the interpreter's
    /// `newarray` / `multianewarray` do; everything else is heap exhaustion.
    /// The parenthesised site detail of the heap arm never reaches Java
    /// (`RuntimeError::as_java_throwable` strips it), but stays for
    /// `CRATONVM_DBG_RTERR`. gc-common w3-c: one place for the rule, so the
    /// boundary that converts this payload (`safe_native_call_impl`) only has
    /// to ask; see
    /// `docs/feature-designs/real-jdk-mode.md`.
    pub fn java_message(&self) -> String {
        if self.what != "object"
            && crate::runtime::interpreter::array_length_exceeds_vm_limit(self.length)
        {
            crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT.to_string()
        } else {
            format!(
                "Java heap space (native {} of length {})",
                self.what, self.length
            )
        }
    }
}

thread_local! {
    /// Depth of native callbacks running directly under
    /// `safe_native_call_impl`'s `catch_unwind` with no JIT-compiled frame in
    /// between. Non-zero means an allocation helper may unwind to report OOM.
    static UNWIND_OK: Cell<u32> = const { Cell::new(0) };
}

/// May an allocation helper on this thread unwind to report heap exhaustion?
#[inline]
pub(crate) fn unwind_ok() -> bool {
    UNWIND_OK.with(|c| c.get() != 0)
}

/// Enter a native callback dispatched directly under `catch_unwind`.
/// Returns the previous depth, which the caller must hand back to [`restore`]
/// on **every** exit path (including the unwind one).
#[inline]
pub(crate) fn enter_native_call() -> u32 {
    UNWIND_OK.with(|c| {
        let prev = c.get();
        c.set(prev.saturating_add(1));
        prev
    })
}

/// Suspend the permission for the duration of a JIT-compiled frame. Returns
/// the previous depth (`0` when no native call is in flight, in which case
/// nothing was written and [`restore`] can be skipped).
#[inline]
pub(crate) fn suspend_for_jit() -> u32 {
    UNWIND_OK.with(|c| {
        let prev = c.get();
        if prev != 0 {
            c.set(0);
        }
        prev
    })
}

/// Restore a depth previously returned by [`enter_native_call`] /
/// [`suspend_for_jit`].
#[inline]
pub(crate) fn restore(prev: u32) {
    UNWIND_OK.with(|c| c.set(prev));
}

/// Unwind with the heap-exhaustion payload. Only ever called when
/// [`unwind_ok`] is true, i.e. when `safe_native_call_impl`'s `catch_unwind`
/// (or a [`catch_alloc_oom`] scope) is the next handler up the stack.
#[cold]
#[inline(never)]
pub(crate) fn raise(length: usize, what: &'static str) -> ! {
    // One line per occurrence at warn level: heap exhaustion inside a native
    // is worth a breadcrumb, but the user-visible signal is the Java
    // OutOfMemoryError the boundary is about to throw, not this.
    tracing::warn!(
        length,
        what,
        "native allocation could not be satisfied — raising a catchable \
         java.lang.OutOfMemoryError"
    );
    // The G1 near-full proxy-define page: `CRATONVM_DBG_OOME_LADDER=all`
    // names the native that refused (diagnostics only; a cold path).
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|v| v.eq_ignore_ascii_case("all"))
    {
        eprintln!(
            "[cratonvm] oome-ladder: native refusal {what} length={length} at:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }
    std::panic::panic_any(NativeAllocOom { length, what })
}

#[cfg(test)]
thread_local! {
    /// zs s2/nd2 (`zgc-zs1nd-proposal-forced-refusal-replay-test-20261014.md`):
    /// `(doors counted, refuse at)`. Every one-attempt door of
    /// `NativeContextImpl` entered under [`unwind_ok`] (inside a native the
    /// funnel dispatched) counts itself; when the count reaches a non-zero
    /// `refuse at`, that door unwinds as a refused allocation would and the
    /// arm is spent (the replay's re-run is clean).
    static FORCED_DOOR: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

/// zs s2/nd2: test-only "refuse the k-th one-attempt door on this thread"
/// knob (the forced-refusal replay test). Compiled out of every non-test
/// build: there it is an empty inline function.
#[inline(always)]
pub(crate) fn forced_door_refusal(_what: &'static str) {
    #[cfg(test)]
    {
        if !unwind_ok() {
            return;
        }
        let hit = FORCED_DOOR.with(|c| {
            let (n, k) = c.get();
            let n = n.saturating_add(1);
            let hit = k != 0 && n == k;
            c.set((n, if hit { 0 } else { k }));
            hit
        });
        if hit {
            raise(0, _what);
        }
    }
}

/// Arm the knob: reset the door count and refuse the `k`-th door (`0`: count
/// only).
#[cfg(test)]
pub(crate) fn forced_door_arm(k: u32) {
    FORCED_DOOR.with(|c| c.set((0, k)));
}

/// Doors counted since the last [`forced_door_arm`].
#[cfg(test)]
pub(crate) fn forced_door_count() -> u32 {
    FORCED_DOOR.with(|c| c.get().0)
}

#[cfg(test)]
thread_local! {
    /// zs s3/nd3: `(fallible doors counted, refuse at)`, the [`FORCED_DOOR`]
    /// knob's twin for the FALLIBLE doors (`try_alloc_object`,
    /// `try_new_ref_array`, `try_new_array`, `try_get_class_mirror`): the
    /// armed k-th answers `None` (the heap's refusal) and the arm is spent, so
    /// a collecting conversion's retry after the reclaim is served.
    static FORCED_TRY: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

/// zs s3/nd3: test-only "the k-th fallible door on this thread refuses"
/// knob, the mechanical positive control of every collecting conversion (a
/// refusal at each fallible door must still give the clean answer, after a
/// reclaim). Compiled out of every non-test build: there it answers `false`.
#[inline(always)]
pub(crate) fn forced_try_refusal(_what: &'static str) -> bool {
    #[cfg(test)]
    {
        if FORCED_TRY_ALL.with(|c| c.get()) {
            return true;
        }
        FORCED_TRY.with(|c| {
            let (n, k) = c.get();
            let n = n.saturating_add(1);
            let hit = k != 0 && n == k;
            c.set((n, if hit { 0 } else { k }));
            hit
        })
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    /// zs s7/nd6: every fallible door on this thread refuses (a spent
    /// reclaim ladder), not counted.
    static FORCED_TRY_ALL: Cell<bool> = const { Cell::new(false) };
}

/// zs s7/nd6: test-only "every fallible door refuses" switch.
#[cfg(test)]
pub(crate) fn forced_try_refuse_all(on: bool) {
    FORCED_TRY_ALL.with(|c| c.set(on));
}

/// Arm the fallible-door knob: reset its count and refuse the `k`-th door
/// (`0`: count only).
#[cfg(test)]
pub(crate) fn forced_try_arm(k: u32) {
    FORCED_TRY.with(|c| c.set((0, k)));
}

/// Fallible doors counted since the last [`forced_try_arm`].
#[cfg(test)]
pub(crate) fn forced_try_count() -> u32 {
    FORCED_TRY.with(|c| c.get().0)
}

/// The exhaustion hook `vm_init` installs on a ZGC heap
/// (`ZgcRealHeap::set_exhaustion_unwind`, zr z2/cp): the collector's
/// INFALLIBLE `alloc_object` / `alloc_array` arms call it before they abort.
/// Raises the [`NativeAllocOom`] unwind exactly when [`raise`] may (a
/// converting `catch_unwind` is the next handler and no JIT frame is between,
/// [`unwind_ok`]) and returns otherwise, so the arm's abort stands where an
/// unwind would have to cross a compiled frame. Generational and G1 never
/// call it.
pub fn infallible_exhaustion_unwind(length: usize, what: &'static str) {
    if unwind_ok() {
        raise(length, what);
    }
}

/// Run `f` with the unwind permission granted, catching the heap-exhaustion
/// unwind [`raise`] produces: `Err(oom)` instead of the allocation it refused.
///
/// For VM-internal code that builds a `NativeContextImpl` OUTSIDE
/// `safe_native_call_impl` and can report failure on its own channel
/// (invokedynamic linkage, `ldc` constant resolution): without this, the
/// native allocators run their infallible arm there, which aborts the process
/// on Generational and ZGC and takes G1's emergency reserve that the callers
/// which truly cannot report failure depend on
/// (`docs/feature-designs/concurrent-gc-maturation.md`). gc-common
/// w4-c; `interpreter::native_alloc_collecting` wraps this in the shared
/// collect-and-retry ladder.
///
/// Sound for the same reason the funnel's `catch_unwind` is: the unwind stops
/// HERE, so it never has to cross a JIT-compiled frame (`JitEntryGuard`
/// suspends the permission for any compiled frame `f` enters, and a native
/// `f` reaches through `safe_native_call_impl` is caught by that funnel
/// first). Any OTHER panic is resumed unchanged, so a genuine bug still
/// propagates exactly as before.
pub(crate) fn catch_alloc_oom<R>(f: impl FnOnce() -> R) -> Result<R, NativeAllocOom> {
    struct Restore(u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            restore(self.0);
        }
    }
    let _restore = Restore(enter_native_call());
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Ok(r),
        Err(payload) => match payload.downcast::<NativeAllocOom>() {
            Ok(oom) => Err(*oom),
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}

/// [`catch_alloc_oom`] for VM-internal code that builds a `NativeContextImpl`
/// on `thread` OUTSIDE `safe_native_call_impl`: runs `f(thread)` with the
/// unwind permission granted and, on a refusal, drops the native pins `f`
/// pushed (as the native funnel and `interpreter::native_alloc_collecting`
/// do) before handing the refusal back. `deliver_catching_native_alloc_oom`
/// (the GC-notification drain) is this scope plus a `MethodCallFailed`
/// mapping (gci i18/na).
///
/// gci i17/na (the audit list of
/// `gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017`):
/// such a context used to reach the allocators' INFALLIBLE arm, which aborts
/// the process on Generational and ZGC and spends G1's emergency reserve.
/// The caller decides what a refusal means at its site: an
/// `OutOfMemoryError` where Java has a channel ([`NativeAllocOom::into_runtime_error`]),
/// a dropped event or a reported start-up failure where it has none. No
/// collection here: the callers hold raw references in Rust locals (the
/// reason the native allocators never collect).
pub(crate) fn catch_alloc_oom_on<R>(
    thread: &mut crate::threading::JvmThread,
    f: impl FnOnce(&mut crate::threading::JvmThread) -> R,
) -> Result<R, NativeAllocOom> {
    let pin_base = thread.native_pin_roots.len();
    match catch_alloc_oom(|| f(&mut *thread)) {
        Ok(r) => Ok(r),
        Err(oom) => {
            thread.native_pin_roots.truncate(pin_base);
            Err(oom)
        }
    }
}

impl NativeAllocOom {
    /// The `RuntimeError` this unwind stands for; see [`Self::java_message`].
    pub(crate) fn into_runtime_error(self) -> crate::error::RuntimeError {
        crate::error::RuntimeError::OutOfMemoryError {
            message: self.java_message(),
        }
    }
}

/// The method-call failure a CAUGHT heap-exhaustion unwind stands for: a
/// `RuntimeError::OutOfMemoryError`, which every caller of the interpreter
/// already turns into a Java throwable.
///
/// For the interpreter's own `catch_unwind`s (`interpreter::execute` and
/// `run_pushed_frame_to_completion`), gc-common w5-c. The permission
/// [`UNWIND_OK`] is a per-thread DEPTH, and the interpreter does not suspend
/// it (only `JitEntryGuard` does), so interpreted Java that a native calls
/// back into runs with it raised. Interpreter-internal code there that builds
/// a `NativeContextImpl` of its own (indy linkage and rendering, reflection
/// helpers) takes the allocators' unwinding arm, and the NEAREST handler above
/// such an allocation is the interpreter's, not the native funnel's. Those
/// handlers stringified the payload, so a heap-exhaustion `OutOfMemoryError`
/// reached Java as `NotImplemented("unknown panic in bytecode execution")`
/// with a `[PANIC_IN]` line on stderr
/// (`common-w4c-native-oom-unwind-inside-a-java-callback-becomes-an-internal-error`).
///
/// Call it only for a payload that [`is_native_alloc_oom_payload`] accepted;
/// any other payload is resumed unchanged, so a genuine bug still propagates.
pub(crate) fn caught_oom_as_call_failure(
    payload: Box<dyn std::any::Any + Send>,
) -> crate::error::MethodCallFailed {
    match payload.downcast::<NativeAllocOom>() {
        Ok(oom) => crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            oom.into_runtime_error(),
        )),
        Err(other) => std::panic::resume_unwind(other),
    }
}

/// Is this caught payload the allocators' heap-exhaustion unwind? The
/// `catch_unwind` arm guard that pairs with [`caught_oom_as_call_failure`].
#[inline]
pub(crate) fn is_native_alloc_oom_payload(payload: &(dyn std::any::Any + Send)) -> bool {
    payload.is::<NativeAllocOom>()
}

/// Is this panic the intentional heap-exhaustion unwind from [`raise`]?
///
/// Used by the panic hooks (`runtime::crash_handler` and the CLI's own hook)
/// to skip crash reporting for a condition the VM handles.
pub fn is_native_oom_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    info.payload().is::<NativeAllocOom>()
}

#[cfg(test)]
mod tests;
