// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

use crate::{obj_arg, try_alloc_concurrent_synthetic};

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
// The forked Thread objects and their Subtasks are tracked per-scope in the
// per-VM `STS_TABLE` (as global roots), beside the scope's owner and joiner.

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
// Keyed by the ScopedValue's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`),
// not its address: the key survives a moving collection, so this table needs
// no GC root scan or remap pass.
//
// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
// rank 24, route R1): the stack held identity hashes, so a ScopedValue that
// shared its hash with one bound on this thread was "bound here" too, and
// read whatever value another thread's binding had put in its field. A lock
// key is unique per object and never minted again. No `forget_*_keys` hook:
// a stack entry lives for one binding's extent, during which
// `carrier_bind_and_invoke` pins its ScopedValue, so its key is never freed
// under it -- and a freed key, never re-minted, can match nothing. `push`
// mints (before the table lock); `is_bound` and `pop` never do.
//
// Known limitation, narrower than the bug it replaces: two threads binding the
// SAME `ScopedValue` at the same time still race on the single object field
// pair. Each sees "bound" correctly, but the *value* is whichever binding ran
// last. Fixing that needs per-thread values, which would take the values out
// of the heap-rooted object field and require their own GC scan/remap hooks.
// ---------------------------------------------------------------------------
static SV_BINDING_OWNERS: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<u64, Vec<usize>>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Per-VM teardown for the ScopedValue ownership table. Called from
/// `release_vm_native_state`.
pub fn forget_vm_scoped_value_owners(vm_identity: usize) {
    SV_BINDING_OWNERS.forget(vm_identity);
}

/// Record that the calling thread has just bound `sv`.
fn push_sv_owner(ctx: &mut dyn NativeContext, sv: ObjectRef) {
    // Infallible in practice (`lock_key_for` always answers `Ok`); a refusal
    // would only leave `sv` unbound here, never bound elsewhere.
    let Ok(id) = crate::gc_stable_weak_lock_key(&*ctx, sv) else {
        return;
    };
    let (vm, thread) = (ctx.vm_identity(), ctx.thread_id());
    SV_BINDING_OWNERS.with(vm, |table| table.entry(thread).or_default().push(id));
}

/// Drop the calling thread's most recent binding of `sv`.
fn pop_sv_owner(ctx: &mut dyn NativeContext, sv: ObjectRef) {
    let Some(id) = crate::existing_weak_lock_key(&*ctx, sv) else {
        return;
    };
    let (vm, thread) = (ctx.vm_identity(), ctx.thread_id());
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
    let Some(id) = crate::existing_weak_lock_key(&*ctx, sv) else {
        return false;
    };
    let (vm, thread) = (ctx.vm_identity(), ctx.thread_id());
    SV_BINDING_OWNERS
        .peek(vm, |table| {
            table.get(&thread).is_some_and(|stack| stack.contains(&id))
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
    alloc_carrier(ctx, [sv_val, value_val, Value::Object(None)])
}

/// A new `Carrier` whose `[ScopedValue, value, parent]` slots hold `slots`.
///
/// gc-common w20-g: `where` stored the three through their entry addresses
/// after the carrier's allocation, which can collect (the value is the
/// caller's object, reachable from nothing else here); each is rooted across
/// it.
fn alloc_carrier(ctx: &mut dyn NativeContext, slots: [Value; 3]) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let handles: Vec<Option<cratonvm_native_api::NativeHandle>> = slots
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        })
        .collect();
    let carrier = try_alloc_concurrent_synthetic(&mut *scope, CLS_CARRIER, CARRIER_NUM_FIELDS)?;
    for ((field, value), handle) in [
        CARRIER_FIELD_SV_REF,
        CARRIER_FIELD_VALUE_REF,
        CARRIER_FIELD_PARENT_REF,
    ]
    .into_iter()
    .zip(slots)
    .zip(&handles)
    {
        let current = sts_handle_value(&scope, handle, value);
        scope.set_field(carrier, field, current);
    }
    Ok(Some(Value::Object(Some(carrier))))
}

/// `Carrier.where(ScopedValue, Object)Carrier` — chain another binding.
fn native_carrier_where(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sv_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let value_val = args.get(2).copied().unwrap_or(Value::Object(None));
    alloc_carrier(ctx, [sv_val, value_val, Value::Object(Some(this))])
}

