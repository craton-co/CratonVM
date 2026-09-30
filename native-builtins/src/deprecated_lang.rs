// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deprecated java.lang.* API native implementations.
//!
//! Every method marked `@Deprecated` (or `@Deprecated(forRemoval=true)`) in
//! the JDK 25 java.lang package is implemented here.  Most throw
//! `UnsupportedOperationException` or `NoSuchMethodError` because the JDK 25
//! runtime disables these operations by default.
//!
//! T8.1.8 (SecurityManager) is already fully covered in `security_manager.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// T8.1.1 — Thread.stop()
// ---------------------------------------------------------------------------

/// Global flag: when `false` (default for JDK 25), Thread.stop() throws
/// `UnsupportedOperationException`.  Tests or compatibility layers can set it
/// to `true` to allow the deprecated behaviour.
pub(crate) static ALLOW_THREAD_STOP: AtomicBool = AtomicBool::new(false);

/// One pending stop request: the VM that filed it and its throwable, as a
/// JNI global root owned by the row (`0` where the context has none; then
/// `fallback` is the throwable).
#[derive(Clone, Copy)]
struct StopRequest {
    vm: usize,
    root: usize,
    fallback: ObjectRef,
}

/// Per-thread stop-request state, and the roots of requests whose Thread
/// died (per VM, awaiting a `ctx` to release them).
#[derive(Default)]
struct StopRequests {
    rows: std::collections::HashMap<usize, StopRequest>,
    orphaned_roots: std::collections::HashMap<usize, Vec<usize>>,
}

/// Per-thread stop-request state.
///
/// GC note (gc-followups-20260706): effectively WRITE-ONLY today — the only
/// reader, `take_stop_throwable`, is `#[allow(dead_code)]` with no callers,
/// and the writer is further gated behind `ALLOW_THREAD_STOP` (default off).
///
/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 36, route R1): keyed by the Thread's WEAK LOCK KEY
/// (`crate::gc_stable_weak_lock_key`). It was keyed by the bare identity hash
/// with NO VM, so two Threads with one hash -- in one VM, or the first
/// Threads of any two VMs -- shared a request, and the throwable was a
/// PERMANENT `register_var_handle_root` root read back by its own hash. A
/// lock key is unique per object across VMs; the row goes when its Thread
/// dies ([`forget_deprecated_thread_keys`]), and the throwable is a global
/// root the row owns.
static THREAD_STOP_REQUESTS: Mutex<Option<StopRequests>> = Mutex::new(None);

fn thread_stop_map() -> std::sync::MutexGuard<'static, Option<StopRequests>> {
    THREAD_STOP_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn store_stop_throwable(
    ctx: &mut dyn NativeContext,
    thread: ObjectRef,
    throwable: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // Minted before the table lock; nothing here allocates.
    let key = crate::gc_stable_weak_lock_key(&*ctx, thread)?;
    let vm = ctx.vm_identity();
    let row = StopRequest {
        vm,
        root: ctx.add_global_root(throwable),
        fallback: throwable,
    };
    let (replaced, orphans) = {
        let mut guard = thread_stop_map();
        let table = guard.get_or_insert_with(Default::default);
        (
            table.rows.insert(key, row),
            table.orphaned_roots.remove(&vm).unwrap_or_default(),
        )
    };
    let released = replaced.map(|old| old.root).into_iter().chain(orphans);
    for root in released.filter(|&root| root != 0) {
        let _ = ctx.remove_global_root(root);
    }
    Ok(())
}

/// Read (and consume) the pending stop-throwable for `thread`, if any, at
/// its CURRENT address; the request's root is released. Never mints a key.
#[allow(dead_code)]
pub(crate) fn take_stop_throwable(ctx: &mut dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let key = crate::existing_weak_lock_key(&*ctx, thread)?;
    let row = thread_stop_map().as_mut().and_then(|t| t.rows.remove(&key))?;
    if row.root == 0 {
        return Some(row.fallback);
    }
    let current = ctx.resolve_global_root(row.root).unwrap_or(row.fallback);
    let _ = ctx.remove_global_root(row.root);
    Some(current)
}

/// The lock-key sweep freed `keys` (their objects are dead): drop those
/// Threads' stop requests (queueing each throwable's root for its VM) and
/// suspended flags. gc-common w29-a; called through
/// `phases_early::forget_thread_state_keys`.
pub(crate) fn forget_deprecated_thread_keys(keys: &[usize]) {
    if let Some(table) = thread_stop_map().as_mut() {
        for key in keys {
            if let Some(row) = table.rows.remove(key) {
                if row.root != 0 {
                    table.orphaned_roots.entry(row.vm).or_default().push(row.root);
                }
            }
        }
    }
    if let Some(map) = suspended_map().as_mut() {
        for key in keys {
            map.remove(key);
        }
    }
}

