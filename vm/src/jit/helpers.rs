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
// Thread-local JvmThread pointer for JIT helper access
// ---------------------------------------------------------------------------

thread_local! {
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

/// Set the current thread's JvmThread pointer for JIT helper access.
/// Returns the previously stored pointer so callers can restore it later
/// (re-entrant JIT calls via interpreter::execute inside jit_invoke_dispatch).
///
/// # Safety contract
/// The caller must ensure that no other `&mut JvmThread` reference exists for the
/// duration of JIT execution. The pointer is only dereferenced inside JIT helpers
/// which execute on the same thread that set it.
pub fn set_jit_thread(thread: &mut JvmThread) -> *mut JvmThread {
    JIT_THREAD.with(|t| {
        let old = t.get();
        t.set(thread as *mut JvmThread);
        old
    })
}

/// Check if the JIT thread pointer is already set.
#[inline(always)]
pub fn is_jit_thread_set() -> bool {
    JIT_THREAD.with(|t| !t.get().is_null())
}

/// Restore a previously saved JIT thread pointer. Used to support re-entrant
/// JIT calls (e.g. JIT put() → jit_invoke_dispatch → interpreter::execute hash()
/// which may JIT-compile hash() and call set_jit_thread again).
pub fn restore_jit_thread(old: *mut JvmThread) {
    JIT_THREAD.with(|t| t.set(old));
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
            debug_assert!(
                !b.get(),
                "jit_thread_mut: aliasing &mut JvmThread borrow detected \
                 (a prior JitThreadGuard is still live)"
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

/// Call a JIT-compiled Java method entry with the VM's extern "C" ABI.
///
/// `entry` must be a live code pointer from [`try_jit_compile_callee`] /
/// `CompiledMethod::entry_ptr`. `args_slice` is the raw `i64` array the JIT
/// stub passes (receiver + parameters in JVM order).
///
/// **Arity limits.** The transmuted call-tables below cover the System V
/// AMD64 / win64 register-arg conventions for up to 4 Java args (plus an
/// optional `vm_ptr` context slot). For methods with more arguments we do
/// **not** silently return 0 — that was a HIGH-severity correctness bug
/// that let any JIT-dispatched callsite with a 5+ arg method (e.g. many
/// `java.util.concurrent` worker constructors, Spring `BeanWrapperImpl`
/// setters) appear to return null/0 to its caller while never actually
/// executing the body.
///
/// TODO(round-4-wave-3): emit stack arg setup for >4 arg JIT calls so we
/// can stay on the compiled fast path. Until then, callees with too many
/// args are routed through the interpreter via `bail_to_interpreter`. The
/// caller passes `vm`, `thread`, `info`, and the decoded `Value` arg
/// vector so the bailout can issue a real `invoke_or_native` and surface
/// any thrown exception through `handle_jit_dispatch_error`.
#[inline]
unsafe fn call_jit_compiled_method_entry(
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    args_slice: &[i64],
    // Bailout context. `bail_args` are the Java-level `Value`s reconstructed
    // by the caller; on too-many-args we hand them to `invoke_or_native`.
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    bail_args: &[Value],
) -> i64 {
    let n = args_slice.len();
    // Register-arg coverage: ctx-ABI can pass 3 Java args plus vm_ptr (4 total
    // System V regs); no-ctx ABI can pass 4 Java args. Beyond that we don't
    // have stack-arg setup, so bail to the interpreter rather than calling
    // through with truncated arguments.
    let register_limit = if needs_ctx { 3 } else { 4 };
    if n > register_limit {
        return bail_to_interpreter(vm, thread, info, bail_args);
    }
    if needs_ctx {
        match n {
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
            // Unreachable — guarded by `register_limit` check above.
            _ => bail_to_interpreter(vm, thread, info, bail_args),
        }
    } else {
        match n {
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
            // Unreachable — guarded by `register_limit` check above.
            _ => bail_to_interpreter(vm, thread, info, bail_args),
        }
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
    let res = crate::vm::invoke_or_native(
        vm,
        thread,
        info.class_name,
        info.method_name,
        info.descriptor,
        args,
    );
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
    let data_size = cratonvm_types::array_data_size(length as usize, elem_type).unwrap_or(0);
    let total_size = cratonvm_types::HEADER_SIZE + data_size;
    if heap.try_alloc_young_probe(total_size).is_none() {
        // Young gen full — trigger GC through the orchestrated STW path.
        // CRIT (jit/gc audit, 2026-05): MUST retire the calling thread's
        // TLAB before kicking off GC. The retire installs a synthetic
        // `int[]` filler at the cursor so the heap walker can stride over
        // the unused TLAB tail in O(1); without it, the walker
        // mis-decodes the tail's zeroed bytes (or a half-init JIT object)
        // and aborts with "implausible object size" / corrupts old gen
        // when promote-on-pressure copies stale pointers. Mirrors
        // `alloc_object_shared` in the interpreter (runtime/interpreter.rs
        // line ~782).
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.tlab.retire();
            // Route allocation-failure GC through the interpreter's
            // orchestrated STW path (`maybe_gc_forced` -> `gc_barrier.request_stw()`
            // + `wait_for_all()`), so other mutator threads are parked
            // before the moving collector rewrites object addresses.
            // (Resolves the prior FIXME that called `heap.collect_garbage`
            // with an unchecked StopTheWorldToken.)
            crate::runtime::interpreter::maybe_gc_forced_pub(vm, thread);
        }
    }
    let obj_ref = heap.alloc_array(ClassId::new(0), elem_type, length as usize);
    let raw = obj_ref.as_ptr();
    if std::env::var_os("CRATON_JIT_NEWARRAY_TRACE").is_some() {
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

    obj_ptr
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and num_fields must match the class metadata resolved at compile time.
// The returned i64 is a raw heap pointer to the newly allocated object.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_new_object(vm_ptr: i64, class_id_raw: i64, num_fields: i64) -> i64 {
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
        return 0;
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
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr = val as u8;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null triggers a pending NPE + `i64::MIN` deopt
// sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_iaload(array_ptr: i64, index: i64) -> i64 {
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
        return 0;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *const i32;
    *elem_ptr as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null aborts the process — see `jit_bastore` for
// the rationale. Out-of-bounds is handled gracefully.
pub unsafe extern "C" fn jit_iastore(array_ptr: i64, index: i64, val: i64) {
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
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *mut i32;
    *elem_ptr = val as i32;
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a reference array object. Null triggers a pending NPE + `i64::MIN`
// deopt sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_aaload(array_ptr: i64, index: i64) -> i64 {
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
        return 0;
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
    if obj_ptr == 0 { return 0; }
    // SAFETY: obj_ptr is non-null and points to a live object. HEADER_SIZE + field_index * SLOT_SIZE
    // is within the object's allocated region because field_index was resolved at JIT compile time.
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

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    if std::env::var_os("CRATON_JIT_PFI_TRACE").is_some() {
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
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated region.
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    std::ptr::write(ptr as *mut Value, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 { return; }
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
    {
        let heap = heap_from_vm(vm_ptr);
        let num_slots = heap.num_fields(obj_ref);
        if field_index < 0 || field_index as usize >= num_slots {
            return;
        }
    }
    if std::env::var_os("CRATON_JIT_PFO_TRACE").is_some() {
        let cid_off = obj_ptr as *const u8;
        let cid: u32 = std::ptr::read(cid_off as *const u32);
        if cid == 394 {
            // Check the value's class_id if it's an object
            let val_class_id = if val != 0 {
                let v_cid_ptr = val as *const u8;
                std::ptr::read(v_cid_ptr as *const u32)
            } else { 0 };
            // Check kind byte of value
            let val_kind = if val != 0 {
                let v_kind_ptr = (val as *const u8).add(4);
                std::ptr::read(v_kind_ptr)
            } else { 0 };
            let val_arrlen = if val != 0 {
                let len_ptr = (val as *const u8).add(12);
                std::ptr::read(len_ptr as *const u32)
            } else { 0 };
            eprintln!("[JIT-PFO] obj=0x{:x} class_id=394 field_index={} val=0x{:x} val_cid={} val_kind={} val_arrlen={}",
                obj_ptr as usize, field_index, val as u64, val_class_id, val_kind, val_arrlen);
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
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
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
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Long(val));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_float(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Float(f32::from_bits(val as u32)));
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_double(vm_ptr: i64, class_id_raw: i64, field_index: i64, val: i64) {
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
    JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))));
    i64::MIN // deopt sentinel — interpreter will detect and throw AIOOBE
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

/// Invocation threshold for triggering JIT compilation from the dispatch helper.
const DISPATCH_JIT_THRESHOLD: u32 = 500;

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
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
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
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush. A dispatched
    // call may transitively enter the GC barrier through callee
    // allocations or `Object.wait` paths.
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr and info_ptr originate from JIT code; both point to valid, live objects.
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
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
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{}",
            info.class_name, info.method_name, info.descriptor, info.invoke_kind, num_args, buf,
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

    // Fast path: check thread-local dispatch cache for a previously-compiled callee.
    // This avoids the JIT cache lock on every call.
    let info_key = info_ptr as usize;
    let cached_entry = DISPATCH_CACHE.with(|dc| {
        dc.borrow().get(&info_key).map(|c| (c.entry, c.needs_context))
    });
    if let Some((entry, needs_ctx)) = cached_entry {
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
            return rc;
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
    {
        let jit_cache = vm.jit_cache.read();
        if let Some(compiled) = jit_cache.get(info.class_name, info.method_name, info.descriptor) {
            let entry = compiled.entry_ptr() as usize;
            let needs_ctx = compiled.needs_context();
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
                return rc;
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }

    // Invocation counting — trigger compilation for hot callees
    let should_compile = DISPATCH_COUNTER.with(|dc| {
        let mut map = dc.borrow_mut();
        let count = map.entry(info_key).or_insert(0);
        *count += 1;
        *count == DISPATCH_JIT_THRESHOLD
    });
    if should_compile {
        // Try to compile the callee and cache it
        if let Some((entry, needs_ctx)) = try_compile_callee(vm, info) {
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(info_key, DispatchCache { entry, needs_context: needs_ctx });
            });
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. CRIT round-5 fix: bail explicitly to the interpreter on
            // >ARG_REGS args via `bail_to_interpreter` (matches the MIC fast-path).
            if let Some(rc) = try_call_compiled_entry(entry, needs_ctx, vm_ptr, args_slice) {
                return rc;
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
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => return 0,
    };

    let mut values = Vec::with_capacity(num_args.max(0) as usize);
    let mut desc_iter = DescriptorParamIter::new(info.descriptor);

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
    values.push(Value::Object(Some(receiver_ref)));

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

    let receiver_class_id = vm.heap.class_id_of(receiver_ref);
    let receiver_cid = receiver_class_id.as_u32();

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
            // `values` already holds the full receiver + decoded param vector
            // for this dispatch site (built above before the cache hit). It
            // is forwarded as the bailout argument list so that callees with
            // more than 4 args route through the interpreter instead of
            // silently returning 0 from the truncated register-arg table.
            return call_jit_compiled_method_entry(
                entry as usize,
                needs_ctx,
                vm_ptr,
                args_slice,
                vm,
                thread,
                info,
                &values,
            );
        }

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
        let class_name: String = if vm.heap.kind_of(receiver_ref)
            == cratonvm_types::ObjectKind::Array
        {
            "java/lang/Object".to_string()
        } else {
            let guard = mic.cached_class_name.lock();
            match &*guard {
                Some(name) => name.clone(),
                None => {
                    drop(guard);
                    let cm = vm.class_manager.read();
                    cm.get_class(receiver_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default()
                }
            }
        };

        let method_args: Vec<Value> = values[1..].to_vec();
        let mut full_args = Vec::with_capacity(1 + method_args.len());
        full_args.push(Value::Object(Some(receiver_ref)));
        full_args.extend_from_slice(&method_args);

        // Try to compile callee for next time (populate cached_entry_ptr + needs_ctx)
        if let Some((entry_ptr, needs_ctx)) = try_compile_callee(vm, info) {
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

        let invoke_res = crate::vm::invoke_or_native(
            vm,
            thread,
            &class_name,
            info.method_name,
            info.descriptor,
            &full_args,
        );
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
            .unwrap_or_default()
    };

    // Try to compile callee for cached entry
    let (entry_ptr, needs_ctx) = match try_compile_callee(vm, info) {
        Some((ptr, nc)) => (ptr as u64, nc),
        None => (0, false),
    };

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

    let method_args: Vec<Value> = values[1..].to_vec();
    let mut full_args = Vec::with_capacity(1 + method_args.len());
    full_args.push(Value::Object(Some(receiver_ref)));
    full_args.extend_from_slice(&method_args);

    let invoke_res = crate::vm::invoke_or_native(
        vm,
        thread,
        &class_name,
        info.method_name,
        info.descriptor,
        &full_args,
    );
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
    fn jit_baload_null_returns_zero() {
        // SAFETY: array_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_baload(0, 0) };
        assert_eq!(result, 0);
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

    #[test]
    fn jit_getfield_null_returns_zero() {
        // SAFETY: obj_ptr is 0 (null), so the function returns early without dereferencing.
        let result = unsafe { jit_getfield(0, 0) };
        assert_eq!(result, 0);
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
        for _ in 0..256 {
            let _ = vm_box.heap.alloc_array(
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
    JIT_THREAD.with(|t| t.get())
}

/// Build the JIT runtime helpers table with real function pointer addresses.
pub fn build_helpers() -> JitRuntimeHelpers {
    // Compute the inline-TLAB offset triple once at startup so the JIT
    // can bake them as immediates. The runtime tests
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // pin the layout against drift.
    let tlab_off = JvmThread::tlab_offset();
    let cursor_in_thread = tlab_off + cratonvm_gc::Tlab::CURSOR_OFFSET;
    let end_in_thread = tlab_off + cratonvm_gc::Tlab::END_OFFSET;

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
    a.mul_add(b, c)
}

/// T1.1.28 — Math.fma(float, float, float) runtime helper.
#[no_mangle]
pub extern "C" fn jit_math_fma_float(a: f32, b: f32, c: f32) -> f32 {
    a.mul_add(b, c)
}