/// Bind every ScopedValue of the carrier chain from `this` on the calling
/// thread, run `invoke`, then restore each one's previous state -- whatever
/// `invoke` answered.
///
/// gc-common w20-g: `run` / `call` are Java and can collect. The restore
/// wrote each ScopedValue's saved state back through its pre-call address,
/// the saved VALUE was a raw copy, and `pop_sv_owner` hashed the stale
/// address -- so after a moving collection the binding was never popped and
/// the ScopedValue stayed bound on this thread for good. Each ScopedValue and
/// each saved object value is pinned across the call and re-read for the
/// restore.
fn carrier_bind_and_invoke(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    invoke: impl FnOnce(&mut dyn NativeContext) -> MethodCallResult,
) -> MethodCallResult {
    let bindings = collect_carrier_bindings(ctx, this);
    // Every pin below goes above this one, so one unpin releases them all.
    let base_pin = ctx.pin_native_root(this);
    // Save previous binding state so we can restore on unbind:
    // (ScopedValue pin, ScopedValue, old bound flag, old value pin, old value).
    type Saved = (usize, ObjectRef, Value, Option<(usize, ObjectRef)>, Value);
    let mut prev_states: Vec<Saved> = Vec::with_capacity(bindings.len());
    for (sv_ref, _) in &bindings {
        let old_bound = ctx.get_field(*sv_ref, SV_FIELD_IS_BOUND);
        let old_value = ctx.get_field(*sv_ref, SV_FIELD_VALUE);
        let sv_pin = ctx.pin_native_root(*sv_ref);
        let value_pin = match old_value {
            Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
            _ => None,
        };
        prev_states.push((sv_pin, *sv_ref, old_bound, value_pin, old_value));
    }
    // Bind all scoped values — on THIS thread (see `SV_BINDING_OWNERS`).
    for (sv_ref, value) in &bindings {
        ctx.set_field(*sv_ref, SV_FIELD_IS_BOUND, Value::Int(1));
        ctx.set_field(*sv_ref, SV_FIELD_VALUE, *value);
        push_sv_owner(ctx, *sv_ref);
    }
    let result = invoke(&mut *ctx);
    // Restore previous scoped value state (unbind), through current addresses.
    for (sv_pin, sv_ref, old_bound, value_pin, old_value) in prev_states {
        let sv_ref = ctx.read_native_pin(sv_pin, sv_ref);
        let old_value = match value_pin {
            Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            None => old_value,
        };
        ctx.set_field(sv_ref, SV_FIELD_IS_BOUND, old_bound);
        ctx.set_field(sv_ref, SV_FIELD_VALUE, old_value);
        pop_sv_owner(ctx, sv_ref);
    }
    ctx.unpin_native_roots(base_pin);
    result
}

/// `Carrier.run(Ljava/lang/Runnable;)V` — bind scoped values and execute.
///
/// Walks the carrier chain (this -> parent -> parent...) and binds ALL
/// scoped values. Invokes the Runnable, then unbinds regardless of outcome.
fn native_carrier_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // Propagate any exception from the Runnable
    carrier_bind_and_invoke(ctx, this, |ctx| match runnable {
        Some(runnable) => ctx.invoke_virtual(runnable, "run", "()V", &[]),
        None => Ok(None),
    })?;
    Ok(None)
}

/// `Carrier.call(Ljava/util/concurrent/Callable;)Ljava/lang/Object;`
///
/// Like `run` but returns a result. Walks the carrier chain, binds all
/// scoped values, invokes the Callable, unbinds, and returns the result.
fn native_carrier_call(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let callable = match args.get(1) {
        Some(Value::Object(Some(c))) => Some(*c),
        _ => None,
    };
    // Propagate exception or return result
    carrier_bind_and_invoke(ctx, this, |ctx| match callable {
        Some(callable) => ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[]),
        None => Ok(Some(Value::Object(None))),
    })
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
// nb-jdk25-concurrency — per-scope forked-worker, owner and joiner tracking
// ---------------------------------------------------------------------------
//
// gc-common w20-g (`common-w19b-structured-task-scope-side-tables-are-process-wide-and-address-keyed`).
// This used to be three process-global maps -- `SCOPE_FORKS`, `SCOPE_OWNERS`,
// `SCOPE_JOINERS` -- keyed by the scope's RAW ADDRESS, two of them holding raw
// `ObjectRef`s. A moving collection between `fork` and `join` made `join` look
// under the scope's new address and find nothing (it returned without waiting
// for the workers or aggregating their outcomes), made the owner check answer
// "no owner" and lost the joiner; the stored subtask / worker / joiner were
// neither roots nor remapped; a scope dropped without `close` left its rows
// for a later scope -- of this VM or another -- allocated at the same address.
// Now there is ONE table, [`STS_TABLE`], the shape of
// `phases_late::concurrent`'s `SynchronousQueue` rendezvous table:
//
// * per VM (dropped by [`forget_vm_jdk25_tables`], from
//   `phases_late::concurrent::forget_vm_concurrent_tables`);
// * rows filed under the scope's identity hash, each holding a GLOBAL ROOT of
//   its scope -- `resolve_global_root(scope_root) == Some(scope)` picks the
//   row, which also tells apart two live scopes sharing a 32-bit hash -- and
//   global roots of the joiner and of every fork's subtask and worker, all
//   remapped by the collector and resolved at use;
// * no `ctx` call under the table lock: roots are minted before a locked step
//   and resolved / released after it;
// * a row lives while it has something to track: `join` takes the forks out,
//   `close` drops the whole row, and a row left with no fork, owner or joiner
//   is pruned (its scope root released) by whoever emptied it. A scope that is
//   opened with an owner or a joiner and then dropped WITHOUT `close` keeps its
//   row -- and, through the scope root, itself -- until VM teardown; the JDK
//   contract (`close` in a try-with-resources) makes that a program error.

/// One `(Subtask, worker Thread)` pair filed by `fork()`: two global roots,
/// released by whoever takes the pair out of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StsFork {
    subtask_root: usize,
    worker_root: usize,
}

/// One live scope's row. See the block comment above.
struct StsRow {
    /// Global root of the scope; identifies the row among those sharing its
    /// identity hash. Released when the row goes.
    scope_root: usize,
    /// The thread that opened an owner-confined scope (`open` with owner
    /// tracking / `open(Joiner)`).
    owner: Option<std::thread::ThreadId>,
    /// Global root of the scope's `Joiner`, for `open(Joiner)` scopes.
    joiner_root: Option<usize>,
    /// Workers `fork()` started and `join()` has not reaped yet.
    forks: Vec<StsFork>,
}

impl StsRow {
    fn is_idle(&self) -> bool {
        self.owner.is_none() && self.joiner_root.is_none() && self.forks.is_empty()
    }
}

