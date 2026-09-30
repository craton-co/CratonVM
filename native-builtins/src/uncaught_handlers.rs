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

use rustc_hash::FxHashMap;

use cratonvm_native_api::registry::NativeMethodRegistry;
use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// Soft cap on tracked threads.  Each entry is a thread lock key plus a
/// handler root, so the worst-case memory footprint is well under 1 MiB at
/// the cap.  Beyond the cap we silently refuse new registrations rather than
/// evict; missing the handler degrades to "default behaviour" which matches
/// the original noop semantics.
const MAX_TRACKED_THREADS: usize = 10_000;

/// A handler held by a row: a JNI global root owned by the row, or `0` in a
/// context without global roots (then `fallback` is the handler).
#[derive(Clone, Copy)]
struct HandlerRoot {
    root: usize,
    fallback: ObjectRef,
}

impl HandlerRoot {
    fn new(ctx: &mut dyn NativeContext, handler: ObjectRef) -> Self {
        HandlerRoot {
            root: ctx.add_global_root(handler),
            fallback: handler,
        }
    }

    /// The handler at its CURRENT (post-GC) address.
    fn resolve(self, ctx: &dyn NativeContext) -> ObjectRef {
        if self.root == 0 {
            self.fallback
        } else {
            ctx.resolve_global_root(self.root).unwrap_or(self.fallback)
        }
    }

    fn release(self, ctx: &mut dyn NativeContext) {
        if self.root != 0 {
            let _ = ctx.remove_global_root(self.root);
        }
    }
}

/// One VM's per-thread handlers.
#[derive(Default)]
struct HandlerTable {
    /// The Thread's weak lock key -> its handler.
    rows: FxHashMap<usize, HandlerRoot>,
    /// Handlers whose rows went without a `&mut` ctx to release them (the
    /// Thread died -- [`forget_uncaught_handler_keys`] -- or
    /// [`take_uncaught_handler`] took the row): released by this VM's next
    /// store.
    released: Vec<HandlerRoot>,
}

/// Per-thread handler table.
///
/// GC (gc-followups-20260706): the previous raw-`ObjectRef` key went stale
/// after every moving young GC, so a relocated Thread's lookup silently
/// missed, and handler objects were collectable (nothing rooted them).
///
/// PER VM (gc-common w9-b,
/// `common-w8a-process-global-object-singletons-in-native-builtins`). Both
/// tables used to be one per process. Identity hashes are numbered from the
/// same seed in every heap, so VM B's thread found VM A's handler for A's
/// thread with the same hash. Rows are dropped by
/// [`forget_vm_uncaught_handlers`] at VM teardown.
///
/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 22, route R1): keyed by the Thread's WEAK LOCK KEY
/// (`crate::gc_stable_weak_lock_key`), no longer its identity hash -- two
/// live Threads of one VM with one hash shared a row, so thread B ran A's
/// handler and B's `set` overwrote A's. A dead Thread's key is freed by the
/// lock-key sweep, whose hook drops its row. The handler is a JNI global root
/// owned by the row; it was a PERMANENT `register_var_handle_root` root read
/// back by the handler's own identity hash (so a colliding registration
/// unrooted it, and every handler ever set stayed reachable).
static HANDLERS: VmScoped<HandlerTable> = VmScoped::new();

/// The VMs that ever filed a handler row: the lock-key hook visits their
/// tables only (the freed keys carry no VM). Dropped per VM at teardown. A
/// leaf lock, never held with another.
fn handler_vms() -> &'static cratonvm_types::lock_order::OrderedPlMutex<Vec<usize>> {
    static VMS: cratonvm_types::lock_order::OrderedPlMutex<Vec<usize>> =
        cratonvm_types::lock_order::OrderedPlMutex::new(
            Vec::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        );
    &VMS
}

