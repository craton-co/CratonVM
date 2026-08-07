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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
use cratonvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// T8.1.1 — Thread.stop()
// ---------------------------------------------------------------------------

/// Global flag: when `false` (default for JDK 25), Thread.stop() throws
/// `UnsupportedOperationException`.  Tests or compatibility layers can set it
/// to `true` to allow the deprecated behaviour.
pub(crate) static ALLOW_THREAD_STOP: AtomicBool = AtomicBool::new(false);

/// Per-thread stop-request state.  Keyed by the ObjectRef pointer of the
/// Thread object (used as a u64 identity key).
///
/// GC note (gc-followups-20260706): effectively WRITE-ONLY today — the only
/// reader, `take_stop_throwable`, is `#[allow(dead_code)]` with no callers,
/// and the writer is further gated behind `ALLOW_THREAD_STOP` (default off).
/// GC note (cce0079 follow-up, applied per this comment's own prescription):
/// keyed by the Thread's identity hash; the throwable value is a
/// `(identity_key, last_addr)` VarHandle-root pair — rooted at store, and
/// the (future) consumer must resolve the CURRENT address via
/// `ctx.read_var_handle_root(identity_key)`.
static THREAD_STOP_REQUESTS: Mutex<Option<std::collections::HashMap<u64, (i32, ObjectRef)>>> =
    Mutex::new(None);

fn thread_stop_map(
) -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<u64, (i32, ObjectRef)>>> {
    THREAD_STOP_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn store_stop_throwable(ctx: &mut dyn NativeContext, thread_key: u64, throwable: ObjectRef) {
    ctx.register_var_handle_root(throwable);
    let vkey = ctx.identity_hash_code(throwable);
    let mut guard = thread_stop_map();
    let map = guard.get_or_insert_with(Default::default);
    map.insert(thread_key, (vkey, throwable));
}

/// Read (and consume) the pending stop-throwable for a thread, if any.
/// Returns the `(identity_key, last_addr)` pair — resolve the CURRENT
/// address via `ctx.read_var_handle_root(identity_key)` before use.
#[allow(dead_code)]
pub(crate) fn take_stop_throwable(thread_key: u64) -> Option<(i32, ObjectRef)> {
    let mut guard = thread_stop_map();
    guard.as_mut().and_then(|m| m.remove(&thread_key))
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
    // GC-stable key (cce0079 follow-up): identity hash, not the raw address.
    let thread_key = ctx.identity_hash_code(this) as u64;
    store_stop_throwable(ctx, thread_key, throwable);
    Ok(None)
}

/// `Thread.stop()V` — public deprecated wrapper that creates ThreadDeath.
fn native_thread_stop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !ALLOW_THREAD_STOP.load(Ordering::Relaxed) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Thread.stop() is not supported".to_string(),
        }
        .into());
    }
    let this = obj_arg(args, 0)?;
    // Family-1 fix (cce0079): the ThreadDeath alloc can move `this` — pin
    // and refresh before the identity read. GC-stable key as in stop0.
    let this_pin = ctx.pin_native_root(this);
    let thread_death = alloc_concurrent_synthetic(ctx, "java/lang/ThreadDeath", 0);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let thread_key = ctx.identity_hash_code(this) as u64;
    store_stop_throwable(ctx, thread_key, thread_death);
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.1.2 — Thread.suspend() / resume()
// ---------------------------------------------------------------------------

/// Per-thread suspended flag.
static THREAD_SUSPENDED: Mutex<Option<std::collections::HashMap<u64, AtomicBool>>> =
    Mutex::new(None);

fn suspended_map(
) -> std::sync::MutexGuard<'static, Option<std::collections::HashMap<u64, AtomicBool>>> {
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
    // GC-stable key (cce0079 follow-up): identity hash, not the raw address
    // (which went stale on the first moving GC, stranding the flag).
    let tid = ctx.identity_hash_code(this) as u64;
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
    // GC-stable key — see `native_thread_suspend0`.
    let tid = ctx.identity_hash_code(this) as u64;
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
// T8.1.3 — Thread.destroy()
// ---------------------------------------------------------------------------

fn native_thread_destroy(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(MethodCallFailed::InternalError(VmError::Linkage(
        LinkageError::NoSuchMethodError {
            class_name: "java/lang/Thread".to_string(),
            method_name: "destroy".to_string(),
            method_descriptor: "()V".to_string(),
        },
    )))
}

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
// T8.1.5 — Object.finalize() — FinalizationTracker
// ---------------------------------------------------------------------------

/// Tracks finalization ordering.  Objects are registered (enqueued) and must
/// be finalized in registration order.  Each object may be finalized at most
/// once.
pub(crate) struct FinalizationTracker {
    /// Objects awaiting finalization, in registration order.
    pending: VecDeque<u64>,
    /// Set of objects that have already been finalized.
    finalized: std::collections::HashSet<u64>,
}