/// The per-VM scope rows, filed under the scope's identity hash.
static STS_TABLE: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<i32, Vec<StsRow>>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// VM teardown: drop `vm`'s `StructuredTaskScope` rows. The global roots they
/// name die with the VM's own root table, so there is nothing to release.
/// Called from `phases_late::concurrent::forget_vm_concurrent_tables`.
pub fn forget_vm_jdk25_tables(vm: usize) {
    STS_TABLE.forget(vm);
}

/// `(identity hash, scope root)` of `scope`'s row, or `None` when it has none.
/// The candidate roots are read under the lock and resolved after it.
fn sts_find_row(ctx: &mut dyn NativeContext, scope: ObjectRef) -> Option<(i32, usize)> {
    let key = ctx.identity_hash_code(scope);
    let roots: Vec<usize> = STS_TABLE
        .peek(ctx.vm_identity(), |t| {
            t.get(&key)
                .map(|rows| rows.iter().map(|r| r.scope_root).collect::<Vec<_>>())
        })
        .flatten()
        .unwrap_or_default();
    roots
        .into_iter()
        .find(|&root| ctx.resolve_global_root(root) == Some(scope))
        .map(|root| (key, root))
}

/// `(identity hash, scope root)` of `scope`'s row, creating the row when there
/// is none. A row is created only while every row under the hash has been
/// resolved and found to be another scope's, so two threads racing to create
/// it end up in one (the loser drops its fresh root).
fn sts_row_for(ctx: &mut dyn NativeContext, scope: ObjectRef) -> (i32, usize) {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(scope);
    let mut not_mine: Vec<usize> = Vec::new();
    let mut fresh: Option<usize> = None;
    loop {
        let unchecked: Vec<usize> = STS_TABLE
            .peek(vm, |t| {
                t.get(&key).map(|rows| {
                    rows.iter()
                        .map(|r| r.scope_root)
                        .filter(|r| !not_mine.contains(r))
                        .collect::<Vec<_>>()
                })
            })
            .flatten()
            .unwrap_or_default();
        for root in unchecked {
            if ctx.resolve_global_root(root) == Some(scope) {
                if let Some(mine) = fresh {
                    ctx.remove_global_root(mine);
                }
                return (key, root);
            }
            not_mine.push(root);
        }
        let root = match fresh {
            Some(root) => root,
            None => *fresh.insert(ctx.add_global_root(scope)),
        };
        let created = STS_TABLE.with(vm, |t| {
            let rows = t.entry(key).or_default();
            if rows.iter().any(|r| !not_mine.contains(&r.scope_root)) {
                return false;
            }
            rows.push(StsRow {
                scope_root: root,
                owner: None,
                joiner_root: None,
                forks: Vec::new(),
            });
            true
        });
        if created {
            return (key, root);
        }
    }
}

/// Run `f` on the row `root` (under `key`) if it is still filed.
fn sts_with_row<R>(
    vm: usize,
    key: i32,
    root: usize,
    f: impl FnOnce(&mut StsRow) -> R,
) -> Option<R> {
    STS_TABLE.with(vm, |t| {
        t.get_mut(&key)?
            .iter_mut()
            .find(|r| r.scope_root == root)
            .map(f)
    })
}

/// Run `f` on `scope`'s row, creating the row first when there is none. `f`
/// runs exactly once. (The loop covers a racing prune of a freshly created,
/// still idle row between the two locked steps.)
fn sts_update(ctx: &mut dyn NativeContext, scope: ObjectRef, mut f: impl FnMut(&mut StsRow)) {
    let vm = ctx.vm_identity();
    loop {
        let (key, root) = sts_row_for(ctx, scope);
        if sts_with_row(vm, key, root, &mut f).is_some() {
            return;
        }
    }
}

/// Drop the row `root` if it has nothing left to track, releasing its scope
/// root once the table lock is gone.
fn sts_prune(ctx: &mut dyn NativeContext, key: i32, root: usize) {
    let dropped = STS_TABLE.with(ctx.vm_identity(), |t| {
        let rows = t.get_mut(&key)?;
        let idx = rows.iter().position(|r| r.scope_root == root)?;
        if !rows[idx].is_idle() {
            return None;
        }
        rows.swap_remove(idx);
        if rows.is_empty() {
            t.remove(&key);
        }
        Some(root)
    });
    if let Some(root) = dropped {
        ctx.remove_global_root(root);
    }
}

/// Release the two roots of each fork in `forks`.
fn sts_release_forks(ctx: &mut dyn NativeContext, forks: &[StsFork]) {
    for fork in forks {
        ctx.remove_global_root(fork.subtask_root);
        ctx.remove_global_root(fork.worker_root);
    }
}

/// `close()`: drop `scope`'s whole row -- forks, owner, joiner -- and release
/// every root it held.
fn sts_forget_scope(ctx: &mut dyn NativeContext, scope: ObjectRef) {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return;
    };
    let removed = STS_TABLE.with(ctx.vm_identity(), |t| {
        let rows = t.get_mut(&key)?;
        let idx = rows.iter().position(|r| r.scope_root == root)?;
        let row = rows.swap_remove(idx);
        if rows.is_empty() {
            t.remove(&key);
        }
        Some(row)
    });
    if let Some(row) = removed {
        sts_release_forks(ctx, &row.forks);
        if let Some(joiner_root) = row.joiner_root {
            ctx.remove_global_root(joiner_root);
        }
        ctx.remove_global_root(row.scope_root);
    }
}

/// File a `(subtask, worker)` pair under `scope` (all three current). Returns
/// the fork, whose roots the table now owns.
fn register_scope_fork(
    ctx: &mut dyn NativeContext,
    scope: ObjectRef,
    subtask: ObjectRef,
    worker: ObjectRef,
) -> StsFork {
    let fork = StsFork {
        subtask_root: ctx.add_global_root(subtask),
        worker_root: ctx.add_global_root(worker),
    };
    sts_update(ctx, scope, |row| row.forks.push(fork));
    fork
}

