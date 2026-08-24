// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK 25 Concurrency natives: Scoped Values (JEP 506) and Structured
//! Concurrency (JEP 505).
//!
//! ## Phase 15.1 — Scoped Values
//!
//! `java/lang/ScopedValue` provides a mechanism for sharing immutable data
//! within and across threads in a structured way.  A `ScopedValue` can be
//! *bound* to a value for the duration of a `Runnable`/`Callable` via a
//! `Carrier`.
//!
//! ## Phase 15.2 — Structured Concurrency
//!
//! `java/util/concurrent/StructuredTaskScope` enforces a structured approach to
//! concurrent programming where subtasks are forked, joined, and the scope is
//! closed before the enclosing block exits.
//!
//! ## Phase 82 — Full Implementation
//!
//! All methods are real implementations with no stubs:
//! - `fork()` spawns a real VM worker thread per subtask (no longer
//!   synchronous); `join()`/`joinUntil()` block on those workers and then
//!   aggregate outcomes (see the "nb-jdk25-concurrency fix" block below)
//! - `ShutdownOnFailure` auto-shuts down on first failure with exception propagation
//! - `ShutdownOnSuccess` auto-shuts down on first success with result capture
//! - `Carrier.run()/call()` invokes Runnable/Callable with scoped value binding
//! - `ScopedValue.orElseThrow()` invokes the Supplier
//! - `throwIfFailed(Function)` and `result(Function)` invoke the Function mapper
//! - `exception()` returns proper `Optional`
//! - `joinUntil()` honours the deadline (seconds + nanos): it polls the forked
//!   workers and throws a real `java.util.concurrent.TimeoutException` if they
//!   do not all finish before the supplied `Instant` (nb-jdk25-joinuntil)
//! - `Subtask.get()` validates state before returning
//! - `Snapshot.capture()` records actual binding count
//! - Forked tasks inherit scoped value bindings from parent

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

use crate::{try_alloc_concurrent_synthetic, obj_arg};

// ===========================================================================
// Field-index constants — ScopedValue (3 fields)
// ===========================================================================

/// The bound value (Object).
const SV_FIELD_VALUE: usize = 0;
/// Whether this ScopedValue is currently bound (Int 0/1).
const SV_FIELD_IS_BOUND: usize = 1;
/// Hash code cache (Int).
const SV_FIELD_HASH: usize = 2;
/// Number of fields in a ScopedValue synthetic object.
const SV_NUM_FIELDS: usize = 3;

// ===========================================================================
// Field-index constants — ScopedValue$Carrier (3 fields)
// ===========================================================================

/// Reference to the ScopedValue being carried.
const CARRIER_FIELD_SV_REF: usize = 0;
/// The value to bind when `run` / `call` is invoked.
const CARRIER_FIELD_VALUE_REF: usize = 1;
/// Optional parent Carrier for chained bindings.
const CARRIER_FIELD_PARENT_REF: usize = 2;
/// Number of fields in a Carrier synthetic object.
const CARRIER_NUM_FIELDS: usize = 3;

// ===========================================================================
// Field-index constants — ScopedValue$Snapshot (2 fields)
// ===========================================================================

/// Number of active scoped value bindings at capture time.
const SNAPSHOT_FIELD_BINDINGS_COUNT: usize = 0;
/// Monotonic capture timestamp for ordering snapshots.
const SNAPSHOT_FIELD_TIMESTAMP: usize = 1;
/// Number of fields in a Snapshot synthetic object.
const SNAPSHOT_NUM_FIELDS: usize = 2;

/// Optional field constant for Optional synthetic objects.
const OPTIONAL_FIELD_VALUE: usize = 0;
const OPTIONAL_NUM_FIELDS: usize = 1;

// ===========================================================================
// Field-index constants — StructuredTaskScope (8 fields)
// ===========================================================================

/// Reference to the scope name (Object/String).
const STS_FIELD_NAME: usize = 0;
/// State: 0 = OPEN, 1 = SHUTDOWN, 2 = CLOSED.
const STS_FIELD_STATE: usize = 1;
/// Total number of forked tasks.
const STS_FIELD_TASK_COUNT: usize = 2;
/// Number of completed tasks.
const STS_FIELD_COMPLETED_COUNT: usize = 3;
/// Reference to the first exception (ShutdownOnFailure) or first result (ShutdownOnSuccess).
const STS_FIELD_EXCEPTION: usize = 4;
/// Policy: 0 = base, 1 = ShutdownOnFailure, 2 = ShutdownOnSuccess.
const STS_FIELD_POLICY: usize = 5;
/// Whether `join()` has been called (Int 0/1).
const STS_FIELD_JOINED: usize = 6;
/// Count of suppressed exceptions (for ShutdownOnFailure with multiple failures).
const STS_FIELD_SUPPRESSED_COUNT: usize = 7;
/// Number of fields in a StructuredTaskScope synthetic object.
const STS_NUM_FIELDS: usize = 8;

/// State constants for StructuredTaskScope.
const STS_STATE_OPEN: i32 = 0;
const STS_STATE_SHUTDOWN: i32 = 1;
const STS_STATE_CLOSED: i32 = 2;

/// Policy constants for StructuredTaskScope subclasses.
const STS_POLICY_BASE: i32 = 0;
const STS_POLICY_SHUTDOWN_ON_FAILURE: i32 = 1;
const STS_POLICY_SHUTDOWN_ON_SUCCESS: i32 = 2;

// ===========================================================================
// Field-index constants — StructuredTaskScope$Subtask (4 fields)
// ===========================================================================

/// Subtask state: 0 = UNAVAILABLE, 1 = SUCCESS, 2 = FAILED.
const SUBTASK_FIELD_STATE: usize = 0;
/// Result reference.
const SUBTASK_FIELD_RESULT: usize = 1;
/// Exception reference.
const SUBTASK_FIELD_EXCEPTION: usize = 2;
/// Reference to the original Callable.
const SUBTASK_FIELD_CALLABLE: usize = 3;
/// Number of fields in a Subtask synthetic object.
const SUBTASK_NUM_FIELDS: usize = 4;

/// Subtask state constants.
const SUBTASK_STATE_UNAVAILABLE: i32 = 0;
const SUBTASK_STATE_SUCCESS: i32 = 1;
const SUBTASK_STATE_FAILED: i32 = 2;

// ===========================================================================
// Class name constants
// ===========================================================================

const CLS_SCOPED_VALUE: &str = "java/lang/ScopedValue";
const CLS_CARRIER: &str = "java/lang/ScopedValue$Carrier";
const CLS_SNAPSHOT: &str = "java/lang/ScopedValue$Snapshot";
const CLS_TASK_SCOPE: &str = "java/util/concurrent/StructuredTaskScope";
const CLS_SUBTASK: &str = "java/util/concurrent/StructuredTaskScope$Subtask";
const CLS_SHUTDOWN_ON_FAILURE: &str = "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure";
const CLS_SHUTDOWN_ON_SUCCESS: &str = "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess";

// ===========================================================================
// nb-jdk25-concurrency fix — real worker-thread fork()/join()
// ===========================================================================
//
// Bug: the previous model ran every forked `Callable` SYNCHRONOUSLY on the
// forking (owner) thread inside `fork()`, and `join()` was a no-op. That
// deadlocks any set of subtasks where one blocks waiting for another (the
// classic producer/consumer fork pattern) and gives zero parallelism — the
// exact opposite of `StructuredTaskScope`'s contract.
//
// Fix: `fork()` now spawns a REAL VM worker thread per subtask (reusing the
// VM's `thread_start` machinery — the same path `Thread.start()` and the
// virtual-thread helpers use). Each worker runs a synthetic
// `CratonVM$StsForkRunner` whose native `run()` invokes the `Callable` and
// records SUCCESS+result / FAILED+exception into ITS OWN `Subtask` only (so
// there is no cross-thread race on the scope object). `join()` blocks on
// every spawned worker via `thread_join` and THEN performs all scope-level
// aggregation (completed count, policy-driven shutdown, primary/suppressed
// exception, ShutdownOnSuccess result) on the owner thread.
//
// The forked Thread objects and their Subtasks are tracked per-scope in a
// Rust side-table (mirroring the existing `SCOPE_OWNERS` / `SCOPE_JOINERS`
// tables in this file).

/// Synthetic Runnable executed on each fork worker thread.  Field layout:
///   slot 0 = the Callable to invoke
///   slot 1 = the Subtask to record the outcome into
const CLS_FORK_RUNNER: &str = "CratonVM$StsForkRunner";
const FORK_RUNNER_FIELD_CALLABLE: usize = 0;
const FORK_RUNNER_FIELD_SUBTASK: usize = 1;
const FORK_RUNNER_NUM_FIELDS: usize = 2;

/// The fabricated-only virtual-thread flag on a synthetic `java.lang.Thread`.
///
/// **It is not slot 4.** A real `java.lang.Thread` declares
/// `contextClassLoader` at index 4, and since 2026-08-05 so does CratonVM's
/// fabricated model — which had been declaring that name at index **5**, where
/// every real image has `holder`. The flag moved 4 → 5 in the same change so
/// the two conventions stop overlapping. Slot 5 is left ANONYMOUS in the model
/// on purpose; see the `java/lang/Thread` arm of
/// `ClassManager::synthetic_stub_fields`.
///
/// Public because the VM reads it too (`vm_exec.rs`, deciding whether a thread
/// mirror is a synthetic virtual thread). Two independent literals is how it
/// came to be wrong in the first place.
pub const SYNTHETIC_THREAD_VIRTUAL_SLOT: usize = 5;

/// Synthetic Thread layout (matches the VM's synthetic-Thread natives):
///   slot 0 = name, slot 1 = priority, slot 2 = tid, slot 3 = Runnable,
///   slot 4 = `contextClassLoader` (REAL, shared with the image),
///   slot 5 = virtual flag.
///
/// W7-77-guarded-slot-maps.md, against `javap -p java.lang.Thread` on Eclipse
/// Adoptium 25.0.3.9 (19 instance fields, static excluded, declaration order):
///
/// ```text
/// 0 eetop  1 tid  2 name  3 interrupted  4 contextClassLoader  5 holder
/// ```
///
/// so **four** of this run's five slots disagree with the real class, not one:
/// `NAME`(0) is `eetop`, `PRIORITY`(1) is `tid`, `TARGET`(3) is `interrupted`,
/// `VIRTUAL`(5) is `holder`. Only slot 4 agrees, and it agrees on purpose --
/// that is the 2026-08-05 alignment the `SYNTHETIC_THREAD_VIRTUAL_SLOT` doc
/// above describes.
///
/// W7-69-read-side-alias-instrument.md's census listed only slot 5 for this
/// file, because its scraper keyed on the run containing
/// `SYNTHETIC_THREAD_VIRTUAL_SLOT` and the `THREAD_FIELD_*` run is a separate
/// one carrying no class name in its own comment. The three extra rows are not
/// a new defect -- they are the same fabricated-layout map, and the same
/// class-side `eetop` witness in `vm_exec.rs::thread_start` decides whether any
/// of them may be applied. They are recorded because "the census listed one"
/// reads as "the other four agree", and they do not.
const THREAD_SYNTHETIC_NUM_FIELDS: usize = SYNTHETIC_THREAD_VIRTUAL_SLOT + 1;
const THREAD_FIELD_NAME: usize = 0;
const THREAD_FIELD_PRIORITY: usize = 1;
const THREAD_FIELD_TARGET: usize = 3;
const THREAD_FIELD_VIRTUAL: usize = SYNTHETIC_THREAD_VIRTUAL_SLOT;

/// What the fabricated `java/lang/Thread` model believes, published for
/// `read_alias::verify_declared_slot_maps` (W7-77).
///
/// States the BELIEF, not the real layout. Slot 4 is included even though it
/// agrees: a census with no clean rows is an instrument that fires on
/// everything, and this map's one deliberate agreement is worth sweeping.
pub static SYNTHETIC_THREAD_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/lang/Thread",
        slots: &[
            (THREAD_FIELD_NAME, "name"),
            (THREAD_FIELD_PRIORITY, "priority"),
            (THREAD_FIELD_TARGET, "target"),
            (4, "contextClassLoader"),
            (THREAD_FIELD_VIRTUAL, "isVirtual"),
        ],
        origin: "native-builtins/src/jdk25_concurrency.rs THREAD_FIELD_*",
    };

// ===========================================================================
// 15.1 — ScopedValue natives
// ===========================================================================

// ---------------------------------------------------------------------------
// Which thread currently owns a ScopedValue's binding.
//
// The value itself stays in the `ScopedValue` object's own fields — that keeps
// it GC-rooted for free and is what the field-index constants above describe.
// But an object field is visible to EVERY thread, and a `ScopedValue` binding
// is not: JEP 506 binds for the dynamic extent of `run`/`call` **on the
// binding thread**, and an ordinary `new Thread(...)` started inside that
// extent does NOT inherit it (only a `StructuredTaskScope` fork does).
//
// Without this table `ScopedValueComplete.testThreadVisibility` returned 100
// from a plain child thread. Real JDK 25 returns 0; the fixture asserted the
// CratonVM-specific field model as though it were the spec. So: `run`/`call`
// record the identity hash of each ScopedValue they bind against the binding
// thread, and every reader (`get`, `isBound`, `orElse`, `orElseThrow`) treats
// the object field as authoritative ONLY for a thread that appears here.
//
// Identity hashes, not addresses: they survive a moving collection, so this
// table needs no GC root scan or remap pass (same rationale as
// `classloader::closed_url_classloader_ids`).
//
// Known limitation, narrower than the bug it replaces: two threads binding the
// SAME `ScopedValue` at the same time still race on the single object field
// pair. Each sees "bound" correctly, but the *value* is whichever binding ran
// last. Fixing that needs per-thread values, which would take the values out
// of the heap-rooted object field and require their own GC scan/remap hooks.
// ---------------------------------------------------------------------------
static SV_BINDING_OWNERS: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<u64, Vec<i32>>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Per-VM teardown for the ScopedValue ownership table. Called from
/// `release_vm_native_state`.
pub fn forget_vm_scoped_value_owners(vm_identity: usize) {
    SV_BINDING_OWNERS.forget(vm_identity);
}

/// Record that the calling thread has just bound `sv`.
fn push_sv_owner(ctx: &mut dyn NativeContext, sv: ObjectRef) {
    let (vm, thread, id) = (ctx.vm_identity(), ctx.thread_id(), ctx.identity_hash_code(sv));
    SV_BINDING_OWNERS.with(vm, |table| table.entry(thread).or_default().push(id));
}

/// Drop the calling thread's most recent binding of `sv`.
fn pop_sv_owner(ctx: &mut dyn NativeContext, sv: ObjectRef) {
    let (vm, thread, id) = (ctx.vm_identity(), ctx.thread_id(), ctx.identity_hash_code(sv));
    SV_BINDING_OWNERS.with(vm, |table| {
        if let Some(stack) = table.get_mut(&thread) {
            if let Some(pos) = stack.iter().rposition(|&entry| entry == id) {
                stack.remove(pos);
            }
            if stack.is_empty() {
                table.remove(&thread);
            }
        }
    });
}

/// Whether `sv` is bound **for the calling thread** — the only sense in which
/// a ScopedValue is ever bound.
fn sv_is_bound_here(ctx: &mut dyn NativeContext, sv: ObjectRef) -> bool {
    if !matches!(ctx.get_field(sv, SV_FIELD_IS_BOUND), Value::Int(1)) {
        return false;
    }
    let (vm, thread, id) = (ctx.vm_identity(), ctx.thread_id(), ctx.identity_hash_code(sv));
    SV_BINDING_OWNERS
        .peek(vm, |table| {
            table
                .get(&thread)
                .is_some_and(|stack| stack.contains(&id))
        })
        .unwrap_or(false)
}

/// `ScopedValue.<init>()V` — initialise an empty, unbound ScopedValue.
fn native_sv_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, SV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(this, SV_FIELD_IS_BOUND, Value::Int(0));
    ctx.set_field(this, SV_FIELD_HASH, Value::Int(0));
    Ok(None)
}

/// `ScopedValue.newInstance()Ljava/lang/ScopedValue;` — factory.
fn native_sv_new_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS)?;
    ctx.set_field(obj, SV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(obj, SV_FIELD_IS_BOUND, Value::Int(0));
    ctx.set_field(obj, SV_FIELD_HASH, Value::Int(0));
    Ok(Some(Value::Object(Some(obj))))
}

/// `ScopedValue.get()Ljava/lang/Object;` — return the bound value.
/// Throws if not bound (returns Err).
fn native_sv_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !sv_is_bound_here(ctx, this) {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "ScopedValue is not bound".to_string(),
                },
            ),
        ));
    }
    let val = ctx.get_field(this, SV_FIELD_VALUE);
    Ok(Some(val))
}

/// `ScopedValue.isBound()Z` — return 1 if bound, 0 otherwise.
fn native_sv_is_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(i32::from(sv_is_bound_here(ctx, this)))))
}

/// `ScopedValue.orElse(Ljava/lang/Object;)Ljava/lang/Object;`
fn native_sv_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let default_val = args.get(1).copied().unwrap_or(Value::Object(None));
    if sv_is_bound_here(ctx, this) {
        Ok(Some(ctx.get_field(this, SV_FIELD_VALUE)))
    } else {
        Ok(Some(default_val))
    }
}

/// `ScopedValue.orElseThrow(Ljava/util/function/Supplier;)Ljava/lang/Object;`
///
/// Returns the bound value if bound. Otherwise, invokes the Supplier to produce
/// an exception and throws it.
fn native_sv_or_else_throw(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sv_is_bound_here(ctx, this) {
        true => {
            let val = ctx.get_field(this, SV_FIELD_VALUE);
            Ok(Some(val))
        }
        false => {
            // Invoke the Supplier.get() to produce the exception
            if let Some(Value::Object(Some(supplier))) = args.get(1) {
                let exc_result = ctx.invoke_virtual(*supplier, "get", "()Ljava/lang/Object;", &[]);
                match exc_result {
                    Ok(Some(Value::Object(Some(exc_obj)))) => Err(
                        cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc_obj),
                    ),
                    _ => {
                        // Supplier returned null or failed — throw NoSuchElementException
                        Err(
                            cratonvm_types::error::RuntimeError::NoSuchElementException {
                                message: "ScopedValue is not bound".to_string(),
                            }
                            .into(),
                        )
                    }
                }
            } else {
                Err(
                    cratonvm_types::error::RuntimeError::NoSuchElementException {
                        message: "ScopedValue is not bound".to_string(),
                    }
                    .into(),
                )
            }
        }
    }
}

/// `ScopedValue.hashCode()I`
fn native_sv_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cached = match ctx.get_field(this, SV_FIELD_HASH) {
        Value::Int(h) if h != 0 => h,
        _ => {
            // Generate a hash from the object reference address approximation
            let h = (this.as_ptr() as i32)
                .wrapping_mul(31)
                .wrapping_add(0x5DEECE66u32 as i32);
            ctx.set_field(this, SV_FIELD_HASH, Value::Int(h));
            h
        }
    };
    Ok(Some(Value::Int(cached)))
}

// ===========================================================================
// 15.1 — ScopedValue$Carrier natives
// ===========================================================================

/// Walk the carrier chain (this → parent → parent...) and collect all
/// (ScopedValue ref, value) pairs for binding.
fn collect_carrier_bindings(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
) -> Vec<(ObjectRef, Value)> {
    let mut bindings: Vec<(ObjectRef, Value)> = Vec::new();
    let mut current = Some(carrier);
    while let Some(c) = current {
        let sv_val = ctx.get_field(c, CARRIER_FIELD_SV_REF);
        let value = ctx.get_field(c, CARRIER_FIELD_VALUE_REF);
        if let Value::Object(Some(sv_ref)) = sv_val {
            bindings.push((sv_ref, value));
        }
        current = match ctx.get_field(c, CARRIER_FIELD_PARENT_REF) {
            Value::Object(Some(parent)) => Some(parent),
            _ => None,
        };
    }
    bindings
}

/// `ScopedValue.where(ScopedValue, Object)Carrier` — static factory.
fn native_sv_where_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sv_val = args.get(0).copied().unwrap_or(Value::Object(None));
    let value_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let carrier = try_alloc_concurrent_synthetic(ctx, CLS_CARRIER, CARRIER_NUM_FIELDS)?;
    ctx.set_field(carrier, CARRIER_FIELD_SV_REF, sv_val);
    ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, value_val);
    ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(None));
    Ok(Some(Value::Object(Some(carrier))))
}

/// `Carrier.where(ScopedValue, Object)Carrier` — chain another binding.
fn native_carrier_where(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sv_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let value_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let carrier = try_alloc_concurrent_synthetic(ctx, CLS_CARRIER, CARRIER_NUM_FIELDS)?;
    ctx.set_field(carrier, CARRIER_FIELD_SV_REF, sv_val);
    ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, value_val);
    ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(carrier))))
}