/// `Thread.stop0(Object throwable)V` — the HotSpot-internal native.
fn native_thread_stop0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !ALLOW_THREAD_STOP.load(Ordering::Relaxed) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Thread.stop() is not supported".to_string(),
        }
        .into());
    }
    // args[0] = this (Thread), args[1] = throwable object
    let this = obj_arg(args, 0)?;
    let throwable = obj_arg(args, 1)?;
    // GC-stable key: the Thread's weak lock key (gc-common w29-a), neither
    // the raw address nor the identity hash.
    store_stop_throwable(ctx, this, throwable)?;
    Ok(None)
}

// `Thread.stop()V` — RETIRED, 2026-08-21 (WORKER 3, `H25-3` R1 / N1).
//
// The native threw `UnsupportedOperationException("Thread.stop() is not
// supported")` unless `ALLOW_THREAD_STOP` was set, and nothing outside this
// file's own tests can set it. MEASURED across nine images — JDK 17.0.20,
// 21.0.12 and 25.0.4 x linux/windows/macos — every one of them declares
// `public final void stop()` WITH a `Code` attribute:
//
// ```text
//   javap -c --system <image> java.lang.Thread     (JDK 21, JDK 25)
//     0: new  #.. // class java/lang/UnsupportedOperationException
//     7: athrow
// ```
//
// so on the two modern images the real bytecode throws the very exception the
// native threw, and on JDK 17 it does the real deprecated work by way of
// `stop0` (still registered below). Retiring it is behaviour-preserving on
// 21/25 and a fidelity IMPROVEMENT — the JDK's own `UnsupportedOperationException`
// carries no message, and ours invented one.
//
// The retirement had to be paired: `native-builtins/src/lib.rs` held a SECOND
// registration of the same triple whose body called `ctx.thread_interrupt` and
// returned normally. Deleting only this one would have promoted that. Both went
// in the same commit; see the note at the `lib.rs` site.

// ---------------------------------------------------------------------------
// T8.1.2 — Thread.suspend() / resume()
// ---------------------------------------------------------------------------

/// Per-thread suspended flag, keyed by the Thread's weak lock key (gc-common
/// w29-a; it was the bare identity hash with no VM, like
/// [`THREAD_STOP_REQUESTS`]). A dead Thread's flag goes with its key.
static THREAD_SUSPENDED: Mutex<Option<std::collections::HashMap<usize, AtomicBool>>> =
    Mutex::new(None);

fn suspended_map(
) -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<usize, AtomicBool>>> {
    THREAD_SUSPENDED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether deprecated suspend/resume is allowed.  Default: false (JDK 25).
pub(crate) static ALLOW_THREAD_SUSPEND: AtomicBool = AtomicBool::new(false);

fn native_thread_suspend0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !ALLOW_THREAD_SUSPEND.load(Ordering::Relaxed) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Thread.suspend() is not supported".to_string(),
        }
        .into());
    }
    let this = obj_arg(args, 0)?;
    // GC-stable key: the Thread's weak lock key (gc-common w29-a), minted
    // before the table lock. Not the raw address (which went stale on the
    // first moving GC, stranding the flag), nor the identity hash (which two
    // Threads can share).
    let tid = crate::gc_stable_weak_lock_key(&*ctx, this)?;
    let mut guard = suspended_map();
    let map = guard.get_or_insert_with(Default::default);
    map.entry(tid)
        .or_insert_with(|| AtomicBool::new(false))
        .store(true, Ordering::Release);
    Ok(None)
}

