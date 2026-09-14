// SPDX-License-Identifier: Apache-2.0
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
//! the compiled frame that called the helper.
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
/// is the next handler up the stack.
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
    std::panic::panic_any(NativeAllocOom { length, what })
}

/// Is this panic the intentional heap-exhaustion unwind from [`raise`]?
///
/// Used by the panic hooks (`runtime::crash_handler` and the CLI's own hook)
/// to skip crash reporting for a condition the VM handles.
pub fn is_native_oom_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    info.payload().is::<NativeAllocOom>()
}