impl FinalizationTracker {
    pub(crate) fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            finalized: std::collections::HashSet::new(),
        }
    }

    /// Register an object for finalization tracking. Uses the ObjectRef
    /// pointer as an identity key.
    pub(crate) fn track_finalization(&mut self, obj_id: u64) {
        if !self.finalized.contains(&obj_id) && !self.pending.contains(&obj_id) {
            self.pending.push_back(obj_id);
        }
    }

    /// Mark the next pending object as finalized.  Returns `Some(obj_id)` if
    /// a finalizer was due, or `None` if the queue is empty.
    pub(crate) fn mark_finalized(&mut self, obj_id: u64) -> bool {
        if self.finalized.contains(&obj_id) {
            // Already finalized — must not finalize twice.
            return false;
        }
        if self.pending.front() == Some(&obj_id) {
            self.pending.pop_front();
            self.finalized.insert(obj_id);
            true
        } else {
            // Out-of-order finalization attempt — not permitted.
            false
        }
    }

    /// Run all pending finalizers in order.  Returns the list of obj_ids
    /// that were finalized.
    pub(crate) fn run_pending(&mut self) -> Vec<u64> {
        let mut result = Vec::with_capacity(self.pending.len());
        while let Some(obj_id) = self.pending.pop_front() {
            if !self.finalized.contains(&obj_id) {
                self.finalized.insert(obj_id);
                result.push(obj_id);
            }
        }
        result
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn is_finalized(&self, obj_id: u64) -> bool {
        self.finalized.contains(&obj_id)
    }
}

/// Global finalization tracker.
static FINALIZATION_TRACKER: Mutex<Option<FinalizationTracker>> = Mutex::new(None);

fn finalization_tracker() -> std::sync::MutexGuard<'static, Option<FinalizationTracker>> {
    FINALIZATION_TRACKER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Ensure the global tracker exists and return a lock guard.
fn with_tracker<R>(f: impl FnOnce(&mut FinalizationTracker) -> R) -> R {
    let mut guard = finalization_tracker();
    let tracker = guard.get_or_insert_with(FinalizationTracker::new);
    f(tracker)
}

// ---------------------------------------------------------------------------
// T8.1.6 — Runtime.runFinalization() / System.runFinalization()
// ---------------------------------------------------------------------------