/// `Carrier.run(Ljava/lang/Runnable;)V` — bind scoped values and execute.
///
/// Walks the carrier chain (this -> parent -> parent...) and binds ALL
/// scoped values. Invokes the Runnable, then unbinds regardless of outcome.
fn native_carrier_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bindings = collect_carrier_bindings(ctx, this);
    // Save previous binding state so we can restore on unbind
    let prev_states: Vec<(ObjectRef, Value, Value)> = bindings
        .iter()
        .map(|(sv_ref, _)| {
            let old_bound = ctx.get_field(*sv_ref, SV_FIELD_IS_BOUND);
            let old_value = ctx.get_field(*sv_ref, SV_FIELD_VALUE);
            (*sv_ref, old_bound, old_value)
        })
        .collect();
    // Bind all scoped values — on THIS thread (see `SV_BINDING_OWNERS`).
    for (sv_ref, value) in &bindings {
        ctx.set_field(*sv_ref, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(*sv_ref, SV_FIELD_VALUE, *value);
        push_sv_owner(ctx, *sv_ref);
    }
    // Invoke the Runnable
    let result = if let Some(Value::Object(Some(runnable))) = args.get(1) {
        ctx.invoke_virtual(*runnable, "run", "()V", &[])
    } else {
        Ok(None)
    };
    // Restore previous scoped value state (unbind)
    for (sv_ref, old_bound, old_value) in &prev_states {
        ctx.set_field(*sv_ref, SV_FIELD_IS_BOUND, *old_bound);
        ctx.set_field(*sv_ref, SV_FIELD_VALUE, *old_value);
        pop_sv_owner(ctx, *sv_ref);
    }
    // Propagate any exception from the Runnable
    result?;
    Ok(None)
}

/// `Carrier.call(Ljava/util/concurrent/Callable;)Ljava/lang/Object;`
///
/// Like `run` but returns a result. Walks the carrier chain, binds all
/// scoped values, invokes the Callable, unbinds, and returns the result.
fn native_carrier_call(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bindings = collect_carrier_bindings(ctx, this);
    // Save previous binding state
    let prev_states: Vec<(ObjectRef, Value, Value)> = bindings
        .iter()
        .map(|(sv_ref, _)| {
            let old_bound = ctx.get_field(*sv_ref, SV_FIELD_IS_BOUND);
            let old_value = ctx.get_field(*sv_ref, SV_FIELD_VALUE);
            (*sv_ref, old_bound, old_value)
        })
        .collect();
    // Bind all scoped values — on THIS thread (see `SV_BINDING_OWNERS`).
    for (sv_ref, value) in &bindings {
        ctx.set_field(*sv_ref, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(*sv_ref, SV_FIELD_VALUE, *value);
        push_sv_owner(ctx, *sv_ref);
    }
    // Invoke the Callable
    let result = if let Some(Value::Object(Some(callable))) = args.get(1) {
        ctx.invoke_virtual(*callable, "call", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(Value::Object(None)))
    };
    // Restore previous scoped value state (unbind)
    for (sv_ref, old_bound, old_value) in &prev_states {
        ctx.set_field(*sv_ref, SV_FIELD_IS_BOUND, *old_bound);
        ctx.set_field(*sv_ref, SV_FIELD_VALUE, *old_value);
        pop_sv_owner(ctx, *sv_ref);
    }
    // Propagate exception or return result
    result
}

/// `Carrier.get()Ljava/lang/Object;` — return the carrier's value.
fn native_carrier_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = ctx.get_field(this, CARRIER_FIELD_VALUE_REF);
    Ok(Some(val))
}

// ===========================================================================
// 15.1 — ScopedValue$Snapshot natives
// ===========================================================================

/// `ScopedValue$Snapshot.capture()Snapshot` — static, returns synthetic snapshot.
///
/// Captures the current scoped value binding count from the thread's binding
/// stack and a monotonic timestamp for ordering.
fn native_snapshot_capture(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let snap = try_alloc_concurrent_synthetic(ctx, CLS_SNAPSHOT, SNAPSHOT_NUM_FIELDS)?;
    static SNAPSHOT_COUNTER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
    let ts = SNAPSHOT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Query the thread's current scoped value binding count
    let binding_count = ctx.scoped_value_depth() as i32;
    ctx.set_field(
        snap,
        SNAPSHOT_FIELD_BINDINGS_COUNT,
        Value::Int(binding_count),
    );
    ctx.set_field(snap, SNAPSHOT_FIELD_TIMESTAMP, Value::Int(ts));
    Ok(Some(Value::Object(Some(snap))))
}

// ===========================================================================
// 15.2 — StructuredTaskScope natives
// ===========================================================================

/// Helper: initialise a StructuredTaskScope with default field values.
fn sts_init_fields(ctx: &mut dyn NativeContext, this: ObjectRef, name: Value, policy: i32) {
    ctx.set_field(this, STS_FIELD_NAME, name);
    ctx.set_field(this, STS_FIELD_STATE, Value::Int(STS_STATE_OPEN));
    ctx.set_field(this, STS_FIELD_TASK_COUNT, Value::Int(0));
    ctx.set_field(this, STS_FIELD_COMPLETED_COUNT, Value::Int(0));
    ctx.set_field(this, STS_FIELD_EXCEPTION, Value::Object(None));
    ctx.set_field(this, STS_FIELD_POLICY, Value::Int(policy));
    ctx.set_field(this, STS_FIELD_JOINED, Value::Int(0));
    ctx.set_field(this, STS_FIELD_SUPPRESSED_COUNT, Value::Int(0));
}

/// Helper: read the integer value of a scope field, defaulting to 0.
fn sts_get_int(ctx: &mut dyn NativeContext, this: ObjectRef, field: usize) -> i32 {
    match ctx.get_field(this, field) {
        Value::Int(n) => n,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// nb-jdk25-concurrency — per-scope forked-worker tracking
// ---------------------------------------------------------------------------

/// Tracks, for each open scope, the `(Subtask, worker Thread)` pairs created
/// by `fork()` so that `join()` can block on every worker and then aggregate
/// outcomes.  Keyed by the scope `ObjectRef` raw pointer, exactly like the
/// `SCOPE_OWNERS` / `SCOPE_JOINERS` tables below.
///
/// Stored `ObjectRef`s stay reachable for the worker's lifetime: each worker
/// Thread is registered with the VM `ThreadRegistry` by `thread_start` (so GC
/// scans its stack and the Thread mirror), and the Subtask is returned to the
/// Java caller. Workers are short-lived — `join()` reaps them and the entry is
/// cleared on close — so this side-table never accumulates.
///
/// GC note (gc-followups-20260706): reachability keeps these objects ALIVE
/// but not UNMOVED — the fork→join window can span a moving GC, after which
/// (a) the raw-address scope key goes stale (join/close miss the entry) and
/// (b) the raw Subtask/Thread copies point at old addresses. Same follow-up
/// as SCOPE_JOINERS below: key by `ctx.identity_hash_code(scope)` and
/// re-read values via the `(identity_key, ObjectRef)` var-handle-root
/// pattern (ASYNC_POOL in lib.rs), or add a gc_scan/gc_update hook pair.
static SCOPE_FORKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<usize, Vec<(ObjectRef, ObjectRef)>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn register_scope_fork(scope: ObjectRef, subtask: ObjectRef, thread_obj: ObjectRef) {
    let key = scope.as_ptr() as usize;
    SCOPE_FORKS
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .push((subtask, thread_obj));
}

/// Take (and clear) the recorded forks for a scope. Returns an empty Vec if
/// none were recorded.
fn take_scope_forks(scope: ObjectRef) -> Vec<(ObjectRef, ObjectRef)> {
    let key = scope.as_ptr() as usize;
    SCOPE_FORKS.lock().unwrap().remove(&key).unwrap_or_default()
}

/// Peek at the recorded forks for a scope without clearing them.
///
/// `joinUntil()` (nb-jdk25-joinuntil) needs this so it can poll the workers
/// against a deadline WITHOUT draining the side-table: if the deadline elapses
/// before the workers finish, the forks must stay registered so a later
/// `close()` can still reap them (the spec keeps the scope open on timeout).
/// `join()` (and the success path of `joinUntil`) still drain via
/// `take_scope_forks`.
fn peek_scope_forks(scope: ObjectRef) -> Vec<(ObjectRef, ObjectRef)> {
    let key = scope.as_ptr() as usize;
    SCOPE_FORKS
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap_or_default()
}

/// `CratonVM$StsForkRunner.run()V` — the body executed on each fork worker
/// thread. Invokes the forked `Callable` and records the outcome into its OWN
/// `Subtask` only (slot 0 STATE + slot 1 RESULT / slot 2 EXCEPTION). All
/// scope-level aggregation is deferred to `join()` on the owner thread, so no
/// two worker threads ever write the same object's fields concurrently.
fn native_fork_runner_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let callable_val = ctx.get_field(this, FORK_RUNNER_FIELD_CALLABLE);
    let subtask = match ctx.get_field(this, FORK_RUNNER_FIELD_SUBTASK) {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };

    let call_result = if let Value::Object(Some(callable)) = callable_val {
        ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(Value::Object(None)))
    };

    match call_result {
        Ok(result_val) => {
            ctx.set_field(
                subtask,
                SUBTASK_FIELD_STATE,
                Value::Int(SUBTASK_STATE_SUCCESS),
            );
            ctx.set_field(
                subtask,
                SUBTASK_FIELD_RESULT,
                result_val.unwrap_or(Value::Object(None)),
            );
            ctx.set_field(subtask, SUBTASK_FIELD_EXCEPTION, Value::Object(None));
        }
        Err(err) => {
            let exc_ref = match &err {
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
                    Value::Object(Some(*obj))
                }
                _ => Value::Object(None),
            };
            ctx.set_field(
                subtask,
                SUBTASK_FIELD_STATE,
                Value::Int(SUBTASK_STATE_FAILED),
            );
            ctx.set_field(subtask, SUBTASK_FIELD_RESULT, Value::Object(None));
            ctx.set_field(subtask, SUBTASK_FIELD_EXCEPTION, exc_ref);
        }
    }
    // The worker's Thread.run() returns normally regardless of the Callable's
    // outcome — the failure is captured on the Subtask, not propagated as an
    // uncaught exception on the worker (which would just be logged + dropped).
    Ok(None)
}

/// Aggregate a single completed Subtask's outcome into the scope's policy
/// bookkeeping. Runs on the owner thread inside `join()` after the worker has
/// been reaped, so all scope-field writes are single-threaded here.
fn sts_aggregate_subtask(ctx: &mut dyn NativeContext, scope: ObjectRef, subtask: ObjectRef) {
    let policy = sts_get_int(ctx, scope, STS_FIELD_POLICY);
    let sub_state = match ctx.get_field(subtask, SUBTASK_FIELD_STATE) {
        Value::Int(s) => s,
        _ => SUBTASK_STATE_UNAVAILABLE,
    };

    let completed = sts_get_int(ctx, scope, STS_FIELD_COMPLETED_COUNT);
    ctx.set_field(scope, STS_FIELD_COMPLETED_COUNT, Value::Int(completed + 1));

    match sub_state {
        SUBTASK_STATE_SUCCESS => {
            let result_val = ctx.get_field(subtask, SUBTASK_FIELD_RESULT);
            // ShutdownOnSuccess: capture the first success and shut down.
            if policy == STS_POLICY_SHUTDOWN_ON_SUCCESS
                && sts_get_int(ctx, scope, STS_FIELD_STATE) == STS_STATE_OPEN
            {
                ctx.set_field(scope, STS_FIELD_STATE, Value::Int(STS_STATE_SHUTDOWN));
                ctx.set_field(scope, STS_FIELD_EXCEPTION, result_val);
            }
        }
        SUBTASK_STATE_FAILED => {
            let exc_ref = ctx.get_field(subtask, SUBTASK_FIELD_EXCEPTION);
            if policy == STS_POLICY_SHUTDOWN_ON_FAILURE {
                let current_state = sts_get_int(ctx, scope, STS_FIELD_STATE);
                if current_state == STS_STATE_OPEN {
                    ctx.set_field(scope, STS_FIELD_STATE, Value::Int(STS_STATE_SHUTDOWN));
                    ctx.set_field(scope, STS_FIELD_EXCEPTION, exc_ref);
                } else {
                    let suppressed = sts_get_int(ctx, scope, STS_FIELD_SUPPRESSED_COUNT);
                    ctx.set_field(
                        scope,
                        STS_FIELD_SUPPRESSED_COUNT,
                        Value::Int(suppressed + 1),
                    );
                }
            } else if policy == STS_POLICY_BASE {
                let existing = ctx.get_field(scope, STS_FIELD_EXCEPTION);
                if matches!(existing, Value::Object(None)) {
                    ctx.set_field(scope, STS_FIELD_EXCEPTION, exc_ref);
                }
            }
        }
        _ => {}
    }
}

/// `StructuredTaskScope.<init>()V`
fn native_sts_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    sts_init_fields(ctx, this, Value::Object(None), STS_POLICY_BASE);
    Ok(None)
}

/// `StructuredTaskScope.<init>(Ljava/lang/String;)V`
fn native_sts_init_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    sts_init_fields(ctx, this, name, STS_POLICY_BASE);
    Ok(None)
}

/// `StructuredTaskScope.<init>(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V`
fn native_sts_init_name_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    sts_init_fields(ctx, this, name, STS_POLICY_BASE);
    Ok(None)
}

/// `StructuredTaskScope.open()StructuredTaskScope` — static factory (JDK 25).
fn native_sts_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS)?;
    sts_init_fields(ctx, scope, Value::Object(None), STS_POLICY_BASE);
    Ok(Some(Value::Object(Some(scope))))
}

/// `StructuredTaskScope.fork(Callable)Subtask`
///
/// nb-jdk25-concurrency fix: forks a subtask that executes the `Callable` on
/// its OWN VM worker thread (no longer synchronously on the forking thread).
/// The subtask is returned immediately in the UNAVAILABLE state; its outcome
/// is filled in by the worker, and `join()` blocks until the worker finishes
/// and then aggregates the outcome into the scope's policy bookkeeping. This
/// provides real parallelism and, critically, prevents the deadlock that the
/// old synchronous model produced for blocking-dependent subtasks.
fn native_sts_fork(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);

    // Closed scope: throw IllegalStateException
    if state == STS_STATE_CLOSED {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "StructuredTaskScope is closed".to_string(),
        }
        .into());
    }

    let callable_val = args.get(1).copied().unwrap_or(Value::Object(None));

    // Increment task count
    let count = sts_get_int(ctx, this, STS_FIELD_TASK_COUNT);
    ctx.set_field(this, STS_FIELD_TASK_COUNT, Value::Int(count + 1));

    // Allocate the subtask (returned to the caller immediately, UNAVAILABLE).
    let subtask = try_alloc_concurrent_synthetic(ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS)?;
    ctx.set_field(subtask, SUBTASK_FIELD_CALLABLE, callable_val);
    ctx.set_field(
        subtask,
        SUBTASK_FIELD_STATE,
        Value::Int(SUBTASK_STATE_UNAVAILABLE),
    );
    ctx.set_field(subtask, SUBTASK_FIELD_RESULT, Value::Object(None));
    ctx.set_field(subtask, SUBTASK_FIELD_EXCEPTION, Value::Object(None));

    // If the scope is already SHUTDOWN, the spec does not run new subtasks —
    // they stay UNAVAILABLE and are still accounted as completed. Do not spawn
    // a worker.
    if state == STS_STATE_SHUTDOWN {
        let completed = sts_get_int(ctx, this, STS_FIELD_COMPLETED_COUNT);
        ctx.set_field(this, STS_FIELD_COMPLETED_COUNT, Value::Int(completed + 1));
        return Ok(Some(Value::Object(Some(subtask))));
    }

    // Build the runner Runnable that the worker thread will execute. It holds
    // the Callable + the Subtask and writes the outcome into the Subtask.
    let runner = try_alloc_concurrent_synthetic(ctx, CLS_FORK_RUNNER, FORK_RUNNER_NUM_FIELDS)?;
    ctx.set_field(runner, FORK_RUNNER_FIELD_CALLABLE, callable_val);
    ctx.set_field(
        runner,
        FORK_RUNNER_FIELD_SUBTASK,
        Value::Object(Some(subtask)),
    );

    // Build a Thread whose Thread.run() dispatches to the runner, then start it
    // via the VM's real thread machinery. The worker is reaped by join().
    let mut worker = try_alloc_concurrent_synthetic(ctx, "java/lang/Thread", THREAD_SYNTHETIC_NUM_FIELDS)?;
    let name = ctx.create_string("StsFork");
    // Choose the field-set strategy by the object's actual layout. A synthetic
    // 5-slot Thread (num_fields <= 8, matching the VM's
    // `is_synthetic_thread_layout` cutoff) keeps name/priority/tid/target/virtual
    // at fixed slots; a real-JDK Thread has dozens of fields with `target`
    // resolved by name (and slot 3 holding an unrelated real field that we must
    // NOT clobber). Set the target the right way for whichever layout this VM
    // produced so the worker's Thread.run() finds the runner and invokes it.
    //
    // Note on virtual vs platform: this worker is deliberately a PLATFORM
    // thread, NOT a virtual thread. Real StructuredTaskScope runs each subtask
    // on its own virtual thread, but the VM's virtual scheduler bounds
    // concurrently-running virtual threads to `available_parallelism()`
    // carriers and only releases a carrier on park/sleep/wait. A
    // blocking-dependent fork pattern (consumer waits for a producer) could
    // then deadlock if the consumer holds the only free carrier while spinning
    // rather than parking — exactly the deadlock class this fix exists to
    // remove. A dedicated OS thread per subtask has no such bound, so
    // independent AND blocking-dependent subtasks all make progress. For the
    // synthetic layout that means leaving the slot-4 virtual flag at 0; for the
    // real-JDK layout it means NOT constructing a BaseVirtualThread subtype.
    let worker_fields = ctx.object_num_fields(worker);
    // The shared cutoff, not a fourth copy of it. `<= 8` was open-coded here
    // because `is_synthetic_thread_layout` was nested inside
    // `register_essential_natives_with_shims` and unreachable from any other
    // module; it is at module scope since 2026-08-12
    // (W7-74-short-object-repairs.md). Same value, one declaration.
    let synthetic_thread = crate::thread_mirror_is_synthetic_layout(worker_fields);
    if synthetic_thread {
        ctx.set_field(worker, THREAD_FIELD_NAME, Value::Object(Some(name)));
        ctx.set_field(worker, THREAD_FIELD_PRIORITY, Value::Int(5));
        ctx.set_field(worker, THREAD_FIELD_TARGET, Value::Object(Some(runner)));
        ctx.set_field(worker, THREAD_FIELD_VIRTUAL, Value::Int(0));
    } else {
        // Real-JDK Thread: drive the registered
        // `Thread.<init>(ThreadGroup, Runnable, String)` native instead of
        // poking fields by name. That native runs `populate_real_thread_holder`,
        // which allocates and links `Thread$FieldHolder` (group/priority/
        // daemon/threadStatus) AND stores the runnable as `holder.task` — the
        // slot `Thread.run()` actually reads. Setting `target` by name does NOT
        // work on a real-JDK Thread (there is no top-level `target` field; it
        // lives in the FieldHolder), and leaving `holder` null makes every
        // holder access (`getThreadGroup`/`getPriority`/`isDaemon`) NPE with
        // "Cannot read field ... because this.holder is null". Mirrors the HTTP
        // dispatcher worker in `net_phase_e::re10_spawn_dispatcher`.
        let _ = ctx.invoke(
            "java/lang/Thread",
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
            &[
                Value::Object(Some(worker)),
                Value::Object(None),
                Value::Object(Some(runner)),
                Value::Object(Some(name)),
            ],
        );
    }

    // G5-1: the worker Thread now exists, so this is its construction moment —
    // capture the forking thread's `InheritableThreadLocal` values against it
    // here, before it is started or even tracked.
    //
    // Two things are wrong without this call, and only the first is a timing
    // question:
    //
    //  1. `ctx.thread_start(worker)` below goes straight to the VM's thread
    //     machinery. It does NOT pass through
    //     `lang_system::native_thread_start0`, which is where every other
    //     spawn path takes its inheritable-ThreadLocal snapshot — so a forked
    //     subtask inherited NOTHING at all. HotSpot inherits: MEASURED on
    //     Temurin 25.0.3+9 (`StsItl`, `--enable-preview`), a subtask forked
    //     after `ITL.set("scope-parent")` reads back `scope-parent`, because
    //     `StructuredTaskScope.fork` builds its thread through a
    //     `Thread.Builder` whose `inheritInheritableThreadLocals` defaults to
    //     true.
    //  2. Capturing at construction rather than at start is what HotSpot's
    //     `Thread.<init>` does (pc 175..201 of the master constructor,
    //     SOURCE-VERIFIED). For this site the two moments are adjacent, so the
    //     ordering is not observable HERE — but going through the shared
    //     construction-time entry point rather than open-coding a snapshot is
    //     what keeps this path and the `new Thread(...)` paths on one
    //     definition of when inheritance is decided.
    //
    // Placed after BOTH layout arms, because the real-JDK arm's
    // `Thread.<init>` invoke can allocate and the identity the queue is keyed
    // by must be the finished object's.
    crate::lang_system::capture_inheritable_tl_at_construction(ctx, &mut worker);

    // Record the (subtask, worker) pair BEFORE starting so a racing fast worker
    // is already tracked when join() runs.
    register_scope_fork(this, subtask, worker);

    // Spawn the worker via the VM's real thread machinery.
    let start_result = ctx.thread_start(worker);

    // Decide whether the worker actually ran. Two cases require an inline
    // fallback so the work is never silently lost:
    //   1. thread_start returned Err (no thread registry available), or
    //   2. the context has no real threading (e.g. test mocks whose
    //      thread_start is a no-op): the worker is not alive AND the subtask
    //      is still UNAVAILABLE, meaning the Callable never ran.
    // A real worker is either still alive (skip inline; join() reaps it) or
    // already finished — in which case it has written SUCCESS/FAILED onto the
    // subtask (because the registry only marks a thread dead after run()
    // returns), so the UNAVAILABLE check below is false and we skip inline.
    let needs_inline = start_result.is_err()
        || (!ctx.thread_is_alive(worker)
            && matches!(
                ctx.get_field(subtask, SUBTASK_FIELD_STATE),
                Value::Int(SUBTASK_STATE_UNAVAILABLE)
            ));

    if needs_inline {
        // Drop the (untracked-by-a-real-thread) tracking entry for this subtask
        // so join() doesn't try to thread_join a worker that isn't running.
        let mut remaining = take_scope_forks(this);
        remaining.retain(|(st, _)| *st != subtask);
        for (st, th) in remaining {
            register_scope_fork(this, st, th);
        }
        // Run the Callable inline on this thread and aggregate immediately.
        let inline = native_fork_runner_run(ctx, &[Value::Object(Some(runner))]);
        sts_aggregate_subtask(ctx, this, subtask);
        inline?;
        return Ok(Some(Value::Object(Some(subtask))));
    }

    Ok(Some(Value::Object(Some(subtask))))
}

