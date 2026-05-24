// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave1-C: per-Thread `UncaughtExceptionHandler` side-table.
//!
//! `Thread.setUncaughtExceptionHandler(handler)` was previously a noop
//! native (see `phases_late.rs::register_phase71_natives`), so user
//! code that relied on the handler being invoked when a thread
//! terminated with an uncaught exception got nothing — the exception
//! was eprintln'd by `vm_exec.rs::thread_start` and dropped on the
//! floor.
//!
//! The fix is two-part:
//!   1. A late-phase native override (`register_uncaught_handler_natives`)
//!      that stores the per-instance handler in a global side-table
//!      keyed by Thread `ObjectRef`.  We can't write to a real-JDK
//!      `Thread.uncaughtExceptionHandler` field directly because
//!      synthetic and real-JDK Thread layouts diverge and the field
//!      offset isn't stable.  A side-table sidesteps that.
//!   2. `vm_exec.rs::thread_start` calls `take_uncaught_handler` after
//!      `Thread.run()` returns Err, and if a handler is registered it
//!      invokes `handler.uncaughtException(thread, throwable)` via the
//!      shared dispatch path.  Default-handler fallback is also
//!      consulted via `default_uncaught_handler`.
//!
//! The side-table is bounded (`MAX_TRACKED_THREADS`) so a long-lived
//! VM with thousands of dead Thread objects can't leak handlers
//! indefinitely.  When a thread terminates and its handler fires, the
//! entry is removed (`take_uncaught_handler`).

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

use cratonvm_native_api::registry::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

/// Soft cap on tracked threads.  Each entry is two pointers
/// (`ObjectRef` + handler `ObjectRef`) so the worst-case memory
/// footprint is ~32 bytes/entry × cap ≈ 320 KiB.  Beyond the cap we
/// silently refuse new registrations rather than evict; missing the
/// handler degrades to "default behaviour" which matches the original
/// noop semantics.
const MAX_TRACKED_THREADS: usize = 10_000;

fn handlers() -> &'static Mutex<FxHashMap<ObjectRef, ObjectRef>> {
    static T: OnceLock<Mutex<FxHashMap<ObjectRef, ObjectRef>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn default_handler() -> &'static Mutex<Option<ObjectRef>> {
    static D: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(None))
}

/// Get the per-thread handler if one was set, else None.  Does not
/// remove the entry — the handler may be queried multiple times.
pub fn get_uncaught_handler(thread: ObjectRef) -> Option<ObjectRef> {
    handlers().lock().get(&thread).copied()
}

/// Atomically read + remove the per-thread handler.  Called from
/// `vm_exec.rs::thread_start` after the thread terminates so the
/// side-table doesn't leak entries for long-lived VMs.
pub fn take_uncaught_handler(thread: ObjectRef) -> Option<ObjectRef> {
    handlers().lock().remove(&thread)
}

/// Default-uncaught-exception-handler fallback (set via
/// `Thread.setDefaultUncaughtExceptionHandler`).  Per Thread spec, if
/// the per-instance handler is unset, the default handler is invoked
/// instead.
pub fn default_uncaught_handler() -> Option<ObjectRef> {
    *default_handler().lock()
}

fn store_handler(thread: ObjectRef, handler: ObjectRef) {
    let mut map = handlers().lock();
    if map.len() >= MAX_TRACKED_THREADS && !map.contains_key(&thread) {
        return; // soft-fail: cap reached, keep behaviour stable
    }
    map.insert(thread, handler);
}

fn clear_handler(thread: ObjectRef) {
    handlers().lock().remove(&thread);
}

/// Register the late-phase overrides.  The phase71 noop registration
/// is overwritten because the `NativeMethodRegistry::register` call
/// just `insert`s and last-write-wins per (class,method,desc) triple.
pub fn register_uncaught_handler_natives(r: &mut NativeMethodRegistry) {
    let th = "java/lang/Thread";

    // setUncaughtExceptionHandler(UncaughtExceptionHandler) — instance
    r.register(
        th,
        "setUncaughtExceptionHandler",
        "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
        |_ctx, args| {
            if std::env::var_os("CRATONVM_UEH_DEBUG").is_some() {
                eprintln!("[UEH] setUncaughtExceptionHandler invoked, args.len={}", args.len());
            }
            let this = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(None),
            };
            match args.get(1) {
                Some(Value::Object(Some(h))) => store_handler(this, *h),
                Some(Value::Object(None)) | None => clear_handler(this),
                _ => {}
            }
            Ok(None)
        },
    );

    // getUncaughtExceptionHandler() — instance.  Per spec, falls back
    // to ThreadGroup or default-handler when unset.  We approximate
    // that by returning the registered per-instance handler if any,
    // else the default handler, else null.
    r.register(
        th,
        "getUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
        |_ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(h) = get_uncaught_handler(this) {
                return Ok(Some(Value::Object(Some(h))));
            }
            Ok(Some(match default_uncaught_handler() {
                Some(h) => Value::Object(Some(h)),
                None => Value::Object(None),
            }))
        },
    );

    // setDefaultUncaughtExceptionHandler(UncaughtExceptionHandler) —
    // STATIC (no `this` receiver, args[0] is the handler).
    r.register(
        th,
        "setDefaultUncaughtExceptionHandler",
        "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
        |_ctx, args| {
            let mut slot = default_handler().lock();
            *slot = match args.first() {
                Some(Value::Object(Some(h))) => Some(*h),
                _ => None,
            };
            Ok(None)
        },
    );

    // getDefaultUncaughtExceptionHandler() — STATIC
    r.register(
        th,
        "getDefaultUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
        |_ctx, _args| {
            Ok(Some(match default_uncaught_handler() {
                Some(h) => Value::Object(Some(h)),
                None => Value::Object(None),
            }))
        },
    );

}