/// Take (and clear) the recorded forks for a scope; the caller owns their
/// roots from here on and releases them ([`sts_release_forks`]). Empty when
/// none were recorded.
fn take_scope_forks(ctx: &mut dyn NativeContext, scope: ObjectRef) -> Vec<StsFork> {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return Vec::new();
    };
    let forks = sts_with_row(ctx.vm_identity(), key, root, |row| {
        std::mem::take(&mut row.forks)
    })
    .unwrap_or_default();
    sts_prune(ctx, key, root);
    forks
}

/// Peek at the recorded forks for a scope without clearing them (the table
/// keeps owning their roots).
///
/// `joinUntil()` (nb-jdk25-joinuntil) needs this so it can poll the workers
/// against a deadline WITHOUT draining the side-table: if the deadline elapses
/// before the workers finish, the forks must stay registered so a later
/// `close()` can still reap them (the spec keeps the scope open on timeout).
/// `join()` (and the success path of `joinUntil`) still drain via
/// `take_scope_forks`.
fn peek_scope_forks(ctx: &mut dyn NativeContext, scope: ObjectRef) -> Vec<StsFork> {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return Vec::new();
    };
    sts_with_row(ctx.vm_identity(), key, root, |row| row.forks.clone()).unwrap_or_default()}

/// Withdraw one fork `fork` filed under `scope` (the inline-fallback path of
/// `fork()`), releasing its roots if it was still filed.
fn remove_scope_fork(ctx: &mut dyn NativeContext, scope: ObjectRef, fork: StsFork) {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return;
    };
    let removed = sts_with_row(ctx.vm_identity(), key, root, |row| {
        let before = row.forks.len();
        row.forks.retain(|f| *f != fork);
        row.forks.len() != before
    })
    .unwrap_or(false);
    if removed {
        sts_release_forks(ctx, &[fork]);
    }
    sts_prune(ctx, key, root);
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

    // gc-common w19-b: `call()` is Java and can collect; the subtask was
    // written through its earlier address. It is pinned across the call.
    let subtask_pin = ctx.pin_native_root(subtask);
    let call_result = if let Value::Object(Some(callable)) = callable_val {
        ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(Value::Object(None)))
    };
    let subtask = ctx.read_native_pin(subtask_pin, subtask);
    ctx.unpin_native_roots(subtask_pin);

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

    // gc-common w19-b: the scope, the Callable, the subtask, the runner and
    // the worker Thread are each rooted here from the moment they exist and
    // re-read at every use. Four allocations follow (subtask, runner, Thread,
    // its name), and the real-JDK arm runs `Thread.<init>`; every one of these
    // objects used to be held raw across all of them.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let callable_h = match callable_val {
        Value::Object(Some(c)) => Some(scope.root(c)),
        _ => None,
    };

    // Allocate the subtask (returned to the caller immediately, UNAVAILABLE).
    let subtask = try_alloc_concurrent_synthetic(&mut *scope, CLS_SUBTASK, SUBTASK_NUM_FIELDS)?;
    let subtask_h = scope.root(subtask);
    let callable_now = sts_handle_value(&scope, &callable_h, callable_val);
    let ctx = &mut *scope;
    ctx.set_field(subtask, SUBTASK_FIELD_CALLABLE, callable_now);
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
        let this = scope.get(&this_h);
        let ctx = &mut *scope;
        let completed = sts_get_int(ctx, this, STS_FIELD_COMPLETED_COUNT);
        ctx.set_field(this, STS_FIELD_COMPLETED_COUNT, Value::Int(completed + 1));
        return Ok(Some(Value::Object(Some(subtask))));
    }

    // Build the runner Runnable that the worker thread will execute. It holds
    // the Callable + the Subtask and writes the outcome into the Subtask.
    let runner =
        try_alloc_concurrent_synthetic(&mut *scope, CLS_FORK_RUNNER, FORK_RUNNER_NUM_FIELDS)?;
    let runner_h = scope.root(runner);
    let callable_now = sts_handle_value(&scope, &callable_h, callable_val);
    let subtask = scope.get(&subtask_h);
    let ctx = &mut *scope;
    ctx.set_field(runner, FORK_RUNNER_FIELD_CALLABLE, callable_now);
    ctx.set_field(
        runner,
        FORK_RUNNER_FIELD_SUBTASK,
        Value::Object(Some(subtask)),
    );

    // Build a Thread whose Thread.run() dispatches to the runner, then start it
    // via the VM's real thread machinery. The worker is reaped by join().
    let worker = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/lang/Thread",
        THREAD_SYNTHETIC_NUM_FIELDS,
    )?;
    let worker_h = scope.root(worker);
    let name = scope.create_string("StsFork");
    let worker = scope.get(&worker_h);
    let runner = scope.get(&runner_h);
    let ctx = &mut *scope;
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
    let mut worker = scope.get(&worker_h);
    crate::lang_system::capture_inheritable_tl_at_construction(&mut *scope, &mut worker);

    // Record the (subtask, worker) pair BEFORE starting so a racing fast worker
    // is already tracked when join() runs.
    let this = scope.get(&this_h);
    let subtask = scope.get(&subtask_h);
    let fork = register_scope_fork(&mut *scope, this, subtask, worker);

    // Spawn the worker via the VM's real thread machinery.
    let start_result = scope.thread_start(worker);
    let worker = scope.get(&worker_h);
    let subtask = scope.get(&subtask_h);
    let ctx = &mut *scope;

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
        let this = scope.get(&this_h);
        remove_scope_fork(&mut *scope, this, fork);
        // Run the Callable inline on this thread and aggregate immediately.
        let runner = scope.get(&runner_h);
        let inline = native_fork_runner_run(&mut *scope, &[Value::Object(Some(runner))]);
        let this = scope.get(&this_h);
        let subtask = scope.get(&subtask_h);
        sts_aggregate_subtask(&mut *scope, this, subtask);
        inline?;
        return Ok(Some(Value::Object(Some(subtask))));
    }

    Ok(Some(Value::Object(Some(subtask))))
}