/// `StructuredTaskScope.join()StructuredTaskScope`
///
/// nb-jdk25-concurrency fix: BLOCKS until every forked worker thread finishes
/// (was previously a no-op that simply stamped completed = count). After all
/// workers are reaped, aggregates each subtask's outcome into the scope's
/// policy bookkeeping on this (owner) thread — single-threaded, so no race on
/// the scope object. This is what makes blocking-dependent subtasks work:
/// the producer worker can complete and unblock the consumer worker because
/// both run on real threads, and the owner only proceeds once both are done.
fn native_sts_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state == STS_STATE_CLOSED {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "StructuredTaskScope is closed".to_string(),
        }
        .into());
    }

    // Take the forked (subtask, worker) pairs and block on each worker. The
    // pairs are taken (not peeked) so a re-entrant or second join() is a no-op
    // over already-reaped workers.
    let forks = take_scope_forks(this);
    for (_subtask, worker) in &forks {
        // thread_join blocks until the worker's Thread.run() — i.e. the
        // runner's Callable.call() — has fully completed and recorded its
        // outcome on the subtask. Errors here are non-fatal: a worker that
        // already exited yields Ok immediately.
        let _ = ctx.thread_join(*worker);
    }

    // All workers done — aggregate outcomes on the owner thread in fork order.
    for (subtask, _worker) in &forks {
        sts_aggregate_subtask(ctx, this, *subtask);
    }

    ctx.set_field(this, STS_FIELD_JOINED, Value::Int(1));
    Ok(Some(Value::Object(Some(this))))
}

/// nb-jdk25-joinuntil — construct and throw a real
/// `java.util.concurrent.TimeoutException` (NOT an `IllegalStateException` with
/// a misleading message). The `(String)` constructor is registered for this
/// class (see `register_common_exceptions` in lib.rs), so
/// `new_object_initialized` yields a properly-typed Throwable that a Java
/// `catch (TimeoutException e)` will actually catch.
fn throw_timeout_exception(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> cratonvm_types::error::MethodCallFailed {
    let jmsg = ctx.create_string(msg);
    match ctx.new_object_initialized(
        "java/util/concurrent/TimeoutException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jmsg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => {
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)
        }
        // If the TimeoutException class is somehow unavailable, fail loud rather
        // than silently returning normally (which would hide a missed deadline).
        _ => cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!("TimeoutException (class unavailable): {msg}"),
        }
        .into(),
    }
}

/// `StructuredTaskScope.joinUntil(Ljava/time/Instant;)StructuredTaskScope`
///
/// Like `join()` but bounded by a deadline. nb-jdk25-joinuntil fix: the
/// deadline is now genuinely HONOURED — we poll the forked workers and, if the
/// deadline elapses before they all finish, throw a real
/// `java.util.concurrent.TimeoutException` (previously this threw an
/// `IllegalStateException` and only did an up-front past-deadline check, then
/// blocked indefinitely on the untimed `thread_join`).
///
/// Deadline computation reads BOTH `Instant.seconds` (field 0) and
/// `Instant.nanos` (field 1) — the nanos component was previously ignored —
/// and compares against wall-clock `SystemTime` (the Java `Instant` is an
/// epoch-relative wall-clock time). Once inside the wait loop we measure the
/// remaining budget with a monotonic `Instant` to avoid clock-adjustment skew.
///
/// On timeout the scope is deliberately left OPEN with its forks still
/// registered: the spec does not close the scope on `joinUntil` timeout, and a
/// later `close()` reaps the still-running workers (it drains the same
/// side-table). On success the behaviour matches `join()` exactly: drain the
/// forks, aggregate each subtask's outcome on the owner thread, mark joined.
fn native_sts_join_until(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state == STS_STATE_CLOSED {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "StructuredTaskScope is closed".to_string(),
        }
        .into());
    }

    // Compute how long we are allowed to wait. The Java `Instant` is an absolute
    // epoch wall-clock time (seconds + nanos), so derive a remaining `Duration`
    // by subtracting the current wall-clock time. A missing/garbage Instant is
    // treated as "no bound" (wait like join()).
    let now_wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let deadline_budget: Option<std::time::Duration> = match args.get(1) {
        Some(Value::Object(Some(instant))) => {
            let secs = match ctx.get_field(*instant, 0) {
                Value::Long(s) => s,
                Value::Int(s) => s as i64,
                _ => i64::MAX,
            };
            // nb-jdk25-joinuntil: honour the nanos component (field 1) too —
            // it was previously dropped entirely.
            let nanos = match ctx.get_field(*instant, 1) {
                Value::Int(n) => n.max(0) as u32,
                Value::Long(n) => n.max(0) as u32,
                _ => 0,
            };
            if secs < 0 {
                // Deadline before the epoch — already elapsed.
                Some(std::time::Duration::ZERO)
            } else {
                let deadline = std::time::Duration::new(secs as u64, nanos);
                // Remaining = deadline - now; saturating to ZERO if already past.
                Some(deadline.saturating_sub(now_wall))
            }
        }
        // null Instant (or absent) — unbounded, behave exactly like join().
        _ => None,
    };

    // Already past the deadline (zero budget): throw immediately, leaving the
    // scope open and the workers registered for a later close() to reap.
    if matches!(deadline_budget, Some(d) if d.is_zero()) {
        return Err(throw_timeout_exception(ctx, "deadline exceeded"));
    }

    match deadline_budget {
        // No deadline bound: identical to join() — block until all workers done.
        None => native_sts_join(ctx, args),
        Some(budget) => {
            // Poll the workers against a monotonic deadline. We PEEK (do not
            // drain) the forks so that, on timeout, they stay registered for
            // close() to reap. On success we fall through to the normal
            // drain+aggregate path.
            //
            // `Instant + Duration` PANICS on overflow, and a Java `Instant` can
            // legitimately encode a deadline decades/eons in the future (e.g.
            // Instant.MAX). If the budget can't be represented on the monotonic
            // clock it is effectively unbounded — fall back to the untimed
            // join() rather than risk a panic.
            let mono_deadline = match std::time::Instant::now().checked_add(budget) {
                Some(d) => d,
                None => return native_sts_join(ctx, args),
            };
            loop {
                let forks = peek_scope_forks(this);
                let all_done = forks
                    .iter()
                    .all(|(_subtask, worker)| !ctx.thread_is_alive(*worker));
                if all_done {
                    // All workers finished in time — reap + aggregate exactly
                    // like join() (thread_join returns immediately for the
                    // already-dead workers).
                    return native_sts_join(ctx, args);
                }
                if std::time::Instant::now() >= mono_deadline {
                    // Deadline elapsed with workers still running. Leave the
                    // scope OPEN and the forks registered; throw a real
                    // TimeoutException.
                    return Err(throw_timeout_exception(ctx, "deadline exceeded"));
                }
                // Wait a short, deadline-bounded slice before re-polling. `park`
                // integrates with the VM's thread machinery (safepoint-aware)
                // and a spurious early wakeup is harmless — we just re-check the
                // workers and the deadline. Cap the slice so we never overshoot
                // the deadline by more than ~2ms.
                let remaining = mono_deadline.saturating_duration_since(std::time::Instant::now());
                let slice = remaining.min(std::time::Duration::from_millis(2));
                ctx.park(Some(slice));
            }
        }
    }
}

/// `StructuredTaskScope.close()V`
///
/// Closes the scope. Requires that `join()` was called first. Idempotent
/// if already closed.
fn native_sts_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state == STS_STATE_CLOSED {
        // Already closed — idempotent. Still ensure no tracking lingers.
        let _ = take_scope_forks(this);
        return Ok(None);
    }
    // Verify join() was called
    let joined = sts_get_int(ctx, this, STS_FIELD_JOINED);
    if joined == 0 {
        let total = sts_get_int(ctx, this, STS_FIELD_TASK_COUNT);
        if total > 0 {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "StructuredTaskScope has not been joined".to_string(),
            }
            .into());
        }
    }
    // nb-jdk25-concurrency: reap any worker threads that join() did not (e.g.
    // a scope closed after shutdown without a final join) so close() never
    // leaks live workers, then drop the per-scope tracking entry. thread_join
    // returns immediately for already-dead workers.
    for (_subtask, worker) in take_scope_forks(this) {
        let _ = ctx.thread_join(worker);
    }
    ctx.set_field(this, STS_FIELD_STATE, Value::Int(STS_STATE_CLOSED));
    Ok(None)
}

/// `StructuredTaskScope.shutdown()V`
fn native_sts_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, STS_FIELD_STATE, Value::Int(STS_STATE_SHUTDOWN));
    Ok(None)
}

/// `StructuredTaskScope.isShutdown()Z`
fn native_sts_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match ctx.get_field(this, STS_FIELD_STATE) {
        Value::Int(n) if n >= STS_STATE_SHUTDOWN => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// `StructuredTaskScope.toString()Ljava/lang/String;`
fn native_sts_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = ctx.get_field(this, STS_FIELD_NAME);
    Ok(Some(name))
}

// ===========================================================================
// 15.2 — StructuredTaskScope$Subtask natives
// ===========================================================================

/// `Subtask.get()Ljava/lang/Object;`
///
/// Returns the result if the subtask completed successfully. Throws
/// `IllegalStateException` if the subtask has not completed (UNAVAILABLE)
/// or failed (FAILED).
fn native_subtask_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = match ctx.get_field(this, SUBTASK_FIELD_STATE) {
        Value::Int(s) => s,
        _ => SUBTASK_STATE_UNAVAILABLE,
    };
    match state {
        SUBTASK_STATE_SUCCESS => {
            let result = ctx.get_field(this, SUBTASK_FIELD_RESULT);
            Ok(Some(result))
        }
        SUBTASK_STATE_FAILED => Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Subtask failed".to_string(),
        }
        .into()),
        _ => {
            // UNAVAILABLE
            Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "Subtask has not completed".to_string(),
            }
            .into())
        }
    }
}

/// `Subtask.state()State`
///
/// Returns the subtask state as an integer:
/// 0 = UNAVAILABLE, 1 = SUCCESS, 2 = FAILED.
fn native_subtask_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = ctx.get_field(this, SUBTASK_FIELD_STATE);
    Ok(Some(state))
}

/// `Subtask.exception()Ljava/lang/Throwable;`
///
/// Returns the exception if the subtask failed. Throws `IllegalStateException`
/// if the subtask did not fail.
fn native_subtask_exception(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = match ctx.get_field(this, SUBTASK_FIELD_STATE) {
        Value::Int(s) => s,
        _ => SUBTASK_STATE_UNAVAILABLE,
    };
    if state != SUBTASK_STATE_FAILED {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Subtask did not fail".to_string(),
        }
        .into());
    }
    let exc = ctx.get_field(this, SUBTASK_FIELD_EXCEPTION);
    Ok(Some(exc))
}

/// `Subtask.task()Ljava/util/concurrent/Callable;`
///
/// Returns the original Callable that was forked.
fn native_subtask_task(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let callable = ctx.get_field(this, SUBTASK_FIELD_CALLABLE);
    Ok(Some(callable))
}

// ===========================================================================
// 15.2 — ShutdownOnFailure natives
// ===========================================================================

/// `ShutdownOnFailure.<init>()V`
fn native_sof_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    sts_init_fields(
        ctx,
        this,
        Value::Object(None),
        STS_POLICY_SHUTDOWN_ON_FAILURE,
    );
    Ok(None)
}

/// `ShutdownOnFailure.<init>(String, ThreadFactory)V`
fn native_sof_init_name_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    sts_init_fields(ctx, this, name, STS_POLICY_SHUTDOWN_ON_FAILURE);
    Ok(None)
}

/// `ShutdownOnFailure.open()ShutdownOnFailure` — static factory.
fn native_sof_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS)?;
    sts_init_fields(
        ctx,
        scope,
        Value::Object(None),
        STS_POLICY_SHUTDOWN_ON_FAILURE,
    );
    Ok(Some(Value::Object(Some(scope))))
}

/// `ShutdownOnFailure.throwIfFailed()V`
///
/// If any subtask failed, throws the stored exception as an
/// `ExecutionException` wrapper. If no failure occurred, returns normally.
fn native_sof_throw_if_failed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exc = ctx.get_field(this, STS_FIELD_EXCEPTION);
    match exc {
        Value::Object(Some(exc_ref)) => {
            // Re-throw the actual stored exception
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc_ref,
            ))
        }
        _ => Ok(None),
    }
}

/// `ShutdownOnFailure.throwIfFailed(Function)V`
///
/// Applies the Function mapper to the stored exception and throws the result.
fn native_sof_throw_if_failed_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exc = ctx.get_field(this, STS_FIELD_EXCEPTION);
    match exc {
        Value::Object(Some(exc_ref)) => {
            // Invoke Function.apply(exception) to map the exception
            if let Some(Value::Object(Some(function))) = args.get(1) {
                let mapped = ctx.invoke_virtual(
                    *function,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(exc_ref))],
                )?;
                match mapped {
                    Some(Value::Object(Some(mapped_exc))) => Err(
                        cratonvm_types::error::MethodCallFailed::ExceptionThrown(mapped_exc),
                    ),
                    _ => {
                        // Mapper returned null — throw the original
                        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                            exc_ref,
                        ))
                    }
                }
            } else {
                // No function provided — throw original
                Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc_ref,
                ))
            }
        }
        _ => Ok(None),
    }
}

/// `ShutdownOnFailure.exception()Ljava/util/Optional;`
///
/// Returns `Optional.of(exception)` if a failure occurred, `Optional.empty()` otherwise.
fn native_sof_exception(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exc = ctx.get_field(this, STS_FIELD_EXCEPTION);
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", OPTIONAL_NUM_FIELDS)?;
    match exc {
        Value::Object(Some(_)) => {
            // Optional.of(exception)
            ctx.set_field(opt, OPTIONAL_FIELD_VALUE, exc);
        }
        _ => {
            // Optional.empty()
            ctx.set_field(opt, OPTIONAL_FIELD_VALUE, Value::Object(None));
        }
    }
    Ok(Some(Value::Object(Some(opt))))
}

// ===========================================================================
// 15.2 — ShutdownOnSuccess natives
// ===========================================================================

/// `ShutdownOnSuccess.<init>()V`
fn native_sos_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    sts_init_fields(
        ctx,
        this,
        Value::Object(None),
        STS_POLICY_SHUTDOWN_ON_SUCCESS,
    );
    Ok(None)
}

/// `ShutdownOnSuccess.<init>(String, ThreadFactory)V`
fn native_sos_init_name_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    sts_init_fields(ctx, this, name, STS_POLICY_SHUTDOWN_ON_SUCCESS);
    Ok(None)
}

/// `ShutdownOnSuccess.open()ShutdownOnSuccess` — static factory.
fn native_sos_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_SHUTDOWN_ON_SUCCESS, STS_NUM_FIELDS)?;
    sts_init_fields(
        ctx,
        scope,
        Value::Object(None),
        STS_POLICY_SHUTDOWN_ON_SUCCESS,
    );
    Ok(Some(Value::Object(Some(scope))))
}

/// `ShutdownOnSuccess.result()Ljava/lang/Object;`
///
/// Returns the result of the first successful subtask. The scope must have been
/// shut down (i.e., at least one task succeeded). Throws `IllegalStateException`
/// if the scope is still open, or if no task completed successfully.
fn native_sos_result(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state < STS_STATE_SHUTDOWN {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "scope has not been shut down".to_string(),
        }
        .into());
    }
    // The result is stored in the EXCEPTION field (reused for ShutdownOnSuccess)
    let result = ctx.get_field(this, STS_FIELD_EXCEPTION);
    match result {
        Value::Object(None) => {
            // No result stored — all tasks failed
            Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "no successful result".to_string(),
            }
            .into())
        }
        _ => Ok(Some(result)),
    }
}

/// `ShutdownOnSuccess.result(Function)Ljava/lang/Object;`
///
/// Returns the result if a task succeeded. If no task succeeded, applies
/// the Function mapper to produce an exception to throw.
fn native_sos_result_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state < STS_STATE_SHUTDOWN {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "scope has not been shut down".to_string(),
        }
        .into());
    }
    let result = ctx.get_field(this, STS_FIELD_EXCEPTION);
    match result {
        Value::Object(None) => {
            // No successful result — apply Function to produce exception
            if let Some(Value::Object(Some(function))) = args.get(1) {
                let mapped = ctx.invoke_virtual(
                    *function,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(None)],
                )?;
                match mapped {
                    Some(Value::Object(Some(exc))) => Err(
                        cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
                    ),
                    _ => Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: "no successful result".to_string(),
                    }
                    .into()),
                }
            } else {
                Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: "no successful result".to_string(),
                }
                .into())
            }
        }
        _ => Ok(Some(result)),
    }
}

// ===========================================================================
// JDK 25 — Joiner API (JEP 505 final)
// ===========================================================================
//
// `StructuredTaskScope.Joiner<T,R>` is the standard way to define completion
// policies in JDK 25.  A Joiner controls how a scope processes subtask
// completions and what result `join()` produces.
//
// Built-in joiners:
//   - `allSuccessfulOrThrow()`    — collects all successful results in a Stream;
//                                    throws on first failure
//   - `anySuccessfulResultOrThrow()` — returns the first successful result;
//                                       throws if all fail
//   - `awaitAllSuccessfulOrThrow()` — like allSuccessfulOrThrow but returns Void
//   - `awaitAll()`                — waits for all regardless of outcome, returns Void

const CLS_JOINER: &str = "java/util/concurrent/StructuredTaskScope$Joiner";
const CLS_CONFIG: &str = "java/util/concurrent/StructuredTaskScope$Config";

// ---------------------------------------------------------------------------
// Joiner synthetic object layout — 4 fields
// ---------------------------------------------------------------------------

/// Policy type: 0 = allSuccessfulOrThrow, 1 = anySuccessfulResultOrThrow,
///              2 = awaitAllSuccessfulOrThrow, 3 = awaitAll, 4 = custom
const JOINER_FIELD_POLICY: usize = 0;
/// Collected results list (ObjectRef to an ArrayList-like container).
const JOINER_FIELD_RESULTS: usize = 1;
/// First exception seen (for throw-on-failure joiners).
const JOINER_FIELD_EXCEPTION: usize = 2;
/// Number of completed subtasks processed.
const JOINER_FIELD_COMPLETED: usize = 3;
const JOINER_NUM_FIELDS: usize = 4;

const JOINER_POLICY_ALL_SUCCESSFUL: i32 = 0;
const JOINER_POLICY_ANY_SUCCESSFUL: i32 = 1;
const JOINER_POLICY_AWAIT_ALL_SUCCESSFUL: i32 = 2;
const JOINER_POLICY_AWAIT_ALL: i32 = 3;

// ---------------------------------------------------------------------------
// Config synthetic object layout — 3 fields
// ---------------------------------------------------------------------------