/// Per-VM default handler slot, a global root owned by the slot (gc-common
/// w29-a: it was the same permanent var-handle-root pair as [`HANDLERS`]).
static DEFAULT_HANDLER: VmScoped<Option<HandlerRoot>> = VmScoped::new();

/// Drop `vm`'s per-thread handlers and its default handler (VM teardown).
/// Idempotent. The roots die with `vm`'s global-ref table.
pub fn forget_vm_uncaught_handlers(vm: usize) {
    HANDLERS.forget(vm);
    DEFAULT_HANDLER.forget(vm);
    handler_vms().lock().retain(|&v| v != vm);
}

/// The lock-key sweep freed `keys` (their objects are dead): drop the
/// handler rows of those Threads, queueing their roots for release.
/// gc-common w29-a; called through `phases_early::forget_thread_state_keys`.
pub(crate) fn forget_uncaught_handler_keys(keys: &[usize]) {
    // Copied out: never hold this list's lock and a table lock at once.
    let vms: Vec<usize> = handler_vms().lock().clone();
    for vm in vms {
        if !HANDLERS.has_row(vm) {
            continue;
        }
        HANDLERS.with(vm, |table| {
            if table.rows.is_empty() {
                return;
            }
            for key in keys {
                if let Some(handler) = table.rows.remove(key) {
                    table.released.push(handler);
                }
            }
        });
    }
}

/// Get the per-thread handler if one was set, else None.  Does not
/// remove the entry — the handler may be queried multiple times.
pub fn get_uncaught_handler(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let tkey = crate::existing_weak_lock_key(ctx, thread)?;
    let handler = HANDLERS
        .peek(ctx.vm_identity(), |table| table.rows.get(&tkey).copied())
        .flatten()?;
    Some(handler.resolve(ctx))
}

/// Atomically read + remove the per-thread handler, so the side-table
/// doesn't leak entries for long-lived VMs. The row's root is queued for the
/// VM's next store to release (this `&dyn` path cannot); the caller roots the
/// answer before its next GC point (`vm_exec::thread_start` pins it at once).
pub fn take_uncaught_handler(ctx: &dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    if !HANDLERS.has_row(vm) {
        return None;
    }
    let tkey = crate::existing_weak_lock_key(ctx, thread)?;
    let handler = HANDLERS.with(vm, |table| table.rows.remove(&tkey))?;
    let current = handler.resolve(ctx);
    HANDLERS.with(vm, |table| table.released.push(handler));
    Some(current)
}

/// Default-uncaught-exception-handler fallback (set via
/// `Thread.setDefaultUncaughtExceptionHandler`).  Per Thread spec, if
/// the per-instance handler is unset, the default handler is invoked
/// instead.
pub fn default_uncaught_handler(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handler = DEFAULT_HANDLER
        .peek(ctx.vm_identity(), |slot| *slot)
        .flatten()?;
    Some(handler.resolve(ctx))
}

/// Replace this VM's default handler (`None` clears it), releasing the root
/// of the one it replaces.
fn store_default_handler(ctx: &mut dyn NativeContext, handler: Option<ObjectRef>) {
    release_queued_handlers(ctx);
    let entry = handler.map(|h| HandlerRoot::new(ctx, h));
    let vm = ctx.vm_identity();
    let previous = DEFAULT_HANDLER.with(vm, |slot| std::mem::replace(slot, entry));
    if let Some(previous) = previous {
        previous.release(ctx);
    }
}