/// The current value of an optional rooted object: the handle's address when
/// there is one, `fallback` (null or a non-object) otherwise.
fn sts_handle_value(
    scope: &cratonvm_native_api::NativeHandleScope<'_>,
    handle: &Option<cratonvm_native_api::NativeHandle>,
    fallback: Value,
) -> Value {
    match handle {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => fallback,
    }
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
    //
    // gc-common w20-g: `thread_join` blocks GC-safe, so a collection can run
    // while this thread waits. The scope is pinned across the waits, and the
    // subtasks and workers are read through their global roots at use (they
    // were raw copies from `fork` time).
    let forks = take_scope_forks(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    for fork in &forks {
        // thread_join blocks until the worker's Thread.run() — i.e. the
        // runner's Callable.call() — has fully completed and recorded its
        // outcome on the subtask. Errors here are non-fatal: a worker that
        // already exited yields Ok immediately.
        if let Some(worker) = ctx.resolve_global_root(fork.worker_root) {
            let _ = ctx.thread_join(worker);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);

    // All workers done — aggregate outcomes on the owner thread in fork order.
    // Nothing below allocates.
    for fork in &forks {
        if let Some(subtask) = ctx.resolve_global_root(fork.subtask_root) {
            sts_aggregate_subtask(ctx, this, subtask);
        }
    }
    sts_release_forks(ctx, &forks);

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
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/util/concurrent/TimeoutException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jmsg))],
        || {
            cratonvm_types::error::RuntimeError::IllegalStateException {
                message: format!("TimeoutException (class unavailable): {msg}"),
            }
            .into()
        },
    )
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
            // gc-common w20-g: `park` is a safepoint, so the scope is pinned
            // across the polling loop and re-read each round; the join that
            // ends it is handed the CURRENT scope, not `args`' copy.
            let this_pin = ctx.pin_native_root(this);
            let outcome = loop {
                let this = ctx.read_native_pin(this_pin, this);
                let forks = peek_scope_forks(ctx, this);
                let all_done =
                    forks
                        .iter()
                        .all(|fork| match ctx.resolve_global_root(fork.worker_root) {
                            Some(worker) => !ctx.thread_is_alive(worker),
                            None => true,
                        });
                if all_done {
                    // All workers finished in time — reap + aggregate exactly
                    // like join() (thread_join returns immediately for the
                    // already-dead workers).
                    break native_sts_join(ctx, &[Value::Object(Some(this))]);
                }
                if std::time::Instant::now() >= mono_deadline {
                    // Deadline elapsed with workers still running. Leave the
                    // scope OPEN and the forks registered; throw a real
                    // TimeoutException.
                    break Err(throw_timeout_exception(ctx, "deadline exceeded"));
                }
                // Wait a short, deadline-bounded slice before re-polling. `park`
                // integrates with the VM's thread machinery (safepoint-aware)
                // and a spurious early wakeup is harmless — we just re-check the
                // workers and the deadline. Cap the slice so we never overshoot
                // the deadline by more than ~2ms.
                let remaining = mono_deadline.saturating_duration_since(std::time::Instant::now());
                let slice = remaining.min(std::time::Duration::from_millis(2));
                ctx.park(Some(slice));
            };
            ctx.unpin_native_roots(this_pin);
            outcome
        }
    }
}

/// `StructuredTaskScope.close()V`
///
/// Closes the scope. Idempotent if already closed. A scope with forks that
/// were never joined is still closed (its workers reaped, its row dropped)
/// and THEN `IllegalStateException` is thrown, as the JDK's `close()` does
/// ("thrown after closing the scope if the scope owner did not attempt to
/// join after forking").
///
/// gc-common w20-g: the not-joined case used to throw BEFORE closing, leaving
/// the scope open and its forks (and, now, their roots) filed for good; the
/// scope row is dropped on every path, since nothing tracks a closed scope.
fn native_sts_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = sts_get_int(ctx, this, STS_FIELD_STATE);
    if state == STS_STATE_CLOSED {
        // Already closed — idempotent. Still ensure no tracking lingers.
        sts_forget_scope(ctx, this);
        return Ok(None);
    }
    // Was join() called?
    let not_joined = sts_get_int(ctx, this, STS_FIELD_JOINED) == 0
        && sts_get_int(ctx, this, STS_FIELD_TASK_COUNT) > 0;
    // nb-jdk25-concurrency: reap any worker threads that join() did not (e.g.
    // a scope closed after shutdown without a final join) so close() never
    // leaks live workers, then drop the per-scope tracking entry. thread_join
    // returns immediately for already-dead workers. It blocks GC-safe, so the
    // scope is pinned across the waits.
    let forks = take_scope_forks(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    for fork in &forks {
        if let Some(worker) = ctx.resolve_global_root(fork.worker_root) {
            let _ = ctx.thread_join(worker);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    sts_release_forks(ctx, &forks);
    ctx.set_field(this, STS_FIELD_STATE, Value::Int(STS_STATE_CLOSED));
    sts_forget_scope(ctx, this);
    if not_joined {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "StructuredTaskScope has not been joined".to_string(),
        }
        .into());
    }
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
// Scope owner thread
// ---------------------------------------------------------------------------

// The owner thread is not a field of the synthetic scope (expanding
// STS_NUM_FIELDS would break existing allocations); it lives in the scope's
// `STS_TABLE` row (gc-common w20-g; it was a process-global map keyed by the
// scope's raw address), and is validated on join/close.

fn register_scope_owner(ctx: &mut dyn NativeContext, scope: ObjectRef) {
    let me = std::thread::current().id();
    sts_update(ctx, scope, |row| row.owner = Some(me));
}

fn check_scope_owner(ctx: &mut dyn NativeContext, scope: ObjectRef) -> bool {
    let me = std::thread::current().id();
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return true; // No owner registered — allow (backward compat)
    };
    sts_with_row(
        ctx.vm_identity(),
        key,
        root,
        |row| !matches!(row.owner, Some(owner) if owner != me),
    )
    .unwrap_or(true)
}