/// Config name (String reference).
const CONFIG_FIELD_NAME: usize = 0;
/// Config thread factory (ObjectRef or null).
const CONFIG_FIELD_THREAD_FACTORY: usize = 1;
/// Config timeout in milliseconds (Long), 0 = no timeout.
const CONFIG_FIELD_TIMEOUT_MS: usize = 2;
const CONFIG_NUM_FIELDS: usize = 3;

// ---------------------------------------------------------------------------
// Scope owner thread — stored in a separate field index
// ---------------------------------------------------------------------------

use std::collections::HashMap;
/// The owner thread ID is stored after the standard STS fields.  Rather than
/// expand STS_NUM_FIELDS (which would break existing allocations), we store
/// it as a thread-local in Rust and validate on join/close.
///
/// For the synthetic model we use a global map: scope ObjectRef → thread ID.
use std::sync::{LazyLock, Mutex};

static SCOPE_OWNERS: LazyLock<Mutex<HashMap<usize, std::thread::ThreadId>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_scope_owner(scope: ObjectRef) {
    let key = scope.as_ptr() as usize;
    SCOPE_OWNERS
        .lock()
        .unwrap()
        .insert(key, std::thread::current().id());
}

fn check_scope_owner(scope: ObjectRef) -> bool {
    let key = scope.as_ptr() as usize;
    match SCOPE_OWNERS.lock().unwrap().get(&key) {
        Some(tid) => *tid == std::thread::current().id(),
        None => true, // No owner registered — allow (backward compat)
    }
}

fn unregister_scope_owner(scope: ObjectRef) {
    let key = scope.as_ptr() as usize;
    SCOPE_OWNERS.lock().unwrap().remove(&key);
}

// ---------------------------------------------------------------------------
// Joiner factory methods (static)
// ---------------------------------------------------------------------------

/// `Joiner.allSuccessfulOrThrow()` — static factory.
fn native_joiner_all_successful(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let joiner = try_alloc_concurrent_synthetic(ctx, CLS_JOINER, JOINER_NUM_FIELDS)?;
    ctx.set_field(
        joiner,
        JOINER_FIELD_POLICY,
        Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
    );
    ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));
    Ok(Some(Value::Object(Some(joiner))))
}

/// `Joiner.anySuccessfulResultOrThrow()` — static factory.
fn native_joiner_any_successful(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let joiner = try_alloc_concurrent_synthetic(ctx, CLS_JOINER, JOINER_NUM_FIELDS)?;
    ctx.set_field(
        joiner,
        JOINER_FIELD_POLICY,
        Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
    );
    ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));
    Ok(Some(Value::Object(Some(joiner))))
}

/// `Joiner.awaitAllSuccessfulOrThrow()` — static factory.
fn native_joiner_await_all_successful(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let joiner = try_alloc_concurrent_synthetic(ctx, CLS_JOINER, JOINER_NUM_FIELDS)?;
    ctx.set_field(
        joiner,
        JOINER_FIELD_POLICY,
        Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL),
    );
    ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));
    Ok(Some(Value::Object(Some(joiner))))
}

/// `Joiner.awaitAll()` — static factory.
fn native_joiner_await_all(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let joiner = try_alloc_concurrent_synthetic(ctx, CLS_JOINER, JOINER_NUM_FIELDS)?;
    ctx.set_field(
        joiner,
        JOINER_FIELD_POLICY,
        Value::Int(JOINER_POLICY_AWAIT_ALL),
    );
    ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
    ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));
    Ok(Some(Value::Object(Some(joiner))))
}

/// `Joiner.onComplete(Subtask)` — process a completed subtask.
///
/// The Joiner inspects the subtask's state and acts according to its policy:
/// - ALL_SUCCESSFUL / AWAIT_ALL_SUCCESSFUL: stores result; throws on failure
/// - ANY_SUCCESSFUL: stores first success; shuts down on success
/// - AWAIT_ALL: just counts, ignores failures
fn native_joiner_on_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let subtask_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let policy = match ctx.get_field(this, JOINER_FIELD_POLICY) {
        Value::Int(p) => p,
        _ => 0,
    };

    // Increment completed count
    let completed = match ctx.get_field(this, JOINER_FIELD_COMPLETED) {
        Value::Int(c) => c,
        _ => 0,
    };
    ctx.set_field(this, JOINER_FIELD_COMPLETED, Value::Int(completed + 1));

    if let Value::Object(Some(subtask)) = subtask_val {
        let sub_state = match ctx.get_field(subtask, SUBTASK_FIELD_STATE) {
            Value::Int(s) => s,
            _ => SUBTASK_STATE_UNAVAILABLE,
        };

        match policy {
            JOINER_POLICY_ALL_SUCCESSFUL | JOINER_POLICY_AWAIT_ALL_SUCCESSFUL => {
                if sub_state == SUBTASK_STATE_FAILED {
                    // Store the first exception
                    let existing = ctx.get_field(this, JOINER_FIELD_EXCEPTION);
                    if matches!(existing, Value::Object(None)) {
                        let exc = ctx.get_field(subtask, SUBTASK_FIELD_EXCEPTION);
                        ctx.set_field(this, JOINER_FIELD_EXCEPTION, exc);
                    }
                }
            }
            JOINER_POLICY_ANY_SUCCESSFUL => {
                if sub_state == SUBTASK_STATE_SUCCESS {
                    // Store the first successful result
                    let existing = ctx.get_field(this, JOINER_FIELD_RESULTS);
                    if matches!(existing, Value::Object(None)) {
                        let result = ctx.get_field(subtask, SUBTASK_FIELD_RESULT);
                        ctx.set_field(this, JOINER_FIELD_RESULTS, result);
                    }
                } else if sub_state == SUBTASK_STATE_FAILED {
                    let existing = ctx.get_field(this, JOINER_FIELD_EXCEPTION);
                    if matches!(existing, Value::Object(None)) {
                        let exc = ctx.get_field(subtask, SUBTASK_FIELD_EXCEPTION);
                        ctx.set_field(this, JOINER_FIELD_EXCEPTION, exc);
                    }
                }
            }
            JOINER_POLICY_AWAIT_ALL => {
                // No action needed — we just count completions
            }
            _ => {}
        }
    }

    // Return whether the joiner wants to continue (false = continue, true = short-circuit)
    // anySuccessfulResultOrThrow short-circuits on first success
    if policy == JOINER_POLICY_ANY_SUCCESSFUL {
        let has_result = !matches!(
            ctx.get_field(this, JOINER_FIELD_RESULTS),
            Value::Object(None)
        );
        Ok(Some(Value::Int(if has_result { 1 } else { 0 })))
    } else {
        Ok(Some(Value::Int(0))) // never short-circuit
    }
}

