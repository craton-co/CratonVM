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
use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// Soft cap on tracked threads.  Each entry is a thread identity key plus a
/// `(identity_key, ObjectRef)` handler pair, so the worst-case memory
/// footprint is well under 1 MiB at the cap.  Beyond the cap we
/// silently refuse new registrations rather than evict; missing the
/// handler degrades to "default behaviour" which matches the original
/// noop semantics.
const MAX_TRACKED_THREADS: usize = 10_000;

/// Per-thread handler table.
///
/// GC (gc-followups-20260706): keyed by the Thread's IDENTITY HASH (stable
/// across GC moves) — the previous raw-`ObjectRef` key went stale after every
/// moving young GC, so a relocated Thread's lookup silently missed. Values
/// are `(identity_key, ObjectRef)` pairs registered via
/// `register_var_handle_root` at store time and re-read via
/// `read_var_handle_root` at every use (ASYNC_POOL pattern, lib.rs): the GC
/// remaps the registry entry, never this raw static copy. Before this fix
/// handler objects were ALSO collectable (nothing rooted them). Trade-off:
/// registration is permanent, so a registered handler stays pinned until
/// process exit even after `clear_handler` — bounded by MAX_TRACKED_THREADS.
fn handlers() -> &'static Mutex<FxHashMap<i32, (i32, ObjectRef)>> {
    static T: OnceLock<Mutex<FxHashMap<i32, (i32, ObjectRef)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Process-wide default handler slot — same `(identity_key, ObjectRef)`
/// var-handle-root pattern as [`handlers`]; previously neither rooted nor
/// remapped (collectable AND movable out from under the cache).
fn default_handler() -> &'static Mutex<Option<(i32, ObjectRef)>> {
    static D: OnceLock<Mutex<Option<(i32, ObjectRef)>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(None))
}

/// Get the per-thread handler if one was set, else None.  Does not
/// remove the entry — the handler may be queried multiple times.
pub fn get_uncaught_handler(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let tkey = ctx.identity_hash_code(thread);
    let (key, cached) = handlers().lock().get(&tkey).copied()?;
    // Re-read the CURRENT (post-GC) address; mocks fall back to the cache.
    Some(ctx.read_var_handle_root(key).unwrap_or(cached))
}

/// Atomically read + remove the per-thread handler, so the side-table
/// doesn't leak entries for long-lived VMs.
pub fn take_uncaught_handler(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let tkey = ctx.identity_hash_code(thread);
    let (key, cached) = handlers().lock().remove(&tkey)?;
    Some(ctx.read_var_handle_root(key).unwrap_or(cached))
}

/// Default-uncaught-exception-handler fallback (set via
/// `Thread.setDefaultUncaughtExceptionHandler`).  Per Thread spec, if
/// the per-instance handler is unset, the default handler is invoked
/// instead.
pub fn default_uncaught_handler(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let (key, cached) = (*default_handler().lock())?;
    Some(ctx.read_var_handle_root(key).unwrap_or(cached))
}

fn store_handler(ctx: &mut dyn NativeContext, thread: ObjectRef, handler: ObjectRef) {
    let tkey = ctx.identity_hash_code(thread);
    {
        let map = handlers().lock();
        if map.len() >= MAX_TRACKED_THREADS && !map.contains_key(&tkey) {
            return; // soft-fail: cap reached, keep behaviour stable
        }
    }
    // Keep alive + registry-remapped across GC moves (VarHandle-root
    // pattern); key computed on the just-registered address, no allocation
    // in between.
    ctx.register_var_handle_root(handler);
    let hkey = ctx.identity_hash_code(handler);
    let mut map = handlers().lock();
    if map.len() >= MAX_TRACKED_THREADS && !map.contains_key(&tkey) {
        return; // racing registrations crossed the cap; orphan pin is benign
    }
    map.insert(tkey, (hkey, handler));
}

fn clear_handler(ctx: &dyn NativeContext, thread: ObjectRef) {
    let tkey = ctx.identity_hash_code(thread);
    handlers().lock().remove(&tkey);
}

/// Register the late-phase overrides.  The phase71 noop registration
/// is overwritten because the `NativeMethodRegistry::register` call
/// just `insert`s and last-write-wins per (class,method,desc) triple.
pub fn register_uncaught_handler_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let th = "java/lang/Thread";

    // setUncaughtExceptionHandler(UncaughtExceptionHandler) — instance
    r.register(
        th,
        "setUncaughtExceptionHandler",
        "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
        |ctx, args| {
            if std::env::var_os("CRATONVM_UEH_DEBUG").is_some() {
                eprintln!(
                    "[UEH] setUncaughtExceptionHandler invoked, args.len={}",
                    args.len()
                );
            }
            let this = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(None),
            };
            match args.get(1) {
                Some(Value::Object(Some(h))) => store_handler(ctx, this, *h),
                Some(Value::Object(None)) | None => clear_handler(&*ctx, this),
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
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(h) = get_uncaught_handler(&*ctx, this) {
                return Ok(Some(Value::Object(Some(h))));
            }
            Ok(Some(match default_uncaught_handler(&*ctx) {
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
        |ctx, args| {
            let entry = match args.first() {
                Some(Value::Object(Some(h))) => {
                    // Keep alive + registry-remapped across GC moves
                    // (VarHandle-root pattern); key computed on the
                    // just-registered address, no allocation in between.
                    ctx.register_var_handle_root(*h);
                    Some((ctx.identity_hash_code(*h), *h))
                }
                _ => None,
            };
            *default_handler().lock() = entry;
            Ok(None)
        },
    );

    // getDefaultUncaughtExceptionHandler() — STATIC
    r.register(
        th,
        "getDefaultUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
        |ctx, _args| {
            Ok(Some(match default_uncaught_handler(&*ctx) {
                Some(h) => Value::Object(Some(h)),
                None => Value::Object(None),
            }))
        },
    );

    r.set_category(__prev_cat);
}