/// Forget `scope`'s owner (tests; `close` drops the whole row).
#[cfg(test)]
fn unregister_scope_owner(ctx: &mut dyn NativeContext, scope: ObjectRef) {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return;
    };
    sts_with_row(ctx.vm_identity(), key, root, |row| row.owner = None);
    sts_prune(ctx, key, root);
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
        _ => Ok(Some(Value::Object(None))),    }}

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
    // Determine policy from joiner. gc-common w20-g: read BEFORE the scope's
    // allocation, which can collect; the joiner was read through its entry
    // address after it.
    let policy = sts_policy_for_joiner(ctx, joiner_val);
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS)?;
    sts_init_fields(ctx, scope, Value::Object(None), policy);
    register_scope_owner(ctx, scope);
    Ok(Some(Value::Object(Some(scope))))
}

/// The scope policy a `Joiner` (or null) implies.
fn sts_policy_for_joiner(ctx: &mut dyn NativeContext, joiner_val: Value) -> i32 {
    match joiner_val {
        Value::Object(Some(joiner)) => match ctx.get_field(joiner, JOINER_FIELD_POLICY) {
            Value::Int(JOINER_POLICY_ANY_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_SUCCESS,
            Value::Int(JOINER_POLICY_ALL_SUCCESSFUL)
            | Value::Int(JOINER_POLICY_AWAIT_ALL_SUCCESSFUL) => STS_POLICY_SHUTDOWN_ON_FAILURE,
            _ => STS_POLICY_BASE,
        },
        _ => STS_POLICY_BASE,
    }
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
    config_with_field(ctx, this, CONFIG_FIELD_NAME, name)
}

/// A copy of the `Config` `this` with field `field` replaced by `value` (the
/// JDK's `Config` is immutable: every `with*` answers a new one).
///
/// gc-common w19-b: the three `with*` natives held `this` and the new value,
/// raw copies out of `args`, across the copy's allocation, and then read the
/// other fields through `this`'s earlier address. Both are rooted across it.
fn config_with_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: usize,
    value: Value,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let value_h = match value {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let config = try_alloc_concurrent_synthetic(&mut *scope, CLS_CONFIG, CONFIG_NUM_FIELDS)?;
    let this_now = scope.get(&this_h);
    let value_now = sts_handle_value(&scope, &value_h, value);
    let ctx = &mut *scope;
    for f in [
        CONFIG_FIELD_NAME,
        CONFIG_FIELD_THREAD_FACTORY,
        CONFIG_FIELD_TIMEOUT_MS,
    ] {
        let v = if f == field {
            value_now
        } else {
            ctx.get_field(this_now, f)
        };
        ctx.set_field(config, f, v);
    }
    Ok(Some(Value::Object(Some(config))))
}

/// `Config.withThreadFactory(ThreadFactory)Config`
fn native_config_with_thread_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let tf = args.get(1).copied().unwrap_or(Value::Object(None));
    config_with_field(ctx, this, CONFIG_FIELD_THREAD_FACTORY, tf)
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
            secs * 1000 + nanos / 1_000_000        }
        _ => 0,
    };
    config_with_field(ctx, this, CONFIG_FIELD_TIMEOUT_MS, Value::Long(timeout_ms))}

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
    register_scope_owner(ctx, scope);
    Ok(Some(Value::Object(Some(scope))))
}

/// Owner-validating join — wraps `native_sts_join` with ownership check.
fn native_sts_join_owned(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "join() called from non-owner thread".to_string(),
        }
        .into());
    }
    native_sts_join(ctx, args)
}

/// Owner-validating close — wraps `native_sts_close` with ownership check.
/// `native_sts_close` drops the scope's whole row (owner included) on every
/// path that closes it.
fn native_sts_close_owned(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "close() called from non-owner thread".to_string(),
        }
        .into());
    }
    native_sts_close(ctx, args)
}

// ---------------------------------------------------------------------------
// Joiner-aware fork — notifies the Joiner on subtask completion
// ---------------------------------------------------------------------------

// The joiner of a scope opened with open(Joiner) lives in the scope's
// `STS_TABLE` row as a global root (gc-common w20-g; it was a raw `ObjectRef`
// in a process-global map keyed by the scope's raw address -- neither rooted
// nor remapped, and lost when the scope moved).

fn register_scope_joiner(ctx: &mut dyn NativeContext, scope: ObjectRef, joiner: ObjectRef) {
    let joiner_root = ctx.add_global_root(joiner);
    let mut previous = None;
    sts_update(ctx, scope, |row| {
        previous = row.joiner_root.replace(joiner_root)
    });
    if let Some(previous) = previous {
        ctx.remove_global_root(previous);
    }
}