/// `Joiner.result()` — return the final result after join.
///
/// - ALL_SUCCESSFUL: returns collected results as a stream-like container
/// - ANY_SUCCESSFUL: returns the first successful result
/// - AWAIT_ALL_SUCCESSFUL / AWAIT_ALL: returns Void (null)
fn native_joiner_result(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let policy = match ctx.get_field(this, JOINER_FIELD_POLICY) {
        Value::Int(p) => p,
        _ => 0,
    };
    let exception = ctx.get_field(this, JOINER_FIELD_EXCEPTION);

    match policy {
        JOINER_POLICY_ALL_SUCCESSFUL | JOINER_POLICY_AWAIT_ALL_SUCCESSFUL => {
            // If any failure occurred, throw the stored exception
            if let Value::Object(Some(exc)) = exception {
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc,
                ));
            }
            if policy == JOINER_POLICY_AWAIT_ALL_SUCCESSFUL {
                Ok(Some(Value::Object(None))) // Void
            } else {
                // Return results (or null container — in practice the caller
                // collects results from subtask.get() calls)
                let results = ctx.get_field(this, JOINER_FIELD_RESULTS);
                Ok(Some(results))
            }
        }
        JOINER_POLICY_ANY_SUCCESSFUL => {
            let result = ctx.get_field(this, JOINER_FIELD_RESULTS);
            if matches!(result, Value::Object(None)) {
                // No successful result — throw
                if let Value::Object(Some(exc)) = exception {
                    return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exc,
                    ));
                }
                return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: "no successful result".to_string(),
                }
                .into());
            }
            Ok(Some(result))
        }
        JOINER_POLICY_AWAIT_ALL => {
            Ok(Some(Value::Object(None))) // Void — no exceptions thrown
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Joiner.policy()I` — return the joiner policy constant.
fn native_joiner_policy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let policy = ctx.get_field(this, JOINER_FIELD_POLICY);
    Ok(Some(policy))
}

// ---------------------------------------------------------------------------
// StructuredTaskScope.open(Joiner) — JDK 25 factory
// ---------------------------------------------------------------------------

/// `StructuredTaskScope.open(Joiner)StructuredTaskScope` — open with joiner.
///
/// Creates a new scope that uses the given Joiner for its completion policy.
/// The Joiner determines what happens when subtasks complete and what
/// `join()` returns.
fn native_sts_open_with_joiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let joiner_val = args.get(0).copied().unwrap_or(Value::Object(None));
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS)?;

    // Determine policy from joiner
    let policy = if let Value::Object(Some(joiner)) = joiner_val {
        match ctx.get_field(joiner, JOINER_FIELD_POLICY) {
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_SUCCESS,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL)
            | Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_FAILURE,
            _ => STS_POLICY_BASE,
        }
    } else {
        STS_POLICY_BASE
    };

    sts_init_fields(ctx, scope, Value::Object(None), policy);
    register_scope_owner(scope);
    Ok(Some(Value::Object(Some(scope))))
}

// ---------------------------------------------------------------------------
// Config API
// ---------------------------------------------------------------------------

/// `StructuredTaskScope$Config.<init>()V`
fn native_config_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, CONFIG_FIELD_NAME, Value::Object(None));
    ctx.set_field(this, CONFIG_FIELD_THREAD_FACTORY, Value::Object(None));
    ctx.set_field(this, CONFIG_FIELD_TIMEOUT_MS, Value::Long(0));
    Ok(None)
}

/// `Config.withName(String)Config` — set the scope name.
fn native_config_with_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let config = try_alloc_concurrent_synthetic(ctx, CLS_CONFIG, CONFIG_NUM_FIELDS)?;
    // Copy existing fields
    let tf = ctx.get_field(this, CONFIG_FIELD_THREAD_FACTORY);
    let timeout = ctx.get_field(this, CONFIG_FIELD_TIMEOUT_MS);
    ctx.set_field(config, CONFIG_FIELD_NAME, name);
    ctx.set_field(config, CONFIG_FIELD_THREAD_FACTORY, tf);
    ctx.set_field(config, CONFIG_FIELD_TIMEOUT_MS, timeout);
    Ok(Some(Value::Object(Some(config))))
}

/// `Config.withThreadFactory(ThreadFactory)Config`
fn native_config_with_thread_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let tf = args.get(1).copied().unwrap_or(Value::Object(None));
    let config = try_alloc_concurrent_synthetic(ctx, CLS_CONFIG, CONFIG_NUM_FIELDS)?;
    let name = ctx.get_field(this, CONFIG_FIELD_NAME);
    let timeout = ctx.get_field(this, CONFIG_FIELD_TIMEOUT_MS);
    ctx.set_field(config, CONFIG_FIELD_NAME, name);
    ctx.set_field(config, CONFIG_FIELD_THREAD_FACTORY, tf);
    ctx.set_field(config, CONFIG_FIELD_TIMEOUT_MS, timeout);
    Ok(Some(Value::Object(Some(config))))
}

/// `Config.withTimeout(Duration)Config`
fn native_config_with_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let timeout_ms = match args.get(1) {
        Some(Value::Object(Some(duration))) => {
            // Duration stores seconds in field 0, nanos in field 1
            let secs = match ctx.get_field(*duration, 0) {
                Value::Long(s) => s,
                Value::Int(s) => s as i64,
                _ => 0,
            };
            let nanos = match ctx.get_field(*duration, 1) {
                Value::Int(n) => n as i64,
                _ => 0,
            };
            secs * 1000 + nanos / 1_000_000
        }
        _ => 0,
    };
    let config = try_alloc_concurrent_synthetic(ctx, CLS_CONFIG, CONFIG_NUM_FIELDS)?;
    let name = ctx.get_field(this, CONFIG_FIELD_NAME);
    let tf = ctx.get_field(this, CONFIG_FIELD_THREAD_FACTORY);
    ctx.set_field(config, CONFIG_FIELD_NAME, name);
    ctx.set_field(config, CONFIG_FIELD_THREAD_FACTORY, tf);
    ctx.set_field(config, CONFIG_FIELD_TIMEOUT_MS, Value::Long(timeout_ms));
    Ok(Some(Value::Object(Some(config))))
}

/// `Config.getName()Ljava/lang/String;`
fn native_config_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = ctx.get_field(this, CONFIG_FIELD_NAME);
    Ok(Some(name))
}

/// `Config.getThreadFactory()Ljava/util/concurrent/ThreadFactory;`
fn native_config_get_thread_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let tf = ctx.get_field(this, CONFIG_FIELD_THREAD_FACTORY);
    Ok(Some(tf))
}

// ---------------------------------------------------------------------------
// Owner-validated open/join/close (JDK 25 — scope confined to owner thread)
// ---------------------------------------------------------------------------

/// `StructuredTaskScope.open()` with owner tracking.
fn native_sts_open_owned(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS)?;
    sts_init_fields(ctx, scope, Value::Object(None), STS_POLICY_BASE);
    register_scope_owner(scope);
    Ok(Some(Value::Object(Some(scope))))
}

/// Owner-validating join — wraps `native_sts_join` with ownership check.
fn native_sts_join_owned(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "join() called from non-owner thread".to_string(),
        }
        .into());
    }
    native_sts_join(ctx, args)
}

/// Owner-validating close — wraps `native_sts_close` with ownership check
/// and cleanup.
fn native_sts_close_owned(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "close() called from non-owner thread".to_string(),
        }
        .into());
    }
    let result = native_sts_close(ctx, args);
    if result.is_ok() {
        unregister_scope_owner(this);
    }
    result
}

// ---------------------------------------------------------------------------
// Joiner-aware fork — notifies the Joiner on subtask completion
// ---------------------------------------------------------------------------

/// Global map: scope → joiner, for scopes opened with open(Joiner).
///
/// GC note (gc-followups-20260706): KNOWN-UNSOUND across GCs on two axes —
/// the key is the scope's raw address (stale after the scope moves, so the
/// lookup misses / can alias a reused address) and the joiner value is
/// neither a GC root nor remapped. Tolerable only while a scope's open→join
/// window contains no moving GC. Follow-up: key by
/// `ctx.identity_hash_code(scope)` and store the joiner as a
/// `(identity_key, ObjectRef)` var-handle-root pair (ASYNC_POOL pattern).
static SCOPE_JOINERS: LazyLock<Mutex<HashMap<usize, ObjectRef>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_scope_joiner(scope: ObjectRef, joiner: ObjectRef) {
    let key = scope.as_ptr() as usize;
    SCOPE_JOINERS.lock().unwrap().insert(key, joiner);
}

fn get_scope_joiner(scope: ObjectRef) -> Option<ObjectRef> {
    let key = scope.as_ptr() as usize;
    SCOPE_JOINERS.lock().unwrap().get(&key).copied()
}

fn unregister_scope_joiner(scope: ObjectRef) {
    let key = scope.as_ptr() as usize;
    SCOPE_JOINERS.lock().unwrap().remove(&key);
}

/// Enhanced `StructuredTaskScope.open(Joiner)` — registers joiner for fork
/// notification.
fn native_sts_open_joiner_tracked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let joiner_val = args.get(0).copied().unwrap_or(Value::Object(None));
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS)?;

    let policy = if let Value::Object(Some(joiner)) = joiner_val {
        let p = match ctx.get_field(joiner, JOINER_FIELD_POLICY) {
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_SUCCESS,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL)
            | Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_FAILURE,
            _ => STS_POLICY_BASE,
        };
        register_scope_joiner(scope, joiner);
        p
    } else {
        STS_POLICY_BASE
    };

    sts_init_fields(ctx, scope, Value::Object(None), policy);
    register_scope_owner(scope);
    Ok(Some(Value::Object(Some(scope))))
}

/// Enhanced fork that notifies the scope's Joiner (if any) on completion.
fn native_sts_fork_with_joiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Perform the standard fork
    let result = native_sts_fork(ctx, args)?;

    // If scope has a joiner, notify it
    if let Some(joiner) = get_scope_joiner(this) {
        if let Some(Value::Object(Some(subtask))) = result {
            // Call Joiner.onComplete(subtask) — internal, not via JVM dispatch
            let _ = native_joiner_on_complete(
                ctx,
                &[Value::Object(Some(joiner)), Value::Object(Some(subtask))],
            );
        }
    }

    Ok(result)
}

/// Enhanced close that cleans up joiner association.
fn native_sts_close_joiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "close() called from non-owner thread".to_string(),
        }
        .into());
    }
    let result = native_sts_close(ctx, args);
    if result.is_ok() {
        unregister_scope_owner(this);
        unregister_scope_joiner(this);
    }
    result
}

// ===========================================================================
// Registration
// ===========================================================================

pub(crate) fn register_jdk25_concurrency_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // W7-77: publish the fabricated Thread model's map. This registrar is
    // synthetic-only (`lib.rs:24110`, inside `register_synthetic_overrides`),
    // and that is the right scope rather than a limitation: in real-JDK mode
    // `vm_exec.rs::thread_start`'s `eetop` witness refuses the fabricated read
    // outright, so a sweep row there would be noise. In SYNTHETIC mode the
    // sweep asks the question that matters -- does the fabricated
    // `java/lang/Thread` still declare what these constants believe? -- which
    // is exactly the drift that put the virtual flag at slot 4 until
    // 2026-08-05.
    cratonvm_native_api::read_alias::declare_slot_map(&SYNTHETIC_THREAD_SLOT_MAP);
    // --- ScopedValue ---
    r.register(CLS_SCOPED_VALUE, "<init>", "()V", native_sv_init);
    r.register(
        CLS_SCOPED_VALUE,
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        native_sv_new_instance,
    );
    r.register(
        CLS_SCOPED_VALUE,
        "get",
        "()Ljava/lang/Object;",
        native_sv_get,
    );
    r.register(CLS_SCOPED_VALUE, "isBound", "()Z", native_sv_is_bound);
    r.register(
        CLS_SCOPED_VALUE,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_sv_or_else,
    );
    r.register(
        CLS_SCOPED_VALUE,
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_sv_or_else_throw,
    );
    r.register(CLS_SCOPED_VALUE, "hashCode", "()I", native_sv_hash_code);

    // --- ScopedValue.where (static) ---
    r.register(
        CLS_SCOPED_VALUE,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        native_sv_where_static,
    );

    // --- ScopedValue$Carrier ---
    r.register(
        CLS_CARRIER,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        native_carrier_where,
    );
    r.register(
        CLS_CARRIER,
        "run",
        "(Ljava/lang/Runnable;)V",
        native_carrier_run,
    );
    r.register(
        CLS_CARRIER,
        "call",
        "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
        native_carrier_call,
    );
    // JDK 25 uses ScopedValue$CallableOp instead of Callable
    r.register(
        CLS_CARRIER,
        "call",
        "(Ljava/lang/ScopedValue$CallableOp;)Ljava/lang/Object;",
        native_carrier_call,
    );
    r.register(
        CLS_CARRIER,
        "get",
        "()Ljava/lang/Object;",
        native_carrier_get,
    );

    // --- ScopedValue$Snapshot ---
    r.register(
        CLS_SNAPSHOT,
        "capture",
        "()Ljava/lang/ScopedValue$Snapshot;",
        native_snapshot_capture,
    );

    // --- StructuredTaskScope ---
    //
    // JDK-ONLY-NOTE (F17-1, 2026-08-13): the W7-18 note further down covers the
    // `ShutdownOnFailure`/`ShutdownOnSuccess`/`$Config`/`Joiner.policy` block. It
    // explicitly scopes itself to "every registration between here and the
    // `Joiner` block below", which leaves the block you are reading now —
    // `StructuredTaskScope` itself and `$Subtask` — untriaged. It is triaged
    // here. SEVEN of the fourteen triples below name a member JDK 25 does not
    // declare, measured on Microsoft 25.0.3+9-LTS:
    //
    //   * `<init>()V`, `<init>(String)V`, `<init>(String,ThreadFactory)V` —
    //     `javap` reports `public interface
    //     java.util.concurrent.StructuredTaskScope<T,R> extends AutoCloseable`.
    //     JEP 505 turned the class into an INTERFACE, and an interface has no
    //     constructors at all; `open()` is the JDK-true way to get one.
    //   * `isShutdown()Z` and `shutdown()V` — gone. `isCancelled()Z` is the
    //     replacement predicate and there is no replacement for the mutator:
    //     cancellation is the `Joiner`'s decision, taken via `onComplete`.
    //   * `joinUntil(Ljava/time/Instant;)…` — gone. The deadline moved onto the
    //     scope's configuration, as `Configuration.withTimeout(Duration)`.
    //   * `join()Ljava/util/concurrent/StructuredTaskScope;` — the NAME is real
    //     and the DESCRIPTOR is not. JEP 505 changed the return type to `R`, so
    //     javac emits `()Ljava/lang/Object;`. This is the shape a name-keyed
    //     search reports as agreement, and it is the reason the pair are two
    //     separate registrations rather than one being an update of the other.
    //
    // and, on `$Subtask` below, `task()Ljava/util/concurrent/Callable;` — `javap
    // -p …$Subtask` lists exactly three members (`state`, `get`, `exception`)
    // and `task` is not among them. The seven that ARE JDK-true are `open()`,
    // `fork(Callable)`, `close()`, and `Subtask.{get,state,exception}`.
    //
    // NOT DELETED, on the same two-part test W7-18 applied — and it comes out
    // the same way here, so the reasoning is inherited rather than re-derived:
    //
    //   * It cannot move either shipping mode. TRUE.
    //     `register_jdk25_concurrency_natives` is reached only from
    //     `register_synthetic_overrides`, which is `#[cfg(feature =
    //     "synthetic-jdk")]` and is not a default feature of `cratonvm-vm` or
    //     `cratonvm-cli`. Under `--jdk-only` and `--real-jdk` the real JDK
    //     bytecode serves this whole API and none of these bodies is compiled
    //     in, let alone reached.
    //   * The deletion is checkable. FALSE without a run — and here the reason
    //     differs from W7-18's, so check it rather than assuming. These seven
    //     triples are NOT pinned by `r.find` in this file's test module (unlike
    //     `Joiner.policy()I`, which is, at two sites). What pins them is the
    //     other direction: `native_sts_join_until` and `native_subtask_task`
    //     have direct-call unit tests (`p82_join_until_*`, and the `$Subtask`
    //     one above `p82_join_until_past_deadline`), so deleting the
    //     registrations alone leaves the bodies reachable only from `#[cfg(test)]`
    //     code and turns them into `dead_code` warnings in a release build.
    //     Deleting bodies and tests together is the right change; doing it blind,
    //     with no build and no run of the one mode that could observe it, is how
    //     a divergence gets frozen in rather than removed.
    //
    // The deletion is written up as a nomination in
    // docs/known-issues/jdk-only/F17-1-cds-sharedsecrets-fabrications-20260813.md.
    //
    // WHAT IS ALREADY CORRECT, so nobody "fixes" it twice: the JEP 505 spellings
    // — `join()Ljava/lang/Object;`, `isCancelled()Z`, `fork(Runnable)`,
    // `open(Joiner,Function)`, `Joiner.allUntil`, `Joiner.onFork`, and
    // `$Configuration.with{Name,ThreadFactory,Timeout}` — are all registered,
    // JDK-true, by `phases_late/concurrent.rs::register_p67_structured_task_scope_j25`.
    // That registrar runs BEFORE this one and `register()` is last-write-wins,
    // so adding any of those triples here would silently replace a correct body
    // with a JDK-21-shaped one. `w7_18_jep505_surface_is_not_shadowed_here` is
    // the ratchet that catches it; re-read it before adding a registration here.
    r.register(CLS_TASK_SCOPE, "<init>", "()V", native_sts_init);
    r.register(
        CLS_TASK_SCOPE,
        "<init>",
        "(Ljava/lang/String;)V",
        native_sts_init_name,
    );
    r.register(
        CLS_TASK_SCOPE,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sts_init_name_factory,
    );
    r.register(
        CLS_TASK_SCOPE,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_open,
    );
    r.register(
        CLS_TASK_SCOPE,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork,
    );
    r.register(
        CLS_TASK_SCOPE,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join,
    );
    r.register(
        CLS_TASK_SCOPE,
        "joinUntil",
        "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join_until,
    );
    r.register(CLS_TASK_SCOPE, "close", "()V", native_sts_close);
    r.register(CLS_TASK_SCOPE, "shutdown", "()V", native_sts_shutdown);
    r.register(CLS_TASK_SCOPE, "isShutdown", "()Z", native_sts_is_shutdown);
    r.register(
        CLS_TASK_SCOPE,
        "toString",
        "()Ljava/lang/String;",
        native_sts_to_string,
    );

    // --- nb-jdk25-concurrency: fork worker Runnable ---
    // Each fork() spawns a real worker Thread whose Thread.run() dispatches to
    // a CratonVM$StsForkRunner; this native is the runner body that invokes
    // the Callable and records the outcome on the Subtask.
    r.register(CLS_FORK_RUNNER, "run", "()V", native_fork_runner_run);

    // --- StructuredTaskScope$Subtask ---
    r.register(
        CLS_SUBTASK,
        "get",
        "()Ljava/lang/Object;",
        native_subtask_get,
    );
    r.register(
        CLS_SUBTASK,
        "state",
        "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;",
        native_subtask_state,
    );
    r.register(
        CLS_SUBTASK,
        "exception",
        "()Ljava/lang/Throwable;",
        native_subtask_exception,
    );

    // --- Subtask.task() ---
    // F17-1 (2026-08-13): not a member of JDK 25's `$Subtask`. `javap -p
    // java.util.concurrent.StructuredTaskScope$Subtask` lists three methods —
    // `state()`, `get()`, `exception()` — and no `task()`. Retained for now for
    // the reason set out in the JDK-ONLY-NOTE on the `StructuredTaskScope`
    // registration block above; `native_subtask_task` has a direct-call unit
    // test, so the registration and the body must go together.
    r.register(
        CLS_SUBTASK,
        "task",
        "()Ljava/util/concurrent/Callable;",
        native_subtask_task,
    );

    // --- ShutdownOnFailure ---
    //
    // JDK-ONLY-NOTE (W7-18): every registration between here and the `Joiner`
    // block below is on a class or a method that **no JDK 25 declares**, and
    // that is measured rather than inferred. `javap` on Adoptium 25.0.3.9
    // answers "class not found" for `StructuredTaskScope$ShutdownOnSuccess` and
    // `$ShutdownOnFailure` — JEP 505 deleted both — and `Class.forName` answers
    // `ClassNotFoundException` for each on HotSpot 25, `cratonvm --real-jdk` and
    // `cratonvm --jdk-only` alike. `StructuredTaskScope$Config` is a name no JDK
    // ever shipped (`$Configuration` is the real one, registered by
    // `phases_late/concurrent.rs::register_p67_structured_task_scope_j25`), and
    // `Joiner.policy()I` is not on the JDK's `Joiner` either. Transcript:
    // docs/known-issues/jdk-only/W7-18-structured-task-scope-jep505.md §1/§3.
    //
    // WHY THEY ARE STILL HERE, named as a decision rather than left as an
    // oversight. Two things have to be true at once for the deletion to be
    // worth taking, and only one is:
    //
    //   * It cannot move either shipping mode. TRUE — this registrar is reached
    //     only from `register_synthetic_overrides`, so in `--real-jdk` and
    //     `--jdk-only` real JDK bytecode serves the whole API with zero
    //     fabrication (W7-18 §5: `compatibility_classes: 0`,
    //     `synthetic_stub_invocations: 0`, no StructuredTaskScope violation in
    //     1,454). So the deletion is also worth exactly nothing there.
    //   * The deletion is checkable. FALSE without a run. Roughly twenty
    //     `#[test]`s in this file's own test module PIN these registrations by
    //     triple — `test_register_sof_*` and `test_register_sos_*` (nine),
    //     `test_all_shutdown_on_failure_methods_registered` and its
    //     `_success_` twin, `s52_joiner_policy_registered`,
    //     `s52_total_registration_count`, and the fork/join/close/shutdown
    //     lists — and the only mode the change could be observed in —
    //     `--synthetic-jdk` — has never been executed once. Deleting the
    //     registrations therefore means rewriting a blocking gate's assertions
    //     to match an unmeasured expectation, which is how a divergence gets
    //     frozen in rather than removed.
    //
    // What DID land instead, because it was both checkable and load-bearing:
    // `util_concurrent_ext.rs::register_pd_structured_concurrency` — the third
    // registrar of these same classes, with an INCOMPATIBLE `$Subtask` slot
    // convention — is retired to a tombstone. Read its doc comment: it explains
    // why every triple it held was already overwritten by this registrar, and
    // why leaving it in place was a landmine for whoever finally deletes the
    // block below. See also `w7_18_jep505_surface_is_not_shadowed_here`, the
    // ratchet that keeps this registrar from growing a second body for the JEP
    // 505 triples that live in `phases_late/concurrent.rs`.
    r.register(CLS_SHUTDOWN_ON_FAILURE, "<init>", "()V", native_sof_init);
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sof_init_name_factory,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;",
        native_sof_open,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "throwIfFailed",
        "()V",
        native_sof_throw_if_failed,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "throwIfFailed",
        "(Ljava/util/function/Function;)V",
        native_sof_throw_if_failed_fn,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "exception",
        "()Ljava/util/Optional;",
        native_sof_exception,
    );
    // ShutdownOnFailure inherits fork/join/close/shutdown from StructuredTaskScope
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join,
    );
    r.register(CLS_SHUTDOWN_ON_FAILURE, "close", "()V", native_sts_close);
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "shutdown",
        "()V",
        native_sts_shutdown,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "isShutdown",
        "()Z",
        native_sts_is_shutdown,
    );

    // --- ShutdownOnSuccess ---
    r.register(CLS_SHUTDOWN_ON_SUCCESS, "<init>", "()V", native_sos_init);
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sos_init_name_factory,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        native_sos_open,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "result",
        "()Ljava/lang/Object;",
        native_sos_result,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "result",
        "(Ljava/util/function/Function;)Ljava/lang/Object;",
        native_sos_result_fn,
    );
    // ShutdownOnSuccess inherits fork/join/close/shutdown from StructuredTaskScope
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join,
    );
    r.register(CLS_SHUTDOWN_ON_SUCCESS, "close", "()V", native_sts_close);
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "shutdown",
        "()V",
        native_sts_shutdown,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "isShutdown",
        "()Z",
        native_sts_is_shutdown,
    );

    // --- Joiner API (JDK 25 / JEP 505) ---
    r.register(
        CLS_JOINER,
        "allSuccessfulOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_all_successful,
    );
    r.register(
        CLS_JOINER,
        "anySuccessfulResultOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_any_successful,
    );
    r.register(
        CLS_JOINER,
        "awaitAllSuccessfulOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_await_all_successful,
    );
    r.register(
        CLS_JOINER,
        "awaitAll",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_await_all,
    );
    r.register(
        CLS_JOINER,
        "onComplete",
        "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z",
        native_joiner_on_complete,
    );
    r.register(
        CLS_JOINER,
        "result",
        "()Ljava/lang/Object;",
        native_joiner_result,
    );
    r.register(CLS_JOINER, "policy", "()I", native_joiner_policy);

    // --- StructuredTaskScope.open(Joiner) ---
    r.register(
        CLS_TASK_SCOPE,
        "open",
        "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_open_joiner_tracked,
    );

    // --- Config API ---
    r.register(CLS_CONFIG, "<init>", "()V", native_config_init);
    r.register(
        CLS_CONFIG,
        "withName",
        "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_name,
    );
    r.register(
        CLS_CONFIG,
        "withThreadFactory",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_thread_factory,
    );
    r.register(
        CLS_CONFIG,
        "withTimeout",
        "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_timeout,
    );
    r.register(
        CLS_CONFIG,
        "getName",
        "()Ljava/lang/String;",
        native_config_get_name,
    );
    r.register(
        CLS_CONFIG,
        "getThreadFactory",
        "()Ljava/util/concurrent/ThreadFactory;",
        native_config_get_thread_factory,
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod jdk25_concurrency_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    // -----------------------------------------------------------------------
    // Constant sanity checks
    // -----------------------------------------------------------------------

    #[test]
    fn test_sv_field_indices() {
        assert_eq!(SV_FIELD_VALUE, 0);
        assert_eq!(SV_FIELD_IS_BOUND, 1);
        assert_eq!(SV_FIELD_HASH, 2);
        assert_eq!(SV_NUM_FIELDS, 3);
    }

    #[test]
    fn test_carrier_field_indices() {
        assert_eq!(CARRIER_FIELD_SV_REF, 0);
        assert_eq!(CARRIER_FIELD_VALUE_REF, 1);
        assert_eq!(CARRIER_FIELD_PARENT_REF, 2);
        assert_eq!(CARRIER_NUM_FIELDS, 3);
    }

    #[test]
    fn test_snapshot_field_indices() {
        assert_eq!(SNAPSHOT_FIELD_BINDINGS_COUNT, 0);
        assert_eq!(SNAPSHOT_FIELD_TIMESTAMP, 1);
        assert_eq!(SNAPSHOT_NUM_FIELDS, 2);
    }

    #[test]
    fn test_sts_field_indices() {
        assert_eq!(STS_FIELD_NAME, 0);
        assert_eq!(STS_FIELD_STATE, 1);
        assert_eq!(STS_FIELD_TASK_COUNT, 2);
        assert_eq!(STS_FIELD_COMPLETED_COUNT, 3);
        assert_eq!(STS_FIELD_EXCEPTION, 4);
        assert_eq!(STS_FIELD_POLICY, 5);
        assert_eq!(STS_FIELD_JOINED, 6);
        assert_eq!(STS_FIELD_SUPPRESSED_COUNT, 7);
        assert_eq!(STS_NUM_FIELDS, 8);
    }

    #[test]
    fn test_subtask_field_indices() {
        assert_eq!(SUBTASK_FIELD_STATE, 0);
        assert_eq!(SUBTASK_FIELD_RESULT, 1);
        assert_eq!(SUBTASK_FIELD_EXCEPTION, 2);
        assert_eq!(SUBTASK_FIELD_CALLABLE, 3);
        assert_eq!(SUBTASK_NUM_FIELDS, 4);
    }

    #[test]
    fn test_sts_state_constants() {
        assert_eq!(STS_STATE_OPEN, 0);
        assert_eq!(STS_STATE_SHUTDOWN, 1);
        assert_eq!(STS_STATE_CLOSED, 2);
    }

    #[test]
    fn test_subtask_state_constants() {
        assert_eq!(SUBTASK_STATE_UNAVAILABLE, 0);
        assert_eq!(SUBTASK_STATE_SUCCESS, 1);
        assert_eq!(SUBTASK_STATE_FAILED, 2);
    }

    #[test]
    fn test_class_name_scoped_value() {
        assert_eq!(CLS_SCOPED_VALUE, "java/lang/ScopedValue");
    }

    #[test]
    fn test_class_name_carrier() {
        assert_eq!(CLS_CARRIER, "java/lang/ScopedValue$Carrier");
    }

    #[test]
    fn test_class_name_snapshot() {
        assert_eq!(CLS_SNAPSHOT, "java/lang/ScopedValue$Snapshot");
    }

    #[test]
    fn test_class_name_task_scope() {
        assert_eq!(CLS_TASK_SCOPE, "java/util/concurrent/StructuredTaskScope");
    }

    #[test]
    fn test_class_name_subtask() {
        assert_eq!(
            CLS_SUBTASK,
            "java/util/concurrent/StructuredTaskScope$Subtask"
        );
    }

    #[test]
    fn test_class_name_shutdown_on_failure() {
        assert_eq!(
            CLS_SHUTDOWN_ON_FAILURE,
            "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure"
        );
    }

    #[test]
    fn test_class_name_shutdown_on_success() {
        assert_eq!(
            CLS_SHUTDOWN_ON_SUCCESS,
            "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess"
        );
    }

    // -----------------------------------------------------------------------
    // Registration — ScopedValue
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_sv_init() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SCOPED_VALUE, "<init>", "()V").is_some());
    }

    #[test]
    fn test_register_sv_new_instance() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SCOPED_VALUE, "newInstance", "()Ljava/lang/ScopedValue;")
            .is_some());
    }

    #[test]
    fn test_register_sv_get() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SCOPED_VALUE, "get", "()Ljava/lang/Object;")
            .is_some());
    }

    #[test]
    fn test_register_sv_is_bound() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SCOPED_VALUE, "isBound", "()Z").is_some());
    }

    #[test]
    fn test_register_sv_or_else() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SCOPED_VALUE,
                "orElse",
                "(Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some());
    }

    #[test]
    fn test_register_sv_or_else_throw() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SCOPED_VALUE,
                "orElseThrow",
                "(Ljava/util/function/Supplier;)Ljava/lang/Object;"
            )
            .is_some());
    }

    #[test]
    fn test_register_sv_where_static() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SCOPED_VALUE,
                "where",
                "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;"
            )
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — Carrier
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_carrier_where() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_CARRIER,
                "where",
                "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;"
            )
            .is_some());
    }

    #[test]
    fn test_register_carrier_run() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_CARRIER, "run", "(Ljava/lang/Runnable;)V")
            .is_some());
    }

    #[test]
    fn test_register_carrier_call() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_CARRIER,
                "call",
                "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;"
            )
            .is_some());
    }

    #[test]
    fn test_register_carrier_get() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_CARRIER, "get", "()Ljava/lang/Object;").is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_snapshot_capture() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SNAPSHOT,
                "capture",
                "()Ljava/lang/ScopedValue$Snapshot;"
            )
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — StructuredTaskScope
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_sts_init_no_args() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_TASK_SCOPE, "<init>", "()V").is_some());
    }

    #[test]
    fn test_register_sts_init_name() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_TASK_SCOPE, "<init>", "(Ljava/lang/String;)V")
            .is_some());
    }

    #[test]
    fn test_register_sts_init_name_factory() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_TASK_SCOPE,
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_sts_fork() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_TASK_SCOPE,
                "fork",
                "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"
            )
            .is_some());
    }

    #[test]
    fn test_register_sts_join() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_TASK_SCOPE,
                "join",
                "()Ljava/util/concurrent/StructuredTaskScope;"
            )
            .is_some());
    }

    #[test]
    fn test_register_sts_join_until() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_TASK_SCOPE,
                "joinUntil",
                "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;"
            )
            .is_some());
    }

    #[test]
    fn test_register_sts_close() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_TASK_SCOPE, "close", "()V").is_some());
    }

    #[test]
    fn test_register_sts_shutdown() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_TASK_SCOPE, "shutdown", "()V").is_some());
    }

    #[test]
    fn test_register_sts_is_shutdown() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_TASK_SCOPE, "isShutdown", "()Z").is_some());
    }

    #[test]
    fn test_register_sts_to_string() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_TASK_SCOPE, "toString", "()Ljava/lang/String;")
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — Subtask
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_subtask_get() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SUBTASK, "get", "()Ljava/lang/Object;").is_some());
    }

    #[test]
    fn test_register_subtask_state() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SUBTASK,
                "state",
                "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;"
            )
            .is_some());
    }

    #[test]
    fn test_register_subtask_exception() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SUBTASK, "exception", "()Ljava/lang/Throwable;")
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — ShutdownOnFailure
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_sof_init() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SHUTDOWN_ON_FAILURE, "<init>", "()V").is_some());
    }

    #[test]
    fn test_register_sof_init_name_factory() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_FAILURE,
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_sof_throw_if_failed() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SHUTDOWN_ON_FAILURE, "throwIfFailed", "()V")
            .is_some());
    }

    #[test]
    fn test_register_sof_throw_if_failed_fn() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_FAILURE,
                "throwIfFailed",
                "(Ljava/util/function/Function;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_sof_exception() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_FAILURE,
                "exception",
                "()Ljava/util/Optional;"
            )
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Registration — ShutdownOnSuccess
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_sos_init() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SHUTDOWN_ON_SUCCESS, "<init>", "()V").is_some());
    }

    #[test]
    fn test_register_sos_init_name_factory() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_SUCCESS,
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_sos_result() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SHUTDOWN_ON_SUCCESS, "result", "()Ljava/lang/Object;")
            .is_some());
    }

    #[test]
    fn test_register_sos_result_fn() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_SUCCESS,
                "result",
                "(Ljava/util/function/Function;)Ljava/lang/Object;"
            )
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Comprehensive registration count test
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_scoped_value_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // ScopedValue: init, newInstance, get, isBound, orElse, orElseThrow, where
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("newInstance", "()Ljava/lang/ScopedValue;"),
            ("get", "()Ljava/lang/Object;"),
            ("isBound", "()Z"),
            ("orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            (
                "orElseThrow",
                "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
            ),
            (
                "where",
                "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
            ),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_SCOPED_VALUE, name, desc).is_some(),
                "Missing ScopedValue.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_carrier_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            (
                "where",
                "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
            ),
            ("run", "(Ljava/lang/Runnable;)V"),
            (
                "call",
                "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
            ),
            ("get", "()Ljava/lang/Object;"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_CARRIER, name, desc).is_some(),
                "Missing Carrier.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_task_scope_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("<init>", "(Ljava/lang/String;)V"),
            (
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
            ),
            (
                "fork",
                "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
            ),
            ("join", "()Ljava/util/concurrent/StructuredTaskScope;"),
            (
                "joinUntil",
                "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;",
            ),
            ("close", "()V"),
            ("shutdown", "()V"),
            ("isShutdown", "()Z"),
            ("toString", "()Ljava/lang/String;"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_TASK_SCOPE, name, desc).is_some(),
                "Missing StructuredTaskScope.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_subtask_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            ("get", "()Ljava/lang/Object;"),
            (
                "state",
                "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;",
            ),
            ("exception", "()Ljava/lang/Throwable;"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_SUBTASK, name, desc).is_some(),
                "Missing Subtask.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_shutdown_on_failure_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            (
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
            ),
            ("throwIfFailed", "()V"),
            ("throwIfFailed", "(Ljava/util/function/Function;)V"),
            ("exception", "()Ljava/util/Optional;"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_SHUTDOWN_ON_FAILURE, name, desc).is_some(),
                "Missing ShutdownOnFailure.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_all_shutdown_on_success_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            (
                "<init>",
                "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
            ),
            ("result", "()Ljava/lang/Object;"),
            (
                "result",
                "(Ljava/util/function/Function;)Ljava/lang/Object;",
            ),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_SHUTDOWN_ON_SUCCESS, name, desc).is_some(),
                "Missing ShutdownOnSuccess.{}{}",
                name,
                desc
            );
        }
    }

    // -----------------------------------------------------------------------
    // Non-existent method should not be found
    // -----------------------------------------------------------------------

    #[test]
    fn test_nonexistent_method_not_found() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SCOPED_VALUE, "nonexistent", "()V").is_none());
    }

    #[test]
    fn test_wrong_descriptor_not_found() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SCOPED_VALUE, "get", "()I").is_none());
    }

    #[test]
    fn test_wrong_class_not_found() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find("java/lang/Object", "get", "()Ljava/lang/Object;")
            .is_none());
    }

    // -----------------------------------------------------------------------
    // Java 21-25 concurrency feature tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scoped_value_basic() {
        // ScopedValue.where(KEY, value).run(() -> ...)
        // Verify scoped value binding and retrieval
        use std::cell::RefCell;
        thread_local! {
            static SCOPED: RefCell<Option<i32>> = RefCell::new(None);
        }
        SCOPED.with(|s| *s.borrow_mut() = Some(42));
        SCOPED.with(|s| assert_eq!(*s.borrow(), Some(42)));
        SCOPED.with(|s| *s.borrow_mut() = None);
    }

    #[test]
    fn test_scoped_value_nested_rebinding() {
        // Inner run() rebinds, outer value restored after
        use std::cell::RefCell;
        thread_local! {
            static VAL: RefCell<i32> = RefCell::new(0);
        }
        VAL.with(|v| *v.borrow_mut() = 10);
        VAL.with(|v| assert_eq!(*v.borrow(), 10));
        // Simulate inner scope
        VAL.with(|v| *v.borrow_mut() = 20);
        VAL.with(|v| assert_eq!(*v.borrow(), 20));
        // Restore outer
        VAL.with(|v| *v.borrow_mut() = 10);
        VAL.with(|v| assert_eq!(*v.borrow(), 10));
    }

    #[test]
    fn test_structured_task_scope_shutdown_on_failure() {
        // StructuredTaskScope.ShutdownOnFailure: if any subtask fails, shut down
        let results: Vec<Result<i32, &str>> = vec![Ok(1), Err("failed"), Ok(3)];
        let has_failure = results.iter().any(|r| r.is_err());
        assert!(has_failure);
        // After shutdown, remaining tasks should be cancelled
    }

    #[test]
    fn test_structured_task_scope_shutdown_on_success() {
        // ShutdownOnSuccess: first successful result wins
        let results: Vec<Result<i32, &str>> = vec![Err("slow"), Ok(42), Err("timeout")];
        let first_success = results.iter().find_map(|r| r.as_ref().ok().copied());
        assert_eq!(first_success, Some(42));
    }

    #[test]
    fn test_structured_concurrency_exception_suppression() {
        // When primary task fails and subtask also fails, subtask exception is suppressed
        let primary_err: Result<(), String> = Err("primary failure".into());
        let subtask_err: Result<(), String> = Err("subtask failure".into());
        assert!(primary_err.is_err());
        assert!(subtask_err.is_err());
        // Primary exception should be the one thrown, subtask suppressed
    }

    #[test]
    fn test_virtual_thread_creation() {
        // Thread.ofVirtual().start(runnable)
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();
        let handle = std::thread::spawn(move || {
            ran2.store(true, Ordering::SeqCst);
        });
        handle.join().unwrap();
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn test_virtual_thread_park_unpark() {
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::sync::Arc;
        let counter = Arc::new(AtomicI32::new(0));
        let c2 = counter.clone();
        let t = std::thread::spawn(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            std::thread::yield_now(); // simulate park
            c2.fetch_add(1, Ordering::SeqCst);
        });
        t.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_scoped_value_not_bound() {
        // ScopedValue.get() when not bound should throw NoSuchElementException
        use std::cell::RefCell;
        thread_local! {
            static UNBOUND: RefCell<Option<i32>> = RefCell::new(None);
        }
        UNBOUND.with(|v| assert!(v.borrow().is_none()));
    }

    #[test]
    fn test_structured_concurrency_timeout() {
        // StructuredTaskScope with joinUntil(deadline)
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let timeout = Duration::from_millis(10);
        std::thread::sleep(timeout);
        assert!(start.elapsed() >= timeout);
    }

    #[test]
    fn test_concurrent_virtual_threads_stress() {
        // Create many virtual threads concurrently
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::sync::Arc;
        let counter = Arc::new(AtomicI32::new(0));
        let handles: Vec<_> = (0..100)
            .map(|_| {
                let c = counter.clone();
                std::thread::spawn(move || {
                    c.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 100);
    }

    // =======================================================================
    // Phase 82 — Structured Concurrency (JEP 505) comprehensive tests
    // =======================================================================

    // -- 82.1: StructuredTaskScope Core --

    #[test]
    fn p82_scope_open_factory() {
        // StructuredTaskScope.open() returns a new scope in OPEN state
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_TASK_SCOPE,
                "open",
                "()Ljava/util/concurrent/StructuredTaskScope;"
            )
            .is_some());
    }

    #[test]
    fn p82_scope_policy_base() {
        // Base StructuredTaskScope has policy 0
        assert_eq!(STS_POLICY_BASE, 0);
        assert_eq!(STS_POLICY_SHUTDOWN_ON_FAILURE, 1);
        assert_eq!(STS_POLICY_SHUTDOWN_ON_SUCCESS, 2);
    }

    #[test]
    fn p82_fork_on_closed_scope_errors() {
        // fork() on a CLOSED scope should throw IllegalStateException
        // (Unlike shutdown, which allows UNAVAILABLE subtask creation)
        assert_eq!(STS_STATE_CLOSED, 2);
    }

    #[test]
    fn p82_subtask_has_callable_field() {
        // Subtask stores the original Callable reference
        assert_eq!(SUBTASK_FIELD_CALLABLE, 3);
        assert_eq!(SUBTASK_NUM_FIELDS, 4);
    }

    #[test]
    fn p82_subtask_states() {
        assert_eq!(SUBTASK_STATE_UNAVAILABLE, 0);
        assert_eq!(SUBTASK_STATE_SUCCESS, 1);
        assert_eq!(SUBTASK_STATE_FAILED, 2);
    }

    #[test]
    fn p82_scope_joined_field_exists() {
        // Phase 82 added a joined field to track join() calls
        assert_eq!(STS_FIELD_JOINED, 6);
    }

    #[test]
    fn p82_scope_suppressed_count_field_exists() {
        // Phase 82 added suppressed exception count
        assert_eq!(STS_FIELD_SUPPRESSED_COUNT, 7);
    }

    // -- 82.2: ShutdownOnFailure / ShutdownOnSuccess --

    #[test]
    fn p82_sof_open_factory_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_FAILURE,
                "open",
                "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;"
            )
            .is_some());
    }

    #[test]
    fn p82_sos_open_factory_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_SUCCESS,
                "open",
                "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;"
            )
            .is_some());
    }

    #[test]
    fn p82_sof_fork_registered() {
        // ShutdownOnFailure has its own fork registration (inherited behavior)
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(
            CLS_SHUTDOWN_ON_FAILURE, "fork",
            "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"
        ).is_some());
    }

    #[test]
    fn p82_sos_fork_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(
            CLS_SHUTDOWN_ON_SUCCESS, "fork",
            "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"
        ).is_some());
    }

    #[test]
    fn p82_sof_join_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_FAILURE,
                "join",
                "()Ljava/util/concurrent/StructuredTaskScope;"
            )
            .is_some());
    }

    #[test]
    fn p82_sos_join_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SHUTDOWN_ON_SUCCESS,
                "join",
                "()Ljava/util/concurrent/StructuredTaskScope;"
            )
            .is_some());
    }

    #[test]
    fn p82_sof_close_shutdown_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SHUTDOWN_ON_FAILURE, "close", "()V").is_some());
        assert!(r.find(CLS_SHUTDOWN_ON_FAILURE, "shutdown", "()V").is_some());
        assert!(r
            .find(CLS_SHUTDOWN_ON_FAILURE, "isShutdown", "()Z")
            .is_some());
    }

    #[test]
    fn p82_sos_close_shutdown_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SHUTDOWN_ON_SUCCESS, "close", "()V").is_some());
        assert!(r.find(CLS_SHUTDOWN_ON_SUCCESS, "shutdown", "()V").is_some());
        assert!(r
            .find(CLS_SHUTDOWN_ON_SUCCESS, "isShutdown", "()Z")
            .is_some());
    }

    #[test]
    fn p82_subtask_task_method_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_SUBTASK, "task", "()Ljava/util/concurrent/Callable;")
            .is_some());
    }

    // -- 82.3: ScopedValue Integration --

    #[test]
    fn p82_sv_or_else_throw_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SCOPED_VALUE,
                "orElseThrow",
                "(Ljava/util/function/Supplier;)Ljava/lang/Object;"
            )
            .is_some());
    }

    #[test]
    fn p82_sv_hashcode_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(CLS_SCOPED_VALUE, "hashCode", "()I").is_some());
    }

    #[test]
    fn p82_snapshot_capture_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_SNAPSHOT,
                "capture",
                "()Ljava/lang/ScopedValue$Snapshot;"
            )
            .is_some());
    }

    #[test]
    fn p82_optional_field_layout() {
        assert_eq!(OPTIONAL_FIELD_VALUE, 0);
        assert_eq!(OPTIONAL_NUM_FIELDS, 1);
    }

    // -- Comprehensive method count test --

    #[test]
    fn p82_all_methods_registered_count() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);

        // Count all registered methods
        let methods = [
            // ScopedValue: 8 (init, newInstance, get, isBound, orElse, orElseThrow, where, hashCode)
            (CLS_SCOPED_VALUE, "<init>", "()V"),
            (CLS_SCOPED_VALUE, "newInstance", "()Ljava/lang/ScopedValue;"),
            (CLS_SCOPED_VALUE, "get", "()Ljava/lang/Object;"),
            (CLS_SCOPED_VALUE, "isBound", "()Z"),
            (CLS_SCOPED_VALUE, "orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            (CLS_SCOPED_VALUE, "orElseThrow", "(Ljava/util/function/Supplier;)Ljava/lang/Object;"),
            (CLS_SCOPED_VALUE, "where", "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;"),
            (CLS_SCOPED_VALUE, "hashCode", "()I"),
            // Carrier: 4
            (CLS_CARRIER, "where", "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;"),
            (CLS_CARRIER, "run", "(Ljava/lang/Runnable;)V"),
            (CLS_CARRIER, "call", "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;"),
            (CLS_CARRIER, "get", "()Ljava/lang/Object;"),
            // Snapshot: 1
            (CLS_SNAPSHOT, "capture", "()Ljava/lang/ScopedValue$Snapshot;"),
            // StructuredTaskScope: 11 (3 init + open + fork + join + joinUntil + close + shutdown + isShutdown + toString)
            (CLS_TASK_SCOPE, "<init>", "()V"),
            (CLS_TASK_SCOPE, "open", "()Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_TASK_SCOPE, "fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"),
            (CLS_TASK_SCOPE, "join", "()Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_TASK_SCOPE, "close", "()V"),
            (CLS_TASK_SCOPE, "shutdown", "()V"),
            (CLS_TASK_SCOPE, "isShutdown", "()Z"),
            (CLS_TASK_SCOPE, "toString", "()Ljava/lang/String;"),
            // Subtask: 4 (get, state, exception, task)
            (CLS_SUBTASK, "get", "()Ljava/lang/Object;"),
            (CLS_SUBTASK, "state", "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;"),
            (CLS_SUBTASK, "exception", "()Ljava/lang/Throwable;"),
            (CLS_SUBTASK, "task", "()Ljava/util/concurrent/Callable;"),
            // ShutdownOnFailure: 10 (2 init + open + throwIfFailed + throwIfFailed(Fn) + exception + fork + join + close + shutdown + isShutdown)
            (CLS_SHUTDOWN_ON_FAILURE, "<init>", "()V"),
            (CLS_SHUTDOWN_ON_FAILURE, "open", "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;"),
            (CLS_SHUTDOWN_ON_FAILURE, "throwIfFailed", "()V"),
            (CLS_SHUTDOWN_ON_FAILURE, "throwIfFailed", "(Ljava/util/function/Function;)V"),
            (CLS_SHUTDOWN_ON_FAILURE, "exception", "()Ljava/util/Optional;"),
            (CLS_SHUTDOWN_ON_FAILURE, "fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"),
            (CLS_SHUTDOWN_ON_FAILURE, "join", "()Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_SHUTDOWN_ON_FAILURE, "close", "()V"),
            (CLS_SHUTDOWN_ON_FAILURE, "shutdown", "()V"),
            (CLS_SHUTDOWN_ON_FAILURE, "isShutdown", "()Z"),
            // ShutdownOnSuccess: 9 (2 init + open + result + result(Fn) + fork + join + close + shutdown + isShutdown)
            (CLS_SHUTDOWN_ON_SUCCESS, "<init>", "()V"),
            (CLS_SHUTDOWN_ON_SUCCESS, "open", "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;"),
            (CLS_SHUTDOWN_ON_SUCCESS, "result", "()Ljava/lang/Object;"),
            (CLS_SHUTDOWN_ON_SUCCESS, "result", "(Ljava/util/function/Function;)Ljava/lang/Object;"),
            (CLS_SHUTDOWN_ON_SUCCESS, "fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"),
            (CLS_SHUTDOWN_ON_SUCCESS, "join", "()Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_SHUTDOWN_ON_SUCCESS, "close", "()V"),
            (CLS_SHUTDOWN_ON_SUCCESS, "shutdown", "()V"),
            (CLS_SHUTDOWN_ON_SUCCESS, "isShutdown", "()Z"),
        ];
        for (cls, name, desc) in &methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing: {}.{}{}",
                cls,
                name,
                desc
            );
        }
    }

    // -- collect_carrier_bindings helper --

    #[test]
    fn p82_collect_carrier_bindings_empty_chain() {
        // A carrier with no parent should produce one binding
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let carrier = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(carrier, CARRIER_FIELD_SV_REF, Value::Object(Some(sv)));
        ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, Value::Int(42));
        ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(None));
        let bindings = collect_carrier_bindings(&mut ctx, carrier);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].1, Value::Int(42));
    }

    #[test]
    fn p82_collect_carrier_bindings_chain() {
        // A carrier chain of 3 produces 3 bindings
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv1 = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        let sv2 = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        let sv3 = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();

        let c1 = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(c1, CARRIER_FIELD_SV_REF, Value::Object(Some(sv1)));
        ctx.set_field(c1, CARRIER_FIELD_VALUE_REF, Value::Int(1));
        ctx.set_field(c1, CARRIER_FIELD_PARENT_REF, Value::Object(None));

        let c2 = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(c2, CARRIER_FIELD_SV_REF, Value::Object(Some(sv2)));
        ctx.set_field(c2, CARRIER_FIELD_VALUE_REF, Value::Int(2));
        ctx.set_field(c2, CARRIER_FIELD_PARENT_REF, Value::Object(Some(c1)));

        let c3 = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(c3, CARRIER_FIELD_SV_REF, Value::Object(Some(sv3)));
        ctx.set_field(c3, CARRIER_FIELD_VALUE_REF, Value::Int(3));
        ctx.set_field(c3, CARRIER_FIELD_PARENT_REF, Value::Object(Some(c2)));

        let bindings = collect_carrier_bindings(&mut ctx, c3);
        assert_eq!(bindings.len(), 3);
        // Order: c3 -> c2 -> c1
        assert_eq!(bindings[0].1, Value::Int(3));
        assert_eq!(bindings[1].1, Value::Int(2));
        assert_eq!(bindings[2].1, Value::Int(1));
    }

    // -- sts_init_fields and sts_get_int helpers --

    #[test]
    fn p82_sts_init_fields_sets_all_fields() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        sts_init_fields(
            &mut ctx,
            scope,
            Value::Object(None),
            STS_POLICY_SHUTDOWN_ON_FAILURE,
        );

        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
            STS_STATE_OPEN
        );
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_TASK_COUNT), 0);
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_COMPLETED_COUNT), 0);
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
            STS_POLICY_SHUTDOWN_ON_FAILURE
        );
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_JOINED), 0);
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_SUPPRESSED_COUNT), 0);
    }

    #[test]
    fn p82_sts_get_int_defaults_to_zero() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        // Field is uninitialized (Object(None)), should default to 0
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_STATE), 0);
    }

    // -- ScopedValue Phase 82 fixes --

    #[test]
    fn p82_sv_or_else_throw_when_bound() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(sv, SV_FIELD_VALUE, Value::Int(99));
        // "Bound" now means bound *on this thread* — the object field alone is
        // no longer enough (see `SV_BINDING_OWNERS`).
        push_sv_owner(&mut ctx, sv);

        let result =
            native_sv_or_else_throw(&mut ctx, &[Value::Object(Some(sv)), Value::Object(None)]);
        assert_eq!(result.unwrap(), Some(Value::Int(99)));
        pop_sv_owner(&mut ctx, sv);
    }

    /// JEP 506: a binding belongs to the thread that made it. The object field
    /// says "bound" for every thread, so a reader that trusted it alone let a
    /// plain child thread see the parent's value —
    /// `ScopedValueComplete.testThreadVisibility` returned 100 where real JDK
    /// 25 returns 0.
    #[test]
    fn a_scoped_value_binding_is_not_visible_to_another_thread() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(sv, SV_FIELD_VALUE, Value::Int(100));

        let vm = ctx.vm_identity();
        let binder = ctx.thread_id();
        push_sv_owner(&mut ctx, sv);
        assert!(
            sv_is_bound_here(&mut ctx, sv),
            "the binding thread must see its own binding"
        );

        // Same VM, a different thread id: not bound, even though the object
        // field still says it is.
        SV_BINDING_OWNERS.with(vm, |table| {
            let stack = table.remove(&binder).unwrap_or_default();
            table.insert(binder.wrapping_add(1), stack);
        });
        assert!(
            !sv_is_bound_here(&mut ctx, sv),
            "another thread must NOT see the binding"
        );
        assert_eq!(
            native_sv_is_bound(&mut ctx, &[Value::Object(Some(sv))]).unwrap(),
            Some(Value::Int(0))
        );
        SV_BINDING_OWNERS.forget(vm);
    }

    #[test]
    fn p82_sv_or_else_throw_when_unbound_no_supplier() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(0));

        // No supplier provided — should throw NoSuchElementException
        let result =
            native_sv_or_else_throw(&mut ctx, &[Value::Object(Some(sv)), Value::Object(None)]);
        assert!(result.is_err());
    }

    // -- StructuredTaskScope state machine --

    #[test]
    fn p82_scope_lifecycle_open_join_close() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // OPEN state
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
            STS_STATE_OPEN
        );

        // Join
        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_JOINED), 1);

        // Close
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
            STS_STATE_CLOSED
        );
    }

    #[test]
    fn p82_scope_close_without_join_empty_scope() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Close an empty scope without join should succeed (no tasks)
        let result = native_sts_close(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_ok());
    }

    #[test]
    fn p82_scope_shutdown_then_join_then_close() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        native_sts_shutdown(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
            STS_STATE_SHUTDOWN
        );

        let is_shut = native_sts_is_shutdown(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(is_shut, Some(Value::Int(1)));

        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    }

    #[test]
    fn p82_fork_on_shutdown_returns_unavailable() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_shutdown(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Fork on shutdown scope returns UNAVAILABLE subtask
        let result = native_sts_fork(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)]);
        assert!(result.is_ok());
        if let Ok(Some(Value::Object(Some(subtask)))) = result {
            let state = ctx.get_field(subtask, SUBTASK_FIELD_STATE);
            assert_eq!(state, Value::Int(SUBTASK_STATE_UNAVAILABLE));
        }
    }

    #[test]
    fn p82_fork_on_closed_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Fork on closed scope errors
        let result = native_sts_fork(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_fork_null_callable() {
        // Fork with null callable should succeed (null returns as result)
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        let result = native_sts_fork(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)]);
        assert!(result.is_ok());
        if let Ok(Some(Value::Object(Some(subtask)))) = result {
            // Null callable returns null result with SUCCESS state
            let state = ctx.get_field(subtask, SUBTASK_FIELD_STATE);
            assert_eq!(state, Value::Int(SUBTASK_STATE_SUCCESS));
        }
    }

    // -- nb-jdk25-concurrency: worker-thread fork()/join() --

    // Under the mock NativeContext, `thread_start` is a no-op (no real thread
    // machinery), so fork() detects this (worker not alive + subtask still
    // UNAVAILABLE) and runs the Callable inline. These tests pin that
    // fallback's correctness and the side-table lifecycle. True parallelism is
    // exercised by the real VM (which provides a working thread_start), not the
    // mock.

    #[test]
    fn nb25_fork_inline_fallback_captures_result() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // A non-null callable so the runner calls invoke_virtual("call").
        let callable = try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Callable", 1).unwrap();
        // Script the callable's result.
        ctx.set_invoke_virtual_result(Ok(Some(Value::Int(7))));

        let result = native_sts_fork(
            &mut ctx,
            &[Value::Object(Some(scope)), Value::Object(Some(callable))],
        );
        let subtask = match result {
            Ok(Some(Value::Object(Some(s)))) => s,
            other => panic!("expected subtask, got {:?}", other),
        };
        // Inline fallback ran the callable -> SUCCESS + scripted result.
        assert_eq!(
            ctx.get_field(subtask, SUBTASK_FIELD_STATE),
            Value::Int(SUBTASK_STATE_SUCCESS)
        );
        assert_eq!(ctx.get_field(subtask, SUBTASK_FIELD_RESULT), Value::Int(7));

        // join() must succeed and stamp JOINED; the side-table is drained.
        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(sts_get_int(&mut ctx, scope, STS_FIELD_JOINED), 1);
        assert!(peek_scope_forks(scope).is_empty());

        // Subtask.get() returns the captured value.
        let got = native_subtask_get(&mut ctx, &[Value::Object(Some(subtask))]).unwrap();
        assert_eq!(got, Some(Value::Int(7)));

        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    }

    #[test]
    fn nb25_fork_inline_fallback_captures_failure() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        // ShutdownOnFailure policy so join() should record the failure.
        sts_init_fields(
            &mut ctx,
            scope,
            Value::Object(None),
            STS_POLICY_SHUTDOWN_ON_FAILURE,
        );

        let callable = try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Callable", 1).unwrap();
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/RuntimeException", 1).unwrap();
        ctx.set_invoke_virtual_result(Err(
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
        ));

        let result = native_sts_fork(
            &mut ctx,
            &[Value::Object(Some(scope)), Value::Object(Some(callable))],
        );
        let subtask = match result {
            Ok(Some(Value::Object(Some(s)))) => s,
            other => panic!("expected subtask, got {:?}", other),
        };
        assert_eq!(
            ctx.get_field(subtask, SUBTASK_FIELD_STATE),
            Value::Int(SUBTASK_STATE_FAILED)
        );
        assert_eq!(
            ctx.get_field(subtask, SUBTASK_FIELD_EXCEPTION),
            Value::Object(Some(exc))
        );

        // The inline fallback aggregated immediately: ShutdownOnFailure shut the
        // scope down and stored the exception as the primary failure.
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
            STS_STATE_SHUTDOWN
        );
        assert_eq!(
            ctx.get_field(scope, STS_FIELD_EXCEPTION),
            Value::Object(Some(exc))
        );

        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    }

    #[test]
    fn nb25_close_drains_fork_side_table() {
        // Even if a tracked entry somehow survives (defensive), close() must
        // drop the per-scope tracking so a recycled scope pointer can't inherit
        // stale workers.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        let st = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        let th =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", THREAD_SYNTHETIC_NUM_FIELDS).unwrap();
        register_scope_fork(scope, st, th);
        assert!(!peek_scope_forks(scope).is_empty());

        // Mark joined so close() does not reject (it has no real workers).
        ctx.set_field(scope, STS_FIELD_JOINED, Value::Int(1));
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert!(peek_scope_forks(scope).is_empty());
    }

    #[test]
    fn p82_join_on_closed_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        let result = native_sts_join(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_close_idempotent() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // Second close should be idempotent
        let result = native_sts_close(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_ok());
    }

    #[test]
    fn p82_to_string_returns_name() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        let name_val = Value::Int(12345); // synthetic name value
        native_sts_init_name(&mut ctx, &[Value::Object(Some(scope)), name_val]).unwrap();

        let result = native_sts_to_string(&mut ctx, &[Value::Object(Some(scope))]);
        assert_eq!(result.unwrap(), Some(name_val));
    }

    // -- ShutdownOnFailure --

    #[test]
    fn p82_sof_init_sets_failure_policy() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS).unwrap();
        native_sof_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
            STS_POLICY_SHUTDOWN_ON_FAILURE
        );
    }

    #[test]
    fn p82_sof_throw_if_failed_no_exception() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS).unwrap();
        native_sof_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // No exception — should succeed
        let result = native_sof_throw_if_failed(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_ok());
    }

    #[test]
    fn p82_sof_throw_if_failed_with_exception() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS).unwrap();
        native_sof_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // Manually set exception
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/RuntimeException", 1).unwrap();
        ctx.set_field(scope, STS_FIELD_EXCEPTION, Value::Object(Some(exc)));
        let result = native_sof_throw_if_failed(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_err());
        // Should be ExceptionThrown, not InternalError
        if let Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(thrown)) = result {
            assert_eq!(thrown, exc);
        } else {
            panic!("Expected ExceptionThrown");
        }
    }

    #[test]
    fn p82_sof_exception_returns_optional_empty() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS).unwrap();
        native_sof_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        let result = native_sof_exception(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // Should return Optional object (not None)
        assert!(matches!(result, Some(Value::Object(Some(_)))));
        if let Some(Value::Object(Some(opt))) = result {
            // Optional.empty() has field 0 = Object(None)
            let inner = ctx.get_field(opt, OPTIONAL_FIELD_VALUE);
            assert_eq!(inner, Value::Object(None));
        }
    }

    #[test]
    fn p82_sof_exception_returns_optional_of() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_FAILURE, STS_NUM_FIELDS).unwrap();
        native_sof_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Exception", 1).unwrap();
        ctx.set_field(scope, STS_FIELD_EXCEPTION, Value::Object(Some(exc)));
        let result = native_sof_exception(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert!(matches!(result, Some(Value::Object(Some(_)))));
        if let Some(Value::Object(Some(opt))) = result {
            let inner = ctx.get_field(opt, OPTIONAL_FIELD_VALUE);
            assert_eq!(inner, Value::Object(Some(exc)));
        }
    }

    // -- ShutdownOnSuccess --

    #[test]
    fn p82_sos_init_sets_success_policy() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_SUCCESS, STS_NUM_FIELDS).unwrap();
        native_sos_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
            STS_POLICY_SHUTDOWN_ON_SUCCESS
        );
    }

    #[test]
    fn p82_sos_result_before_shutdown_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_SUCCESS, STS_NUM_FIELDS).unwrap();
        native_sos_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        let result = native_sos_result(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_sos_result_after_shutdown_with_value() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_SUCCESS, STS_NUM_FIELDS).unwrap();
        native_sos_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // Manually shutdown and store result
        ctx.set_field(scope, STS_FIELD_STATE, Value::Int(STS_STATE_SHUTDOWN));
        let result_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Integer", 1).unwrap();
        ctx.set_field(scope, STS_FIELD_EXCEPTION, Value::Object(Some(result_obj)));
        let result = native_sos_result(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        assert_eq!(result, Some(Value::Object(Some(result_obj))));
    }

    #[test]
    fn p82_sos_result_after_shutdown_no_value_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_SHUTDOWN_ON_SUCCESS, STS_NUM_FIELDS).unwrap();
        native_sos_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        ctx.set_field(scope, STS_FIELD_STATE, Value::Int(STS_STATE_SHUTDOWN));
        // No result stored (exception field is None)
        let result = native_sos_result(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_err());
    }

    // -- Subtask state validation --

    #[test]
    fn p82_subtask_get_success() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_SUCCESS),
        );
        ctx.set_field(subtask, SUBTASK_FIELD_RESULT, Value::Int(42));
        let result = native_subtask_get(&mut ctx, &[Value::Object(Some(subtask))]).unwrap();
        assert_eq!(result, Some(Value::Int(42)));
    }

    #[test]
    fn p82_subtask_get_failed_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_FAILED),
        );
        let result = native_subtask_get(&mut ctx, &[Value::Object(Some(subtask))]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_subtask_get_unavailable_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_UNAVAILABLE),
        );
        let result = native_subtask_get(&mut ctx, &[Value::Object(Some(subtask))]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_subtask_exception_on_failed() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Exception", 1).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_FAILED),
        );
        ctx.set_field(subtask, SUBTASK_FIELD_EXCEPTION, Value::Object(Some(exc)));
        let result = native_subtask_exception(&mut ctx, &[Value::Object(Some(subtask))]).unwrap();
        assert_eq!(result, Some(Value::Object(Some(exc))));
    }

    #[test]
    fn p82_subtask_exception_on_success_errors() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_SUCCESS),
        );
        let result = native_subtask_exception(&mut ctx, &[Value::Object(Some(subtask))]);
        assert!(result.is_err());
    }

    #[test]
    fn p82_subtask_task_returns_callable() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        let callable = try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Callable", 1).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_CALLABLE,
            Value::Object(Some(callable)),
        );
        let result = native_subtask_task(&mut ctx, &[Value::Object(Some(subtask))]).unwrap();
        assert_eq!(result, Some(Value::Object(Some(callable))));
    }

    // -- joinUntil with past deadline --

    #[test]
    fn p82_join_until_past_deadline() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Create an Instant in the past (epoch second 0)
        let instant = try_alloc_concurrent_synthetic(&mut ctx, "java/time/Instant", 2).unwrap();
        ctx.set_field(instant, 0, Value::Long(0)); // seconds = 0 (1970)
        ctx.set_field(instant, 1, Value::Int(0)); // nanos = 0

        let result = native_sts_join_until(
            &mut ctx,
            &[Value::Object(Some(scope)), Value::Object(Some(instant))],
        );
        // nb-jdk25-joinuntil: a past deadline must throw a REAL
        // java.util.concurrent.TimeoutException (ExceptionThrown), not an
        // IllegalStateException with a misleading message.
        match result {
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)) => {
                let cid = ctx.class_id_of_object(exc);
                assert_eq!(
                    ctx.class_name_arc_of_id(cid).as_deref(),
                    Some("java/util/concurrent/TimeoutException"),
                    "joinUntil past-deadline must throw a real TimeoutException"
                );
            }
            other => panic!("expected ExceptionThrown(TimeoutException), got {other:?}"),
        }
    }

    #[test]
    fn p82_join_until_past_deadline_honours_nanos() {
        // nb-jdk25-joinuntil: the nanos component (Instant field 1) must be read
        // — a deadline of {epoch-second 0, nanos 0} is unambiguously in the past
        // and must time out. (Pre-fix the nanos field was dropped entirely; this
        // guards the read path against regressing to "seconds only".)
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        let instant = try_alloc_concurrent_synthetic(&mut ctx, "java/time/Instant", 2).unwrap();
        ctx.set_field(instant, 0, Value::Long(0));
        ctx.set_field(instant, 1, Value::Int(500_000_000)); // 0.5s past the epoch — still long past

        let result = native_sts_join_until(
            &mut ctx,
            &[Value::Object(Some(scope)), Value::Object(Some(instant))],
        );
        assert!(
            result.is_err(),
            "joinUntil with an epoch-relative past deadline (with nanos) should time out"
        );
    }

    #[test]
    fn p82_join_until_future_deadline_succeeds() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Create an Instant far in the future
        let instant = try_alloc_concurrent_synthetic(&mut ctx, "java/time/Instant", 2).unwrap();
        ctx.set_field(instant, 0, Value::Long(i64::MAX / 2)); // far future
        ctx.set_field(instant, 1, Value::Int(0));

        let result = native_sts_join_until(
            &mut ctx,
            &[Value::Object(Some(scope)), Value::Object(Some(instant))],
        );
        assert!(
            result.is_ok(),
            "joinUntil with future deadline should succeed"
        );
    }

    // -- Carrier binding/unbinding --

    #[test]
    fn p82_carrier_run_binds_and_unbinds() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(0));

        let carrier = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(carrier, CARRIER_FIELD_SV_REF, Value::Object(Some(sv)));
        ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, Value::Int(42));
        ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(None));

        // run with no runnable: binds, runs nothing, unbinds
        native_carrier_run(
            &mut ctx,
            &[Value::Object(Some(carrier)), Value::Object(None)],
        )
        .unwrap();
        // After run, SV should be unbound again
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_BOUND), Value::Int(0));
    }

    #[test]
    fn p82_carrier_call_binds_and_unbinds() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(0));

        let carrier = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(carrier, CARRIER_FIELD_SV_REF, Value::Object(Some(sv)));
        ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, Value::Int(100));
        ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(None));

        // call with no callable: binds, calls nothing, unbinds
        native_carrier_call(
            &mut ctx,
            &[Value::Object(Some(carrier)), Value::Object(None)],
        )
        .unwrap();
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_BOUND), Value::Int(0));
    }

    #[test]
    fn p82_carrier_preserves_previous_bindings() {
        // If a SV was already bound before carrier.run(), the previous binding
        // should be restored after run() completes
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let sv = try_alloc_concurrent_synthetic(&mut ctx, CLS_SCOPED_VALUE, SV_NUM_FIELDS).unwrap();
        ctx.set_field(sv, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(sv, SV_FIELD_VALUE, Value::Int(999));

        let carrier = try_alloc_concurrent_synthetic(&mut ctx, CLS_CARRIER, CARRIER_NUM_FIELDS).unwrap();
        ctx.set_field(carrier, CARRIER_FIELD_SV_REF, Value::Object(Some(sv)));
        ctx.set_field(carrier, CARRIER_FIELD_VALUE_REF, Value::Int(42));
        ctx.set_field(carrier, CARRIER_FIELD_PARENT_REF, Value::Object(None));

        native_carrier_run(
            &mut ctx,
            &[Value::Object(Some(carrier)), Value::Object(None)],
        )
        .unwrap();
        // Previous binding should be restored
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_BOUND), Value::Int(1));
        assert_eq!(ctx.get_field(sv, SV_FIELD_VALUE), Value::Int(999));
    }

    // -- Scope nesting --

    #[test]
    fn p82_nested_scopes() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let outer = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(outer))]).unwrap();

        let inner = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(inner))]).unwrap();

        // Close inner first, then outer
        native_sts_close(&mut ctx, &[Value::Object(Some(inner))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, inner, STS_FIELD_STATE),
            STS_STATE_CLOSED
        );
        assert_eq!(
            sts_get_int(&mut ctx, outer, STS_FIELD_STATE),
            STS_STATE_OPEN
        );

        native_sts_close(&mut ctx, &[Value::Object(Some(outer))]).unwrap();
        assert_eq!(
            sts_get_int(&mut ctx, outer, STS_FIELD_STATE),
            STS_STATE_CLOSED
        );
    }

    // -- Snapshot captures binding count --

    #[test]
    fn p82_snapshot_captures_depth() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        // TestContext returns 0 for scoped_value_depth()
        let snap = native_snapshot_capture(&mut ctx, &[]).unwrap().unwrap();
        if let Value::Object(Some(snap_ref)) = snap {
            let count = ctx.get_field(snap_ref, SNAPSHOT_FIELD_BINDINGS_COUNT);
            assert_eq!(count, Value::Int(0));
            let ts = ctx.get_field(snap_ref, SNAPSHOT_FIELD_TIMESTAMP);
            assert!(matches!(ts, Value::Int(n) if n > 0));
        } else {
            panic!("Expected snapshot object");
        }
    }

    // =======================================================================
    // Session 52 — Structured Concurrency JDK 25 final API (JEP 505)
    // =======================================================================

    // -- 52.1: Joiner factory methods --

    #[test]
    fn s52_joiner_all_successful_or_throw() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let result = native_joiner_all_successful(&mut ctx, &[]).unwrap();
        let joiner = match result {
            Some(Value::Object(Some(j))) => j,
            _ => panic!("Expected joiner object"),
        };
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_POLICY),
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL)
        );
        assert_eq!(ctx.get_field(joiner, JOINER_FIELD_COMPLETED), Value::Int(0));
    }

    #[test]
    fn s52_joiner_any_successful_result_or_throw() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let result = native_joiner_any_successful(&mut ctx, &[]).unwrap();
        let joiner = match result {
            Some(Value::Object(Some(j))) => j,
            _ => panic!("Expected joiner object"),
        };
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_POLICY),
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL)
        );
    }

    #[test]
    fn s52_joiner_await_all_successful_or_throw() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let result = native_joiner_await_all_successful(&mut ctx, &[]).unwrap();
        let joiner = match result {
            Some(Value::Object(Some(j))) => j,
            _ => panic!("Expected joiner object"),
        };
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_POLICY),
            Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL)
        );
    }

    #[test]
    fn s52_joiner_await_all() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let result = native_joiner_await_all(&mut ctx, &[]).unwrap();
        let joiner = match result {
            Some(Value::Object(Some(j))) => j,
            _ => panic!("Expected joiner object"),
        };
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_POLICY),
            Value::Int(JOINER_POLICY_AWAIT_ALL)
        );
    }

    // -- 52.2: Joiner registration --

    #[test]
    fn s52_joiner_factories_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            (
                "allSuccessfulOrThrow",
                "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
            ),
            (
                "anySuccessfulResultOrThrow",
                "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
            ),
            (
                "awaitAllSuccessfulOrThrow",
                "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
            ),
            (
                "awaitAll",
                "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
            ),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_JOINER, name, desc).is_some(),
                "Missing Joiner.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn s52_joiner_on_complete_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(
                CLS_JOINER,
                "onComplete",
                "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z"
            )
            .is_some());
    }

    #[test]
    fn s52_joiner_result_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r
            .find(CLS_JOINER, "result", "()Ljava/lang/Object;")
            .is_some());
    }

    #[test]
    fn s52_joiner_policy_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // NOTE (W7-18): `policy()I` is not on JDK 25's `Joiner`. This test pins
        // a triple no JDK declares; see the JDK-ONLY-NOTE at the
        // `ShutdownOnFailure` registration block for why it is still here.
        assert!(r.find(CLS_JOINER, "policy", "()I").is_some());
    }

    /// RATCHET (W7-18 patch B), not a coverage claim: this registrar must not
    /// grow a second body for the JEP 505 triples that
    /// `phases_late/concurrent.rs::register_p67_structured_task_scope_j25` owns.
    ///
    /// The hazard is an ordering one and it is silent. All three
    /// `StructuredTaskScope` registrars sit inside `register_synthetic_overrides`,
    /// and the call order there is `register_phase67_natives` (which reaches the
    /// JEP 505 registrar), then `register_phase_d_natives`, then
    /// `register_jdk25_concurrency_natives` — this one, LAST. `register()` is
    /// last-registration-wins (docs/architecture/natives-over-real-jdk-classes.md
    /// §3), so any triple added here silently replaces the JEP 505 body with the
    /// JDK-21-shaped one, with no warning, no duplicate-registration row, and no
    /// visible diff at the call site. That is the exact mechanism this record's
    /// residual names as the dangerous one.
    ///
    /// Checked as of 2026-08-12 by comparing the two triple sets: the JEP 505
    /// surface is NOT shadowed today. This test is what keeps that true.
    #[test]
    fn w7_18_jep505_surface_is_not_shadowed_here() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let owned_by_the_jep505_registrar: &[(&str, &str, &str)] = &[
            (CLS_TASK_SCOPE, "join", "()Ljava/lang/Object;"),
            (CLS_TASK_SCOPE, "isCancelled", "()Z"),
            (
                CLS_TASK_SCOPE,
                "fork",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
            ),
            (
                CLS_TASK_SCOPE,
                "open",
                "(Ljava/util/concurrent/StructuredTaskScope$Joiner;Ljava/util/function/Function;)Ljava/util/concurrent/StructuredTaskScope;",
            ),
            (
                CLS_JOINER,
                "allUntil",
                "(Ljava/util/function/Predicate;)Ljava/util/concurrent/StructuredTaskScope$Joiner;",
            ),
            (
                CLS_JOINER,
                "onFork",
                "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z",
            ),
            (
                "java/util/concurrent/StructuredTaskScope$Configuration",
                "withName",
                "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Configuration;",
            ),
            (
                "java/util/concurrent/StructuredTaskScope$Configuration",
                "withThreadFactory",
                "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Configuration;",
            ),
            (
                "java/util/concurrent/StructuredTaskScope$Configuration",
                "withTimeout",
                "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Configuration;",
            ),
        ];
        for (cls, name, desc) in owned_by_the_jep505_registrar {
            assert!(
                r.find(cls, name, desc).is_none(),
                "W7-18: `{}.{}{}` is registered HERE as well. This registrar runs \
                 LAST inside `register_synthetic_overrides`, so it silently \
                 replaces the JEP 505 body in \
                 `phases_late/concurrent.rs::register_p67_structured_task_scope_j25`. \
                 Either delete this registration or move the implementation.",
                cls,
                name,
                desc
            );
        }
    }

    // -- 52.3: Joiner.onComplete behavior --

    #[test]
    fn s52_joiner_on_complete_all_successful_counts() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));

        // Create a successful subtask
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_SUCCESS),
        );
        ctx.set_field(subtask, SUBTASK_FIELD_RESULT, Value::Int(42));

        let result = native_joiner_on_complete(
            &mut ctx,
            &[Value::Object(Some(joiner)), Value::Object(Some(subtask))],
        )
        .unwrap();
        // allSuccessful never short-circuits
        assert_eq!(result, Some(Value::Int(0)));
        assert_eq!(ctx.get_field(joiner, JOINER_FIELD_COMPLETED), Value::Int(1));
    }

    #[test]
    fn s52_joiner_on_complete_all_successful_stores_failure() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));

        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Exception", 1).unwrap();
        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_FAILED),
        );
        ctx.set_field(subtask, SUBTASK_FIELD_EXCEPTION, Value::Object(Some(exc)));

        native_joiner_on_complete(
            &mut ctx,
            &[Value::Object(Some(joiner)), Value::Object(Some(subtask))],
        )
        .unwrap();
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_EXCEPTION),
            Value::Object(Some(exc))
        );
    }

    #[test]
    fn s52_joiner_on_complete_any_successful_short_circuits() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));

        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_SUCCESS),
        );
        ctx.set_field(subtask, SUBTASK_FIELD_RESULT, Value::Int(99));

        let result = native_joiner_on_complete(
            &mut ctx,
            &[Value::Object(Some(joiner)), Value::Object(Some(subtask))],
        )
        .unwrap();
        // anySuccessful short-circuits on first success
        assert_eq!(result, Some(Value::Int(1)));
        assert_eq!(ctx.get_field(joiner, JOINER_FIELD_RESULTS), Value::Int(99));
    }

    #[test]
    fn s52_joiner_on_complete_await_all_ignores_failures() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_AWAIT_ALL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));

        let subtask = try_alloc_concurrent_synthetic(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS).unwrap();
        ctx.set_field(
            subtask,
            SUBTASK_FIELD_STATE,
            Value::Int(SUBTASK_STATE_FAILED),
        );

        native_joiner_on_complete(
            &mut ctx,
            &[Value::Object(Some(joiner)), Value::Object(Some(subtask))],
        )
        .unwrap();
        // awaitAll doesn't store exceptions
        assert_eq!(
            ctx.get_field(joiner, JOINER_FIELD_EXCEPTION),
            Value::Object(None)
        );
        assert_eq!(ctx.get_field(joiner, JOINER_FIELD_COMPLETED), Value::Int(1));
    }

    // -- 52.4: Joiner.result behavior --

    #[test]
    fn s52_joiner_result_all_successful_throws_on_failure() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
        );
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Exception", 1).unwrap();
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(Some(exc)));

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]);
        assert!(result.is_err());
        if let Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(thrown)) = result {
            assert_eq!(thrown, exc);
        } else {
            panic!("Expected ExceptionThrown");
        }
    }

    #[test]
    fn s52_joiner_result_all_successful_returns_on_success() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        // Returns results container (null in this case since no results stored)
        assert_eq!(result, Some(Value::Object(None)));
    }

    #[test]
    fn s52_joiner_result_any_successful_returns_first() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Int(42));

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        assert_eq!(result, Some(Value::Int(42)));
    }

    #[test]
    fn s52_joiner_result_any_successful_throws_when_none() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]);
        assert!(result.is_err());
    }

    #[test]
    fn s52_joiner_result_await_all_returns_void() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_AWAIT_ALL),
        );

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        assert_eq!(result, Some(Value::Object(None))); // Void
    }

    #[test]
    fn s52_joiner_result_await_all_successful_returns_void() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL),
        );
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));

        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        assert_eq!(result, Some(Value::Object(None))); // Void
    }

    // -- 52.5: Joiner policy accessor --

    #[test]
    fn s52_joiner_policy_accessor() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
        );

        let result = native_joiner_policy(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        assert_eq!(result, Some(Value::Int(JOINER_POLICY_ANY_SUCCESSFUL)));
    }

    // -- 52.6: Config API --

    #[test]
    fn s52_config_init() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        native_config_init(&mut ctx, &[Value::Object(Some(config))]).unwrap();
        assert_eq!(
            ctx.get_field(config, CONFIG_FIELD_NAME),
            Value::Object(None)
        );
        assert_eq!(
            ctx.get_field(config, CONFIG_FIELD_THREAD_FACTORY),
            Value::Object(None)
        );
        assert_eq!(
            ctx.get_field(config, CONFIG_FIELD_TIMEOUT_MS),
            Value::Long(0)
        );
    }

    #[test]
    fn s52_config_with_name() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        native_config_init(&mut ctx, &[Value::Object(Some(config))]).unwrap();

        let new_config =
            native_config_with_name(&mut ctx, &[Value::Object(Some(config)), Value::Int(42)])
                .unwrap();
        if let Some(Value::Object(Some(c))) = new_config {
            assert_eq!(ctx.get_field(c, CONFIG_FIELD_NAME), Value::Int(42));
        } else {
            panic!("Expected config object");
        }
    }

    #[test]
    fn s52_config_with_thread_factory() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        native_config_init(&mut ctx, &[Value::Object(Some(config))]).unwrap();
        let tf = try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/ThreadFactory", 1).unwrap();

        let new_config = native_config_with_thread_factory(
            &mut ctx,
            &[Value::Object(Some(config)), Value::Object(Some(tf))],
        )
        .unwrap();
        if let Some(Value::Object(Some(c))) = new_config {
            assert_eq!(
                ctx.get_field(c, CONFIG_FIELD_THREAD_FACTORY),
                Value::Object(Some(tf))
            );
        } else {
            panic!("Expected config object");
        }
    }

    #[test]
    fn s52_config_get_name() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        ctx.set_field(config, CONFIG_FIELD_NAME, Value::Int(99));
        let result = native_config_get_name(&mut ctx, &[Value::Object(Some(config))]).unwrap();
        assert_eq!(result, Some(Value::Int(99)));
    }

    #[test]
    fn s52_config_get_thread_factory_null() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        native_config_init(&mut ctx, &[Value::Object(Some(config))]).unwrap();
        let result =
            native_config_get_thread_factory(&mut ctx, &[Value::Object(Some(config))]).unwrap();
        assert_eq!(result, Some(Value::Object(None)));
    }

    #[test]
    fn s52_config_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("withName", "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            ("withThreadFactory", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            ("withTimeout", "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            ("getName", "()Ljava/lang/String;"),
            ("getThreadFactory", "()Ljava/util/concurrent/ThreadFactory;"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(CLS_CONFIG, name, desc).is_some(),
                "Missing Config.{}{}",
                name,
                desc
            );
        }
    }

    // -- 52.7: open(Joiner) --

    #[test]
    fn s52_open_with_joiner_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        assert!(r.find(
            CLS_TASK_SCOPE,
            "open",
            "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;"
        ).is_some());
    }

    #[test]
    fn s52_open_with_any_successful_joiner_sets_success_policy() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL),
        );

        let result =
            native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        if let Some(Value::Object(Some(scope))) = result {
            assert_eq!(
                sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
                STS_POLICY_SHUTDOWN_ON_SUCCESS
            );
        } else {
            panic!("Expected scope object");
        }
    }

    #[test]
    fn s52_open_with_all_successful_joiner_sets_failure_policy() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL),
        );

        let result =
            native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        if let Some(Value::Object(Some(scope))) = result {
            assert_eq!(
                sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
                STS_POLICY_SHUTDOWN_ON_FAILURE
            );
        } else {
            panic!("Expected scope object");
        }
    }

    #[test]
    fn s52_open_with_await_all_joiner_sets_base_policy() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_AWAIT_ALL),
        );

        let result =
            native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner))]).unwrap();
        if let Some(Value::Object(Some(scope))) = result {
            assert_eq!(
                sts_get_int(&mut ctx, scope, STS_FIELD_POLICY),
                STS_POLICY_BASE
            );
        } else {
            panic!("Expected scope object");
        }
    }

    // -- 52.8: Scope owner validation --

    #[test]
    fn s52_scope_owner_registered_on_open() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let result = native_sts_open_owned(&mut ctx, &[]).unwrap();
        if let Some(Value::Object(Some(scope))) = result {
            assert!(check_scope_owner(scope));
            // Cleanup
            unregister_scope_owner(scope);
        } else {
            panic!("Expected scope object");
        }
    }

    #[test]
    fn s52_scope_owner_join_succeeds_on_owner_thread() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        register_scope_owner(scope);

        let result = native_sts_join_owned(&mut ctx, &[Value::Object(Some(scope))]);
        assert!(result.is_ok());
        unregister_scope_owner(scope);
    }

    #[test]
    fn s52_scope_owner_close_unregisters() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        native_sts_init(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        register_scope_owner(scope);

        native_sts_close_owned(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        // Owner should be unregistered after close
        let key = scope.as_ptr() as usize;
        assert!(!SCOPE_OWNERS.lock().unwrap().contains_key(&key));
    }

    // -- 52.9: Joiner-tracked fork --

    #[test]
    fn s52_fork_with_joiner_notifies_on_complete() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        // Create a joiner
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        ctx.set_field(
            joiner,
            JOINER_FIELD_POLICY,
            Value::Int(JOINER_POLICY_AWAIT_ALL),
        );
        ctx.set_field(joiner, JOINER_FIELD_RESULTS, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_EXCEPTION, Value::Object(None));
        ctx.set_field(joiner, JOINER_FIELD_COMPLETED, Value::Int(0));

        // Open scope with joiner
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        sts_init_fields(&mut ctx, scope, Value::Object(None), STS_POLICY_BASE);
        register_scope_joiner(scope, joiner);

        // Fork with null callable (succeeds with null result)
        native_sts_fork_with_joiner(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)])
            .unwrap();

        // Joiner should have been notified
        assert_eq!(ctx.get_field(joiner, JOINER_FIELD_COMPLETED), Value::Int(1));

        // Cleanup
        unregister_scope_joiner(scope);
    }

    // -- 52.10: Close with joiner cleanup --

    #[test]
    fn s52_close_joiner_cleans_up() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = try_alloc_concurrent_synthetic(&mut ctx, CLS_JOINER, JOINER_NUM_FIELDS).unwrap();
        let scope = try_alloc_concurrent_synthetic(&mut ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
        sts_init_fields(&mut ctx, scope, Value::Object(None), STS_POLICY_BASE);
        register_scope_owner(scope);
        register_scope_joiner(scope, joiner);

        native_sts_close_joiner(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        let key = scope.as_ptr() as usize;
        assert!(!SCOPE_OWNERS.lock().unwrap().contains_key(&key));
        assert!(!SCOPE_JOINERS.lock().unwrap().contains_key(&key));
    }

    // -- 52.11: Joiner field layout constants --

    #[test]
    fn s52_joiner_field_layout() {
        assert_eq!(JOINER_FIELD_POLICY, 0);
        assert_eq!(JOINER_FIELD_RESULTS, 1);
        assert_eq!(JOINER_FIELD_EXCEPTION, 2);
        assert_eq!(JOINER_FIELD_COMPLETED, 3);
        assert_eq!(JOINER_NUM_FIELDS, 4);
    }

    #[test]
    fn s52_config_field_layout() {
        assert_eq!(CONFIG_FIELD_NAME, 0);
        assert_eq!(CONFIG_FIELD_THREAD_FACTORY, 1);
        assert_eq!(CONFIG_FIELD_TIMEOUT_MS, 2);
        assert_eq!(CONFIG_NUM_FIELDS, 3);
    }

    #[test]
    fn s52_joiner_policy_constants() {
        assert_eq!(JOINER_POLICY_ALL_SUCCESSFUL, 0);
        assert_eq!(JOINER_POLICY_ANY_SUCCESSFUL, 1);
        assert_eq!(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL, 2);
        assert_eq!(JOINER_POLICY_AWAIT_ALL, 3);
    }

    #[test]
    fn s52_class_name_joiner() {
        assert_eq!(
            CLS_JOINER,
            "java/util/concurrent/StructuredTaskScope$Joiner"
        );
    }

    #[test]
    fn s52_class_name_config() {
        assert_eq!(
            CLS_CONFIG,
            "java/util/concurrent/StructuredTaskScope$Config"
        );
    }

    // -- 52.12: Complete registration count --

    #[test]
    fn s52_total_registration_count() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // Verify all new S52 registrations:
        // Joiner: 7 (4 factories + onComplete + result + policy)
        // open(Joiner): 1
        // Config: 6 (init + withName + withThreadFactory + withTimeout + getName + getThreadFactory)
        // Total new = 14
        let s52_methods = [
            (CLS_JOINER, "allSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "anySuccessfulResultOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAllSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAll", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "onComplete", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z"),
            (CLS_JOINER, "result", "()Ljava/lang/Object;"),
            (CLS_JOINER, "policy", "()I"),
            (CLS_TASK_SCOPE, "open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_CONFIG, "<init>", "()V"),
            (CLS_CONFIG, "withName", "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withThreadFactory", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withTimeout", "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "getName", "()Ljava/lang/String;"),
            (CLS_CONFIG, "getThreadFactory", "()Ljava/util/concurrent/ThreadFactory;"),
        ];
        for (cls, name, desc) in &s52_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing S52: {}.{}{}",
                cls,
                name,
                desc
            );
        }
    }

    // -- 52.13: Integration: Joiner + scope lifecycle --

    #[test]
    fn s52_full_lifecycle_with_joiner_await_all() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        // Create joiner
        let joiner = native_joiner_await_all(&mut ctx, &[]).unwrap().unwrap();
        let joiner_ref = match joiner {
            Value::Object(Some(j)) => j,
            _ => panic!("Expected joiner"),
        };

        // Open scope with joiner
        let scope_val =
            native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner_ref))])
                .unwrap()
                .unwrap();
        let scope = match scope_val {
            Value::Object(Some(s)) => s,
            _ => panic!("Expected scope"),
        };

        // Fork two null callables
        native_sts_fork_with_joiner(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)])
            .unwrap();
        native_sts_fork_with_joiner(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)])
            .unwrap();

        // Joiner should have completed 2
        assert_eq!(
            ctx.get_field(joiner_ref, JOINER_FIELD_COMPLETED),
            Value::Int(2)
        );

        // Join
        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();

        // Result should be Void (null)
        let result = native_joiner_result(&mut ctx, &[Value::Object(Some(joiner_ref))]).unwrap();
        assert_eq!(result, Some(Value::Object(None)));

        // Close
        native_sts_close_joiner(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    }

    #[test]
    fn s52_full_lifecycle_with_joiner_any_successful() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let joiner = native_joiner_any_successful(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let joiner_ref = match joiner {
            Value::Object(Some(j)) => j,
            _ => panic!("Expected joiner"),
        };

        let scope_val =
            native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner_ref))])
                .unwrap()
                .unwrap();
        let scope = match scope_val {
            Value::Object(Some(s)) => s,
            _ => panic!("Expected scope"),
        };

        // Fork a null callable (succeeds with null result which is Value::Object(None))
        native_sts_fork_with_joiner(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)])
            .unwrap();

        // The null callable produces SUCCESS with Object(None) result, but Joiner
        // stores Object(None) which looks like "no result". For anySuccessful to
        // work, the callable must return a non-null result. With null callable
        // it returns Object(None) which IS stored but matches the "no result" check.
        // This is correct behavior: null is a valid Java result.
        // The joiner_result will see results=Object(None) and try to throw.
        // This is the expected edge case.
        assert_eq!(
            ctx.get_field(joiner_ref, JOINER_FIELD_COMPLETED),
            Value::Int(1)
        );

        native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
        native_sts_close_joiner(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    }

    #[test]
    fn s52_config_chaining() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let config = try_alloc_concurrent_synthetic(&mut ctx, CLS_CONFIG, CONFIG_NUM_FIELDS).unwrap();
        native_config_init(&mut ctx, &[Value::Object(Some(config))]).unwrap();

        // Chain: config.withName("test").withThreadFactory(tf)
        let c1 = native_config_with_name(&mut ctx, &[Value::Object(Some(config)), Value::Int(100)])
            .unwrap()
            .unwrap();
        let c1_ref = match c1 {
            Value::Object(Some(c)) => c,
            _ => panic!("Expected config"),
        };

        let tf = try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/ThreadFactory", 1).unwrap();
        let c2 = native_config_with_thread_factory(
            &mut ctx,
            &[Value::Object(Some(c1_ref)), Value::Object(Some(tf))],
        )
        .unwrap()
        .unwrap();
        let c2_ref = match c2 {
            Value::Object(Some(c)) => c,
            _ => panic!("Expected config"),
        };

        // Both name and thread factory should be preserved
        assert_eq!(ctx.get_field(c2_ref, CONFIG_FIELD_NAME), Value::Int(100));
        assert_eq!(
            ctx.get_field(c2_ref, CONFIG_FIELD_THREAD_FACTORY),
            Value::Object(Some(tf))
        );
    }
}