/// Release the handler roots queued for this VM.
fn release_queued_handlers(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    if !HANDLERS.has_row(vm) {
        return;
    }
    let queued = HANDLERS.with(vm, |table| std::mem::take(&mut table.released));
    for handler in queued {
        handler.release(ctx);
    }
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
    release_queued_handlers(ctx);
    let vm = ctx.vm_identity();
    // Minted before any table lock; nothing between here and the insert
    // allocates, so `thread` / `handler` stay current.
    let Ok(tkey) = crate::gc_stable_weak_lock_key(&*ctx, thread) else {
        return;
    };
    let full = move |table: &HandlerTable| {
        table.rows.len() >= MAX_TRACKED_THREADS && !table.rows.contains_key(&tkey)
    };
    if HANDLERS.peek(vm, full).unwrap_or(false) {
        return; // soft-fail: cap reached, keep behaviour stable
    }
    {
        let mut vms = handler_vms().lock();
        if !vms.contains(&vm) {
            vms.push(vm);
        }
    }
    let entry = HandlerRoot::new(ctx, handler);
    let outcome = HANDLERS.with(vm, |table| {
        if full(&*table) {
            return Err(()); // racing registrations crossed the cap
        }
        Ok(table.rows.insert(tkey, entry))
    });
    match outcome {
        Ok(Some(previous)) => previous.release(ctx),
        Ok(None) => {}
        Err(()) => entry.release(ctx),
    }
}