fn get_scope_joiner(ctx: &mut dyn NativeContext, scope: ObjectRef) -> Option<ObjectRef> {
    let (key, root) = sts_find_row(ctx, scope)?;
    let joiner_root = sts_with_row(ctx.vm_identity(), key, root, |row| row.joiner_root)??;
    ctx.resolve_global_root(joiner_root)
}

#[cfg(test)]
fn unregister_scope_joiner(ctx: &mut dyn NativeContext, scope: ObjectRef) {
    let Some((key, root)) = sts_find_row(ctx, scope) else {
        return;
    };
    let joiner_root =
        sts_with_row(ctx.vm_identity(), key, root, |row| row.joiner_root.take()).flatten();
    if let Some(joiner_root) = joiner_root {
        ctx.remove_global_root(joiner_root);
    }
    sts_prune(ctx, key, root);
}

/// Enhanced `StructuredTaskScope.open(Joiner)` — registers joiner for fork
/// notification.
fn native_sts_open_joiner_tracked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let joiner_val = args.get(0).copied().unwrap_or(Value::Object(None));
    // gc-common w20-g: the policy is read, and the joiner pinned, before the
    // scope's allocation (which can collect); both used the joiner's entry
    // address after it.
    let policy = sts_policy_for_joiner(ctx, joiner_val);
    let joiner_pin = match joiner_val {
        Value::Object(Some(joiner)) => Some((ctx.pin_native_root(joiner), joiner)),
        _ => None,
    };
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS);
    let joiner = joiner_pin.map(|(pin, joiner)| ctx.read_native_pin(pin, joiner));
    if let Some((pin, _)) = joiner_pin {
        ctx.unpin_native_roots(pin);
    }
    let scope = scope?;
    if let Some(joiner) = joiner {
        register_scope_joiner(ctx, scope, joiner);
    }
    sts_init_fields(ctx, scope, Value::Object(None), policy);
    register_scope_owner(ctx, scope);
    Ok(Some(Value::Object(Some(scope))))
}

/// Enhanced fork that notifies the scope's Joiner (if any) on completion.
fn native_sts_fork_with_joiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Perform the standard fork. gc-common w20-g: it allocates four objects
    // and may run `Thread.<init>` and the Callable itself, so the scope is
    // pinned across it and the joiner looked up through its current address.
    let this_pin = ctx.pin_native_root(this);
    let result = native_sts_fork(ctx, args);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let result = result?;

    // If scope has a joiner, notify it
    if let Some(joiner) = get_scope_joiner(ctx, this) {
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

/// Enhanced close for joiner-tracked scopes: owner-checked; `native_sts_close`
/// drops the scope's row, joiner included.
fn native_sts_close_joiner(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !check_scope_owner(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "close() called from non-owner thread".to_string(),
        }
        .into());
    }
    native_sts_close(ctx, args)
}

// ===========================================================================
// Registration
// ===========================================================================