fn native_thread_resume0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !ALLOW_THREAD_SUSPEND.load(Ordering::Relaxed) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Thread.resume() is not supported".to_string(),
        }
        .into());
    }
    let this = obj_arg(args, 0)?;
    // GC-stable key — see `native_thread_suspend0`. Never minted here: a
    // Thread that was never suspended has no flag to clear.
    let Some(tid) = crate::existing_weak_lock_key(&*ctx, this) else {
        return Ok(None);
    };
    let mut guard = suspended_map();
    if let Some(map) = guard.as_mut() {
        if let Some(flag) = map.get(&tid) {
            flag.store(false, Ordering::Release);
        }
    }
    // In a real VM we would also notify any thread waiting on the suspend
    // condition variable. The flag flip is sufficient for correctness tests.
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.1.3 — `Thread.destroy()V`: RETIRED, 2026-08-21 (WORKER 3).
//
// The body raised `NoSuchMethodError(java/lang/Thread.destroy()V)` — which is
// exactly what an unregistered call to a method no image declares raises
// anyway, so the registration bought nothing and cost a row.
//
// MEASURED, `javap -p --system <image> java.lang.Thread` over all nine
// supported images (JDK 17.0.20 / 21.0.12 / 25.0.4 x linux/windows/macos):
// **no image declares `destroy` at all**. It was removed in JDK 11, not
// deprecated. This is the one verb `H14-1` had no name for and `H25-1` sized
// at 342 registrations without being able to act on any of them — a triple no
// supported image declares anywhere on the receiver's hierarchy — and it is
// the first row retired on that evidence.
//
// The multi-image part is load-bearing and is why `H25-1` refused: four of the
// six `java/lang` rows that looked identical to this one on a JDK 25 census
// (`stop0`, `suspend0`, `resume0`, `countStackFrames`) ARE declared by JDK 17
// or JDK 21 and must stay. `scripts/jdk-only-no-image-methods.py` is the sweep
// that separates them.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// T8.1.4 — Thread.countStackFrames()
// ---------------------------------------------------------------------------

fn native_thread_count_stack_frames(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Err(RuntimeError::UnsupportedOperationException {
        message: "Thread.countStackFrames() is unsupported".to_string(),
    }
    .into())
}

// ---------------------------------------------------------------------------
// T8.1.5 — Object.finalize(): `FinalizationTracker` RETIRED, gc-common w4-d
// (2026-09-23).
//
// A process-global queue (`FINALIZATION_TRACKER`) that nothing in production
// fed — `track_finalization` had test callers only — drained by the
// `runFinalization` natives below, which therefore did nothing. It was also a
// process global holding compatibility state, which AGENTS.md forbids. The
// VM's real finalizer queue is per VM (`SharedVm::mem.finalizer_thread`), fed
// by the collector, and is what `runFinalization` now drains through
// `NativeContext::run_finalization`. See
// `docs/internal/gc-common-round-20260923/common-w3d-run-finalization-never-drains-the-vm-queue-FIXED-20260923.md`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// T8.1.6 — Runtime.runFinalization() / System.runFinalization()
// ---------------------------------------------------------------------------

/// `Runtime.runFinalization()` / `System.runFinalization()`: run every
/// finalizer the VM has queued so far (on its reference-delivery thread when
/// one serves), then return. `Finalizer.register` is intercepted in this VM,
/// so the JDK's own `Finalizer` queue — the one the real bytecode's secondary
/// finalizer would drain — is always empty; the VM's queue is the only one.
fn native_run_finalization(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ctx.run_finalization();
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.1.7 — `System.runFinalizersOnExit(Z)V`: RETIRED, 2026-08-21 (WORKER 3).
//
// MEASURED, `javap -p --system <image> java.lang.System` over all nine
// supported images: none declares it. It was removed in JDK 11. Same verb as
// `Thread.destroy` above.
//
// The flag it wrote, `RUN_FINALIZERS_ON_EXIT`, went with it: `grep -rn` found
// exactly one writer (this native) and no reader outside this file's own test.
// A write-only flag set by an unreachable native is two absences agreeing with
// each other, and neither is evidence the feature exists.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// T8.1.9 — ClassLoader.defineClass(byte[], int, int)
// ---------------------------------------------------------------------------

fn native_classloader_define_class_3arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // 3-arg deprecated variant: `this`, `byte[]`, `offset`, `length`.
    // Delegates to the 4-arg form with name = null.
    // args[0] = this (ClassLoader), args[1] = byte[], args[2] = int off, args[3] = int len
    let this = obj_arg(args, 0)?;
    let byte_array = args.get(1).cloned().unwrap_or(Value::Object(None));
    let offset = args.get(2).cloned().unwrap_or(Value::Int(0));
    let length = args.get(3).cloned().unwrap_or(Value::Int(0));

    // Attempt to invoke the 4-arg defineClass(String, byte[], int, int) with
    // name = null.
    ctx.invoke_virtual(
        this,
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        &[Value::Object(None), byte_array, offset, length],
    )
}

// ---------------------------------------------------------------------------
// T8.1.10 — Compiler class (removed in JDK 9, legacy stubs)
// ---------------------------------------------------------------------------

