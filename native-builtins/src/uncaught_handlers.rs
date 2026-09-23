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
//!      keyed by Thread `ObjectRef`.
//!   2. `vm_exec.rs::thread_start` calls `take_uncaught_handler` after
//!      `Thread.run()` returns Err, and if a handler is registered it
//!      invokes `handler.uncaughtException(thread, throwable)` via the
//!      shared dispatch path.  Default-handler fallback is also
//!      consulted via `default_uncaught_handler`.
//!
//! REAL-JDK MIRRORING (2026-07-27).  The side table alone is invisible to
//! real `java.lang.Thread` bytecode: HotSpot's own
//! `dispatchUncaughtException` reads the REAL
//! `Thread.uncaughtExceptionHandler` instance field (and, through the
//! `ThreadGroup` chain, the REAL `Thread.defaultUncaughtExceptionHandler`
//! static), so in the default real-JDK mode a handler that only ever
//! reached the side table was silently dropped — `setUncaughtExceptionHandler`
//! and `setDefaultUncaughtExceptionHandler` both had no observable effect.
//! Whether the native or the real bytecode wins dispatch for these four
//! methods differs per call path (see
//! `interpreter::force_native_over_real_jdk_bytecode` vs.
//! `invoke_or_native`), so the two stores are now kept in sync in BOTH
//! directions:
//!   * every setter writes the side table AND the real field/static;
//!   * every getter reads the side table first and falls back to the real
//!     field/static.
//! The real writes go through `set_field_by_name` /
//! `set_static_field_by_name`, i.e. they are resolved by NAME against the
//! loaded class — never a hardcoded slot index, which would be wrong for a
//! real JDK layout.  Both helpers are documented no-ops when the field does
//! not exist, so on a SYNTHETIC `java/lang/Thread` stub (flat `_f0.._fN`
//! slots, no `uncaughtExceptionHandler` field — see
//! `class_manager::synthetic_stub_fields`) the mirror silently does nothing
//! and the side table remains the only storage, exactly as before.
//!
//! The side-table lookups intentionally keep priority over the real field:
//! they are the only storage that survives a synthetic Thread layout, and
//! when both are populated they hold the same object anyway.
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

/// Name of the real `java.lang.Thread` instance field that HotSpot's own
/// `Thread.uncaughtExceptionHandler(UncaughtExceptionHandler)` bytecode
/// writes and that `getUncaughtExceptionHandler()` /
/// `dispatchUncaughtException(Throwable)` read back.
const REAL_HANDLER_FIELD: &str = "uncaughtExceptionHandler";

/// Name of the real `java.lang.Thread` STATIC field behind
/// `set/getDefaultUncaughtExceptionHandler`.  `ThreadGroup.uncaughtException`
/// consults it at the end of the parent chain, so mirroring into it keeps the
/// real dispatch chain working even when the native setter is the one that
/// ran.
const REAL_DEFAULT_HANDLER_FIELD: &str = "defaultUncaughtExceptionHandler";

/// Mirror the per-thread handler into the REAL `Thread.uncaughtExceptionHandler`
/// field, resolved by NAME on the receiver's own loaded class.
///
/// No-op when the field does not exist (synthetic `Thread` stub) — see
/// `NativeHeapAccess::set_field_by_name`, which is specified to do nothing when
/// the name does not resolve in the receiver's class hierarchy.  Writing a
/// field never allocates and never safepoints, so the caller's `thread` /
/// `handler` locals cannot go stale across it.
fn mirror_real_handler(ctx: &dyn NativeContext, thread: ObjectRef, handler: Option<ObjectRef>) {
    ctx.set_field_by_name(thread, REAL_HANDLER_FIELD, Value::Object(handler));
}

/// Read the REAL `Thread.uncaughtExceptionHandler` instance field, by name.
/// `None` when the field is absent (synthetic layout) or null.
pub fn real_thread_handler(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(thread, REAL_HANDLER_FIELD) {
        Value::Object(Some(h)) => Some(h),
        _ => None,
    }
}

/// Mirror the process-wide default into the REAL
/// `Thread.defaultUncaughtExceptionHandler` static, resolved by name.
/// No-op when `java/lang/Thread` or the static cannot be resolved.
fn mirror_real_default_handler(ctx: &mut dyn NativeContext, handler: Option<ObjectRef>) {
    ctx.set_static_field_by_name(
        "java/lang/Thread",
        REAL_DEFAULT_HANDLER_FIELD,
        Value::Object(handler),
    );
}

/// Read the REAL `Thread.defaultUncaughtExceptionHandler` static, by name.
/// `None` when the class/field cannot be resolved (synthetic stub) or null.
pub fn real_default_handler(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name("java/lang/Thread")?;
    let index = ctx.static_field_index_by_name(class_id, REAL_DEFAULT_HANDLER_FIELD)?;
    match ctx.get_static_field(class_id, index) {
        Value::Object(Some(h)) => Some(h),
        _ => None,
    }
}