fn clear_handler(ctx: &mut dyn NativeContext, thread: ObjectRef) {
    let vm = ctx.vm_identity();
    if !HANDLERS.has_row(vm) {
        return;
    }
    release_queued_handlers(ctx);
    let Some(tkey) = crate::existing_weak_lock_key(&*ctx, thread) else {
        return;
    };
    if let Some(previous) = HANDLERS.with(vm, |table| table.rows.remove(&tkey)) {
        previous.release(ctx);
    }
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
                    clear_handler(ctx, this);
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
            // JDK: `if (isTerminated()) return null;` -- a finished thread
            // has no handler, neither its own nor its group (HotSpot clears
            // the field in `Thread.exit`). This native answered the handler
            // (or the group) for a dead thread too (interpreter round i1 wave
            // 24, lane L7; `tools/probes/interp/L7/L7W24JoinAndHandler.java`).
            // `thread_run_state` answers TERMINATED (2) from the registry,
            // which the VM marks after the uncaught-exception dispatch, so the
            // dying thread's own dispatch still sees its handler.
            if ctx.thread_run_state(this) == 2 {
                return Ok(Some(Value::Object(None)));
            }
            // Side table first, then the REAL field: the handler may have been
            // installed by the real `Thread` bytecode on a call path where the
            // native lost dispatch, in which case only the field is populated.
            if let Some(h) =
                get_uncaught_handler(&*ctx, this).or_else(|| real_thread_handler(&*ctx, this))
            {
                return Ok(Some(Value::Object(Some(h))));
            }
            // Then the ThreadGroup: what the JDK returns for a thread with no
            // handler of its own (`uncaughtExceptionHandler != null ?
            // uncaughtExceptionHandler : getThreadGroup()`), never the
            // process-wide default -- `ThreadGroup.uncaughtException` consults
            // that one itself, after its parents, and a group subclass that
            // overrides it must be the one called (interpreter round i1 wave
            // 23, lane L7; this answered the default first). The default stays
            // as the last resort for a layout without a readable group.
            let fallback = real_thread_group(&*ctx, this)
                .or_else(|| default_uncaught_handler(&*ctx))
                .or_else(|| real_default_handler(&*ctx));
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
            // Kept alive and remapped by a global root the slot owns
            // (gc-common w29-a; no allocation in between).
            store_default_handler(ctx, handler);
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

/// gc-common w9-b (`common-w8a-process-global-object-singletons-in-native-builtins`):
/// two VMs' threads with one identity hash, and two VMs' default handlers,
/// never see each other's handler.
#[cfg(test)]
mod w9b_uncaught_handler_vm_isolation_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0x9B00_0901;
    const VM_B: usize = 0x9B00_0902;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: a key / cached value only, never dereferenced (the mock's
        // identity hash is the address; its var-handle registry is empty).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Forgets this module's VMs (keys and rows) even when an assertion
    /// fails; each test names only its own VMs (lesson pp).
    struct Forget(&'static [usize]);
    impl Drop for Forget {
        fn drop(&mut self) {
            for &vm in self.0 {
                crate::forget_vm_lock_keys(vm);
                forget_vm_uncaught_handlers(vm);
            }
        }
    }

    #[test]
    fn handlers_and_default_handler_are_per_vm() {
        let _forget = Forget(&[VM_A, VM_B]);
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_A);
        let b = crate::test_utils::mock_ctx();
        b.set_vm_identity(VM_B);
        let (thread, handler, default) = (at(0x9B04_1000), at(0x9B04_2000), at(0x9B04_3000));

        store_handler(&mut a, thread, handler);
        assert_eq!(get_uncaught_handler(&a, thread), Some(handler));
        assert_eq!(
            get_uncaught_handler(&b, thread),
            None,
            "VM B's thread with the same identity hash must not see VM A's handler"
        );
        assert_eq!(take_uncaught_handler(&b, thread), None);
        assert_eq!(get_uncaught_handler(&a, thread), Some(handler), "B's take left A's row");

        store_default_handler(&mut a, Some(default));
        assert_eq!(default_uncaught_handler(&a), Some(default));
        assert_eq!(default_uncaught_handler(&b), None);

        forget_vm_uncaught_handlers(VM_A);
        forget_vm_uncaught_handlers(VM_B);
        assert_eq!(get_uncaught_handler(&a, thread), None);
        assert_eq!(default_uncaught_handler(&a), None);
    }

    /// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
    /// rank 22): two Threads of ONE VM with one identity hash keep their own
    /// handlers, and a dead Thread's row -- and its handler's root -- goes
    /// with its lock key. Fake Threads 4 GiB apart share the mock's hash (the
    /// address truncated to `i32`); these paths never dereference them.
    #[test]
    fn w29a_colliding_threads_keep_their_own_handlers() {
        const VM: usize = 0x29a0_9B01;
        let _forget = Forget(&[VM]);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (thread_a, thread_b) = (at(0x1_29a0_9b08), at(0x2_29a0_9b08));
        assert_eq!(
            ctx.identity_hash_code(thread_a),
            ctx.identity_hash_code(thread_b),
            "premise: one identity hash"
        );
        let (handler_a, handler_b) = (at(0x29a0_9c08), at(0x29a0_9d08));
        let before = ctx.global_root_count();

        store_handler(&mut ctx, thread_a, handler_a);
        assert_eq!(
            get_uncaught_handler(&ctx, thread_b),
            None,
            "thread B ran thread A's handler"
        );
        store_handler(&mut ctx, thread_b, handler_b);
        assert_eq!(get_uncaught_handler(&ctx, thread_a), Some(handler_a), "B's set overwrote A's");
        assert_eq!(get_uncaught_handler(&ctx, thread_b), Some(handler_b));
        assert_eq!(ctx.global_root_count(), before + 2);

        // Thread A dies: its key is freed and its row goes; the root is
        // released by the VM's next store.
        let a_addr = thread_a.as_ptr() as usize;
        assert_eq!(crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr), 1);
        assert_eq!(get_uncaught_handler(&ctx, thread_b), Some(handler_b));
        store_handler(&mut ctx, thread_b, handler_b);
        assert_eq!(
            ctx.global_root_count(),
            before + 1,
            "the dead Thread's handler must stop being rooted"
        );
        assert_eq!(
            get_uncaught_handler(&ctx, thread_a),
            None,
            "a new Thread at A's address inherited A's handler"
        );

        // `take` removes only B's row, and its root goes at the next store.
        assert_eq!(take_uncaught_handler(&ctx, thread_b), Some(handler_b));
        assert_eq!(get_uncaught_handler(&ctx, thread_b), None);
        store_handler(&mut ctx, thread_a, handler_a);
        clear_handler(&mut ctx, thread_a);
        assert_eq!(ctx.global_root_count(), before);
    }
}