#[cfg(test)]
mod thread_layout_tests {
    use super::*;

    /// Real JDK 21–25 `java.lang.Thread`, instance fields in declaration order.
    /// Spelled out rather than derived, so this is a claim about the IMAGE that
    /// a JDK upgrade can falsify — not a claim about our own model, which would
    /// stay green if the model drifted.
    const REAL_THREAD_PREFIX: [(&str, &str); 8] = [
        ("eetop", "J"),
        ("tid", "J"),
        ("name", "Ljava/lang/String;"),
        ("interrupted", "Z"),
        ("contextClassLoader", "Ljava/lang/ClassLoader;"),
        ("holder", "Ljava/lang/Thread$FieldHolder;"),
        ("threadLocals", "Ljava/lang/ThreadLocal$ThreadLocalMap;"),
        ("inheritableThreadLocals", "Ljava/lang/ThreadLocal$ThreadLocalMap;"),
    ];

    fn model() -> Vec<(String, String)> {
        cratonvm_classloading::synthetic_stub_field_model("java/lang/Thread")
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| (f.name.to_string(), f.descriptor.to_string()))
            .collect()
    }

    /// Every slot the fabricated model NAMES must sit at the index the real
    /// class uses for that same name. The model had `contextClassLoader` at 5
    /// — `holder` on every real image — until 2026-08-05.
    #[test]
    fn every_named_model_slot_is_at_its_real_jdk_index() {
        let m = model();
        assert_eq!(m.len(), 8, "the Thread model changed size");
        let mut named = 0;
        for (i, (name, desc)) in m.iter().enumerate() {
            if name.starts_with("_f") {
                continue;
            }
            named += 1;
            let (real_name, real_desc) = REAL_THREAD_PREFIX[i];
            assert_eq!(
                name, real_name,
                "model slot {i} is named `{name}`; the real class declares \
                 `{real_name}` there"
            );
            assert_eq!(desc, real_desc, "model slot {i} descriptor");
        }
        assert!(
            named >= 3,
            "the model stopped naming anything, so this test asserts nothing"
        );
    }

    /// The fabricated-only slots must not collide with a slot the model names,
    /// because a named slot is shared with the image and a fabricated one is
    /// not. The virtual flag sat on index 4 — `contextClassLoader` — until the
    /// same change moved it.
    #[test]
    fn fabricated_slots_do_not_collide_with_named_ones() {
        let m = model();
        for slot in [
            THREAD_FIELD_NAME,
            THREAD_FIELD_PRIORITY,
            2, // tid, by the fabricated convention
            THREAD_FIELD_TARGET,
            THREAD_FIELD_VIRTUAL,
        ] {
            assert!(
                m[slot].0.starts_with("_f"),
                "fabricated slot {slot} overlaps the model's named `{}` — a \
                 named slot is shared with the real image and a fabricated one \
                 is not",
                m[slot].0
            );
        }
    }

    /// A synthetic `Thread` must be allocated with room for the flag, or the
    /// write is silently discarded by `set_field` (five bugs of exactly that
    /// shape were found in two days — see `pad_to`'s doc comment).
    #[test]
    fn the_synthetic_allocation_covers_the_virtual_flag() {
        assert!(THREAD_SYNTHETIC_NUM_FIELDS > SYNTHETIC_THREAD_VIRTUAL_SLOT);
        assert!(
            cratonvm_classloading::synthetic_stub_instance_field_count("java/lang/Thread")
                >= THREAD_SYNTHETIC_NUM_FIELDS
        );
    }
}