/// The thread's `ThreadGroup`, read the way `Thread.getThreadGroup()` reads it.
///
/// This is the rung the handler chain was missing. `ThreadGroup` **implements
/// `Thread.UncaughtExceptionHandler`**, and the JDK's own
/// `getUncaughtExceptionHandler()` returns it whenever no per-thread handler is
/// set — which is the default state of every thread a program ever creates. Its
/// `uncaughtException` is what prints `Exception in thread "..."` plus the
/// stack trace, after giving `Thread.getDefaultUncaughtExceptionHandler()` its
/// chance.
///
/// Dropping it made the native return `null` for the overwhelmingly common
/// case, which is not merely a missing print: the JDK's
/// `Thread.dispatchUncaughtException(Throwable)` — the fallback this VM invokes
/// when it finds no handler of its own — is
/// `getUncaughtExceptionHandler().uncaughtException(this, e)`, so a null there
/// is an immediate `NullPointerException` *inside the uncaught-exception
/// handler*. Every uncaught exception on a thread with no explicit handler
/// therefore became a double fault that named neither throwable. Measured
/// against HotSpot on the same class file (`UncaughtProbe`):
/// `t.getUncaughtExceptionHandler()` is `ThreadGroup[name=main,maxpri=10]`
/// there and was `null` here.
///
/// Two layouts are accepted, in the order `Thread.getThreadGroup()` itself
/// would resolve them: JDK 19+ keeps `group` on the inner
/// `Thread$FieldHolder`, older layouts keep it directly on `Thread`. A virtual
/// thread has no `FieldHolder` at all and answers `None` here — correct by
/// omission rather than by accident, since the JDK gives virtual threads a
/// constant group whose `uncaughtException` this VM does not route through
/// this native.
fn real_thread_group(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(thread, "holder") {
        if let Value::Object(Some(group)) = ctx.get_field_by_name(holder, "group") {
            return Some(group);
        }
    }
    match ctx.get_field_by_name(thread, "group") {
        Value::Object(Some(group)) => Some(group),
        _ => None,
    }
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
            if crate::nbflags().ueh_debug {
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
                Some(Value::Object(Some(h))) => {
                    // Mirror into the REAL field FIRST, while `this`/`h` are
                    // still exactly the addresses `safe_native_call` pinned on
                    // entry: `set_field_by_name` neither allocates nor
                    // safepoints, so nothing can have moved yet.
                    mirror_real_handler(&*ctx, this, Some(*h));
                    store_handler(ctx, this, *h);
                }
                Some(Value::Object(None)) | None => {
                    mirror_real_handler(&*ctx, this, None);
                    clear_handler(&*ctx, this);
                }
                _ => {}
            }
            Ok(None)
        },
    );

    // getUncaughtExceptionHandler() — instance.  Per spec, falls back to the
    // process default and then to the thread's ThreadGroup when unset.
    //
    // The ThreadGroup rung used to be missing, and the comment here used to
    // call that an "approximation". It was not an approximation, it was the
    // default case: a thread with no explicit handler is every thread a
    // program creates, so this native answered `null` almost always — and
    // `Thread.dispatchUncaughtException` is
    // `getUncaughtExceptionHandler().uncaughtException(this, e)`, so `null`
    // there is an NPE raised inside the uncaught-exception handler itself.
    // See `real_thread_group`.
    r.register(
        th,
        "getUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(t))) => *t,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Side table first, then the REAL field: the handler may have been
            // installed by the real `Thread` bytecode on a call path where the
            // native lost dispatch, in which case only the field is populated.
            if let Some(h) =
                get_uncaught_handler(&*ctx, this).or_else(|| real_thread_handler(&*ctx, this))
            {
                return Ok(Some(Value::Object(Some(h))));
            }
            // Then the process-wide default, then the ThreadGroup. The last
            // rung is what the JDK returns for a thread with no handler of its
            // own, and `ThreadGroup.uncaughtException` is what prints
            // `Exception in thread "..."` and the stack trace.
            let fallback = default_uncaught_handler(&*ctx)
                .or_else(|| real_default_handler(&*ctx))
                .or_else(|| real_thread_group(&*ctx, this));
            Ok(match fallback {
                Some(h) => Some(Value::Object(Some(h))),
                None => Some(Value::Object(None)),
            })
        },
    );

    // setDefaultUncaughtExceptionHandler(UncaughtExceptionHandler) —
    // STATIC (no `this` receiver, args[0] is the handler).
    r.register(
        th,
        "setDefaultUncaughtExceptionHandler",
        "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
        |ctx, args| {
            let handler = match args.first() {
                Some(Value::Object(Some(h))) => Some(*h),
                _ => None,
            };
            // Mirror into the REAL static first (no allocation, no safepoint),
            // so `ThreadGroup.uncaughtException`'s parent-chain fallback — real
            // bytecode that cannot see the side table — finds it too.
            mirror_real_default_handler(ctx, handler);
            let entry = match handler {
                Some(h) => {
                    // Keep alive + registry-remapped across GC moves
                    // (VarHandle-root pattern); key computed on the
                    // just-registered address, no allocation in between.
                    ctx.register_var_handle_root(h);
                    Some((ctx.identity_hash_code(h), h))
                }
                None => None,
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
            Ok(
                match default_uncaught_handler(&*ctx).or_else(|| real_default_handler(&*ctx)) {
                    Some(h) => Some(Value::Object(Some(h))),
                    None => Some(Value::Object(None)),
                },
            )
        },
    );

    r.set_category(__prev_cat);
}