fn native_compiler_compile_class(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Always returns false — no JIT compiler accessible via this API.
    Ok(Some(Value::Int(0)))
}

fn native_compiler_compile_classes(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_compiler_command(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Returns null — the command is a no-op.
    Ok(Some(Value::Object(None)))
}

fn native_compiler_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register_deprecated_lang_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let thread = "java/lang/Thread";

    // T8.1.1 — Thread.stop0.
    //
    // KEPT, and its DUPLICATE in `native-builtins/src/lib.rs` was deleted
    // rather than this one: all three JDK 17 images declare
    // `private native void stop0(java.lang.Object)`, so this is a
    // cross-version registration, not a dead stub. JDK 21 and JDK 25 declare
    // no `stop0` at all, which is why a JDK-25-only census reports it as
    // reaching nothing.
    r.register(
        thread,
        "stop0",
        "(Ljava/lang/Object;)V",
        native_thread_stop0,
    );
    // T8.1.1 — `Thread.stop()V` is NOT registered: see the retirement note above.

    // T8.1.2 — Thread.suspend / resume.
    //
    // KEPT even though JDK 21 and JDK 25 declare neither: MEASURED,
    // `javap -p --system <image> java.lang.Thread` finds
    // `private native void suspend0()` and `resume0()` on all three JDK 17
    // images. This is the `StringUTF16.isBigEndian` shape — a registration that
    // is correct precisely because a supported image other than the one on this
    // host declares the method — and it is why a single-image census may not
    // retire anything in this population.
    r.register(thread, "suspend0", "()V", native_thread_suspend0);
    r.register(thread, "resume0", "()V", native_thread_resume0);

    // T8.1.3 — `Thread.destroy()V` is NOT registered: no supported image
    // declares it. See the retirement note above.

    // T8.1.4 — Thread.countStackFrames. KEPT: declared by JDK 17 AND JDK 21
    // (removed in 25), so the same cross-version argument applies.
    r.register(
        thread,
        "countStackFrames",
        "()I",
        native_thread_count_stack_frames,
    );

    // T8.1.6 — Runtime.runFinalization / System.runFinalization.
    //
    // `Intrinsic`, stated here: this VM intercepts `Finalizer.register`, so
    // the JDK `Finalizer` queue the real bytecode drains is always empty and
    // the VM's own queue (`NativeContext::run_finalization`) is the only one.
    // Must win over the bytecode under `--jdk-only` too. gc-common w4-d.
    r.register_with_kind(
        "java/lang/Runtime",
        "runFinalization",
        "()V",
        native_run_finalization,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register_with_kind(
        "java/lang/System",
        "runFinalization",
        "()V",
        native_run_finalization,
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    // T8.1.7 — `System.runFinalizersOnExit(Z)V` is NOT registered: no supported
    // image declares it. See the retirement note above.

    // T8.1.8 — SecurityManager is already registered in security_manager.rs.
    // No action needed here; see security_manager::register_security_manager_natives.

    // T8.1.9 — ClassLoader.defineClass (deprecated 3-arg form)
    r.register(
        "java/lang/ClassLoader",
        "defineClass",
        "([BII)Ljava/lang/Class;",
        native_classloader_define_class_3arg,
    );

    // T8.1.10 — Compiler (removed in JDK 9, legacy stubs)
    let compiler = "java/lang/Compiler";
    r.register(
        compiler,
        "compileClass",
        "(Ljava/lang/Class;)Z",
        native_compiler_compile_class,
    );
    r.register(
        compiler,
        "compileClasses",
        "(Ljava/lang/String;)Z",
        native_compiler_compile_classes,
    );
    r.register(compiler, "enable", "()V", native_compiler_noop);
    r.register(compiler, "disable", "()V", native_compiler_noop);
    r.register(
        compiler,
        "command",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_compiler_command,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn call_native(
        registry: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        class: &str,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let cb = registry
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
        cb(ctx, args)
    }

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_deprecated_lang_natives(&mut r);
        r
    }

    /// Serialize tests that mutate global ALLOW_THREAD_STOP / ALLOW_THREAD_SUSPEND.
    static GLOBAL_FLAG_LOCK: Mutex<()> = Mutex::new(());

    // ----- T8.1.1 Thread.stop -----

    /// `Thread.stop()V` must stay UNREGISTERED, and this is the assertion that
    /// keeps it that way. Re-adding it would silently take the method back off
    /// the real bytecode of every supported image — and, if the `lib.rs` copy
    /// ever came back with it, would restore the "interrupt and return
    /// normally" body this retirement removed.
    #[test]
    fn t8_1_1_thread_stop_is_retired_not_registered() {
        let reg = make_registry();
        assert!(
            reg.find("java/lang/Thread", "stop", "()V").is_none(),
            "java/lang/Thread.stop()V is served by real bytecode on every \
             supported image; it must not be registered here"
        );
    }

    #[test]
    fn test_thread_stop0_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ThreadDeath", 0).unwrap();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "stop0",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(thr)), Value::Object(Some(exc))],
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_thread_stop0_stores_throwable_when_allowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_STOP.store(true, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        // gc-common w29-a: rows are keyed by weak lock keys, per (VM,
        // address); a private VM identity, forgotten at the end.
        ctx.set_vm_identity(0x29a0_D501);
        let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Throwable", 0).unwrap();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "stop0",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(thr)), Value::Object(Some(exc))],
        );
        // Clean up the sticky flag before any assertion can fail.
        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
        assert!(res.is_ok());
        let taken = take_stop_throwable(&mut ctx, thr);
        crate::forget_vm_lock_keys(0x29a0_D501);
        assert_eq!(taken, Some(exc), "Expected stored throwable");
    }

    /// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
    /// rank 36): two Threads with one identity hash keep their own stop
    /// requests, and a dead Thread's request -- and its throwable's root --
    /// goes with its lock key. Fake Threads 4 GiB apart share the mock's hash
    /// (the address truncated to `i32`); this path never dereferences them.
    #[test]
    fn w29a_colliding_threads_keep_their_own_stop_requests() {
        const VM: usize = 0x29a0_D502;
        struct Forget;
        impl Drop for Forget {
            fn drop(&mut self) {
                crate::forget_vm_lock_keys(VM);
            }
        }
        let _forget = Forget;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        // SAFETY: keys only, never dereferenced.
        let (thread_a, thread_b) = unsafe {
            (
                ObjectRef::from_raw(0x1_29a0_d508usize as *mut u8),
                ObjectRef::from_raw(0x2_29a0_d508usize as *mut u8),
            )
        };
        assert_eq!(
            ctx.identity_hash_code(thread_a),
            ctx.identity_hash_code(thread_b),
            "premise: one identity hash"
        );
        let (exc_a, exc_b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let before = ctx.global_root_count();
        store_stop_throwable(&mut ctx, thread_a, exc_a).expect("store A");
        store_stop_throwable(&mut ctx, thread_b, exc_b).expect("store B");
        assert_eq!(ctx.global_root_count(), before + 2, "B's store released A's request");
        assert_eq!(take_stop_throwable(&mut ctx, thread_b), Some(exc_b));
        assert_eq!(ctx.global_root_count(), before + 1);

        // Thread A dies: its request goes with its key; the root is released
        // by the VM's next store.
        let a_addr = thread_a.as_ptr() as usize;
        assert_eq!(crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr), 1);
        assert_eq!(take_stop_throwable(&mut ctx, thread_a), None);
        store_stop_throwable(&mut ctx, thread_b, exc_b).expect("store B again");
        assert_eq!(take_stop_throwable(&mut ctx, thread_b), Some(exc_b));
        assert_eq!(
            ctx.global_root_count(),
            before,
            "the dead Thread's throwable must stop being rooted"
        );
    }

    // ----- T8.1.2 Thread.suspend / resume -----

    #[test]
    fn test_thread_suspend_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_SUSPEND.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "suspend0",
            "()V",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("not supported"),
            "Expected unsupported, got: {msg}"
        );
    }

    #[test]
    fn test_thread_resume_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_SUSPEND.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "resume0",
            "()V",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_err());
    }

    // ----- T8.1.3 Thread.destroy -----

    /// `Thread.destroy()V` must stay UNREGISTERED. No supported image declares
    /// it (MEASURED over nine), and an unregistered call to a method no image
    /// declares already raises `NoSuchMethodError` — which is all the retired
    /// native did.
    #[test]
    fn t8_1_3_thread_destroy_is_retired_not_registered() {
        let reg = make_registry();
        assert!(
            reg.find("java/lang/Thread", "destroy", "()V").is_none(),
            "no supported JDK image declares java/lang/Thread.destroy()V"
        );
    }

    // ----- T8.1.4 Thread.countStackFrames -----

    #[test]
    fn test_thread_count_stack_frames_throws() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5).unwrap();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "countStackFrames",
            "()I",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("unsupported"),
            "Expected unsupported, got: {msg}"
        );
    }

    // ----- T8.1.6 runFinalization -----

    /// Both `runFinalization` natives reach the VM's hook (which drains the
    /// VM's own finalizer queue — `vm_exec.rs`,
    /// `run_pending_finalizers_for_runtime`), once per call, and return void.
    /// gc-common w4-d: they used to drain a process-global tracker nothing fed.
    #[test]
    fn test_run_finalization_reaches_the_vm_hook() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        assert_eq!(ctx.run_finalization_calls.get(), 0);

        for class in ["java/lang/Runtime", "java/lang/System"] {
            let res = call_native(&reg, &mut ctx, class, "runFinalization", "()V", &[]);
            assert!(matches!(res, Ok(None)), "{class}.runFinalization: {res:?}");
        }
        assert_eq!(ctx.run_finalization_calls.get(), 2);
    }

    #[test]
    fn test_system_run_finalization_registered() {
        let reg = make_registry();
        assert!(
            reg.find("java/lang/System", "runFinalization", "()V")
                .is_some(),
            "System.runFinalization should be registered"
        );
    }

    // ----- T8.1.7 runFinalizersOnExit -----

    /// `System.runFinalizersOnExit(Z)V` must stay UNREGISTERED: no supported
    /// image declares it, and the flag it set had no reader.
    #[test]
    fn t8_1_7_run_finalizers_on_exit_is_retired_not_registered() {
        let reg = make_registry();
        assert!(
            reg.find("java/lang/System", "runFinalizersOnExit", "(Z)V")
                .is_none(),
            "no supported JDK image declares java/lang/System.runFinalizersOnExit(Z)V"
        );
    }

    // ----- T8.1.8 SecurityManager — already in security_manager.rs -----

    #[test]
    fn test_security_manager_already_registered() {
        let mut reg = NativeMethodRegistry::new();
        crate::security_manager::register_security_manager_natives(&mut reg);
        assert!(
            reg.find(
                "java/lang/SecurityManager",
                "checkPermission",
                "(Ljava/security/Permission;)V"
            )
            .is_some(),
            "SecurityManager.checkPermission must be registered by security_manager.rs"
        );
    }

    // ----- T8.1.9 ClassLoader.defineClass 3-arg -----

    #[test]
    fn test_classloader_define_class_3arg_delegates() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let loader = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ClassLoader", 0).unwrap();

        // Pre-arm invoke_virtual to return a class-like object
        let fake_class = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 0).unwrap();
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(fake_class)))));
        }

        let byte_arr = try_alloc_concurrent_synthetic(&mut ctx, "[B", 0).unwrap();
        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/ClassLoader",
            "defineClass",
            "([BII)Ljava/lang/Class;",
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(byte_arr)),
                Value::Int(0),
                Value::Int(10),
            ],
        );
        assert!(res.is_ok());
        match res.unwrap() {
            Some(Value::Object(Some(obj))) => assert_eq!(obj, fake_class),
            other => panic!("Expected Object(Some(fake_class)), got: {:?}", other),
        }
    }

    // ----- T8.1.10 Compiler -----

    #[test]
    fn test_compiler_compile_class_returns_false() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Compiler",
            "compileClass",
            "(Ljava/lang/Class;)Z",
            &[Value::Object(None)],
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_compiler_compile_classes_returns_false() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Compiler",
            "compileClasses",
            "(Ljava/lang/String;)Z",
            &[Value::Object(None)],
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_compiler_command_returns_null() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Compiler",
            "command",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(None)],
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn test_compiler_enable_disable_noop() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let res = call_native(&reg, &mut ctx, "java/lang/Compiler", "enable", "()V", &[]);
        assert!(res.is_ok());
        assert!(res.unwrap().is_none());

        let res = call_native(&reg, &mut ctx, "java/lang/Compiler", "disable", "()V", &[]);
        assert!(res.is_ok());
        assert!(res.unwrap().is_none());
    }

    // ----- Registration count -----

    #[test]
    fn test_registration_count() {
        let reg = make_registry();
        // 1 (stop0) + 2 (suspend0/resume0) + 1 (countStackFrames)
        // + 2 (runFinalization) + 1 (defineClass 3-arg) + 5 (Compiler) = 12.
        // `stop`, `destroy` and `runFinalizersOnExit` were retired 2026-08-21;
        // the three assertions above pin their ABSENCE, so this bound only has
        // to stop the registrar losing anything else.
        assert!(
            reg.len() >= 12,
            "Expected at least 12 registered natives, got {}",
            reg.len()
        );
    }
}