pub fn register_jdk25_concurrency_natives(r: &mut NativeMethodRegistry) {
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
    r.register(CLS_SCOPED_VALUE, "<init>", "()V", native_sv_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SCOPED_VALUE,
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        native_sv_new_instance, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SCOPED_VALUE,
        "get",
        "()Ljava/lang/Object;",
        native_sv_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_SCOPED_VALUE, "isBound", "()Z", native_sv_is_bound, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SCOPED_VALUE,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_sv_or_else, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SCOPED_VALUE,
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_sv_or_else_throw, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_SCOPED_VALUE, "hashCode", "()I", native_sv_hash_code, cratonvm_native_api::NativeKind::Bridge);

    // --- ScopedValue.where (static) ---
    r.register(
        CLS_SCOPED_VALUE,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        native_sv_where_static, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- ScopedValue$Carrier ---
    r.register(
        CLS_CARRIER,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        native_carrier_where, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CARRIER,
        "run",
        "(Ljava/lang/Runnable;)V",
        native_carrier_run, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CARRIER,
        "call",
        "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
        native_carrier_call, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 25 uses ScopedValue$CallableOp instead of Callable
    r.register(
        CLS_CARRIER,
        "call",
        "(Ljava/lang/ScopedValue$CallableOp;)Ljava/lang/Object;",
        native_carrier_call, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CARRIER,
        "get",
        "()Ljava/lang/Object;",
        native_carrier_get, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- ScopedValue$Snapshot ---
    r.register(
        CLS_SNAPSHOT,
        "capture",
        "()Ljava/lang/ScopedValue$Snapshot;",
        native_snapshot_capture, cratonvm_native_api::NativeKind::Bridge,
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
    //     `cratonvm-cli`. Under `--real-jdk` and `--fast-boot` the real JDK
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
    // docs/feature-designs/jit-ir-optimizations.md.
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
    r.register(CLS_TASK_SCOPE, "<init>", "()V", native_sts_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_TASK_SCOPE,
        "<init>",
        "(Ljava/lang/String;)V",
        native_sts_init_name, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_TASK_SCOPE,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sts_init_name_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_TASK_SCOPE,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_open, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_TASK_SCOPE,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_TASK_SCOPE,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_TASK_SCOPE,
        "joinUntil",
        "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join_until, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_TASK_SCOPE, "close", "()V", native_sts_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(CLS_TASK_SCOPE, "shutdown", "()V", native_sts_shutdown, cratonvm_native_api::NativeKind::Bridge);
    r.register(CLS_TASK_SCOPE, "isShutdown", "()Z", native_sts_is_shutdown, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_TASK_SCOPE,
        "toString",
        "()Ljava/lang/String;",
        native_sts_to_string, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- nb-jdk25-concurrency: fork worker Runnable ---
    // Each fork() spawns a real worker Thread whose Thread.run() dispatches to
    // a CratonVM$StsForkRunner; this native is the runner body that invokes
    // the Callable and records the outcome on the Subtask.
    r.register(CLS_FORK_RUNNER, "run", "()V", native_fork_runner_run, cratonvm_native_api::NativeKind::Bridge);

    // --- StructuredTaskScope$Subtask ---
    r.register(
        CLS_SUBTASK,
        "get",
        "()Ljava/lang/Object;",
        native_subtask_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SUBTASK,
        "state",
        "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;",
        native_subtask_state, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SUBTASK,
        "exception",
        "()Ljava/lang/Throwable;",
        native_subtask_exception, cratonvm_native_api::NativeKind::Bridge,
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
        native_subtask_task, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- ShutdownOnFailure ---
    //
    // JDK-ONLY-NOTE (W7-18): every registration between here and the `Joiner`
    // block below is on a class or a method that **no JDK 25 declares**, and
    // that is measured rather than inferred. `javap` on Adoptium 25.0.3.9
    // answers "class not found" for `StructuredTaskScope$ShutdownOnSuccess` and
    // `$ShutdownOnFailure` — JEP 505 deleted both — and `Class.forName` answers
    // `ClassNotFoundException` for each on HotSpot 25, `cratonvm --fast-boot` and
    // `cratonvm --real-jdk` alike. `StructuredTaskScope$Config` is a name no JDK
    // ever shipped (`$Configuration` is the real one, registered by
    // `phases_late/concurrent.rs::register_p67_structured_task_scope_j25`), and
    // `Joiner.policy()I` is not on the JDK's `Joiner` either. Transcript:
    // docs/feature-designs/native-dispatch-architecture.md §1/§3.
    //
    // WHY THEY ARE STILL HERE, named as a decision rather than left as an
    // oversight. Two things have to be true at once for the deletion to be
    // worth taking, and only one is:
    //
    //   * It cannot move either shipping mode. TRUE — this registrar is reached
    //     only from `register_synthetic_overrides`, so in `--fast-boot` and
    //     `--real-jdk` real JDK bytecode serves the whole API with zero
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
    r.register(CLS_SHUTDOWN_ON_FAILURE, "<init>", "()V", native_sof_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sof_init_name_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;",
        native_sof_open, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "throwIfFailed",
        "()V",
        native_sof_throw_if_failed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "throwIfFailed",
        "(Ljava/util/function/Function;)V",
        native_sof_throw_if_failed_fn, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "exception",
        "()Ljava/util/Optional;",
        native_sof_exception, cratonvm_native_api::NativeKind::Bridge,
    );
    // ShutdownOnFailure inherits fork/join/close/shutdown from StructuredTaskScope
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_SHUTDOWN_ON_FAILURE, "close", "()V", native_sts_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "shutdown",
        "()V",
        native_sts_shutdown, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_FAILURE,
        "isShutdown",
        "()Z",
        native_sts_is_shutdown, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- ShutdownOnSuccess ---
    r.register(CLS_SHUTDOWN_ON_SUCCESS, "<init>", "()V", native_sos_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        native_sos_init_name_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "open",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        native_sos_open, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "result",
        "()Ljava/lang/Object;",
        native_sos_result, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "result",
        "(Ljava/util/function/Function;)Ljava/lang/Object;",
        native_sos_result_fn, cratonvm_native_api::NativeKind::Bridge,
    );
    // ShutdownOnSuccess inherits fork/join/close/shutdown from StructuredTaskScope
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        native_sts_fork, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_join, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_SHUTDOWN_ON_SUCCESS, "close", "()V", native_sts_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "shutdown",
        "()V",
        native_sts_shutdown, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SHUTDOWN_ON_SUCCESS,
        "isShutdown",
        "()Z",
        native_sts_is_shutdown, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Joiner API (JDK 25 / JEP 505) ---
    r.register(
        CLS_JOINER,
        "allSuccessfulOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_all_successful, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_JOINER,
        "anySuccessfulResultOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_any_successful, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_JOINER,
        "awaitAllSuccessfulOrThrow",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_await_all_successful, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_JOINER,
        "awaitAll",
        "()Ljava/util/concurrent/StructuredTaskScope$Joiner;",
        native_joiner_await_all, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_JOINER,
        "onComplete",
        "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z",
        native_joiner_on_complete, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_JOINER,
        "result",
        "()Ljava/lang/Object;",
        native_joiner_result, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_JOINER, "policy", "()I", native_joiner_policy, cratonvm_native_api::NativeKind::Bridge);

    // --- StructuredTaskScope.open(Joiner) ---
    r.register(
        CLS_TASK_SCOPE,
        "open",
        "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;",
        native_sts_open_joiner_tracked, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Config API ---
    r.register(CLS_CONFIG, "<init>", "()V", native_config_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_CONFIG,
        "withName",
        "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_name, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CONFIG,
        "withThreadFactory",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_thread_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CONFIG,
        "withTimeout",
        "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;",
        native_config_with_timeout, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CONFIG,
        "getName",
        "()Ljava/lang/String;",
        native_config_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_CONFIG,
        "getThreadFactory",
        "()Ljava/util/concurrent/ThreadFactory;",
        native_config_get_thread_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod jdk25_concurrency_tests;

#[cfg(test)]
mod thread_layout_tests;

/// gc-common w20-g (`common-w19b-structured-task-scope-side-tables-are-process-wide-and-address-keyed`):
/// the per-VM, root-holding `STS_TABLE`. Each test owns a private VM identity
/// (the tables are process-wide statics shared by parallel tests).
#[cfg(test)]
mod w20g_scope_table_tests;