fn native_run_finalization(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    with_tracker(|tracker| {
        tracker.run_pending();
    });
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.1.7 — System.runFinalizersOnExit(boolean)
// ---------------------------------------------------------------------------

/// Global flag: if true, finalizers run on VM exit.
pub(crate) static RUN_FINALIZERS_ON_EXIT: AtomicBool = AtomicBool::new(false);

fn native_run_finalizers_on_exit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let flag = match args.first() {
        Some(Value::Int(i)) => *i != 0,
        _ => false,
    };
    RUN_FINALIZERS_ON_EXIT.store(flag, Ordering::Release);
    Ok(None)
}

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

    // T8.1.1 — Thread.stop
    r.register(
        thread,
        "stop0",
        "(Ljava/lang/Object;)V",
        native_thread_stop0,
    );
    r.register(thread, "stop", "()V", native_thread_stop);

    // T8.1.2 — Thread.suspend / resume
    r.register(thread, "suspend0", "()V", native_thread_suspend0);
    r.register(thread, "resume0", "()V", native_thread_resume0);

    // T8.1.3 — Thread.destroy
    r.register(thread, "destroy", "()V", native_thread_destroy);

    // T8.1.4 — Thread.countStackFrames
    r.register(
        thread,
        "countStackFrames",
        "()I",
        native_thread_count_stack_frames,
    );

    // T8.1.6 — Runtime.runFinalization / System.runFinalization
    r.register(
        "java/lang/Runtime",
        "runFinalization",
        "()V",
        native_run_finalization,
    );
    r.register(
        "java/lang/System",
        "runFinalization",
        "()V",
        native_run_finalization,
    );

    // T8.1.7 — System.runFinalizersOnExit
    r.register(
        "java/lang/System",
        "runFinalizersOnExit",
        "(Z)V",
        native_run_finalizers_on_exit,
    );

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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::MockNativeContext;

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

    #[test]
    fn test_thread_stop_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "stop",
            "()V",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("UnsupportedOperationException") || msg.contains("not supported"),
            "Expected UnsupportedOperationException, got: {msg}"
        );
    }

    #[test]
    fn test_thread_stop0_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);
        let exc = alloc_concurrent_synthetic(&mut ctx, "java/lang/ThreadDeath", 0);

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
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);
        let exc = alloc_concurrent_synthetic(&mut ctx, "java/lang/Throwable", 0);
        // GC-stable keying: the natives key by identity hash now.
        let tid = ctx.identity_hash_code(thr) as u64;

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "stop0",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(thr)), Value::Object(Some(exc))],
        );
        assert!(res.is_ok());
        let taken = take_stop_throwable(tid);
        assert!(taken.is_some(), "Expected stored throwable");
        assert_eq!(taken.unwrap().1, exc);

        // Clean up
        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_thread_stop_creates_thread_death_when_allowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_STOP.store(true, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);
        // GC-stable keying: the natives key by identity hash now.
        let tid = ctx.identity_hash_code(thr) as u64;

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "stop",
            "()V",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_ok());
        let taken = take_stop_throwable(tid);
        assert!(taken.is_some(), "Expected ThreadDeath stored");

        ALLOW_THREAD_STOP.store(false, Ordering::SeqCst);
    }

    // ----- T8.1.2 Thread.suspend / resume -----

    #[test]
    fn test_thread_suspend_throws_when_disallowed() {
        let _g = GLOBAL_FLAG_LOCK.lock().unwrap();
        ALLOW_THREAD_SUSPEND.store(false, Ordering::SeqCst);
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);

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
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);

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

    #[test]
    fn test_thread_destroy_throws_no_such_method() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Thread",
            "destroy",
            "()V",
            &[Value::Object(Some(thr))],
        );
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("no such method") || msg.contains("NoSuchMethod"),
            "Expected NoSuchMethodError, got: {msg}"
        );
    }

    // ----- T8.1.4 Thread.countStackFrames -----

    #[test]
    fn test_thread_count_stack_frames_throws() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let thr = alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 5);

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

    // ----- T8.1.5 FinalizationTracker -----

    #[test]
    fn test_finalization_tracker_ordering() {
        let mut ft = FinalizationTracker::new();
        ft.track_finalization(1);
        ft.track_finalization(2);
        ft.track_finalization(3);

        assert_eq!(ft.pending_count(), 3);

        // Must finalize in order: 1 first
        assert!(!ft.mark_finalized(2), "Out-of-order should fail");
        assert!(ft.mark_finalized(1), "First-in-line should succeed");
        assert!(ft.is_finalized(1));
        assert!(!ft.is_finalized(2));

        assert!(ft.mark_finalized(2));
        assert!(ft.mark_finalized(3));
        assert_eq!(ft.pending_count(), 0);
    }

    #[test]
    fn test_finalization_tracker_no_double_finalize() {
        let mut ft = FinalizationTracker::new();
        ft.track_finalization(10);
        assert!(ft.mark_finalized(10));
        // Second finalize must fail
        assert!(!ft.mark_finalized(10));
    }

    #[test]
    fn test_finalization_tracker_run_pending() {
        let mut ft = FinalizationTracker::new();
        ft.track_finalization(100);
        ft.track_finalization(200);
        ft.track_finalization(300);

        let finalized = ft.run_pending();
        assert_eq!(finalized, vec![100, 200, 300]);
        assert_eq!(ft.pending_count(), 0);
        assert!(ft.is_finalized(100));
        assert!(ft.is_finalized(200));
        assert!(ft.is_finalized(300));
    }

    #[test]
    fn test_finalization_tracker_no_duplicate_registration() {
        let mut ft = FinalizationTracker::new();
        ft.track_finalization(42);
        ft.track_finalization(42); // duplicate — ignored
        assert_eq!(ft.pending_count(), 1);
    }

    // ----- T8.1.6 runFinalization -----

    #[test]
    fn test_run_finalization_executes_pending() {
        // Seed the global tracker with some objects
        with_tracker(|t| {
            t.track_finalization(9001);
            t.track_finalization(9002);
        });

        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Runtime",
            "runFinalization",
            "()V",
            &[],
        );
        assert!(res.is_ok());

        // Verify they were finalized
        with_tracker(|t| {
            assert!(t.is_finalized(9001));
            assert!(t.is_finalized(9002));
            assert_eq!(t.pending_count(), 0);
        });
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

    #[test]
    fn test_run_finalizers_on_exit_stores_flag() {
        RUN_FINALIZERS_ON_EXIT.store(false, Ordering::Relaxed);

        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Set to true
        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/System",
            "runFinalizersOnExit",
            "(Z)V",
            &[Value::Int(1)],
        );
        assert!(res.is_ok());
        assert!(RUN_FINALIZERS_ON_EXIT.load(Ordering::Relaxed));

        // Set to false
        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/System",
            "runFinalizersOnExit",
            "(Z)V",
            &[Value::Int(0)],
        );
        assert!(res.is_ok());
        assert!(!RUN_FINALIZERS_ON_EXIT.load(Ordering::Relaxed));
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
        let loader = alloc_concurrent_synthetic(&mut ctx, "java/lang/ClassLoader", 0);

        // Pre-arm invoke_virtual to return a class-like object
        let fake_class = alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 0);
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(fake_class)))));
        }

        let byte_arr = alloc_concurrent_synthetic(&mut ctx, "[B", 0);
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
        // 2 (stop) + 2 (suspend/resume) + 1 (destroy) + 1 (countStackFrames)
        // + 2 (runFinalization) + 1 (runFinalizersOnExit)
        // + 1 (defineClass 3-arg) + 5 (Compiler) = 15
        assert!(
            reg.len() >= 15,
            "Expected at least 15 registered natives, got {}",
            reg.len()
        );
    }
}
