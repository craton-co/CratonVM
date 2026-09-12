// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.concurrent` (+ `.atomic`, `.locks`) natives: AQS, locks, latches, semaphores, executors, CompletableFuture.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

pub(crate) fn register_queue_bridge_natives(registry: &mut NativeMethodRegistry) {
    let bridge_enabled = crate::nbflags().netty_queue_bridge;
    if !bridge_enabled {
        return;
    }
    let netty_classes = [
        "io/netty/util/internal/shaded/org/jctools/queues/BaseMpscLinkedArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/MpscUnboundedArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/MpscChunkedArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/MpscArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/atomic/BaseMpscLinkedAtomicArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/atomic/MpscUnboundedAtomicArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/atomic/MpscChunkedAtomicArrayQueue",
        "io/netty/util/internal/shaded/org/jctools/queues/atomic/MpscAtomicArrayQueue",
    ];
    for class_name in netty_classes {
        registry.register(
            class_name,
            "offer",
            "(Ljava/lang/Object;)Z",
            native_netty_mpsc_offer,
        );
        registry.register(
            class_name,
            "relaxedOffer",
            "(Ljava/lang/Object;)Z",
            native_netty_mpsc_offer,
        );
        registry.register(
            class_name,
            "poll",
            "()Ljava/lang/Object;",
            native_netty_mpsc_poll,
        );
        registry.register(
            class_name,
            "relaxedPoll",
            "()Ljava/lang/Object;",
            native_netty_mpsc_poll,
        );
        registry.register(
            class_name,
            "peek",
            "()Ljava/lang/Object;",
            native_netty_mpsc_peek,
        );
        registry.register(
            class_name,
            "relaxedPeek",
            "()Ljava/lang/Object;",
            native_netty_mpsc_peek,
        );
        registry.register(class_name, "size", "()I", native_netty_mpsc_size);
        registry.register(class_name, "isEmpty", "()Z", native_netty_mpsc_is_empty);
        registry.register(class_name, "clear", "()V", native_netty_mpsc_clear);
    }
}

/// CRATONVM_DBG_AQS_TRACE support — cached env gate + capped stderr ledger.
pub fn aqs_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::nbflags().dbg_aqs_trace)
}

pub fn aqs_trace_line(line: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 500_000 {
        eprintln!("{line}");
    }
}

// ===========================================================================
// java.util.concurrent.atomic.AtomicInteger
// ===========================================================================

pub(crate) fn register_atomic_integer_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicInteger get/set/CAS are VM-internal atomic primitives
    // (Unsafe-equivalent) with spec-exact semantics → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicInteger";
    r.register(c, "<init>", "()V", native_atomic_int_init_default);
    r.register(c, "<init>", "(I)V", native_atomic_int_init_value);
    // LEAF: the plain accessors are one descriptor-typed volatile read or write
    // of field 0 (`private volatile int value`) — no allocation, no safepoint,
    // no collection, no JNI exception. See `NativeMethodRegistry::set_leaf`.
    //
    // This is item 2 of `aqs-thread-handoff-latency-RETIRED-20260805.md`:
    // `AtomicInteger.get()` measured **969 ns**, more than an empty bytecode
    // call, for a body that is `return value;`. Being native it can never be
    // compiled or inlined, so the whole cost was the funnel around it.
    //
    // The CAS / fetch-add members are deliberately NOT here. They go through
    // `compare_and_swap_field` / `atomic_fetch_add_int`, which take the
    // monitor table's per-object CAS lock — a lock this thread can be made to
    // wait on, which is exactly what contract item 2 (no blocking) excludes.
    r.set_leaf(true);
    r.register(c, "get", "()I", native_atomic_int_get);
    r.register(c, "set", "(I)V", native_atomic_int_set);
    r.register(c, "lazySet", "(I)V", native_atomic_int_set);
    r.set_leaf(false);
    r.register(c, "getAndSet", "(I)I", native_atomic_int_get_and_set);
    r.register(c, "compareAndSet", "(II)Z", native_atomic_int_cas);
    r.register(c, "weakCompareAndSet", "(II)Z", native_atomic_int_cas);
    r.register(
        c,
        "getAndIncrement",
        "()I",
        native_atomic_int_get_and_increment,
    );
    r.register(
        c,
        "getAndDecrement",
        "()I",
        native_atomic_int_get_and_decrement,
    );
    r.register(c, "getAndAdd", "(I)I", native_atomic_int_get_and_add);
    r.register(
        c,
        "incrementAndGet",
        "()I",
        native_atomic_int_increment_and_get,
    );
    r.register(
        c,
        "decrementAndGet",
        "()I",
        native_atomic_int_decrement_and_get,
    );
    r.register(c, "addAndGet", "(I)I", native_atomic_int_add_and_get);
    // LEAF, same argument as `get`/`set` above: both are plain reads of field 0.
    r.set_leaf(true);
    r.register(c, "intValue", "()I", native_atomic_int_get);
    r.register(c, "longValue", "()J", native_atomic_int_long_value);
    r.set_leaf(false);
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_atomic_int_to_string,
    );
    r.set_category(__prev_cat);
}

fn native_atomic_int_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Int(0));
    }
    Ok(None)
}

fn native_atomic_int_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let val = args.get(1).copied().unwrap_or(Value::Int(0));
        ctx.set_field_volatile(*this, 0, val);
    }
    Ok(None)
}

fn native_atomic_int_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_volatile(this, 0)))
}

fn native_atomic_int_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = args.get(1).copied().unwrap_or(Value::Int(0));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

fn native_atomic_int_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_val = args.get(1).copied().unwrap_or(Value::Int(0));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_atomic_int_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let expected = args.get(1).copied().unwrap_or(Value::Int(0));
    let new_val = args.get(2).copied().unwrap_or(Value::Int(0));
    let result = ctx.compare_and_swap_field(this, 0, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

// audit-round5 fix #9 (HIGH): replace per-iteration CAS loops with the
// `atomic_fetch_add_int` intrinsic. The default trait impl preserves CAS
// behavior for non-VM contexts (mocks); the VM override can collapse this
// into a single `LOCK XADD`. One trait dispatch per call instead of two
// per loop iteration.
fn native_atomic_int_get_and_increment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, 1)?;
    Ok(Some(Value::Int(old)))
}

fn native_atomic_int_get_and_decrement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, -1)?;
    Ok(Some(Value::Int(old)))
}

fn native_atomic_int_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delta = match args.get(1) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_int(this, 0, delta)?;
    Ok(Some(Value::Int(old)))
}

fn native_atomic_int_increment_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, 1)?;
    Ok(Some(Value::Int(old.wrapping_add(1))))
}

fn native_atomic_int_decrement_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, -1)?;
    Ok(Some(Value::Int(old.wrapping_sub(1))))
}

fn native_atomic_int_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delta = match args.get(1) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_int(this, 0, delta)?;
    Ok(Some(Value::Int(old.wrapping_add(delta))))
}

fn native_atomic_int_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = ctx.get_field_volatile(this, 0);
    match val {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

fn native_atomic_int_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = ctx.get_field_volatile(this, 0);
    let text = match val {
        Value::Int(v) => format!("{v}"),
        _ => "0".to_string(),
    };
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

// ===========================================================================
// java.util.concurrent.atomic.AtomicLong
// ===========================================================================

pub(crate) fn register_atomic_long_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicLong atomic primitives (Unsafe-equivalent) → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicLong";
    // VMSupportsCS8 is consulted by AtomicLong.<clinit> to choose between
    // a lock-free 64-bit CAS implementation and a synchronized fallback.
    // See `native_atomic_long_vm_supports_cs8` below for the cfg rules.
    r.register(
        c,
        "VMSupportsCS8",
        "()Z",
        native_atomic_long_vm_supports_cs8,
    );
    r.register(c, "<init>", "()V", native_atomic_long_init_default);
    r.register(c, "<init>", "(J)V", native_atomic_long_init_value);
    // LEAF, for the same reason as the `AtomicInteger` accessors above: a
    // descriptor-typed volatile read/write of field 0, and nothing else. The
    // CAS / fetch-add members stay on the funnel because they take the CAS lock.
    r.set_leaf(true);
    r.register(c, "get", "()J", native_atomic_long_get);
    r.register(c, "set", "(J)V", native_atomic_long_set);
    r.register(c, "lazySet", "(J)V", native_atomic_long_set);
    r.set_leaf(false);
    r.register(c, "getAndSet", "(J)J", native_atomic_long_get_and_set);
    r.register(c, "compareAndSet", "(JJ)Z", native_atomic_long_cas);
    r.register(c, "weakCompareAndSet", "(JJ)Z", native_atomic_long_cas);
    r.register(
        c,
        "getAndIncrement",
        "()J",
        native_atomic_long_get_and_increment,
    );
    r.register(
        c,
        "getAndDecrement",
        "()J",
        native_atomic_long_get_and_decrement,
    );
    r.register(c, "getAndAdd", "(J)J", native_atomic_long_get_and_add);
    r.register(
        c,
        "incrementAndGet",
        "()J",
        native_atomic_long_increment_and_get,
    );
    r.register(
        c,
        "decrementAndGet",
        "()J",
        native_atomic_long_decrement_and_get,
    );
    r.register(c, "addAndGet", "(J)J", native_atomic_long_add_and_get);
    r.set_leaf(true);
    r.register(c, "intValue", "()I", native_atomic_long_int_value);
    r.register(c, "longValue", "()J", native_atomic_long_get);
    r.set_leaf(false);
    r.set_category(__prev_cat);
}

/// `AtomicLong.VMSupportsCS8()Z` — static query used by `AtomicLong.<clinit>`
/// to pick between a lock-free 64-bit CAS path and a synchronized fallback.
///
/// Rust's `std::sync::atomic::AtomicI64` guarantees lock-free
/// compare-exchange on every 64-bit target (tier-1 and tier-2). `cmpxchg16b`
/// advertises a 128-bit CAS on x86-64 and implies the narrower 64-bit CAS as
/// well. 32-bit hosts that genuinely lack an 8-byte CAS (e.g. ARM v6 without
/// LDREXD/STREXD, SPARC v8) fall through to `false`; cratonvm ships only
/// 64-bit targets so in practice this always answers `true`.
///
/// Zero args, no side effects, no panics.
fn native_atomic_long_vm_supports_cs8(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let supports = cfg!(target_pointer_width = "64") || cfg!(target_feature = "cmpxchg16b");
    Ok(Some(Value::Int(if supports { 1 } else { 0 })))
}

fn native_atomic_long_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Long(0));
    }
    Ok(None)
}

fn native_atomic_long_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let val = args.get(1).copied().unwrap_or(Value::Long(0));
        ctx.set_field_volatile(*this, 0, val);
    }
    Ok(None)
}

/// The receiver of an `Atomic{Long,Reference}` instance native, or the
/// `NullPointerException` the JVM specifies when there is not one.
///
/// # Why this exists rather than `unsafe_obj(args, 0).unwrap()`
///
/// Every native in this block used to open with that `unwrap()`, on the
/// reasoning that an instance native cannot be reached without a receiver: a
/// null-receiver `invokevirtual` is an NPE the interpreter raises before any
/// native runs, and every JIT dispatch entry point screens `args[0]` (see
/// `jit_invoke_dispatch`'s `Value::Object(None) => set_jit_pending_npe()` arm
/// and `jit_invoke_virtual_mic`'s `receiver_raw == 0` arm). The reasoning is
/// sound and the `unwrap()` was still wrong, because it makes the VM's own
/// answer to "that invariant was violated" a **panic**.
///
/// MEASURED (Tomcat suite, 2026-09-05, Generational shard-0): three
/// `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite*` classes hit
///
/// ```text
/// thread 'http-nio-127.0.' panicked at native-builtins/src/util_concurrent_ext.rs:583:36:
/// called `Option::unwrap()` on a `None` value
/// ```
///
/// from `AtomicReference.getAndSet`. The panic-catching machinery caught all
/// three, so the process survived — but in two of them the catch rethrew into
/// Java as
/// `InternalError: JIT dispatch into ...AtomicReference.getAndSet... failed:
/// native method panic`, which `Http2AsyncUpgradeHandler.handleAsyncException`
/// then handled INSTEAD of the state transition the `getAndSet` was there to
/// perform: the HTTP/2 header write never completed and the client's read
/// timed out. A panic is thus not a safe way to report a broken invariant on a
/// request-serving thread — it converts a null dereference (recoverable, and
/// exactly what the spec asks for) into an unrelated `InternalError` on a path
/// no application expects one from.
///
/// So: raise the NPE JVMS §invokevirtual specifies, and make the *invariant
/// violation itself* visible through a diagnostic rather than through a stack
/// unwind. The census below names the offending call site's Java stack, which
/// is the datum the panic never carried.
///
/// This is deliberately NOT a silent null-tolerance shim. The `Err` return is
/// a real, catchable Java exception and the diagnostic is on by default (rate
/// limited), because "a live receiver reached a native as `None`" is a VM
/// defect that must not become quiet.
fn atomic_receiver(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match unsafe_obj(args, 0) {
        Some(obj) => Ok(obj),
        None => Err(report_lost_atomic_receiver(ctx, args, method)),
    }
}

/// The cold half of [`atomic_receiver`]: count, report once in a while, and
/// build the exception.
///
/// Rate limit is the house shape (`n < 4 || n.is_power_of_two()`, see
/// `gc::heap::note_field_coercion_loss`), so a workload that trips this
/// thousands of times costs a dozen lines rather than a flood — while the FIRST
/// occurrence, which is the one a bisect needs, always prints.
#[cold]
fn report_lost_atomic_receiver(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> MethodCallFailed {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    // What the slot actually held. `Object(None)` and "not an object at all"
    // (a `Long` in a receiver slot, i.e. an argument-shift miscompile) are
    // different defects and the message has to tell them apart.
    let shape = match args.first() {
        None => "no argument at all (the call passed zero arguments)".to_string(),
        Some(Value::Object(None)) => "a null reference".to_string(),
        Some(other) => format!("a non-object value ({other:?})"),
    };
    // THE ARITY IS THE DISCRIMINATOR, and it is why this prints the whole
    // vector rather than just slot 0. An instance native's `args` is
    // `[receiver, ..declared params]`, so `AtomicReference.getAndSet(Object)`
    // must arrive with LENGTH 2. Length 1 means the receiver was never laid
    // down and the DECLARED PARAMETER is sitting in slot 0 — a different
    // defect entirely (an invoke-kind / argument-shift mismatch at the call
    // site), and one that Tomcat's `applicationIOE.getAndSet(null)` disguises
    // perfectly: the argument it passes IS null, so slot 0 reads
    // `Object(None)` under both hypotheses.
    let arity = args.len();
    let all: Vec<String> = args.iter().map(|v| format!("{v:?}")).collect();
    if n < 4 || n.is_power_of_two() {
        eprintln!(
            "[cratonvm] VM DEFECT: {method} reached its native with {shape} as the receiver \
             (occurrence {n}, args.len()={arity}, args={all:?}). An instance native cannot be \
             entered without a live receiver: the interpreter and every JIT dispatch entry \
             point raise NullPointerException for a null one first, and an args.len() short of \
             `1 + declared params` means the receiver slot was never laid down at all. Raising \
             NullPointerException here (JVMS invokevirtual) rather than panicking; the Java \
             call site follows."
        );
        // The Rust half names the dispatch path -- which of the several JIT
        // and interpreter doors built these `args` -- and that is the datum
        // the panic never carried either, because the panic was CAUGHT and
        // its backtrace discarded. Opt-in on the standard variable so an
        // ordinary run pays nothing.
        //
        // Through `flags::runtime_var_os`, not `std::env` directly, and the
        // behaviour is unchanged by that: the boundary returns the immutable
        // snapshot only for a name in `declared_flag_names()`, and
        // `RUST_BACKTRACE` is not one, so this is the same live read it always
        // was. The rule it satisfies is `tools/flag-census/check-surface.sh`
        // check 4 -- core runtime crates read every variable through one
        // function, so that the census of what this VM reads is a census and
        // not a sample. An ordinary OS variable is not an exception to that;
        // it is the case the boundary's live-read arm exists for.
        if cratonvm_types::flags::runtime_var_os("RUST_BACKTRACE").is_some() {
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        for entry in ctx.capture_stack_trace(0).iter().rev() {
            eprintln!(
                "  at {}.{}({}:{})",
                entry.class_name,
                entry.method_name,
                entry.source_file.as_deref().unwrap_or("?"),
                entry.line_number
            );
        }
    }
    RuntimeError::NullPointerException {
        message: Some(format!(
            "Cannot invoke \"{method}\" because the receiver is null"
        )),
    }
    .into()
}

fn native_atomic_long_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.get()")?;
    Ok(Some(ctx.get_field_volatile(this, 0)))
}

fn native_atomic_long_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.set(long)")?;
    let val = args.get(1).copied().unwrap_or(Value::Long(0));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

fn native_atomic_long_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.getAndSet(long)")?;
    let new_val = args.get(1).copied().unwrap_or(Value::Long(0));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_atomic_long_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.compareAndSet(long, long)")?;
    let expected = args.get(1).copied().unwrap_or(Value::Long(0));
    let new_val = args.get(2).copied().unwrap_or(Value::Long(0));
    let result = ctx.compare_and_swap_field(this, 0, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

// audit-round5 fix #9 (HIGH): AtomicLong analogue of the AtomicInteger
// fetch_add migration — single trait dispatch per call, VM override can
// emit a single `LOCK XADDQ`.
fn native_atomic_long_get_and_increment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.getAndIncrement()")?;
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old)))
}

fn native_atomic_long_get_and_decrement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.getAndDecrement()")?;
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old)))
}

fn native_atomic_long_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.getAndAdd(long)")?;
    let delta = match args.get(1) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_long(this, 0, delta)?;
    Ok(Some(Value::Long(old)))
}

fn native_atomic_long_increment_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.incrementAndGet()")?;
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old.wrapping_add(1))))
}

fn native_atomic_long_decrement_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.decrementAndGet()")?;
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old.wrapping_sub(1))))
}

fn native_atomic_long_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.addAndGet(long)")?;
    let delta = match args.get(1) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_long(this, 0, delta)?;
    Ok(Some(Value::Long(old.wrapping_add(delta))))
}

fn native_atomic_long_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.intValue()")?;
    let val = ctx.get_field_volatile(this, 0);
    match val {
        Value::Long(v) => Ok(Some(Value::Int(v as i32))),
        _ => Ok(Some(Value::Int(0))),
    }
}

// ===========================================================================
// java.util.concurrent.atomic.AtomicReference
// ===========================================================================

pub(crate) fn register_atomic_reference_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicReference atomic primitives (Unsafe-equivalent) → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicReference";
    r.register(c, "<init>", "()V", native_atomic_ref_init_default);
    r.register(
        c,
        "<init>",
        "(Ljava/lang/Object;)V",
        native_atomic_ref_init_value,
    );
    r.register(c, "get", "()Ljava/lang/Object;", native_atomic_ref_get);
    r.register(c, "set", "(Ljava/lang/Object;)V", native_atomic_ref_set);
    r.register(c, "lazySet", "(Ljava/lang/Object;)V", native_atomic_ref_set);
    // Deliberately NOT registered (2026-08-29): real-JDK
    // `AtomicReference.compareAndSet` is one line --
    // `return VALUE.compareAndSet(this, expectedValue, newValue);` -- and that
    // `VarHandle.compareAndSet` is now thin-direct-bound (see
    // `varhandle-compareandset-thin-direct-bind-FIXED-20260828.md`),
    // so running the real bytecode is now FASTER than this synthetic stub, not
    // slower. A previous attempt at this exact change (recorded in
    // `juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`,
    // "What was tried and refuted") measured the stub WINNING (257 vs 289 ns)
    // because at the time the CAS underneath the real bytecode was still
    // funnel-served at ~233 ns; that arithmetic flips now that the bind exists.
    // MEASURED (six interleaved runs, `HibfixVarHandleProbe`, kill-switch as the
    // only difference, `--dump-native-registry` confirmed no native registered
    // either way for this exact tuple): stub kept 519.5-543.8 ns (median 531.7),
    // stub dropped 331.4-344.0 ns (median 332.0) -- a 1.6x win, with
    // `AtomicInteger.incrementAndGet` unmoved (5.5-6.0 ns both arms) as the
    // control that says this is the change and not the box.
    r.register(
        c,
        "getAndSet",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_atomic_ref_get_and_set,
    );
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_atomic_ref_to_string,
    );
    r.set_category(__prev_cat);
}

fn native_atomic_ref_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Object(None));
    }
    Ok(None)
}

fn native_atomic_ref_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field_volatile(*this, 0, val);
    }
    Ok(None)
}

fn native_atomic_ref_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicReference.get()")?;
    let val = ctx.get_field_volatile(this, 0);
    if crate::nbflags().dbg_loader_trace {
        if let Value::Object(Some(o)) = val {
            let val_cid = ctx.class_id_of_object(o);
            let val_cn = ctx.class_name_of_id(val_cid).unwrap_or_default();
            if val_cn.contains("RootReference") {
                let frames = ctx.frame_class_ids();
                let caller_cid = frames.first().copied();
                let caller_cn = caller_cid
                    .and_then(|c| ctx.class_name_of_id(c))
                    .unwrap_or_default();
                eprintln!(
                    "[LOADER-TRACE] native_atomic_ref_get thread={} holder_obj={:p} val_obj={:p} val_class={} val_cid={} caller_class_id={:?} caller_class={}",
                    ctx.thread_id(), this.as_ptr(), o.as_ptr(), val_cn, val_cid.as_u32(), caller_cid, caller_cn
                );
            }
        }
    }
    Ok(Some(val))
}

fn native_atomic_ref_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicReference.set(Object)")?;
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

fn native_atomic_ref_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicReference.getAndSet(Object)")?;
    let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_atomic_ref_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicReference.toString()")?;
    let val = ctx.get_field_volatile(this, 0);
    let text = match val {
        Value::Object(Some(obj)) => crate::lang_string::invoke_to_string(ctx, obj)?,
        _ => "null".to_string(),
    };
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

// ===========================================================================
// java.util.concurrent.locks.LockSupport
// ===========================================================================

pub(crate) fn register_lock_support_natives(r: &mut NativeMethodRegistry) {
    // census-tag: LockSupport.park/unpark are real ACC_NATIVE methods bridging
    // to the VM's thread park/unpark primitives → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/locks/LockSupport";
    r.register(c, "park", "(Ljava/lang/Object;)V", native_lock_support_park);
    // Also register no-arg versions (Java public API uses no-arg park())
    r.register(c, "park", "()V", native_lock_support_park);
    r.register(
        c,
        "parkNanos",
        "(Ljava/lang/Object;J)V",
        native_lock_support_park_nanos,
    );
    r.register(c, "parkNanos", "(J)V", |ctx, args| {
        if ctx.is_interrupted(false) {
            return Ok(None);
        }
        // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
        // tag-loss for category-2 longs (CompactValue::long stores raw i64 untagged,
        // and `to_value()` decodes untagged bits as Double). Recover via bit-reinterpret.
        let nanos: i64 = match args.first() {
            Some(Value::Long(n)) => *n,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(n)) => *n as i64,
            _ => 0,
        };
        if nanos > 0 {
            let timeout = Some(std::time::Duration::from_nanos(nanos as u64));
            if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
                return yielded;
            }
            ctx.park(timeout);
        }
        Ok(None)
    });
    r.register(
        c,
        "parkUntil",
        "(Ljava/lang/Object;J)V",
        native_lock_support_park_until,
    );
    r.register(
        c,
        "unpark",
        "(Ljava/lang/Thread;)V",
        native_lock_support_unpark,
    );
    r.register(
        c,
        "getBlocker",
        "(Ljava/lang/Thread;)Ljava/lang/Object;",
        native_lock_support_get_blocker,
    );
    r.set_category(__prev_cat);
}

fn native_lock_support_park(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker]
    // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
    if ctx.is_interrupted(false) {
        return Ok(None);
    }
    if let Some(yielded) = try_yield_virtual_park(ctx, None) {
        return yielded;
    }
    // AQS-PARK-PIN: this bridge only fires when a real-JDK `LockSupport.park`
    // overload is routed straight to us instead of running its bytecode (which
    // would otherwise stash `blocker` into `Thread.parkBlocker` itself via
    // `setCurrentBlocker` — see the matching fix in `NativeContextImpl::park`,
    // vm/src/vm/vm_exec.rs). Pin the explicit argument across the block so a
    // caller like `AbstractQueuedSynchronizer$ConditionNode.block()` (blocker =
    // `this`) can't lose it to the JIT register-invisibility gap regardless of
    // which path is live. Mirrors `monitor_wait_keepalive`.
    let pin = match args.first() {
        Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    ctx.park(None);
    if let Some(pin) = pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(None)
}

fn native_lock_support_park_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker, nanos(long)]
    // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
    if ctx.is_interrupted(false) {
        return Ok(None);
    }
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs. Recover via bit-reinterpret.
    let nanos: i64 = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    };
    if nanos > 0 {
        let timeout = Some(std::time::Duration::from_nanos(nanos as u64));
        if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
            return yielded;
        }
        // AQS-PARK-PIN — see `native_lock_support_park`.
        let pin = match args.first() {
            Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
            _ => None,
        };
        ctx.park(timeout);
        if let Some(pin) = pin {
            ctx.unpin_native_roots(pin);
        }
    }
    Ok(None)
}

fn native_lock_support_park_until(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker, deadline(long)] — deadline in millis since epoch
    // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
    if ctx.is_interrupted(false) {
        return Ok(None);
    }
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs. Recover via bit-reinterpret.
    let deadline: i64 = match args.get(1) {
        Some(Value::Long(d)) => *d,
        Some(Value::Double(x)) => x.to_bits() as i64,
        Some(Value::Int(x)) => *x as i64,
        _ => 0,
    };
    if deadline > 0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let remaining = (deadline - now_ms).max(0) as u64;
        let timeout = Some(std::time::Duration::from_millis(remaining));
        if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
            return yielded;
        }
        // AQS-PARK-PIN — see `native_lock_support_park`.
        let pin = match args.first() {
            Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
            _ => None,
        };
        ctx.park(timeout);
        if let Some(pin) = pin {
            ctx.unpin_native_roots(pin);
        }
    }
    Ok(None)
}

fn native_lock_support_unpark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [thread_obj]
    if let Some(Value::Object(Some(thread_obj))) = args.first() {
        ctx.unpark(*thread_obj);
    } else {
    }
    Ok(None)
}

fn native_lock_support_get_blocker(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Simplified: always return null
    Ok(Some(Value::Object(None)))
}

/// Report a caller that imposes its own field layout on a class which already
/// has a different one — in EITHER direction.
///
/// `alloc_concurrent_synthetic` does NOT truncate: both it and
/// `NativeContext::alloc_object` clamp the slot count UP to the resolved
/// class's real field count. That clamp is what keeps a `getfield` at a real
/// inherited index in bounds, and it is load-bearing. What it cannot fix is
/// the caller's intent: an object allocated as `("java/lang/Process", 3)` comes
/// back with the REAL six-slot `java.lang.Process` layout, and the caller's
/// three writes at slots 0..2 then land on `java.lang.Process`'s own
/// `outputWriter`/`outputCharset`/`inputReader` fields. Meanwhile any OTHER
/// native that reads a different synthetic layout for the same class name --
/// one offset past those six -- runs off the end of the object and its reads
/// are dropped by the heap guard.
///
/// That is two layouts on one class, and it is how Tomcat's CGI response body
/// came back empty (see
/// runtime-exec-returned-a-process-with-no-streams-FIXED-20260806.md).
///
/// `num_fields != real` is a self-discriminating test for it: a class this call
/// FABRICATED would declare exactly `num_fields` fields, so `real == num_fields`
/// and nothing is reported. Any inequality means somebody else -- the real class
/// file, or another native fabricating a different shape -- already owns the
/// layout.
///
/// **Both directions are reported, and the OVER direction is the dangerous one.**
/// Until 2026-08-11 the test was `num_fields < real`, so the instrument was blind
/// to exactly the half `docs/architecture/natives-over-real-jdk-classes.md` §5
/// calls out: *"a slot index against a real layout is not a wrong answer -- it is
/// heap corruption"*. A request WIDER than the class is the caller stating, in
/// the one place it is machine-readable, that it holds a slot map with more
/// entries than the class has fields. Two consequences, and neither is visible
/// at the allocation:
///
/// * The object comes back with `num_fields` slots while its class declares
///   `real`, so its header disagrees with `num_total_fields`. That is precisely
///   the condition `vm/src/memory/gc.rs::validate_object_sizes`
///   (`CRATONVM_DBG_VALIDATE_NEW=1`) prints as `BAD ... num_slots=N EXPECTED=M`
///   -- it was written for a JIT `new` with a wrong-size header, and this funnel
///   manufactures the same shape deliberately.
/// * The same class is then allocated in TWO widths: `real` by every real
///   bytecode `new` and by the JIT, `num_fields` here. The wide slot map is not
///   restricted to the objects this funnel made. Any native that applies it to a
///   receiver it did NOT allocate -- and these natives do receive real-JDK
///   objects, see `native_cf_complete`'s slot-1 type discriminator below -- reads
///   or writes past the end of that object.
///
/// It is reported, not refused, and one sub-population is why. `jca/kem.rs`,
/// `jca/signature.rs`, `jca/key_factory.rs` and `jca/key_agreement.rs`
/// over-allocate ON PURPOSE, through `synthetic_base_offset` (27 uses): it asks
/// `class_num_total_fields` for the real width and appends private slots ABOVE
/// it, *"so reference writes never land on a slot the real layout declares with
/// an incompatible descriptor"*. That is the correct remedy for this species,
/// and it necessarily shows up here as `over`. This funnel cannot tell it apart
/// from a hard-coded wide guess -- both arrive as one integer -- so the census
/// names both and the reader subtracts. See the ENABLED note in the body for
/// what a follow-up must measure before this could be made fatal.
///
/// `real == 0` was excluded from BOTH directions until 2026-08-12, on the
/// argument that 0 is overloaded: it means "class not loaded yet" (the reason
/// the `max` below exists at all) and it also means "genuinely no instance
/// fields" -- every interface, and `java/lang/Object`. This funnel is routinely
/// asked for interface names (`java/util/concurrent/locks/Condition`,
/// `java/util/concurrent/Flow$Subscription`), where a non-zero request is the
/// intended fabrication and not an alias.
///
/// **The argument was sound and the exclusion was still wrong**, because 0 is
/// also the ONLY value of `real` for which the `max` below does nothing -- and
/// therefore the only case in which the object handed back is genuinely NARROWER
/// than the class it is handed out as. Every `under` row describes an object the
/// clamp already widened; the short objects were all in the excluded bucket.
/// Those sites now report `direction=undeclared`, which says "this instrument
/// cannot adjudicate this allocation" instead of saying nothing, because saying
/// nothing is what a consumer reads as clean. See
/// W7-73-short-object-blind-spot.md.
///
/// Deduplicated by (class, requested, declared, site) so a hot allocation loop
/// reports once, not once per object. The site is IN the key on purpose: two
/// natives making the same mistake on the same class are two findings.
///
/// `#[track_caller]` so the site reported is the NATIVE that asked for the
/// shape: `alloc_concurrent_synthetic` is itself `#[track_caller]`, so the
/// attribute chains through it to the original call site. Without it every
/// report would name this file.
///
/// # This is a forwarder, not an implementation
///
/// The counting, the flag, the dedup key and the output channel moved to
/// `cratonvm_native_api::layout_alias` on 2026-08-12 (W7-59). They had to: this
/// funnel is busy but it is not the only allocator, and the ~200 production
/// `alloc_object` call sites in `native-builtins`, `native-io` and
/// `native-collections` that bypass it were never censused — including the live
/// owner of the widest over-allocation in the workspace. See
/// W7-59-layout-detector-coverage.md and W7-49-slot-index-recensus.md.
///
/// **Why this call still exists after the base allocator was instrumented.**
/// The base allocator sees the count this funnel passes it, and that count is
/// already clamped: `n = num_fields.max(real)`. An UNDER-request therefore
/// arrives there as `n == real` and is invisible. Reporting here, before the
/// clamp, is the only place the under direction survives. Dropping this line
/// would make the detector quieter in the direction it has reported since it
/// was written.
/// Forward to `native-api`'s uninstantiable-receiver census.
///
/// The census itself deliberately does NOT live here: it needs a process-global
/// dedup set, and `lock_discipline_ratchet` holds this crate to a raw-lock
/// baseline because this crate re-enters the VM. `native-api`'s
/// `instantiable::observe_uninstantiable_receiver` carries the whole rationale,
/// and it sits next to the `ACC_INTERFACE` / `ACC_ABSTRACT` predicate it uses.
///
/// `#[track_caller]` on every hop, so the location that reaches the census is
/// the NATIVE that asked for the shape, not this forwarding line and not
/// `try_alloc_concurrent_synthetic` in between — the same chain
/// `report_layout_alias` relies on.
#[track_caller]
fn report_uninstantiable_receiver(
    ctx: &dyn NativeContext,
    class_name: &str,
    class_id: cratonvm_types::ClassId,
) {
    let flags = ctx.class_access_flags(class_id);
    let _ = cratonvm_native_api::instantiable::observe_uninstantiable_receiver(class_name, flags);
}

#[track_caller]
fn report_layout_alias(class_name: &str, num_fields: usize, real: usize) {
    // `observe_from_rust` is `#[track_caller]` and so is this function, so the
    // location that reaches the census is the NATIVE that asked for the shape,
    // not this forwarding line and not `alloc_concurrent_synthetic` in between.
    let _ = cratonvm_native_api::layout_alias::observe_from_rust(class_name, num_fields, real);
}

// The infallible `alloc_concurrent_synthetic` twin is DELETED (JDK-only wave 2,
// step 3, 2026-08-10). Its 1,904 call sites moved to the fallible spelling on
// 2026-08-07 (attempt 5), leaving one straggler in `lookup_define.rs`; with that
// migrated it survived only as a way back to `ensure_synthetic_class`, which is
// the entry point step 3 removes.

/// Allocate a synthetic object through the workspace's busiest fabrication
/// funnel, with the refusal `--jdk-only` requires.
///
/// The deleted infallible twin reached `ensure_synthetic_class`, whose signature
/// had no error channel, so under `--jdk-only` it recorded a
/// `CompatibilityClassRequested` violation and fabricated anyway. This one goes
/// through `try_ensure_synthetic_class`, so the refusal reaches the caller as a
/// `NoClassDefFoundError` naming the class.
///
/// Under the default `Compatible` mode this is byte-for-byte what the twin did.
///
/// Every native returning `MethodCallResult` should prefer this spelling;
/// `ClassIdentityError` converts with `?`.
///
/// `#[track_caller]` so the class-origin census's `requested_by` names the
/// native that wanted the shape, not this one forwarding line — see the
/// matching note on `NativeContext::try_ensure_synthetic_class`. This funnel has
/// ~2,000 call sites, so without it the census cannot name a single one.
/// Build a Java reference array whose elements come from an ALLOCATING
/// producer, keeping the array rooted across every one of those allocations.
///
/// **The bug this exists to stop.** `let arr = ctx.new_ref_array(..); for i {
/// let el = <allocates>; ctx.set_array_element(arr, i, el) }` is wrong: `arr`
/// is a raw `ObjectRef`, the producer can trigger a moving young collection,
/// and every `set_array_element` after that point writes through a stale
/// reference — silently DROPPED by the heap guard. The live array keeps
/// whatever the collector left in those slots.
///
/// It is not a theoretical hazard. Both `getAcceptedIssuers` implementations
/// had exactly this shape, and netty's `ParameterizedSslHandlerTest` saw both
/// of its faces intermittently through
/// `ReferenceCountedOpenSslServerContext.newSessionContext`:
/// `IllegalArgumentException: Null element in chain: [null × 32]`, and
/// `NoSuchMethodError: sun.security.util.DerValue.getEncoded()` — a `DerValue`
/// left in a vacated slot by the certificate parsing the producer had just
/// done.
///
/// `make` is handed the context and the index and must return the element; if
/// IT allocates after building the element, IT must pin the element (see
/// `keystore::make_x509_mirror`, which does). An `Err` stops the fill and
/// propagates, after the array is unpinned.
pub(crate) fn build_rooted_ref_array<F>(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    len: usize,
    mut make: F,
) -> Result<ObjectRef, MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, usize) -> Result<ObjectRef, MethodCallFailed>,
{
    let arr0 = ctx.new_ref_array(class_id, len);
    let pin = ctx.pin_native_root(arr0);
    let mut arr = arr0;
    let mut failure = None;
    for i in 0..len {
        match make(ctx, i) {
            Ok(element) => {
                arr = ctx.read_native_pin(pin, arr0);
                ctx.set_array_element(arr, i, cratonvm_types::Value::Object(Some(element)));
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    match failure {
        Some(e) => Err(e),
        None => Ok(arr),
    }
}

#[track_caller]
pub(crate) fn try_alloc_concurrent_synthetic(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.ensure_class_initialized(class_name) {
        Ok(class_id) => {
            let resolved_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            let cid = if resolved_name == class_name || class_name == "java/lang/Object" {
                class_id
            } else {
                // Some class-loading fallbacks report success with Object's
                // ClassId for helper/interface-like synthetic classes. Keep
                // the requested identity so field writes and native dispatch
                // use the helper layout instead of Object's zero-slot layout.
                match ctx.class_id_by_name(class_name) {
                    Some(id) => id,
                    None => refused_class(ctx, class_name, num_fields)?,
                }
            };
            // In real-JDK mode the loaded class's actual instance-field count
            // often exceeds the synthetic-mode hard-coded number. Allocating
            // with too few slots causes out-of-bounds field access later (KC16
            // bootstrap tripped this on ClassId 355/359 with index=4 vs
            // num_slots=3). Use the larger of the two so both paths have enough
            // room. 0 means the class isn't loaded yet — keep the caller's
            // requested size.
            let real = ctx.class_num_total_fields(cid);
            // `!=`, not `<`, since 2026-08-11 (JDK-only lane W4-4).
            //
            // Call the SHARED rule; do not re-derive it. This line read
            // `if num_fields > 0 && real > 0 && num_fields != real` until
            // 2026-08-12, which is `layout_alias::classify` open-coded — a second
            // implementation of the one primitive
            // W7-59-layout-detector-coverage.md said it had eliminated ("there is
            // one implementation here"). It had eliminated the second copy of the
            // reporting MACHINERY and left a second copy of the DECISION, and the
            // two then drifted on the case that matters: `real > 0` is exactly
            // the `declared == 0` exclusion W7-73-short-object-blind-spot.md
            // removed, so this funnel — the busiest allocator in the workspace —
            // would have stayed blind to the short-object species after
            // `classify` learned to report it.
            //
            // Strictly louder: `classify` returns `Some` for every input this
            // predicate accepted, plus `Undeclared` for `real == 0`.
            if cratonvm_native_api::layout_alias::classify(num_fields, real).is_some() {
                report_layout_alias(class_name, num_fields, real);
            }
            report_uninstantiable_receiver(ctx, class_name, cid);
            // ALLOCATION IS UNCHANGED by the widening above: still `max`, so an
            // over-request still gets the slots it asked for and an under-request
            // is still clamped up. Reporting and refusing are separate changes and
            // this lane makes only the first -- the over-allocating population has
            // never been counted (see the ENABLED note), and a funnel with ~2,000
            // call sites is not where you discover that number by failing.
            let n = num_fields.max(real);
            // `try_alloc_object_gc_safe` first (proactively collects, then walks
            // young -> old gen without aborting): this is the shared allocator
            // behind `java.net.URI`, `HttpURLConnection`, and many other
            // synthetic native objects — a gdb backtrace confirmed
            // TestResponsePerformance's doUri() hot loop (`new URI(...)` x
            // 1,000,000) hard-aborted the whole process here on young-gen
            // exhaustion. Falling back to the aborting `alloc_object` only if
            // the GC-safe path still reports genuine exhaustion (both
            // generations full even after a fresh collection) keeps that now
            // much narrower case behaving as it did.
            Ok(ctx
                .try_alloc_object_gc_safe(cid, n)
                .unwrap_or_else(|| ctx.alloc_object(cid, n)))
        }
        Err(_) => {
            // The real `.class` file could not be loaded. Allocating with
            // `ClassId::new(0)` (`java/lang/Object`, zero declared fields) but a
            // non-zero slot count produces an "undersized object layout" object
            // — the GC's `get_field` bounds guard rejects every field access on
            // it. Ask the policy for a synthetic class declaring `num_fields`
            // instance fields, so the header's `class_id` matches the allocated
            // slot count; under `--jdk-only` that ask is refused instead.
            let cid = refused_class(ctx, class_name, num_fields)?;
            Ok(ctx.alloc_object(cid, num_fields))
        }
    }
}

/// Where a native's PRIVATE slot map starts on an instance of `class_name`.
///
/// Zero when the class is a fabricated stub (its fields are `_f0.._fN` and the
/// synthetic map IS the layout); otherwise the real class's transitive declared
/// field count, so every private slot lands ABOVE every field the real layout
/// declares. This is the idiom `jca/kem.rs::synthetic_base_offset` uses, with
/// the stub arm added: without it the base RATCHETS in synthetic-JDK mode,
/// because the first allocation fabricates a class declaring `base + width`
/// fields and the next call reads that number back as the new base, so two
/// objects of one class end up with two different slot maps in one run.
/// `is_class_synthetic_stub` is stable under that — a stub stays a stub.
///
/// Read W7-49-slot-index-recensus.md for why the alternative — guessing the
/// layout from the object's own slot count — cannot work for a receiver this
/// native did not allocate.
///
/// **The body moved to `cratonvm_native_api::appended_slots` on 2026-08-12**
/// and this is now a forwarder, not a second copy. W7-68-live-under-allocations.md
/// found the `under` direction's live cases in `native-io`
/// (`java/nio/channels/FileChannel`, `java/nio/MappedByteBuffer`), which cannot
/// reach a `pub(crate)` helper in this crate; copying it here would have made
/// it the sixteenth private re-implementation of a primitive
/// W7-59-layout-detector-coverage.md §2.1 already counted fifteen copies of.
/// Kept as a name because this crate's call sites read better with it and
/// because deleting it would churn them for nothing.
pub(crate) fn appended_slot_base_for_class(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
    cratonvm_native_api::appended_slots::base_for_class(ctx, class_name)
}

/// Allocate `class_name` carrying `width` private slots appended above the real
/// layout, and hand back the base those slots start at.
///
/// The pair (object, base) is what makes the write in-bounds AND non-aliasing:
/// the object is `base + width` slots wide, so `base + i` for `i < width` is
/// inside it, and no `base + i` collides with a field the real class declares.
/// Contrast the shape this replaces — allocate `width` slots on a class that
/// declares `real > 0` fields and write `0..width`, which puts the native's
/// `Int` into whatever reference the real layout declares at slot 0.
#[track_caller]
pub(crate) fn try_alloc_with_appended_slots(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    width: usize,
) -> Result<(ObjectRef, usize), MethodCallFailed> {
    let base = appended_slot_base_for_class(ctx, class_name);
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, base + width)?;
    let carried = ctx.object_num_fields(obj);
    debug_assert!(
        carried >= base + width,
        "appended-slot allocation of {class_name} came back with {carried} \
         slots, needed base {base} + width {width}"
    );
    Ok((obj, base))
}

// NOT PROVIDED, and the omission is deliberate: a `base_for_this_receiver`
// companion (W7-49, 2026-08-12). It was written, its only caller was reverted,
// and it is not left here unused — but the reason it cannot exist usefully is
// worth keeping, because it is the first thing the next reader will reach for.
//
// Given an object this native did NOT allocate, "how many private slots does it
// carry" is not answerable from its width. A real-layout instance of the exact
// class is narrow and can be refused; a real SUBCLASS instance is wide, for its
// own reasons, and `width - real` lands squarely inside its own fields. So such
// a helper can only ever refuse the narrow case, which is the easy half. The
// sound remedy for foreign receivers is a side table keyed on object identity —
// `jca/key_factory.rs` already runs one, with the GC-stable key that lane had to
// invent when the raw `ObjectRef` address aliased across a young collection.

/// `try_ensure_synthetic_class`, with the refusal converted to a **catchable**
/// Java throwable.
///
/// The plain `?` conversion yields `MethodCallFailed::InternalError`, which the
/// exception model defines as uncatchable and fatal — the wrong shape for a
/// policy refusal. Contract §5 asks for the specification's
/// `NoClassDefFoundError`, which is what `refusal_to_java_failure` builds.
///
/// `pub(crate)` since 2026-08-10 (JDK-only wave 2, step 3): the crate's
/// remaining direct `ensure_synthetic_class` callers — the enterprise-shim
/// `alloc_object_for` helpers, `alloc_impl`, the array-element class lookups in
/// `lang_string`/`keystore`/`regex_matcher` — each needed the same three lines,
/// and three per-site copies of a conversion is how the two spellings drift
/// apart. One idiom, one place.
#[track_caller]
pub(crate) fn refused_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<cratonvm_types::ClassId, MethodCallFailed> {
    match ctx.try_ensure_synthetic_class(class_name, num_fields) {
        Ok(id) => Ok(id),
        Err(err) => Err(cratonvm_native_api::refusal_to_java_failure(ctx, err)),
    }
}

/// The synthetic `java.util.concurrent.CyclicBarrier` surface.
///
/// Registered from TWO places, because two different conditions need it and
/// neither can see the other:
///
///  * [`register_concurrent_natives`], when `CRATONVM_SYNTHETIC_AQS` is set —
///    the real `CyclicBarrier` bytecode is present but its `ReentrantLock` /
///    `Condition` are being served synthetically, so the barrier is served
///    synthetically too;
///  * `vm_init`'s synthetic-JDK arm, where there IS no real bytecode. This one
///    was missing. `67c5e048c` narrowed the whole surface to the env flag, on
///    the reasoning that the default real-JDK build should run the real class —
///    correct for that build, but synthetic-JDK mode has only a 3-field
///    compatibility STUB for `CyclicBarrier` (`class_manager`'s
///    `synthetic_stub_fields`) and no method bodies at all, so it lost the
///    constructor outright: all four `JucComplete` barrier fixtures went to
///    `NoSuchMethodError: java.util.concurrent.CyclicBarrier.<init>(I)V` while
///    the TCK table still listed them as passing. A flag is not a mode
///    ([`crate::nbflags`] cannot see the JDK mode; `vm_init` can), which is
///    exactly why the call lives there and not behind another `nbflags` test.
///
/// Deleting these natives instead — the standing preference for synthetic
/// shadows of pure-Java JDK classes — is not available for the same reason:
/// synthetic-JDK mode has nothing to fall back to.
pub fn register_cyclic_barrier_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let cb = "java/util/concurrent/CyclicBarrier";
    registry.register(cb, "<init>", "(I)V", native_cb_init);
    registry.register(
        cb,
        "<init>",
        "(ILjava/lang/Runnable;)V",
        native_cb_init_action,
    );
    registry.register(cb, "await", "()I", native_cb_await);
    registry.register(
        cb,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)I",
        native_cb_await_timeout,
    );
    registry.register(cb, "getParties", "()I", native_cb_get_parties);
    registry.register(cb, "getNumberWaiting", "()I", native_cb_get_number_waiting);
    registry.register(cb, "isBroken", "()Z", native_cb_is_broken);
    registry.register(cb, "reset", "()V", native_cb_reset);
    registry.set_category(__prev_cat);
}

pub fn register_concurrent_natives(registry: &mut NativeMethodRegistry) {
    // Real AQS is now the DEFAULT: skip the synthetic ReentrantLock/Lock/
    // Condition natives so the REAL java.util.concurrent AQS bytecode runs
    // instead. The synthetic implementation (rl_state side-table + object-
    // monitor conditions) deadlocks under the blocking producer/consumer
    // pattern (ArrayBlockingQueue/LinkedBlockingQueue take/put with waiters
    // that actually block) — most visibly it leaves ThreadPoolExecutor workers
    // blocked in workQueue.take() so they never terminate after shutdown()
    // (interruptIdleWorkers never wakes them through the synthetic
    // lock/condition), which leaks the pool's non-daemon threads and hangs
    // every Elasticsearch AbstractWireTestCase (testConcurrentSerialization)
    // and the Gradle test worker. Real AQS uses Unsafe CAS + LockSupport.park/
    // unpark, all of which CratonVM implements; with it the full
    // ByteSizeValueTests suite reaches HotSpot parity (42/42, clean exit).
    // Opt OUT with CRATONVM_SYNTHETIC_AQS=1 (legacy synthetic lock/condition).
    // CountDownLatch/CyclicBarrier synthetic natives below are unaffected.
    let real_aqs = !crate::nbflags().synthetic_aqs || crate::nbflags().real_aqs;
    if !real_aqs {
        register_synthetic_aqs_natives(registry);
    } // end if !real_aqs

    // JDK-ONLY-CLASSIFY: stub — the CountDownLatch and CyclicBarrier blocks
    // below (14 registrations). `java.util.concurrent` is pure Java: JDK 25
    // declares no `ACC_NATIVE` method on either class, so contract §1.5 cannot
    // call these bridges. `SyntheticStub` is also what they carry today — it is
    // what the ambient category happened to hold when this function ran — so
    // stating it changes no kind. What it changes is that the kind is no longer
    // a property of whoever called us.
    //
    // These fourteen were among the 45 registrations that turned out to have NO
    // category scope over them at all once `current_category` became an
    // `Option`. Before that, `category_chosen` was sticky: it was set by the
    // first `set_category` in boot and never cleared, so every later
    // registration reported "chosen" and this hole was invisible.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);

    // --- CountDownLatch ---
    let cdl = "java/util/concurrent/CountDownLatch";
    registry.register(cdl, "<init>", "(I)V", native_cdl_init);
    registry.register(cdl, "countDown", "()V", native_cdl_count_down);
    registry.register(cdl, "await", "()V", native_cdl_await);
    registry.register(
        cdl,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_cdl_await_timeout,
    );
    registry.register(cdl, "getCount", "()J", native_cdl_get_count);
    registry.register(
        cdl,
        "toString",
        "()Ljava/lang/String;",
        native_cdl_to_string,
    );

    // --- Semaphore ---
    // See the matching early-registration guard above.  In the default real
    // AQS mode, Semaphore must keep its real `sync: Semaphore$Sync` field so
    // protected AQS operations invoked by third-party subclasses remain sound.
    // Semaphore's synthetic natives live in `register_synthetic_aqs_natives`
    // alongside the lock ones; both are registered together above.

    // --- CyclicBarrier ---
    //
    // Synthetic-AQS mode only HERE. With real AQS (the default) the real JDK
    // `CyclicBarrier` — ReentrantLock + Condition + an identity-compared
    // `Generation` — is correct and needs no help, exactly as for
    // ReentrantLock/Lock/Condition and Semaphore above. The constructors are
    // gated with the rest, not separately: `native_cb_init` stores its state
    // holder in the receiver's slot 0, which is the real layout's `lock` field,
    // so registering only the constructors while `await()` runs real bytecode
    // would hand that bytecode a barrier whose `lock` is an array.
    //
    // Synthetic-JDK mode registers the same set from `vm_init`, where the mode
    // is known — see [`register_cyclic_barrier_natives`].
    if !real_aqs {
        register_cyclic_barrier_natives(registry);
    }
    registry.set_category(__prev_cat);

    // --- CopyOnWriteArrayList (M18) ---
    // Two supported layouts:
    //   - Real JDK `CopyOnWriteArrayList`: slot 0 = `lock` (Object),
    //     slot 1 = `array` (Object[]).  `size()` is `array.length`;
    //     there is no separate size field.
    //   - Legacy synthetic stub layout (used when the real classfile
    //     wasn't on the runtime classpath): slot 0 = backing array,
    //     slot 1 = int size.
    //
    // The pre-fix shims assumed the synthetic layout unconditionally,
    // which on a real COWAL clobbered the `lock` monitor and stored
    // the backing array into the wrong slot.  Spring 6.x's
    // `AbstractBeanFactory$BeanPostProcessorCacheAwareList` extends
    // COWAL, so every BeanPostProcessor add became a no-op-from-the-
    // outside (the underlying array stayed null/empty), and Spring's
    // `applyBeanPostProcessorsBeforeInitialization` iterated to an
    // empty collection.  Net effect: `ApplicationContextAwareProcessor`
    // never invoked `setResourceLoader` on
    // `SharedMetadataReaderFactoryBean`, its `metadataReaderFactory`
    // field stayed null, `getObject()` returned null, and Spring's
    // `RuntimeBeanReference` resolution wrapped the null in a
    // `NullBean` — surfacing as
    //   `BeanCreationException ... Property 'metadataReaderFactory'
    //    threw exception: ... MetadataReaderFactory must not be null`
    // on `internalConfigurationAnnotationProcessor`.
    //
    // The helpers below resolve `array` / `size` by field name first
    // (real-JDK path) and only fall back to slot 0/1 when the real
    // classfile didn't define those fields.  All COWAL natives below
    // use these helpers so they work for both layouts without further
    // per-method branching.
    //
    // FIX (2026-07-14, java.home/Locale bootstrap regression family): pin
    // these to Bridge explicitly. This whole block (like
    // register_properties_sidetable's) inherited whatever category was
    // ambient at this function's call site instead of declaring its own,
    // and was silently getting SyntheticStub in real-JDK mode. Every
    // mutator here (add/set/remove/clear/addIfAbsent) is a permanent
    // bridge, not an approximation: `<init>()V` is left to run real JDK
    // bytecode on purpose (see the comment below) so `this.lock` gets
    // properly constructed, but real JDK's own `add`/`set`/`remove`/`clear`
    // bytecode (`getfield lock; monitorenter`, confirmed via `javap`) is
    // what these natives exist to bypass — dropping them made every
    // mutating call on a real COWAL instance fall through to bytecode that
    // is otherwise fine but exposed a `this.lock` NPE downstream in
    // Spring's own COWAL-backed listener/post-processor lists once the
    // ByteBuffer/CodingErrorAction stubs (this same file) were confirmed
    // safe to drop and the Properties fix above unmasked deeper bootstrap
    // progress. Found via CRATONVM_DBG_DROPPED_STUBS during a live
    // CrossOriginAnnotationIntegrationTests run.
    registry.with_category(cratonvm_native_api::NativeKind::Bridge, |registry| {
        let cowal = "java/util/concurrent/CopyOnWriteArrayList";
        // We removed the bytecode `<init>` override entirely so the real
        // JDK constructor runs and properly initialises `lock` and
        // `array` (=EMPTY_ELEMENTDATA).  The previous override delegated
        // to `native_al_init`, which wrote `elementData=Object[8]` and
        // `size=0` — i.e. wrote a non-null Object[] into slot 0 (the
        // `lock` field) and Int(0) into slot 1 (the `array` field).
        // That made subsequent reads of `array` (a real `Object[]`-typed
        // volatile) return `Value::Int(0)`, which `iterator()` then
        // misinterpreted as "no backing array" and produced an empty
        // iteration.

        /// Read (array, size) from a COWAL receiver, supporting both real
        /// and synthetic layouts.
        fn cowal_read_state(
            ctx: &dyn NativeContext,
            this: ObjectRef,
        ) -> (Option<ObjectRef>, usize) {
            const COWAL_CLASS: &str = "java/util/concurrent/CopyOnWriteArrayList";
            if let Some(slot) = ctx.resolve_field_index(COWAL_CLASS, "array") {
                match ctx.get_field(this, slot) {
                    Value::Object(Some(a)) => {
                        let n = ctx.array_length(a);
                        return (Some(a), n);
                    }
                    _ => return (None, 0),
                }
            }
            // Synthetic-stub fallback (legacy layout).
            let sz = match ctx.get_field(this, 1) {
                Value::Int(n) => n.max(0) as usize,
                _ => 0,
            };
            let d = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => Some(a),
                _ => None,
            };
            (d, sz)
        }

        /// Write a new backing array into a COWAL receiver (size is implicit
        /// from `array.length` in real layout). For synthetic layout, also
        /// update the int size slot.
        fn cowal_write_array(ctx: &mut dyn NativeContext, this: ObjectRef, new_arr: ObjectRef) {
            const COWAL_CLASS: &str = "java/util/concurrent/CopyOnWriteArrayList";
            if let Some(slot) = ctx.resolve_field_index(COWAL_CLASS, "array") {
                ctx.set_field(this, slot, Value::Object(Some(new_arr)));
                return;
            }
            // Synthetic-stub fallback: write to slot 0/1.
            let len = ctx.array_length(new_arr) as i32;
            ctx.set_field(this, 0, Value::Object(Some(new_arr)));
            ctx.set_field(this, 1, Value::Int(len));
        }

        /// Compare a stored COWAL element against a search target with Java
        /// `equals` semantics. Real `CopyOnWriteArrayList.indexOf`/`contains`
        /// use `target.equals(elem)`, **not** reference identity. Spring's
        /// `MutablePropertySources.precedenceOf` / `assertPresentAndGetIndex`
        /// search with `PropertySource.named(name)` — a distinct
        /// `ComparisonPropertySource` equal only by name — so an identity-only
        /// comparison wrongly returned -1 (StandardEnvironmentTests
        /// `propertySourceOrder`, MutablePropertySourcesTests `test`).
        fn cowal_element_matches(ctx: &mut dyn NativeContext, elem: Value, target: Value) -> bool {
            if elem == target {
                return true;
            }
            if let (Value::Object(Some(t)), Value::Object(Some(e))) = (target, elem) {
                if let Ok(Some(Value::Int(v))) = ctx.invoke_virtual(
                    t,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(e))],
                ) {
                    return v != 0;
                }
            }
            false
        }
        /// Allocate the copy-on-write destination array with everything that
        /// must survive it PINNED, and hand back their post-allocation
        /// addresses.
        ///
        /// `ctx.new_array` ALLOCATES, and an allocation is a collection point.
        /// Every mutator below then reads the OLD array, writes the receiver's
        /// `array` field and releases the receiver's monitor — so all three
        /// have to be re-read afterwards. A stale `old` copies out of memory
        /// the sweep may have reclaimed; a stale `this` writes the swap into
        /// it; and a stale monitor makes `MonitorTable::exit` dereference a
        /// dead header, which is an `EXCEPTION_ACCESS_VIOLATION` when the young
        /// slot was reclaimed and a PERMANENTLY LEAKED lock when it merely
        /// moved (every later writer on that list then blocks forever).
        fn cowal_pinned_new_array(
            ctx: &mut dyn NativeContext,
            len: usize,
            this: &mut ObjectRef,
            old_arr: &mut Option<ObjectRef>,
            elem: &mut Value,
        ) -> ObjectRef {
            let base = ctx.pin_native_root(*this);
            let old_h = old_arr.map(|a| ctx.pin_native_root(a));
            let elem_h = match *elem {
                Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                _ => None,
            };
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
            *this = ctx.read_native_pin(base, *this);
            if let (Some(a), Some(h)) = (*old_arr, old_h) {
                *old_arr = Some(ctx.read_native_pin(h, a));
            }
            if let (Value::Object(Some(o)), Some(h)) = (*elem, elem_h) {
                *elem = Value::Object(Some(ctx.read_native_pin(h, o)));
            }
            ctx.unpin_native_roots(base);
            new_arr
        }

        /// First index whose element matches `needle`, with the scan's three
        /// references kept current across `cowal_element_matches`.
        ///
        /// That helper calls `Object.equals` — USER code, so it allocates and
        /// can collect on every turn. The loops that used to inline this scan
        /// held `this`, the array and the needle as pre-scan addresses and
        /// dereferenced all three on the next turn.
        fn cowal_find_pinned(
            ctx: &mut dyn NativeContext,
            this: &mut ObjectRef,
            old_arr: &mut Option<ObjectRef>,
            needle: &mut Value,
            size: usize,
        ) -> Option<usize> {
            let base = ctx.pin_native_root(*this);
            let old_h = old_arr.map(|a| ctx.pin_native_root(a));
            let needle_h = match *needle {
                Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                _ => None,
            };
            let mut found = None;
            for i in 0..size {
                let Some(old) = *old_arr else { break };
                let elem = ctx.get_array_element(old, i);
                let n = *needle;
                let matched = cowal_element_matches(ctx, elem, n);
                *this = ctx.read_native_pin(base, *this);
                if let (Some(a), Some(h)) = (*old_arr, old_h) {
                    *old_arr = Some(ctx.read_native_pin(h, a));
                }
                if let (Value::Object(Some(o)), Some(h)) = (*needle, needle_h) {
                    *needle = Value::Object(Some(ctx.read_native_pin(h, o)));
                }
                if matched {
                    found = Some(i);
                    break;
                }
            }
            ctx.unpin_native_roots(base);
            found
        }

        // Reads — re-routed to use real-COWAL layout when available.  The
        // previous registrations delegated to `native_al_*` which assumed
        // ArrayList slot semantics; on a real COWAL receiver they read the
        // wrong fields and returned `size=0` regardless of contents.
        registry.register(cowal, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let (_, size) = cowal_read_state(ctx, this);
            Ok(Some(Value::Int(size as i32)))
        });
        registry.register(cowal, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let (_, size) = cowal_read_state(ctx, this);
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
        });
        registry.register(cowal, "get", "(I)Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let raw_idx = match args.get(1) {
                Some(Value::Int(i)) => *i,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (data, size) = cowal_read_state(ctx, this);
            // HotSpot 25 measured (`repro/ListItrEndRepro.java`): every
            // out-of-range absolute accessor on this class throws
            // `ArrayIndexOutOfBoundsException` -- it indexes its `array`
            // directly. Returning `null` instead let `get(size())` and
            // `get(-1)` read as "the element there is null"; the `as usize`
            // this replaces also wrapped a negative index to a huge value, so
            // both took the same silent path.
            if raw_idx < 0 || raw_idx as usize >= size {
                return Err(
                    cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                        index: raw_idx,
                        message: Some(format!("Index {raw_idx} out of bounds for length {size}")),
                    }
                    .into(),
                );
            }
            let idx = raw_idx as usize;
            Ok(Some(
                data.map(|a| ctx.get_array_element(a, idx))
                    .unwrap_or(Value::Object(None)),
            ))
        });
        registry.register(cowal, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (mut data, size) = cowal_read_state(ctx, this);
            let found = cowal_find_pinned(ctx, &mut this, &mut data, &mut needle, size);
            Ok(Some(Value::Int(i32::from(found.is_some()))))
        });
        registry.register(cowal, "indexOf", "(Ljava/lang/Object;)I", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let mut needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (mut data, size) = cowal_read_state(ctx, this);
            let found = cowal_find_pinned(ctx, &mut this, &mut data, &mut needle, size);
            Ok(Some(Value::Int(found.map_or(-1, |i| i as i32))))
        });
        registry.register(cowal, "toArray", "()[Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (mut data, size) = cowal_read_state(ctx, this);
            // `new_array` collects; `data` is dereferenced right after it.
            let mut no_elem = Value::Object(None);
            let out = cowal_pinned_new_array(ctx, size, &mut this, &mut data, &mut no_elem);
            if let Some(arr) = data {
                for i in 0..size {
                    ctx.set_array_element(out, i, ctx.get_array_element(arr, i));
                }
            }
            Ok(Some(Value::Object(Some(out))))
        });
        // Iterator returns a snapshot — safe for concurrent iteration.
        //
        // Two layouts are supported:
        // - Real JDK `CopyOnWriteArrayList`: instance slot 0 is the `lock`
        //   monitor (a `java/lang/Object`), slot 1 is the volatile `array`
        //   (`Object[]`). `size()` is `array.length`.
        // - Legacy synthetic stub (when the real classfile wasn't on the
        //   classpath): slot 0 is the backing array, slot 1 is an `int`
        //   size. We keep this path as the fallback.
        //
        // The original implementation read slot 0 as the data array
        // unconditionally, which on a real COWAL pulled the `lock` monitor
        // out — coerced to `Value::Int`, that produces `0`. Consumers
        // (`ArrayList$Itr.hasNext`) then saw `size=0` and the iteration ran
        // zero loops.  The user-visible blast radius is large: Spring
        // 6.x's `AbstractBeanFactory$BeanPostProcessorCacheAwareList`
        // extends COWAL, so `applyBeanPostProcessorsBeforeInitialization`
        // iterated to an empty collection. `ApplicationContextAwareProcessor`
        // never ran on `SharedMetadataReaderFactoryBean`, its
        // `setResourceLoader` was skipped, `getObject()` returned null,
        // and `ConfigurationClassPostProcessor.setMetadataReaderFactory`
        // failed `Assert.notNull` with
        //   `IllegalArgumentException: MetadataReaderFactory must not be null`
        // — surfacing as a `BeanCreationException` on
        // `internalConfigurationAnnotationProcessor` before Spring Boot
        // could finish bootstrapping its main config class.
        registry.register(cowal, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (mut data_opt, size) = cowal_read_state(ctx, this);
            // Copy into snapshot array (matches COWAL semantics: writes after
            // iterator creation do not affect what the iterator sees).
            // `new_array` collects, and `data_opt` is read out right after it.
            let mut no_elem = Value::Object(None);
            let snap = cowal_pinned_new_array(ctx, size, &mut this, &mut data_opt, &mut no_elem);
            if let Some(data) = data_opt {
                for i in 0..size {
                    let elem = ctx.get_array_element(data, i);
                    ctx.set_array_element(snap, i, elem);
                }
            }
            // Return the REAL `COWIterator` over that snapshot. It is the same
            // array-plus-cursor shape the generic helper builds, so this is one
            // allocation and two name-resolved field writes either way; the
            // difference is that `getClass()` now says
            // `java.util.concurrent.CopyOnWriteArrayList$COWIterator` instead of
            // naming an iterator that belongs to a different collection
            // (`probes/SnapshotIteratorShapeProbe`, line `cowal.class`).
            //
            // `remove()` still throws `UnsupportedOperationException` — now out
            // of the real class's own body rather than by our arranging it —
            // matching real COWAL, which never supports removal
            // (MutablePropertySourcesTests `iteratorContainsPropertySource`).
            // The wrapper before this one reused the mutating
            // `ArrayList$Itr.remove` native, so `it.remove()` silently succeeded
            // instead of throwing.
            match cratonvm_native_collections::alloc_real_snapshot_iterator_of(
                ctx,
                "java/util/concurrent/CopyOnWriteArrayList$COWIterator",
                "snapshot",
                snap,
            ) {
                Some(itr) => Ok(Some(Value::Object(Some(itr)))),
                // No real `COWIterator` in this image (`synthetic-jdk`) — keep
                // the generic snapshot iterator, which iterates correctly and
                // also throws from `remove()`.
                None => cratonvm_native_collections::make_iterator_from_array(ctx, snap, size),
            }
        });
        // Writes — true copy-on-write: copy array, mutate copy, swap reference.
        // Uses `cowal_read_state` / `cowal_write_array` so both real and
        // synthetic COWAL layouts work.
        registry.register(
            cowal,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let raw_idx = match args.get(1) {
                    Some(Value::Int(i)) => *i,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let mut new_val = args.get(2).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let (mut old_arr, size) = cowal_read_state(ctx, this);
                // See the `get` registration above for why this throws rather
                // than answering `null`.
                if raw_idx < 0 || raw_idx as usize >= size {
                    ctx.monitor_exit(this);
                    return Err(
                        cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                            index: raw_idx,
                            message: Some(format!(
                                "Index {raw_idx} out of bounds for length {size}"
                            )),
                        }
                        .into(),
                    );
                }
                let idx = raw_idx as usize;
                let new_arr =
                    cowal_pinned_new_array(ctx, size, &mut this, &mut old_arr, &mut new_val);
                let mut old_val = Value::Object(None);
                if let Some(old) = old_arr {
                    for i in 0..size {
                        let v = ctx.get_array_element(old, i);
                        if i == idx {
                            old_val = v;
                            ctx.set_array_element(new_arr, i, new_val);
                        } else {
                            ctx.set_array_element(new_arr, i, v);
                        }
                    }
                }
                cowal_write_array(ctx, this, new_arr);
                ctx.monitor_exit(this);
                Ok(Some(old_val))
            },
        );
        registry.register(cowal, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (mut old_arr, size) = cowal_read_state(ctx, this);
            let new_arr =
                cowal_pinned_new_array(ctx, size + 1, &mut this, &mut old_arr, &mut elem);
            if let Some(old) = old_arr {
                for i in 0..size {
                    ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                }
            }
            ctx.set_array_element(new_arr, size, elem);
            cowal_write_array(ctx, this, new_arr);
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(
            cowal,
            "addIfAbsent",
            "(Ljava/lang/Object;)Z",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let mut elem = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let (mut old_arr, size) = cowal_read_state(ctx, this);
                if cowal_find_pinned(ctx, &mut this, &mut old_arr, &mut elem, size).is_some() {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
                let new_arr =
                    cowal_pinned_new_array(ctx, size + 1, &mut this, &mut old_arr, &mut elem);
                if let Some(old) = old_arr {
                    for i in 0..size {
                        ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                    }
                }
                ctx.set_array_element(new_arr, size, elem);
                cowal_write_array(ctx, this, new_arr);
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            },
        );
        registry.register(cowal, "add", "(ILjava/lang/Object;)V", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let raw_idx = match args.get(1) {
                Some(Value::Int(i)) => *i,
                _ => return Ok(None),
            };
            let mut elem = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (mut old_arr, size) = cowal_read_state(ctx, this);
            // `add(int, E)` uses `rangeCheckForAdd`, so `index == size` is
            // legal and the exception is the PLAIN `IndexOutOfBoundsException`
            // (measured on HotSpot 25), unlike the absolute accessors above.
            // This clamped with `idx.min(size)` instead and silently APPENDED:
            // `cowal.add(9, "z")` on a 2-element list returned normally and
            // left `[a, b, z]`.
            if raw_idx < 0 || raw_idx as usize > size {
                ctx.monitor_exit(this);
                return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
                    "Index: {raw_idx}, Size: {size}"
                ))
                .into());
            }
            let idx = raw_idx as usize;
            let new_arr =
                cowal_pinned_new_array(ctx, size + 1, &mut this, &mut old_arr, &mut elem);
            if let Some(old) = old_arr {
                for i in 0..idx.min(size) {
                    ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                }
                ctx.set_array_element(new_arr, idx.min(size), elem);
                for i in idx..size {
                    ctx.set_array_element(new_arr, i + 1, ctx.get_array_element(old, i));
                }
            } else {
                ctx.set_array_element(new_arr, 0, elem);
            }
            cowal_write_array(ctx, this, new_arr);
            ctx.monitor_exit(this);
            Ok(None)
        });
        registry.register(cowal, "remove", "(I)Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let raw_idx = match args.get(1) {
                Some(Value::Int(i)) => *i,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let (mut old_arr, size) = cowal_read_state(ctx, this);
            // See the `get` registration above.
            if raw_idx < 0 || raw_idx as usize >= size {
                ctx.monitor_exit(this);
                return Err(
                    cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                        index: raw_idx,
                        message: Some(format!("Index {raw_idx} out of bounds for length {size}")),
                    }
                    .into(),
                );
            }
            let idx = raw_idx as usize;
            let mut no_elem = Value::Object(None);
            let new_arr =
                cowal_pinned_new_array(ctx, size - 1, &mut this, &mut old_arr, &mut no_elem);
            let mut removed = Value::Object(None);
            if let Some(old) = old_arr {
                for i in 0..idx {
                    ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                }
                removed = ctx.get_array_element(old, idx);
                for i in (idx + 1)..size {
                    ctx.set_array_element(new_arr, i - 1, ctx.get_array_element(old, i));
                }
            }
            cowal_write_array(ctx, this, new_arr);
            ctx.monitor_exit(this);
            Ok(Some(removed))
        });
        // remove(Object)Z — remove first occurrence by value.
        registry.register(cowal, "remove", "(Ljava/lang/Object;)Z", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut needle = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (mut old_arr, size) = cowal_read_state(ctx, this);
            let found_idx = cowal_find_pinned(ctx, &mut this, &mut old_arr, &mut needle, size);
            if let Some(idx) = found_idx {
                let new_arr =
                    cowal_pinned_new_array(ctx, size - 1, &mut this, &mut old_arr, &mut needle);
                if let Some(old) = old_arr {
                    for i in 0..idx {
                        ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                    }
                    for i in (idx + 1)..size {
                        ctx.set_array_element(new_arr, i - 1, ctx.get_array_element(old, i));
                    }
                }
                cowal_write_array(ctx, this, new_arr);
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            } else {
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(0)))
            }
        });
        registry.register(cowal, "clear", "()V", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            ctx.monitor_enter(this);
            let mut none_arr: Option<ObjectRef> = None;
            let mut no_elem = Value::Object(None);
            let new_arr = cowal_pinned_new_array(ctx, 0, &mut this, &mut none_arr, &mut no_elem);
            cowal_write_array(ctx, this, new_arr);
            ctx.monitor_exit(this);
            Ok(None)
        });
    });
}

// ===========================================================================
// M18: ConcurrentHashMap atomic ops, LinkedBlockingQueue, ArrayBlockingQueue
// ===========================================================================

pub(crate) fn register_m18_concurrent_fixes(registry: &mut NativeMethodRegistry) {
    // --- ConcurrentHashMap: monitor-wrapped atomic operations ---
    let chm = "java/util/concurrent/ConcurrentHashMap";

    // <init> NOT overridden — let real JDK ConcurrentHashMap constructor run.
    // Previously our synthetic `<init>` wrote to AbstractMap.keySet/values
    // (slots 0/1) and CHM.table (slot 2 conflict with real layout), corrupting
    // the real field layout. Real CHM `<init>()V` is a no-op; `<init>(I)V`
    // sets `sizeCtl` based on capacity. Letting real bytecode run is correct.

    // addCount(JI)V — concurrent size-counter increment.
    //
    // Real JDK uses a LongAdder-style striped counter (baseCount + counterCells)
    // that, under our interpreter's tag-erasure model on long fields, races in
    // a way that loses ~50-95% of increments at >8-thread contention (size()
    // returns far less than the actual entry count).  Reproduces in
    // `ChmStress.java` and `ChmTrace.java`: hits=1600 size=1020.
    //
    // The actual addCount native is registered in `register_essential_natives`
    // so it applies in both real-JDK and synthetic-jdk modes.

    // putIfAbsent — atomic: monitor_enter, get, conditionally put, monitor_exit
    registry.register(
        chm,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let existing = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            let result = match existing {
                Some(Value::Object(None)) | None => {
                    ctx.invoke_virtual(
                        this,
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key, val],
                    )?;
                    Some(Value::Object(None))
                }
                other => other,
            };
            ctx.monitor_exit(this);
            Ok(result)
        },
    );

    // computeIfAbsent — atomic
    registry.register(
        chm,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let func = match args.get(2) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let existing = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            let result = match existing {
                Some(Value::Object(None)) | None => {
                    let computed = ctx.invoke_virtual(
                        func,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key],
                    )?;
                    if let Some(ref v) = computed {
                        if !matches!(v, Value::Object(None)) {
                            ctx.invoke_virtual(
                                this,
                                "put",
                                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                                &[key, *v],
                            )?;
                        }
                    }
                    computed
                }
                other => other,
            };
            ctx.monitor_exit(this);
            Ok(result)
        },
    );

    // compute — atomic
    registry.register(
        chm,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let func = match args.get(2) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let old_val = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            let old_v = old_val.unwrap_or(Value::Object(None));
            let new_val = ctx.invoke_virtual(
                func,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, old_v],
            )?;
            if let Some(ref v) = new_val {
                if !matches!(v, Value::Object(None)) {
                    ctx.invoke_virtual(
                        this,
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key, *v],
                    )?;
                } else {
                    ctx.invoke_virtual(
                        this,
                        "remove",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key],
                    )?;
                }
            } else {
                ctx.invoke_virtual(
                    this,
                    "remove",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key],
                )?;
            }
            ctx.monitor_exit(this);
            Ok(new_val)
        },
    );

    // merge — atomic
    registry.register(
        chm,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            let func = match args.get(3) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let old_val = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            let new_val = match old_val {
                Some(Value::Object(None)) | None => Some(value),
                Some(old) => ctx.invoke_virtual(
                    func,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[old, value],
                )?,
            };
            if let Some(ref v) = new_val {
                if !matches!(v, Value::Object(None)) {
                    ctx.invoke_virtual(
                        this,
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key, *v],
                    )?;
                } else {
                    ctx.invoke_virtual(
                        this,
                        "remove",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key],
                    )?;
                }
            }
            ctx.monitor_exit(this);
            Ok(new_val)
        },
    );

    // getOrDefault
    registry.register(
        chm,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let default_val = args.get(2).copied().unwrap_or(Value::Object(None));
            let existing = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            match existing {
                Some(Value::Object(None)) | None => Ok(Some(default_val)),
                other => Ok(other),
            }
        },
    );

    // forEach
    registry.register(
        chm,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let entries = match ctx.get_field(this, 0) {
                Value::Object(Some(e)) => e,
                _ => return Ok(None),
            };
            let arr_len = ctx.array_length(entries);
            // GC-safety: `accept` is an arbitrary user lambda; the consumer and
            // the bucket array are both carried across every turn.
            let action_pin = ctx.pin_native_root(action);
            let entries_pin = ctx.pin_native_root(entries);
            for i in 0..arr_len {
                let action = ctx.read_native_pin(action_pin, action);
                let entries = ctx.read_native_pin(entries_pin, entries);
                if let Value::Object(Some(entry)) = ctx.get_array_element(entries, i) {
                    let key = ctx.get_field(entry, 0);
                    let val = ctx.get_field(entry, 1);
                    ctx.invoke_virtual(
                        action,
                        "accept",
                        "(Ljava/lang/Object;Ljava/lang/Object;)V",
                        &[key, val],
                    )?;
                }
            }
            Ok(None)
        },
    );

    // --- LinkedBlockingQueue / ArrayBlockingQueue: synthetic 3-field overrides ---
    //
    // These overrides assume slot 0 = element array, slot 1 = size, slot 2 =
    // capacity / head — the synthetic-jdk layout.  Real JDK 25 LBQ has fields
    // head/last/count/putLock/takeLock/notEmpty/notFull/capacity, and its
    // <init> is responsible for instantiating putLock and takeLock.  When we
    // shadowed `<init>` in real-JDK mode, putLock/takeLock stayed null, so
    // real bytecode (e.g. drainTo line 706 -> `takeLock.lock()`) NPEd.
    // Gate the entire block behind synthetic-jdk so real-JDK boot lets the
    // real constructor and real methods run.
    #[cfg(feature = "synthetic-jdk")]
    {
        let lbq = "java/util/concurrent/LinkedBlockingQueue";
        registry.register(lbq, "<init>", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX));
            Ok(None)
        });
        registry.register(lbq, "<init>", "(I)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cap = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => i32::MAX,
            };
            let arr = ctx.new_array(
                cratonvm_types::ArrayElementType::Reference,
                cap.min(64) as usize,
            );
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(cap));
            Ok(None)
        });
        registry.register(lbq, "<init>", "(Ljava/util/Collection;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX));
            if let Some(Value::Object(Some(coll))) = args.get(1) {
                if let Some(Value::Object(Some(iter))) =
                    ctx.invoke_virtual(*coll, "iterator", "()Ljava/util/Iterator;", &[])?
                {
                    // GC-safety: `hasNext`/`next` are real bytecode and
                    // `m18_lbq_add_internal` grows a backing array; the queue
                    // being filled and the iterator driving it are both carried
                    // in from outside.
                    let this_pin = ctx.pin_native_root(this);
                    let iter_pin = ctx.pin_native_root(iter);
                    loop {
                        let this = ctx.read_native_pin(this_pin, this);
                        let iter = ctx.read_native_pin(iter_pin, iter);
                        let has = ctx.invoke_virtual(iter, "hasNext", "()Z", &[])?;
                        if has != Some(Value::Int(1)) {
                            break;
                        }
                        let iter = ctx.read_native_pin(iter_pin, iter);
                        let elem = ctx
                            .invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[])?
                            .unwrap_or(Value::Object(None));
                        let this = ctx.read_native_pin(this_pin, this);
                        m18_lbq_add_internal(ctx, this, elem);
                    }
                    ctx.unpin_native_roots(this_pin);
                }
            }
            Ok(None)
        });
        registry.register(lbq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        });
        registry.register(lbq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let cap = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => i32::MAX,
            };
            if size >= cap {
                ctx.monitor_exit(this);
                return Ok(Some(Value::Int(0)));
            }
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(
            lbq,
            "offer",
            "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let elem = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                let cap = match ctx.get_field(this, 2) {
                    Value::Int(n) => n,
                    _ => i32::MAX,
                };
                if size >= cap {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
                m18_lbq_add_internal(ctx, this, elem);
                ctx.monitor_notify_all(this)?;
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            },
        );
        registry.register(lbq, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(lbq, "take", "()Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            loop {
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size > 0 {
                    let result = m18_lbq_remove_head(ctx, this);
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(Some(result));
                }
                // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
                this = monitor_wait_keepalive(ctx, this, Some(10))?;
                ctx.monitor_exit(this);
            }
        });
        registry.register(lbq, "poll", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let result = if size > 0 {
                m18_lbq_remove_head(ctx, this)
            } else {
                Value::Object(None)
            };
            ctx.monitor_exit(this);
            Ok(Some(result))
        });
        registry.register(
            lbq,
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
                    _ => 2,
                };
                let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

                loop {
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    if size > 0 {
                        let result = m18_lbq_remove_head(ctx, this);
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        return Ok(Some(result));
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        ctx.monitor_exit(this);
                        return Ok(Some(Value::Object(None))); // timed out
                    }
                    let wait_ms = bounded_monitor_wait_ms(remaining, 10);
                    // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
                    this = monitor_wait_keepalive(ctx, this, Some(wait_ms))?;
                    ctx.monitor_exit(this);
                }
            },
        );
        registry.register(lbq, "peek", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            if size == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(ctx.get_array_element(arr, 0)))
        });
        registry.register(lbq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))
        });
        registry.register(lbq, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
        });
        registry.register(lbq, "remainingCapacity", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let cap = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => i32::MAX,
            };
            Ok(Some(Value::Int(cap - size)))
        });
        registry.register(lbq, "clear", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            ctx.monitor_enter(this);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        });
        registry.register(lbq, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let target = args.get(1).copied().unwrap_or(Value::Object(None));
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            for i in 0..size {
                if ctx.get_array_element(arr, i) == target {
                    return Ok(Some(Value::Int(1)));
                }
            }
            Ok(Some(Value::Int(0)))
        });
        registry.register(lbq, "toArray", "()[Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let e = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    return Ok(Some(Value::Object(Some(e))));
                }
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    let e = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    return Ok(Some(Value::Object(Some(e))));
                }
            };
            let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
            for i in 0..size {
                ctx.set_array_element(result, i, ctx.get_array_element(arr, i));
            }
            Ok(Some(Value::Object(Some(result))))
        });
        registry.register(lbq, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n.max(0) as usize,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    let iter = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList$Itr", 3)?;
                    ctx.set_field(iter, 0, Value::Int(0));
                    ctx.set_field(iter, 1, Value::Int(0));
                    return Ok(Some(Value::Object(Some(iter))));
                }
            };
            let snap = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
            for i in 0..size {
                ctx.set_array_element(snap, i, ctx.get_array_element(arr, i));
            }
            let iter = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList$Itr", 3)?;
            ctx.set_field(iter, 0, Value::Object(Some(snap)));
            ctx.set_field(iter, 1, Value::Int(size as i32));
            ctx.set_field(iter, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(iter))))
        });
        registry.register(lbq, "drainTo", "(Ljava/util/Collection;)I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let coll = match args.get(1) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Int(0))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
            };
            // GC-safety: `Collection.add` is real bytecode and can grow the
            // target; the drained queue, its backing array and the destination
            // are all carried across every turn.
            let this_pin = ctx.pin_native_root(this);
            let arr_pin = ctx.pin_native_root(arr);
            let coll_pin = ctx.pin_native_root(coll);
            for i in 0..size as usize {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let coll = ctx.read_native_pin(coll_pin, coll);
                let elem = ctx.get_array_element(arr, i);
                ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
            }
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(size)))
        });

        // --- ArrayBlockingQueue: 3-field (data=0 array, size=1 int, head=2 int) ---
        let abq = "java/util/concurrent/ArrayBlockingQueue";
        registry.register(abq, "<init>", "(I)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cap = match args.get(1) {
                Some(Value::Int(v)) => (*v).max(1) as usize,
                _ => 16,
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        });
        registry.register(abq, "<init>", "(IZ)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cap = match args.get(1) {
                Some(Value::Int(v)) => (*v).max(1) as usize,
                _ => 16,
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        });
        registry.register(abq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            // GC-safety: this loop parks on the monitor, which is the widest
            // collection window there is, and dereferences `this` on the next
            // turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                let arr = match ctx.get_field(this, 0) {
                    Value::Object(Some(a)) => a,
                    _ => {
                        ctx.monitor_exit(this);
                        return Ok(None);
                    }
                };
                let cap = ctx.array_length(arr) as i32;
                if size < cap {
                    let head = match ctx.get_field(this, 2) {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    let tail = (head + size) % cap;
                    ctx.set_array_element(arr, tail as usize, elem);
                    ctx.set_field(this, 1, Value::Int(size + 1));
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(None);
                }
                monitor_wait_release(ctx, &mut this, Some(10))?;
            }
        });
        registry.register(abq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
            };
            let cap = ctx.array_length(arr) as i32;
            if size >= cap {
                ctx.monitor_exit(this);
                return Ok(Some(Value::Int(0)));
            }
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            let tail = (head + size) % cap;
            ctx.set_array_element(arr, tail as usize, elem);
            ctx.set_field(this, 1, Value::Int(size + 1));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(abq, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
            };
            let cap = ctx.array_length(arr) as i32;
            if size >= cap {
                ctx.monitor_exit(this);
                return Ok(Some(Value::Int(0)));
            }
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            let tail = (head + size) % cap;
            ctx.set_array_element(arr, tail as usize, elem);
            ctx.set_field(this, 1, Value::Int(size + 1));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(abq, "take", "()Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // GC-safety: parks on the monitor and dereferences `this` on
            // the next turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size > 0 {
                    let result = m18_abq_remove_head(ctx, this);
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(Some(result));
                }
                monitor_wait_release(ctx, &mut this, Some(10))?;
            }
        });
        registry.register(abq, "poll", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let result = if size > 0 {
                m18_abq_remove_head(ctx, this)
            } else {
                Value::Object(None)
            };
            ctx.monitor_exit(this);
            Ok(Some(result))
        });
        registry.register(
            abq,
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
                    _ => 2,
                };
                let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

                // GC-safety: parks on the monitor and dereferences `this`
                // on the next turn.
                let this_pin = ctx.pin_native_root(this);
                loop {
                    let mut this = ctx.read_native_pin(this_pin, this);
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    if size > 0 {
                        let result = m18_abq_remove_head(ctx, this);
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        return Ok(Some(result));
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        ctx.monitor_exit(this);
                        return Ok(Some(Value::Object(None))); // timed out
                    }
                    let wait_ms = bounded_monitor_wait_ms(remaining, 10);
                    monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
                }
            },
        );
        registry.register(abq, "peek", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            if size == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            Ok(Some(ctx.get_array_element(arr, head)))
        });
        registry.register(abq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))
        });
        registry.register(abq, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
        });
        registry.register(abq, "remainingCapacity", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let cap = ctx.array_length(arr) as i32;
            Ok(Some(Value::Int(cap - size)))
        });
        registry.register(abq, "clear", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            ctx.monitor_enter(this);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        });
        registry.register(abq, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let target = args.get(1).copied().unwrap_or(Value::Object(None));
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let cap = ctx.array_length(arr) as i32;
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            for i in 0..size {
                let idx = ((head + i) % cap) as usize;
                if ctx.get_array_element(arr, idx) == target {
                    return Ok(Some(Value::Int(1)));
                }
            }
            Ok(Some(Value::Int(0)))
        });
        registry.register(abq, "toArray", "()[Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let e = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    return Ok(Some(Value::Object(Some(e))));
                }
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    let e = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    return Ok(Some(Value::Object(Some(e))));
                }
            };
            let cap = ctx.array_length(arr) as i32;
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size as usize);
            for i in 0..size {
                let idx = ((head + i) % cap) as usize;
                ctx.set_array_element(result, i as usize, ctx.get_array_element(arr, idx));
            }
            Ok(Some(Value::Object(Some(result))))
        });
        registry.register(abq, "drainTo", "(Ljava/util/Collection;)I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let coll = match args.get(1) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Int(0))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Int(0)));
                }
            };
            let cap = ctx.array_length(arr) as i32;
            let head = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            // GC-safety: as in the LBQ drain above.
            let this_pin = ctx.pin_native_root(this);
            let arr_pin = ctx.pin_native_root(arr);
            let coll_pin = ctx.pin_native_root(coll);
            for i in 0..size {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let coll = ctx.read_native_pin(coll_pin, coll);
                let idx = ((head + i) % cap) as usize;
                let elem = ctx.get_array_element(arr, idx);
                ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
            }
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(size)))
        });
    } // end of #[cfg(feature = "synthetic-jdk")] block for LBQ/ABQ
}

// ===========================================================================
// T3.1: ConcurrentSkipListMap, LinkedTransferQueue, Flow
// ===========================================================================

/// ConcurrentSkipListMap — backed by a sorted array of key-value pairs.
/// Fields: 0=keys_array, 1=values_array, 2=size
/// Uses binary search on keys (via compareTo) for NavigableMap operations.
pub(crate) fn register_t31_concurrent_extras(registry: &mut NativeMethodRegistry) {
    let cslm = "java/util/concurrent/ConcurrentSkipListMap";

    registry.register(cslm, "<init>", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // `this` and the first array both predate the second allocation.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let keys_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let keys_h = scope.root(keys_obj);
        let vals = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let this = scope.get(&this_h);
        let keys = scope.get(&keys_h);
        scope.set_field(this, 0, Value::Object(Some(keys)));
        scope.set_field(this, 1, Value::Object(Some(vals)));
        scope.set_field(this, 2, Value::Int(0));
        Ok(None)
    });

    // put(K, V) -> V — sorted insert
    registry.register(
        cslm,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // `compareTo` below runs arbitrary Java on every probe of the
            // search loop, and the grow arm allocates twice. The receiver, both
            // arrays and both arguments cross all of that, so the whole body
            // works through the scope and re-reads each address after anything
            // that can collect.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let key_h = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(scope.root(*o)),
                _ => None,
            };
            let val_h = match args.get(2) {
                Some(Value::Object(Some(o))) => Some(scope.root(*o)),
                _ => None,
            };
            let key_plain = args.get(1).copied().unwrap_or(Value::Object(None));
            let val_plain = args.get(2).copied().unwrap_or(Value::Object(None));
            let key = |scope: &cratonvm_native_api::NativeHandleScope| match &key_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => key_plain,
            };
            let val = |scope: &cratonvm_native_api::NativeHandleScope| match &val_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => val_plain,
            };
            scope.monitor_enter(this);
            let size = match scope.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_src_h = match scope.get_field(this, 0) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            let vals_src_h = match scope.get_field(this, 1) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };

            // Find insertion point via linear scan (compareTo)
            let mut pos = size;
            for i in 0..size {
                let keys_arr = scope.get(&keys_src_h);
                let existing_key = scope.get_array_element(keys_arr, i);
                if let Value::Object(Some(ek)) = existing_key {
                    let probe = key(&scope);
                    if let Ok(Some(Value::Int(cmp))) =
                        scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[probe])
                    {
                        if cmp == 0 {
                            // Key exists — replace value
                            let vals_arr = scope.get(&vals_src_h);
                            let old = scope.get_array_element(vals_arr, i);
                            let v = val(&scope);
                            scope.set_array_element(vals_arr, i, v);
                            let this = scope.get(&this_h);
                            scope.monitor_exit(this);
                            return Ok(Some(old));
                        } else if cmp > 0 {
                            pos = i;
                            break;
                        }
                    }
                }
            }

            // Grow if needed
            let keys_arr = scope.get(&keys_src_h);
            let cap = scope.array_length(keys_arr);
            if size >= cap {
                let new_cap = cap * 2;
                let new_keys_obj =
                    scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let new_keys_h = scope.root(new_keys_obj);
                let new_vals = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let new_keys = scope.get(&new_keys_h);
                let keys_arr = scope.get(&keys_src_h);
                let vals_arr = scope.get(&vals_src_h);
                let this = scope.get(&this_h);
                let key = key(&scope);
                let val = val(&scope);
                for i in 0..size {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(new_keys, i, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(new_vals, i, v);
                }
                scope.set_field(this, 0, Value::Object(Some(new_keys)));
                scope.set_field(this, 1, Value::Object(Some(new_vals)));
                // Re-fetch after resize
                let keys_arr = new_keys;
                let vals_arr = new_vals;
                // Shift right from pos
                for i in (pos..size).rev() {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(keys_arr, i + 1, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(vals_arr, i + 1, v);
                }
                scope.set_array_element(keys_arr, pos, key);
                scope.set_array_element(vals_arr, pos, val);
            } else {
                // Shift right from pos
                let keys_arr = scope.get(&keys_src_h);
                let vals_arr = scope.get(&vals_src_h);
                for i in (pos..size).rev() {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(keys_arr, i + 1, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(vals_arr, i + 1, v);
                }
                let k = key(&scope);
                let v = val(&scope);
                scope.set_array_element(keys_arr, pos, k);
                scope.set_array_element(vals_arr, pos, v);
            }
            let this = scope.get(&this_h);
            scope.set_field(this, 2, Value::Int((size + 1) as i32));
            scope.monitor_exit(this);
            Ok(Some(Value::Object(None)))
        },
    );

    // get(Object) -> V
    registry.register(
        cslm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let size = match ctx.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let vals_arr = match ctx.get_field(this, 1) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            for i in 0..size {
                if let Value::Object(Some(ek)) = ctx.get_array_element(keys_arr, i) {
                    if let Ok(Some(Value::Int(cmp))) =
                        ctx.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[key])
                    {
                        if cmp == 0 {
                            return Ok(Some(ctx.get_array_element(vals_arr, i)));
                        }
                        if cmp > 0 {
                            break;
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // size() -> int
    registry.register(cslm, "size", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(ctx.get_field(this, 2)))
    });

    // isEmpty() -> boolean
    registry.register(cslm, "isEmpty", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(1))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
    });

    // containsKey(Object) -> boolean
    registry.register(cslm, "containsKey", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        for i in 0..size {
            if let Value::Object(Some(ek)) = ctx.get_array_element(keys_arr, i) {
                if let Ok(Some(Value::Int(0))) =
                    ctx.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[key])
                {
                    return Ok(Some(Value::Int(1)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });

    // remove(Object) -> V
    registry.register(
        cslm,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            let vals_arr = match ctx.get_field(this, 1) {
                Value::Object(Some(a)) => a,
                _ => {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            for i in 0..size {
                if let Value::Object(Some(ek)) = ctx.get_array_element(keys_arr, i) {
                    if let Ok(Some(Value::Int(0))) =
                        ctx.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[key])
                    {
                        let old = ctx.get_array_element(vals_arr, i);
                        // Shift left
                        for j in i..size - 1 {
                            ctx.set_array_element(
                                keys_arr,
                                j,
                                ctx.get_array_element(keys_arr, j + 1),
                            );
                            ctx.set_array_element(
                                vals_arr,
                                j,
                                ctx.get_array_element(vals_arr, j + 1),
                            );
                        }
                        ctx.set_array_element(keys_arr, size - 1, Value::Object(None));
                        ctx.set_array_element(vals_arr, size - 1, Value::Object(None));
                        ctx.set_field(this, 2, Value::Int((size - 1) as i32));
                        ctx.monitor_exit(this);
                        return Ok(Some(old));
                    }
                }
            }
            ctx.monitor_exit(this);
            Ok(Some(Value::Object(None)))
        },
    );

    // firstKey() -> K
    registry.register(cslm, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        if size == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "NoSuchElementException".to_string(),
            }
            .into());
        }
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_array_element(keys_arr, 0)))
    });

    // lastKey() -> K
    registry.register(cslm, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        if size == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "NoSuchElementException".to_string(),
            }
            .into());
        }
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_array_element(keys_arr, size - 1)))
    });

    // subMap(fromKey, toKey) -> SortedMap (returns self-type as simplified view)
    // For a real JVM this would return a view; we return a new ConcurrentSkipListMap with the subrange
    registry.register(
        cslm,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let from = args.get(1).copied().unwrap_or(Value::Object(None));
            let to = args.get(2).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, Some(from), Some(to))
        },
    );

    // headMap(toKey) -> SortedMap
    registry.register(
        cslm,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let to = args.get(1).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, None, Some(to))
        },
    );

    // tailMap(fromKey) -> SortedMap
    registry.register(
        cslm,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let from = args.get(1).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, Some(from), None)
        },
    );

    // --- LinkedTransferQueue ---
    // Backed by same array structure as LinkedBlockingQueue: 0=array, 1=size, 2=capacity.
    // Real JDK 25 LTQ has a totally different layout (head/tail Node refs etc.) so the
    // synthetic <init> would leave real fields null and break later real bytecode.
    // Gate behind synthetic-jdk so real-JDK boot uses the real constructor.
    #[cfg(feature = "synthetic-jdk")]
    {
        let ltq = "java/util/concurrent/LinkedTransferQueue";
        registry.register(ltq, "<init>", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            ctx.set_field(this, 0, Value::Object(Some(arr)));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX)); // unbounded
            Ok(None)
        });
        // add/offer/put — same as LBQ
        registry.register(ltq, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(ltq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        registry.register(ltq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        });
        // transfer(E) — blocking: adds element and waits until it is consumed
        registry.register(ltq, "transfer", "(Ljava/lang/Object;)V", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            // Wait until the item is consumed (size decreases)
            let size_before = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.monitor_exit(this);
            // GC-safety: parks on the monitor until a consumer takes the
            // element, and dereferences `this` on the next turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                let size_now = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size_now < size_before {
                    return Ok(None);
                }
                ctx.monitor_enter(this);
                monitor_wait_release(ctx, &mut this, Some(5))?;
            }
        });
        // tryTransfer(E) — non-blocking: add if there's a waiting consumer
        registry.register(ltq, "tryTransfer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        });
        // tryTransfer(E, long, TimeUnit) — timed
        registry.register(
            ltq,
            "tryTransfer",
            "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let elem = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                m18_lbq_add_internal(ctx, this, elem);
                ctx.monitor_notify_all(this)?;
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            },
        );
        // poll() — non-blocking
        registry.register(ltq, "poll", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let result = if size > 0 {
                let r = m18_lbq_remove_head(ctx, this);
                ctx.monitor_notify_all(this)?;
                r
            } else {
                Value::Object(None)
            };
            ctx.monitor_exit(this);
            Ok(Some(result))
        });
        // poll(long, TimeUnit) — timed blocking
        registry.register(
            ltq,
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
                    _ => 2,
                };
                let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
                // GC-safety: parks on the monitor -- the widest collection
                // window there is -- and dereferences `this` on the next turn.
                let this_pin = ctx.pin_native_root(this);
                loop {
                    let mut this = ctx.read_native_pin(this_pin, this);
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    if size > 0 {
                        let result = m18_lbq_remove_head(ctx, this);
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        return Ok(Some(result));
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        ctx.monitor_exit(this);
                        return Ok(Some(Value::Object(None)));
                    }
                    let wait_ms = bounded_monitor_wait_ms(remaining, 10);
                    monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
                }
            },
        );
        // take() — blocking
        registry.register(ltq, "take", "()Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // GC-safety: parks on the monitor and dereferences `this` on
            // the next turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size > 0 {
                    let result = m18_lbq_remove_head(ctx, this);
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(Some(result));
                }
                monitor_wait_release(ctx, &mut this, Some(10))?;
            }
        });
        // peek() — non-blocking
        registry.register(ltq, "peek", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            if size == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(ctx.get_array_element(arr, 0)))
        });
        // size()
        registry.register(ltq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))
        });
        // isEmpty()
        registry.register(ltq, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
        });
    } // end of #[cfg(feature = "synthetic-jdk")] block for LinkedTransferQueue

    // --- Flow.Publisher / Flow.Subscriber / Flow.Subscription ---
    // These are interfaces in Java; register minimal natives for the reactive-streams bridge.
    let flow_pub = "java/util/concurrent/Flow$Publisher";
    registry.register(
        flow_pub,
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        |ctx, args| {
            // Default: immediately call onSubscribe then onComplete
            if let Some(Value::Object(Some(subscriber))) = args.get(1) {
                // 2-field layout, matching phase 60 / SubmissionPublisher.subscribe:
                // 0 = cancelled flag, 1 = accumulated demand (Long). The second
                // slot is what makes `request(n)` above observable.
                let sub = try_alloc_concurrent_synthetic(
                    ctx,
                    "java/util/concurrent/Flow$Subscription",
                    2,
                )?;
                ctx.set_field(sub, 0, Value::Int(0)); // cancelled flag
                ctx.set_field(sub, 1, Value::Long(0)); // demand
                let _ = ctx.invoke_virtual(
                    *subscriber,
                    "onSubscribe",
                    "(Ljava/util/concurrent/Flow$Subscription;)V",
                    &[Value::Object(Some(sub))],
                );
                let _ = ctx.invoke_virtual(*subscriber, "onComplete", "()V", &[]);
            }
            Ok(None)
        },
    );

    let flow_sub = "java/util/concurrent/Flow$Subscription";
    // W4: the "SUPERSEDED, never runs" comment that used to sit here was WRONG
    // for the default (real-JDK) build. `register_stream_overrides` only wins
    // when it is registered LAST, and it is not: in real-JDK mode
    // `vm_init.rs` calls `register_essential_natives_with_shims` (which reaches
    // `register_stream_overrides` via `register_annotation_overrides`) and only
    // THEN calls `register_concurrent_natives`, i.e. this module. So this
    // registration is the live one in the default build; only in
    // `--synthetic-jdk` mode does lib.rs order the stream overrides (and, later
    // still, `register_p60_flow`) after it.
    //
    // Accumulate the demand for real instead of dropping it, using the same
    // layout phase 60 uses and that `SubmissionPublisher.subscribe` allocates
    // (field 0 = cancelled flag, field 1 = Long demand). Objects with fewer
    // slots (older 1-field synthetic subscriptions) keep the no-op behaviour
    // rather than writing out of bounds.
    registry.register(flow_sub, "request", "(J)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        // Reactive-streams: a non-positive request is a protocol error; with no
        // subscriber reference to call `onError` on, ignore it (same choice as
        // `streams::native_flow_request`).
        if n <= 0 || ctx.object_num_fields(this) < 2 {
            return Ok(None);
        }
        let cur = match ctx.get_field(this, 1) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Long(cur.saturating_add(n)));
        Ok(None)
    });
    registry.register(flow_sub, "cancel", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        ctx.set_field(this, 0, Value::Int(1));
        Ok(None)
    });

    // SubmissionPublisher — B6 dedup (nb-core-mediums, fable-2026-06-10):
    // the stale `<init>`/`close`/`isClosed` registrations that used to live here
    // were registered LAST (register_t31_concurrent_extras runs after
    // register_phase60_natives), so they SHADOWED the working phase-60
    // (`register_p60_flow`) versions in phases_late.rs that Round 5 wired up for
    // real subscriber delivery. In particular this `close()V` only flipped the
    // closed flag and never fired `onComplete()`, defeating the fix. The block
    // is deleted so the delivering phase-60 registrations win. See
    // phases_late.rs:16549+ for the canonical implementations.
}

// --- ReentrantLock ---

pub(crate) fn native_rl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = rl_key(ctx, this);
    rl_with(key, |st| {
        *st = RlState {
            owner: RL_UNOWNED,
            hold: 0,
            fair: false,
        };
    });
    Ok(None)
}

pub(crate) fn native_rl_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let fair = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let key = rl_key(ctx, this);
    rl_with(key, |st| {
        *st = RlState {
            owner: RL_UNOWNED,
            hold: 0,
            fair,
        };
    });
    Ok(None)
}

pub(crate) fn native_rl_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        // Atomically claim (or reentrantly re-claim) the lock under the
        // side-table mutex. `claimed` is true iff this call now holds it.
        let claimed = rl_with(key, |st| {
            if st.owner == RL_UNOWNED || st.owner == tid {
                st.owner = tid;
                st.hold += 1;
                true
            } else {
                false
            }
        });
        if claimed {
            return Ok(None);
        }
        // Owned by another thread — park on the lock object's monitor until
        // a releaser notifies. The 5ms timeout re-checks defensively.
        ctx.monitor_enter(this);
        let still_owned = {
            let st = rl_get(key);
            st.owner != RL_UNOWNED && st.owner != tid
        };
        if still_owned {
            // `ReentrantLock.lock()` (unlike `lockInterruptibly()` /
            // `tryLock(timeout, unit)`) is specified to never throw on
            // interrupt -- it defers: per the `Lock#lock()` javadoc, "If the
            // current thread... is interrupted while acquiring the lock...
            // it will continue to wait... but upon acquiring the lock its
            // interrupted status will be set." `monitor_wait` throws
            // `InterruptedException` just like `Object.wait()` does (and
            // already consumes the thread's interrupt flag when it does) --
            // absorb that here, restore the flag via `thread_interrupt` on
            // our own thread object, and keep retrying instead of
            // propagating the exception out of a method that must not throw.
            if let Err(e) = monitor_wait_release(ctx, &mut this, Some(5)) {
                if is_interrupted_exception(&e) {
                    let self_thread = ctx.current_thread_object();
                    ctx.thread_interrupt(self_thread);
                    continue;
                }
                return Err(e);
            }
        } else {
            ctx.monitor_exit(this);
        }
    }
}

pub(crate) fn native_rl_unlock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    // Atomically verify ownership and decrement; report whether the lock is
    // now fully released so we can notify a waiter outside the mutex.
    let outcome = rl_with(key, |st| {
        if st.owner != tid {
            return None;
        }
        st.hold -= 1;
        if st.hold <= 0 {
            st.hold = 0;
            st.owner = RL_UNOWNED;
            Some(true)
        } else {
            Some(false)
        }
    });
    match outcome {
        None => Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread is not owner".to_string(),
        }
        .into()),
        Some(fully_released) => {
            if fully_released {
                // Wake one thread waiting in `native_rl_lock` to acquire.
                ctx.monitor_enter(this);
                ctx.monitor_notify(this)?;
                ctx.monitor_exit(this);
            }
            Ok(None)
        }
    }
}

pub(crate) fn native_rl_try_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let claimed = rl_with(key, |st| {
        if st.owner == RL_UNOWNED || st.owner == tid {
            st.owner = tid;
            st.hold += 1;
            true
        } else {
            false
        }
    });
    Ok(Some(Value::Int(if claimed { 1 } else { 0 })))
}

pub(crate) fn native_rl_try_lock_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, long timeout, TimeUnit unit]
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ord = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord);
    if timeout_ms <= 0 {
        return native_rl_try_lock(ctx, args);
    }
    let key = rl_key(ctx, this);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        let claimed = rl_with(key, |st| {
            if st.owner == RL_UNOWNED || st.owner == tid {
                st.owner = tid;
                st.hold += 1;
                true
            } else {
                false
            }
        });
        if claimed {
            return Ok(Some(Value::Int(1)));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0)));
        }
        let wait_ms = remaining.as_millis().min(5).max(1) as u64;
        ctx.monitor_enter(this);
        monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
    }
}

pub(crate) fn native_rl_is_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).owner != RL_UNOWNED {
        1
    } else {
        0
    })))
}

pub(crate) fn native_rl_is_held_by_current_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).owner == tid {
        1
    } else {
        0
    })))
}

pub(crate) fn native_rl_get_hold_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let st = rl_get(key);
    // `getHoldCount()` reports the count for the *current* thread only.
    Ok(Some(Value::Int(if st.owner == tid { st.hold } else { 0 })))
}

pub(crate) fn native_rl_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).fair { 1 } else { 0 })))
}

pub(crate) fn native_rl_new_condition(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cond = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/locks/Condition", 1)?;
    ctx.set_field(cond, COND_FIELD_LOCK, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cond))))
}

pub(crate) fn native_rl_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = rl_key(ctx, this);
    let state = if rl_get(key).owner != RL_UNOWNED {
        "locked"
    } else {
        "unlocked"
    };
    let s = format!(
        "java.util.concurrent.locks.ReentrantLock@{:x}[{state}]",
        this.as_ptr() as usize
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

// --- Condition ---

pub(crate) fn native_cond_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Get the associated lock and verify ownership
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    // FELIX FIX (lost-wakeup race): acquire the *condition* monitor BEFORE
    // releasing the ReentrantLock. `signal`/`signalAll` must `monitor_enter`
    // this same monitor to notify, so holding it here forces a concurrent
    // signaller to block until we reach `monitor_wait` (which atomically
    // releases the monitor and waits). Previously the lock was released and
    // lock-waiters notified BEFORE `monitor_enter(this)`, leaving a window
    // where a `signalAll()` from the lock's new owner ran to completion and
    // its `notify_all` reached no waiter — the awaiter then blocked on
    // `monitor_wait` forever. This is exactly Felix's `acquireBundleLock`/
    // `releaseBundleLock` handoff, which otherwise deadlocks `Felix.start()`.
    ctx.monitor_enter(this);
    let saved_hold = match rl_release_for_await(lock_key, tid) {
        Some(h) => h,
        None => {
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "current thread is not owner".to_string(),
            }
            .into());
        }
    };
    // Notify any threads waiting to acquire the now-free lock.
    ctx.monitor_enter(lock_ref);
    ctx.monitor_notify(lock_ref)?;
    ctx.monitor_exit(lock_ref);
    // Atomically release the condition monitor and wait for a signal.
    //
    // GC-SAFEPOINT FIX: `monitor_wait` parks this thread, so a collection can
    // run while we are blocked — the moving collector, or the non-moving
    // young sweep's selective promotion — and relocate both `this` and
    // `lock_ref`. The raw ObjectRefs captured at entry would then be stale, and
    // the `monitor_exit` / `reacquire` below would `header_of` a dead address →
    // EXCEPTION_ACCESS_VIOLATION (TestSwallowAbortedUploads SIGSEGV in
    // MonitorTable::exit). Pin both across the wait and read back their post-GC
    // addresses; `native_pin_roots` is remapped by the collector (see
    // vm/src/memory/gc.rs `update_all_roots`). Unpin even on the error path.
    let this_pin = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock_ref);
    let wr = ctx.monitor_wait(this, None);
    let this = ctx.read_native_pin(this_pin, this);
    let lock_ref = ctx.read_native_pin(lock_pin, lock_ref);
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    // Condition contract (java.util.concurrent.locks.Condition#await): "In all
    // cases, before this method can return the current thread must re-acquire
    // the lock associated with this condition." That includes the interrupted
    // case — so re-acquire the lock BEFORE propagating the interrupt. Otherwise
    // the caller's `lock.unlock()` (e.g. `LinkedBlockingQueue.take`'s `finally`)
    // runs without holding the lock and throws IllegalMonitorStateException,
    // which corrupts a ThreadPoolExecutor worker's shutdown under takeLock
    // contention (multiple idle workers) → workers never terminate.
    reacquire_lock_after_await(ctx, lock_ref, lock_key, tid, saved_hold)?;
    wr?;
    Ok(None)
}

pub(crate) fn native_cond_await_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // RD.5: args = [this, long timeout, TimeUnit]. Convert via TimeUnit ordinal.
    let timeout_raw = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_raw, unit_ordinal).max(0) as u64;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    cond_await_millis(ctx, this, timeout_ms)
}

/// `Condition.awaitUntil(Date deadline)` — wait until the absolute deadline.
/// Returns Z: false (0) if the deadline has already elapsed, otherwise the
/// timed-wait result. Without this the synthetic `Condition` had no body for
/// `awaitUntil` and the abstract interface method raised `AbstractMethodError`
/// (keycloak `WaitConditionShutdownListenerTest`).
pub(crate) fn native_cond_await_until(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let deadline_ms = match args.get(1) {
        Some(Value::Object(Some(d))) => match ctx.invoke_virtual(*d, "getTime", "()J", &[])? {
            Some(Value::Long(t)) => t,
            _ => 0,
        },
        // null deadline → behave as an immediate timeout (false).
        _ => return Ok(Some(Value::Int(0))),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let remaining = deadline_ms - now_ms;
    if remaining <= 0 {
        // Deadline already passed — never wait (Some(0) means "wait forever"
        // to monitor_wait), just report the timeout.
        return Ok(Some(Value::Int(0)));
    }
    cond_await_millis(ctx, this, remaining as u64)
}

pub(crate) fn native_cond_await_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Extract nanos timeout from args[1] (Long)
    let nanos = match args.get(1) {
        Some(Value::Long(v)) => (*v).max(0),
        _ => 0,
    };
    let timeout_ms = (nanos / 1_000_000).max(1) as u64;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    // FELIX FIX (lost-wakeup race): hold the condition monitor across the
    // lock release — see `native_cond_await` for the full rationale.
    ctx.monitor_enter(this);
    let saved_hold = match rl_release_for_await(lock_key, tid) {
        Some(h) => h,
        None => {
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "current thread is not owner".to_string(),
            }
            .into());
        }
    };
    ctx.monitor_enter(lock_ref);
    ctx.monitor_notify(lock_ref)?;
    ctx.monitor_exit(lock_ref);
    let start = std::time::Instant::now();
    // GC-SAFEPOINT FIX (see native_cond_await): pin `this`/`lock_ref` across
    // the blocking wait so a relocating GC can't leave them stale.
    let this_pin = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock_ref);
    let wr = ctx.monitor_wait(this, Some(timeout_ms));
    let this = ctx.read_native_pin(this_pin, this);
    let lock_ref = ctx.read_native_pin(lock_pin, lock_ref);
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    let elapsed_nanos = start.elapsed().as_nanos() as i64;
    let remaining = nanos.saturating_sub(elapsed_nanos).max(0);
    // Condition contract: re-acquire the lock before returning, even on
    // interrupt (see `native_cond_await`).
    reacquire_lock_after_await(ctx, lock_ref, lock_key, tid, saved_hold)?;
    wr?;
    Ok(Some(Value::Long(remaining)))
}

pub(crate) fn native_cond_signal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Verify current thread owns the associated lock
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    if rl_get(lock_key).owner != tid {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread does not hold the lock".to_string(),
        }
        .into());
    }
    // Wake one thread waiting on this condition
    ctx.monitor_enter(this);
    ctx.monitor_notify(this)?;
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_cond_signal_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Verify current thread owns the associated lock
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    if rl_get(lock_key).owner != tid {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread does not hold the lock".to_string(),
        }
        .into());
    }
    // Wake all threads waiting on this condition
    ctx.monitor_enter(this);
    ctx.monitor_notify_all(this)?;
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_cdl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let count = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if count < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "count < 0".to_string(),
        }
        .into());
    }
    cdl_set_count(ctx, this, count);
    Ok(None)
}

pub(crate) fn native_cdl_count_down(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Serialize the read-modify-write against concurrent countDown() calls —
    // two racing decrements must not lose one (the boot-thread/test-thread
    // handshake counts on exactly-N decrements releasing the latch).
    // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
    // completing GC pause; use its returned reference so a relocated `this`
    // doesn't go stale under the calls below.
    let this = ctx.monitor_enter_gc_safe(this);
    let count = cdl_count(ctx, this);
    if count > 0 {
        cdl_set_count(ctx, this, count - 1);
        if count - 1 == 0 {
            // Count reached zero — wake all waiting threads
            let wait_key = cdl_wait_key(ctx, this);
            ctx.vt_wake_waiters(wait_key);
            let notify_result = ctx.monitor_notify_all(this);
            ctx.monitor_exit(this);
            notify_result?;
            return Ok(None);
        }
    }
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_cdl_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if ctx.is_current_virtual() && ctx.vt_pin_count() == 0 {
        if cdl_count(ctx, this) <= 0 {
            return Ok(None);
        }
        let wait_key = cdl_wait_key(ctx, this);
        if ctx.vt_wait_on_key(wait_key) {
            // Close the registration-vs-countDown race. If count reached zero
            // first, cancel locally and complete without unmounting.
            if cdl_count(ctx, this) <= 0 {
                ctx.vt_cancel_wait_on_key(wait_key);
                return Ok(None);
            }
            return Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::ContinuationYield {
                    wake_after_nanos: 0,
                },
            ));
        }
    }
    // Block on the monitor instead of spinning. The bounded wait (10ms)
    // covers the lost-wakeup window between the count read and the wait.
    loop {
        if cdl_count(ctx, this) <= 0 {
            return Ok(None);
        }
        // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
        // completing GC pause; use its returned reference so a relocated
        // `this` doesn't go stale under the wait below.
        this = ctx.monitor_enter_gc_safe(this);
        // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
        let wait_result = monitor_wait_keepalive(ctx, this, Some(10));
        this = wait_result?;
        ctx.monitor_exit(this);
    }
}

pub(crate) fn native_cdl_await_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // args: this, timeout(long), TimeUnit
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // A null `TimeUnit` is the JDK's FIRST action, not a defaulting decision:
    // (this guard is shared by `native_cdl_await_timeout`, which IS registered
    // and was measurably wrong, and `native_sem_try_acquire_timeout`, which the
    // registry says is registered NOWHERE -- its copy of the defect is latent,
    // and is fixed here so it cannot arrive with the registration.)
    // `await` opens with `unit.toNanos(timeout)`. Defaulting to MILLISECONDS
    // turned `await(1, null)` into a one-millisecond wait returning `false`,
    // where HotSpot (and this VM's own strict arm, which refuses this native
    // and runs the bytecode) raises NPE. Message transcribed from the oracle.
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

    loop {
        let count = cdl_count(ctx, this);
        if count == 0 {
            return Ok(Some(Value::Int(1))); // true — count reached zero
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0))); // false — timed out
        }
        let wait_ms = bounded_monitor_wait_ms(remaining, 10);
        // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
        // completing GC pause; use its returned reference so a relocated
        // `this` doesn't go stale under the wait below.
        this = ctx.monitor_enter_gc_safe(this);
        // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
        this = monitor_wait_keepalive(ctx, this, Some(wait_ms))?;
        ctx.monitor_exit(this);
    }
}

pub(crate) fn native_cdl_get_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let count = cdl_count(ctx, this) as i64;
    Ok(Some(Value::Long(count)))
}

fn native_cdl_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = cdl_count(ctx, this);
    let s = format!(
        "java.util.concurrent.CountDownLatch@{:x}[Count = {count}]",
        this.as_ptr() as usize
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

fn sem_obj_key_registry(
) -> &'static parking_lot::Mutex<std::collections::HashMap<u32, Vec<SemObjKeyEntry>>> {
    static R: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<u32, Vec<SemObjKeyEntry>>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn sem_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = sem_obj_key_registry().lock();
    let slots = reg.entry(hash).or_default();
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return pack_sem_obj_key(hash, slot.generation);
    }
    if hash != 0 && slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return pack_sem_obj_key(hash, slots[0].generation);
    }
    let generation = slots.len() as u32;
    slots.push(SemObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    pack_sem_obj_key(hash, generation)
}

fn sem_states_by_obj() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, SemState>> {
    static H: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<usize, SemState>>> =
        std::sync::OnceLock::new();
    H.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn sem_side_state(ctx: &dyn NativeContext, this: ObjectRef) -> SemState {
    let key = sem_obj_key_for(ctx, this);
    let mut states = sem_states_by_obj().lock();
    *states.entry(key).or_insert(SemState {
        permits: 0,
        fair: 0,
    })
}

fn sem_set_side_state(ctx: &dyn NativeContext, this: ObjectRef, state: SemState) {
    let key = sem_obj_key_for(ctx, this);
    sem_states_by_obj().lock().insert(key, state);
}

fn sem_has_holder_slot(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) > SEM_FIELD_PERMITS
}

fn sem_prepare(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    if sem_has_holder_slot(ctx, this) {
        sem_holder(ctx, this).0
    } else {
        let _ = sem_side_state(ctx, this);
        this
    }
}

fn sem_holder(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, ObjectRef) {
    let has_permits_slot = sem_has_holder_slot(ctx, this);
    if !has_permits_slot {
        let _ = sem_side_state(ctx, this);
        return (this, this);
    }
    if let Value::Object(Some(h)) = ctx.get_field(this, SEM_FIELD_PERMITS) {
        return (this, h);
    }
    // Install the holder, migrating any legacy raw-Int values.
    let legacy_permits = if has_permits_slot {
        match ctx.get_field(this, SEM_FIELD_PERMITS) {
            Value::Int(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    let legacy_fair = if ctx.object_num_fields(this) > SEM_FIELD_FAIR {
        match ctx.get_field(this, SEM_FIELD_FAIR) {
            Value::Int(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    // Pin `this` across the allocation — a moving GC during `new_array` would
    // relocate the receiver and leave the Rust-local copy stale (the
    // StringBuffer-GC-hazard pattern).
    let this_pin = ctx.pin_native_root(this);
    let h = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_array_element(h, 0, Value::Int(legacy_permits));
    ctx.set_array_element(h, 1, Value::Int(legacy_fair));
    if ctx.object_num_fields(this) > SEM_FIELD_PERMITS {
        ctx.set_field(this, SEM_FIELD_PERMITS, Value::Object(Some(h)));
    } else {
        sem_set_side_state(
            ctx,
            this,
            SemState {
                permits: legacy_permits,
                fair: legacy_fair,
            },
        );
    }
    (this, h)
}

fn sem_permits(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if !sem_has_holder_slot(ctx, this) {
        return sem_side_state(ctx, this).permits;
    }
    let (_, h) = sem_holder(ctx, this);
    match ctx.get_array_element(h, 0) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn sem_set_permits(ctx: &mut dyn NativeContext, this: ObjectRef, permits: i32) {
    if !sem_has_holder_slot(ctx, this) {
        let mut state = sem_side_state(ctx, this);
        state.permits = permits;
        sem_set_side_state(ctx, this, state);
        return;
    }
    let (_, h) = sem_holder(ctx, this);
    ctx.set_array_element(h, 0, Value::Int(permits));
}

pub(crate) fn native_sem_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let permits = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if sem_has_holder_slot(ctx, this) {
        let (this, h) = sem_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, 0, Value::Int(permits));
        ctx.set_array_element(h, 1, Value::Int(0));
    } else {
        sem_set_side_state(ctx, this, SemState { permits, fair: 0 });
    }
    Ok(None)
}

pub(crate) fn native_sem_init_fair(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let permits = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let fair = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if sem_has_holder_slot(ctx, this) {
        let (this, h) = sem_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, 0, Value::Int(permits));
        ctx.set_array_element(h, 1, Value::Int(fair));
    } else {
        sem_set_side_state(ctx, this, SemState { permits, fair });
    }
    Ok(None)
}

/// Shared blocking-acquire: take `n` permits, waiting until enough are
/// available. The read-modify-write runs under the object monitor so racing
/// acquirers can't both observe and consume the same permit; waiting uses
/// bounded monitor-waits (woken by release's notify) instead of a busy spin.
fn sem_acquire_blocking(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> MethodCallResult {
    // Install the holder up-front (re-binding `this` across the possible
    // allocation) so no allocation happens inside the monitor section.
    let mut this = sem_prepare(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        // GC-SAFEPOINT FIX (STW cross-thread JIT-takeover barrier deadlock,
        // same class as native_cdl_await's fix in commit 51a508e1): a raw
        // `ctx.monitor_enter` never marks this thread GC-blocked, so a
        // contended wait here stays counted in the barrier's `expected` set
        // forever, deadlocking any pause requested while we wait on another
        // thread's held lock (a 3-way wedge: us waiting on the owner, the
        // owner GC-blocked waiting for the pause, the pause waiting on us).
        // `monitor_enter_gc_safe` wraps the wait in the proper GC-blocked
        // protocol and returns the possibly-relocated reference — use it for
        // every subsequent access.
        this = ctx.monitor_enter_gc_safe(this);
        let permits = sem_permits(ctx, this);
        if permits >= n {
            sem_set_permits(ctx, this, permits - n);
            ctx.monitor_exit(this);
            return Ok(None);
        }
        // GC-SAFEPOINT FIX: the wait itself can also span a relocating GC;
        // keep `this` valid across it. On error (e.g. InterruptedException)
        // fall back to the pre-wait reference for the cleanup exit below —
        // matches the original (always-exit) control flow rather than the
        // CDL precedent's skip-exit-on-error shape.
        let wait_result = monitor_wait_keepalive(ctx, this, Some(10));
        this = match &wait_result {
            Ok(o) => *o,
            Err(_) => this,
        };
        ctx.monitor_exit(this);
        wait_result?;
    }
}

pub(crate) fn native_sem_acquire(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    sem_acquire_blocking(ctx, this, 1)
}

pub(crate) fn native_sem_acquire_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    sem_acquire_blocking(ctx, this, n.max(0))
}

fn sem_release_n_inner(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> MethodCallResult {
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above for the full story —
    // this is the exact call site live-gdb-confirmed to hang
    // CrossOriginAnnotationIntegrationTests/RequestMappingMessageConversionIntegrationTests
    // (main-vm parked forever in MonitorTable::enter's contended wait,
    // uncounted-as-blocked, while the true owner is itself GC-blocked
    // waiting on a pause that waits on us).
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    sem_set_permits(ctx, this, permits + n);
    let notify_result = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    notify_result?;
    Ok(None)
}

pub(crate) fn native_sem_release(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    sem_release_n_inner(ctx, this, 1)
}

pub(crate) fn native_sem_release_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    sem_release_n_inner(ctx, this, n.max(0))
}

fn sem_try_acquire_inner(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> bool {
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above.
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    let ok = permits >= n;
    if ok {
        sem_set_permits(ctx, this, permits - n);
    }
    ctx.monitor_exit(this);
    ok
}

pub(crate) fn native_sem_try_acquire(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if sem_try_acquire_inner(ctx, this, 1) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_sem_try_acquire_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    Ok(Some(Value::Int(if sem_try_acquire_inner(ctx, this, n) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_sem_try_acquire_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: this, timeout(long), TimeUnit — honor the timeout with bounded
    // monitor-waits (was: ignored the timeout and returned tryAcquire()).
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // A null `TimeUnit` is the JDK's FIRST action, not a defaulting decision:
    // (this guard is shared by `native_cdl_await_timeout`, which IS registered
    // and was measurably wrong, and `native_sem_try_acquire_timeout`, which the
    // registry says is registered NOWHERE -- its copy of the defect is latent,
    // and is fixed here so it cannot arrive with the registration.)
    // `await` opens with `unit.toNanos(timeout)`. Defaulting to MILLISECONDS
    // turned `await(1, null)` into a one-millisecond wait returning `false`,
    // where HotSpot (and this VM's own strict arm, which refuses this native
    // and runs the bytecode) raises NPE. Message transcribed from the oracle.
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        if sem_try_acquire_inner(ctx, this, 1) {
            return Ok(Some(Value::Int(1)));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0)));
        }
        let wait_ms = bounded_monitor_wait_ms(remaining, 10);
        // GC-SAFEPOINT FIX: see sem_acquire_blocking above; `this` must stay
        // fresh across both calls below AND into the next loop iteration's
        // `sem_try_acquire_inner(ctx, this, 1)`.
        this = ctx.monitor_enter_gc_safe(this);
        let wait_result = monitor_wait_keepalive(ctx, this, Some(wait_ms));
        this = match &wait_result {
            Ok(o) => *o,
            Err(_) => this,
        };
        ctx.monitor_exit(this);
        wait_result?;
    }
}

pub(crate) fn native_sem_available_permits(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let permits = sem_permits(ctx, this);
    Ok(Some(Value::Int(permits)))
}

pub(crate) fn native_sem_drain_permits(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above.
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    sem_set_permits(ctx, this, 0);
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(permits)))
}

pub(crate) fn native_sem_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !sem_has_holder_slot(ctx, this) {
        return Ok(Some(Value::Int(sem_side_state(ctx, this).fair)));
    }
    let (_, h) = sem_holder(ctx, this);
    let fair = match ctx.get_array_element(h, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(fair)))
}

pub(crate) fn native_sem_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let permits = sem_permits(ctx, this);
    let s = format!(
        "java.util.concurrent.Semaphore@{:x}[Permits = {permits}]",
        this.as_ptr() as usize
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

// ===========================================================================
// Phase 28: java.util.concurrent — Executors, Future, CompletableFuture
// ===========================================================================

pub(crate) fn register_executor_natives(registry: &mut NativeMethodRegistry) {
    let es = "java/util/concurrent/ExecutorService";
    let exec = "java/util/concurrent/Executors";
    let tp = "java/util/concurrent/ThreadPoolExecutor";
    let tf = "java/util/concurrent/ThreadFactory";

    // Executors factory methods — return synthetic executor objects
    registry.register(
        exec,
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        native_new_single_thread,
    );
    registry.register(
        exec,
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        native_new_fixed_pool,
    );
    registry.register(
        exec,
        "newCachedThreadPool",
        "()Ljava/util/concurrent/ExecutorService;",
        native_new_cached_pool,
    );
    registry.register(
        exec,
        "newCachedThreadPool",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
        native_new_cached_pool,
    );

    // ExecutorService methods
    registry.register(
        es,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_es_submit_runnable,
    );
    registry.register(
        es,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_es_submit_callable,
    );
    // 2026-09-11, lane 5 residual §9.3 — the four
    // `java/util/concurrent/AbstractExecutorService` registrations that stood
    // here are DELETED, not retired.
    //
    // `submit(Runnable)`, `submit(Callable)`, `submit(Runnable, T)` and
    // `invokeAny(Collection)` were registered on an ABSTRACT class. A dispatch
    // door asks the registry about the DECLARING class of the resolved method,
    // and every concrete executor in the image — `ThreadPoolExecutor`,
    // `ForkJoinPool` — carries its own registration of the same names, so the
    // abstract one is never the answer. `apps/probes/L5ExecutorSweep.java`
    // builds the one receiver shape that could reach it (a direct subclass
    // declaring only `execute`) and the rows still read `invocations: 0`, on
    // every probe run and in all 132 `--jdk-only` corpus reports.
    //
    // Lane 0 §1: a door that never opens is dead weight, not a §1.4 shadow, so
    // this is a deletion and the triples are deliberately NOT in
    // `RETIRED_SHADOW_L5_TRIPLES` — a retirement table entry would claim a
    // dispatch nobody has ever observed.
    //
    // What still names this class: `native_es_submit_runnable` and
    // `native_es_submit_callable` (`lucene_es.rs`) and the two closures below
    // pass it to `invoke_special_bytecode_only` as the class whose BYTECODE to
    // run for a genuinely-real executor. That call does not consult the
    // registry, so it is unaffected by these deletions — and with them gone it
    // is now the only way this class name reaches dispatch, which is the state
    // those guards were written assuming.
    // JDK-ONLY-WAVE2 (2026-08-06): `native_es_execute` is adjudicated a
    // `SyntheticStub`, not the ambient kind this registrar would otherwise give
    // it. It is a compatibility stand-in for CratonVM's synthetic 2-field
    // `Executors.new*ThreadPool()` receiver shape, and once the real
    // `ThreadPoolExecutor.<init>` runs for every factory shortcut
    // (`initialize_real_thread_pool_executor`) there is no receiver left for it
    // to stand in for on a real-JDK image. The tag is what lets
    // `real_protected_stub_class` yield it to the real `execute()` bytecode
    // structurally, for every receiver, which is what replaced the eight
    // hand-written receiver-shape probes in `vm`. See
    // `jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`.
    registry.register_with_kind(
        es,
        "execute",
        "(Ljava/lang/Runnable;)V",
        native_es_execute,
        NativeKind::SyntheticStub,
    );
    registry.register(es, "shutdown", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // Real ThreadPoolExecutor: run the real bytecode so the pool's own
        // shutdown state machine transitions correctly, instead of writing
        // the synthetic 2-field placeholder's shutdown slot (would corrupt a
        // real field) or silently no-op'ing. See
        // `NativeContext::invoke_virtual_bytecode_only`'s doc for why this
        // can't just re-call `execute`/`shutdown` via `invoke_virtual` (would
        // recurse back into this same native).
        if executor_has_real_workers(ctx, this) {
            // NOT invoke_virtual_bytecode_only: this native is ALSO reached
            // via ScheduledThreadPoolExecutor.shutdown()'s super.shutdown()
            // (invokespecial always checks the native registry first -- see
            // populate_invoke_cache), and that dynamic-receiver-class helper
            // would just re-find STPE's own overriding shutdown() again --
            // infinite recursion. invoke_special_bytecode_only resolves
            // statically on the NAMED class instead of the receiver's
            // dynamic class, so it lands on ThreadPoolExecutor's own body.
            transition_real_executor_to_shutdown(ctx, this)?;
        } else {
            ctx.set_field(this, EXEC_FIELD_SHUTDOWN, Value::Int(1));
        }
        Ok(None)
    });
    registry.register(
        es,
        "shutdownNow",
        "()Ljava/util/List;",
        native_es_shutdown_now,
    );
    registry.register(es, "isShutdown", "()Z", native_es_is_shutdown);
    registry.register(es, "isTerminated", "()Z", native_es_is_shutdown);
    registry.register(
        es,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_es_await_termination,
    );

    // Also register on ThreadPoolExecutor
    registry.register(
        tp,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_es_submit_runnable,
    );
    registry.register(
        tp,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_es_submit_callable,
    );
    // Same adjudication as the `ExecutorService` registration above — this is
    // the copy that matters, because `java/util/concurrent/ThreadPoolExecutor`
    // is the class whose real bytecode is always loaded and which
    // `real_protected_stub_class` therefore protects.
    registry.register_with_kind(
        tp,
        "execute",
        "(Ljava/lang/Runnable;)V",
        native_es_execute,
        NativeKind::SyntheticStub,
    );
    registry.register(tp, "shutdown", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // Real ThreadPoolExecutor: run the real bytecode so the pool's own
        // shutdown state machine transitions correctly, instead of writing
        // the synthetic 2-field placeholder's shutdown slot (would corrupt a
        // real field) or silently no-op'ing. See
        // `NativeContext::invoke_virtual_bytecode_only`'s doc for why this
        // can't just re-call `execute`/`shutdown` via `invoke_virtual` (would
        // recurse back into this same native).
        if executor_has_real_workers(ctx, this) {
            // NOT invoke_virtual_bytecode_only: this native is ALSO reached
            // via ScheduledThreadPoolExecutor.shutdown()'s super.shutdown()
            // (invokespecial always checks the native registry first -- see
            // populate_invoke_cache), and that dynamic-receiver-class helper
            // would just re-find STPE's own overriding shutdown() again --
            // infinite recursion. invoke_special_bytecode_only resolves
            // statically on the NAMED class instead of the receiver's
            // dynamic class, so it lands on ThreadPoolExecutor's own body.
            transition_real_executor_to_shutdown(ctx, this)?;
        } else {
            ctx.set_field(this, EXEC_FIELD_SHUTDOWN, Value::Int(1));
        }
        Ok(None)
    });

    // RD.6: submit(Runnable, T) — bind the given result.
    let submit_rt_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        // BUG-CCE-0716, and this overload was the one it MISSED. Its two
        // siblings -- `native_es_submit_runnable` and
        // `native_es_submit_callable` -- both delegate a genuinely-real
        // receiver to `AbstractExecutorService`'s own bytecode; this one ran
        // the task inline on the CALLING thread and handed back an
        // already-completed future, so a real `ThreadPoolExecutor` never saw
        // the task at all.
        //
        // The tell is the pool's own accounting, and `apps/probes/L5TpeCount.java`
        // reads it per submission shape on a 2-worker pool given 3 tasks and
        // then shut down and joined -- at which point every count is
        // determined:
        //
        //   shape                 HotSpot            CratonVM (before)
        //   execute               completed=3        completed=3
        //   submit(Callable)      completed=3        completed=3
        //   submit(Runnable)      completed=3        completed=3
        //   submit(Runnable, T)   completed=3        completed=0   <-- here
        //   invokeAll             completed=3        completed=3
        //   invokeAny             completed=1        completed=0   <-- and here
        //
        // A count is the mild half of it: the task also ran on the wrong
        // THREAD, which is the difference a start-gate `CountDownLatch`
        // deadlocks on (see `spawn_runnable_on_real_thread`'s note on the
        // eager-inline policy this is the last of).
        if let Some(Value::Object(Some(this))) = args.first() {
            if crate::executor_is_real(ctx, *this) {
                return ctx.invoke_special_bytecode_only(
                    "java/util/concurrent/AbstractExecutorService",
                    "submit",
                    "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
                    args,
                );
            }
        }
        if let Some(Value::Object(Some(runnable))) = args.get(1) {
            ctx.invoke_virtual(*runnable, "run", "()V", &[])?;
        }
        let result_val = args.get(2).copied().unwrap_or(Value::Object(None));
        let future = completed_executor_future(ctx, result_val)?;
        Ok(Some(Value::Object(Some(future))))
    };
    registry.register(
        es,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        submit_rt_closure,
    );
    registry.register(
        tp,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        submit_rt_closure,
    );
    // See the §9.3 note above: the `AbstractExecutorService` copy of this
    // overload was deleted with the other three.

    // RD.6: invokeAll(Collection<Callable>) -> List<Future> — run sequentially.
    let invoke_all_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let coll = match args.get(1) {
            Some(Value::Object(Some(c))) => *c,
            _ => return Ok(Some(Value::Object(None))),
        };
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))])?;
        let task_count =
            match cratonvm_native_collections::native_al_size(ctx, &[Value::Object(Some(coll))])? {
                Some(Value::Int(size)) => size,
                _ => 0,
            };
        for index in 0..task_count {
            let callable_val = cratonvm_native_collections::native_al_get(
                ctx,
                &[Value::Object(Some(coll)), Value::Int(index)],
            )?
            .unwrap_or(Value::Object(None));
            let future = if let Value::Object(Some(c)) = callable_val {
                match ctx.invoke_virtual(c, "call", "()Ljava/lang/Object;", &[]) {
                    Ok(r) => completed_executor_future(ctx, r.unwrap_or(Value::Object(None)))?,
                    Err(e) => failed_executor_future(ctx, &e)?,
                }
            } else {
                completed_executor_future(ctx, Value::Object(None))?
            };
            cratonvm_native_collections::native_al_add(
                ctx,
                &[Value::Object(Some(list)), Value::Object(Some(future))],
            )?;
        }
        Ok(Some(Value::Object(Some(list))))
    };

    // RD.6: invokeAny(Collection<Callable>) — returns the first successful result.
    let invoke_any_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let coll = match args.get(1) {
            Some(Value::Object(Some(c))) => *c,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Same omission as `submit_rt_closure` above, and the same remedy: a
        // genuinely-real executor runs `AbstractExecutorService.invokeAny`,
        // so the task is submitted to the pool rather than called inline on
        // the caller. `L5TpeCount` reads `completed=0` against HotSpot's `1`
        // without this.
        if let Some(Value::Object(Some(this))) = args.first() {
            if crate::executor_is_real(ctx, *this) {
                return ctx.invoke_special_bytecode_only(
                    "java/util/concurrent/AbstractExecutorService",
                    "invokeAny",
                    "(Ljava/util/Collection;)Ljava/lang/Object;",
                    args,
                );
            }
        }
        let iter_val = ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[])?;
        let mut last_err: Option<cratonvm_types::error::MethodCallFailed> = None;
        if let Some(Value::Object(Some(iter))) = iter_val {
            loop {
                let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[])?;
                if has_next != Some(Value::Int(1)) {
                    break;
                }
                let callable_val = ctx
                    .invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[])?
                    .unwrap_or(Value::Object(None));
                if let Value::Object(Some(c)) = callable_val {
                    match ctx.invoke_virtual(c, "call", "()Ljava/lang/Object;", &[]) {
                        Ok(r) => return Ok(Some(r.unwrap_or(Value::Object(None)))),
                        Err(e) => {
                            last_err = Some(e);
                        }
                    }
                }
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            Err(RuntimeError::IllegalStateException {
                message: "ExecutionException: no task succeeded in invokeAny".to_string(),
            }
            .into())
        }
    };
    registry.register(
        es,
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        invoke_any_closure,
    );
    registry.register(
        tp,
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        invoke_any_closure,
    );
    // See the §9.3 note above: the `AbstractExecutorService` copy of
    // `invokeAny` was deleted with the other three.
}

/// Bug D (kafka-suite-0617) — submit a `Runnable` to a real, **bounded** pool of
/// Java worker threads instead of running it inline on the calling thread.
///
/// The historical eager-inline policy (`runnable.run()` on the caller) was a
/// stopgap because CratonVM's *Rust* ForkJoinPool workers cannot run Java
/// bytecode. But it **deadlocks** whenever the submitted task blocks waiting for
/// a signal the *submitting* thread sends later — the canonical case is a
/// start-gate `CountDownLatch.await()` (the submitter `countDown()`s only after
/// submitting), and any `CompletableFuture.*Async` stage whose completer is the
/// submitter. The whole Kafka consumer/producer `internals` TIMEOUT cluster is
/// this pattern.
///
/// A naive "spawn one real thread per task" fix trades deadlock for **thread
/// explosion** (RecordHeadersTest spawned ~477 OS threads → starvation). The
/// robust fix mirrors HotSpot's common pool: a single bounded
/// `ThreadPoolExecutor` (cached-style — grows on demand to a cap, reuses idle
/// workers, retires them after a keep-alive so the VM still exits cleanly past
/// `main()` without daemon threads). `submit()` on a *real* TPE runs real JDK
/// bytecode on a real worker thread (only the synthetic-executor stubs and the
/// force-gated `ForkJoinPool.execute` are intercepted), so blocking tasks park a
/// worker while the submitter stays free.
///
/// This covers the executor-`execute(Runnable)` path only; the recursive
/// `ForkJoinTask.fork/invoke/submit(ForkJoinTask)` compute path stays
/// eager-inline (CPU-bound, must not spawn threads). If the pool can't be
/// created we fall back to inline so the completion contract still holds.
static ASYNC_FUTURE_ROOTS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

fn track_async_future(ctx: &mut dyn NativeContext, future: ObjectRef) {
    let handle = ctx.add_global_root(future);
    if handle != 0 {
        ASYNC_FUTURE_ROOTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle);
    }
}

pub(crate) fn async_tasks_quiescent(ctx: &mut dyn NativeContext) -> bool {
    let mut roots = ASYNC_FUTURE_ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut pending = false;
    roots.retain(|handle| {
        let done = ctx.resolve_global_root(*handle).and_then(|future| {
            match ctx.invoke_virtual(future, "isDone", "()Z", &[]) {
                Ok(Some(Value::Int(done))) => Some(done != 0),
                _ => None,
            }
        });
        if done == Some(true) {
            ctx.remove_global_root(*handle);
            false
        } else {
            pending = true;
            true
        }
    });
    !pending
}

pub(crate) fn spawn_runnable_on_real_thread(
    ctx: &mut dyn NativeContext,
    runnable: ObjectRef,
) -> MethodCallResult {
    // Pin across the pool get-or-create below — the first call runs
    // <clinit>/<init> bytecode and allocates, and a moving young GC there
    // would relocate the runnable out from under this raw Rust local
    // (native stale-local family). Re-read via the pin before every use.
    let pin = ctx.pin_native_root(runnable);
    if let Some(pool) = async_worker_pool(ctx) {
        // Record before handing the task to the executor: a fast worker can
        // enter user code (and its short scheduling sleeps) before execute()
        // returns to the submitting thread.
        note_async_runnable_submitted();
        // execute() is the ForkJoinPool.execute contract we are emulating here;
        // avoid wrapping every CompletableFuture async task in a FutureTask.
        let runnable_cur = ctx.read_native_pin(pin, runnable);
        let future = match ctx.new_object_initialized(
            "java/util/concurrent/FutureTask",
            "(Ljava/lang/Runnable;Ljava/lang/Object;)V",
            &[Value::Object(Some(runnable_cur)), Value::Object(None)],
        )? {
            Some(Value::Object(Some(future))) => future,
            _ => {
                ctx.unpin_native_roots(pin);
                return ctx.invoke_virtual(runnable_cur, "run", "()V", &[]);
            }
        };
        let future_pin = ctx.pin_native_root(future);
        let submitted = ctx.invoke_virtual(
            pool,
            "execute",
            "(Ljava/lang/Runnable;)V",
            &[Value::Object(Some(ctx.read_native_pin(future_pin, future)))],
        );
        let future = ctx.read_native_pin(future_pin, future);
        ctx.unpin_native_roots(future_pin);
        ctx.unpin_native_roots(pin);
        submitted?;
        track_async_future(ctx, future);
        let grace = async_submit_handoff_grace();
        if grace.is_zero() {
            std::thread::yield_now();
        } else {
            ctx.begin_blocking_region();
            std::thread::sleep(grace);
            ctx.end_blocking_region();
        }
        return Ok(None);
    }
    // Fallback: pool creation failed — preserve the completion contract inline.
    let runnable_cur = ctx.read_native_pin(pin, runnable);
    ctx.unpin_native_roots(pin);
    ctx.invoke_virtual(runnable_cur, "run", "()V", &[])?;
    Ok(None)
}

pub(crate) fn register_completable_future_natives(registry: &mut NativeMethodRegistry) {
    let cf = "java/util/concurrent/CompletableFuture";
    let fut = "java/util/concurrent/Future";
    let ft = "java/util/concurrent/FutureTask";

    // CompletableFuture constructors and factory
    registry.register(cf, "<init>", "()V", native_cf_init);
    registry.register(
        cf,
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_completed,
    );
    registry.register(
        cf,
        "supplyAsync",
        "(Ljava/util/function/Supplier;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_supply_async,
    );

    // CompletableFuture instance methods
    registry.register(cf, "get", "()Ljava/lang/Object;", native_fut_get);
    registry.register(cf, "join", "()Ljava/lang/Object;", native_fut_get);
    registry.register(cf, "isDone", "()Z", native_fut_is_done);
    registry.register(cf, "complete", "(Ljava/lang/Object;)Z", native_cf_complete);
    registry.register(
        cf,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_apply,
    );
    registry.register(
        cf,
        "thenAccept",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_accept,
    );
    registry.register(
        cf,
        "getNow",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cf_get_now,
    );

    // CompletableFuture cancel/isCancelled
    // done field: 0=pending, 1=normal, 2=exceptional, 3=cancelled
    registry.register(cf, "cancel", "(Z)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let done = match ctx.get_field(this, 1) {
            Value::Int(d) => d,
            _ => 0,
        };
        if done != 0 {
            return Ok(Some(Value::Int(0))); // already completed
        }
        ctx.set_field(this, 1, Value::Int(3)); // 3 = cancelled
        Ok(Some(Value::Int(1)))
    });
    registry.register(cf, "isCancelled", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let done = match ctx.get_field(this, 1) {
            Value::Int(3) => 1,
            _ => 0,
        };
        Ok(Some(Value::Int(done)))
    });

    // Future interface methods
    registry.register(fut, "get", "()Ljava/lang/Object;", native_fut_get);
    registry.register(
        fut,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed,
    );
    registry.register(fut, "isDone", "()Z", native_fut_is_done);
    // Real cancel/isCancelled over the shared future layout (field
    // FUT_FIELD_DONE: 0=pending, 1=normal, 2=exceptional, 3=cancelled) — the
    // same encoding `cf.cancel`/`cf.isCancelled` above use. The previous
    // constant `false` made every `Future.cancel(..)` fail and every
    // `isCancelled()` deny a cancellation that had in fact happened, so
    // `while (!f.isCancelled())` loops never terminated.
    //
    // NOTE (registration order): `register_phase55_executors`
    // (phases_late/concurrent.rs) re-registers all three of
    // `java/util/concurrent/Future`'s isDone/isCancelled/cancel LATER in
    // `register_synthetic_overrides`, so on the interface triple that one wins.
    // These stay correct-by-construction in case that order changes.
    registry.register(fut, "cancel", "(Z)Z", native_fut_cancel);
    registry.register(fut, "isCancelled", "()Z", native_fut_is_cancelled);

    // FutureTask
    registry.register(ft, "get", "()Ljava/lang/Object;", native_fut_get);
    registry.register(
        ft,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed,
    );
    registry.register(ft, "isDone", "()Z", native_fut_is_done);
    // Synthetic FutureTask uses the same (result=0, done=1) layout — see the
    // `try_alloc_concurrent_synthetic(.., "java/util/concurrent/FutureTask", 2)?`
    // call sites in phases_late/net_channels.rs. Nothing else registers
    // FutureTask.cancel/isCancelled, so the old constant `false` pair was the
    // live answer: `FutureTask.cancel(true)` could never succeed and a
    // cancelled task still reported `isCancelled() == false`.
    registry.register(ft, "cancel", "(Z)Z", native_fut_cancel);
    registry.register(ft, "isCancelled", "()Z", native_fut_is_cancelled);

    // CompletableFuture timed get
    registry.register(
        cf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed,
    );
}

/// `Future.cancel(boolean)` / `FutureTask.cancel(boolean)` over the shared
/// synthetic future layout. `FUT_FIELD_DONE` encodes 0=pending, 1=completed
/// normally, 2=completed exceptionally, 3=cancelled (the encoding
/// `CompletableFuture.cancel` already used). Per the `Future` contract this
/// returns false when the task has already completed and true once the task is
/// (or already was) cancelled.
fn native_fut_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == 3 {
        // Already cancelled — the JDK returns true for repeated cancel().
        return Ok(Some(Value::Int(1)));
    }
    if done != 0 {
        // Already completed normally/exceptionally: cannot cancel.
        return Ok(Some(Value::Int(0)));
    }
    ctx.set_field(this, FUT_FIELD_DONE, Value::Int(3));
    Ok(Some(Value::Int(1)))
}

/// `Future.isCancelled()` / `FutureTask.isCancelled()` — see
/// [`native_fut_cancel`] for the `FUT_FIELD_DONE` encoding.
fn native_fut_is_cancelled(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cancelled = matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(3));
    Ok(Some(Value::Int(if cancelled { 1 } else { 0 })))
}

fn native_fut_get_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RD.6: timed Future.get. Our executors run synchronously so the future is
    // already done by the time .get is called. Still, we honour the timeout
    // contract and throw TimeoutException if it is somehow unset.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ord = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord).max(0) as u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        let done = match ctx.get_field(this, FUT_FIELD_DONE) {
            Value::Int(d) => d,
            _ => 0,
        };
        if done != 0 && done != -1 {
            return native_fut_get(ctx, args);
        }
        if std::time::Instant::now() >= deadline {
            return Err(RuntimeError::IllegalStateException {
                message: "TimeoutException: future did not complete in time".to_string(),
            }
            .into());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let wait_ms = remaining.as_millis().min(5).max(1) as u64;
        ctx.monitor_enter(this);
        monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
    }
}

fn native_cf_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, FUT_FIELD_RESULT, Value::Object(None));
    ctx.set_field(this, FUT_FIELD_DONE, Value::Int(0));
    Ok(None)
}

fn native_cf_completed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    let cf = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4)?;
    ctx.set_field(cf, FUT_FIELD_RESULT, val);
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_supply_async(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cf = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4)?;
    if let Some(Value::Object(Some(supplier))) = args.first() {
        let result = ctx.invoke_virtual(*supplier, "get", "()Ljava/lang/Object;", &[])?;
        ctx.set_field(cf, FUT_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
    }
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_fut_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Check for deferred exceptionally stage (done == -1)
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == -1 {
        // Deferred: check source CF and invoke handler if source is exceptionally completed
        let source = match ctx.get_field(this, 2) {
            // CF_FIELD_SOURCE
            Value::Object(Some(s)) => s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let source_done = match ctx.get_field(source, FUT_FIELD_DONE) {
            Value::Int(d) => d,
            _ => 0,
        };
        if source_done == 2 {
            // Exceptionally completed — invoke the handler
            let handler = match ctx.get_field(this, 3) {
                // CF_FIELD_HANDLER
                Value::Object(Some(h)) => h,
                _ => return Ok(Some(Value::Object(None))),
            };
            let exc = ctx.get_field(source, FUT_FIELD_RESULT);
            let result = ctx.invoke_virtual(
                handler,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[exc],
            )?;
            let val = result.unwrap_or(Value::Object(None));
            // Cache the result
            ctx.set_field(this, FUT_FIELD_RESULT, val.clone());
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
            return Ok(Some(val));
        } else if source_done != 0 {
            // Source completed normally — pass through result
            let val = ctx.get_field(source, FUT_FIELD_RESULT);
            ctx.set_field(this, FUT_FIELD_RESULT, val.clone());
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(source_done));
            return Ok(Some(val));
        }
        // Source still pending — return null for now
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
}

fn native_fut_is_done(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == -1 {
        // Deferred: check if source is done
        let source_done = match ctx.get_field(this, 2) {
            Value::Object(Some(s)) => match ctx.get_field(s, FUT_FIELD_DONE) {
                Value::Int(d) if d != 0 => 1,
                _ => 0,
            },
            _ => 0,
        };
        return Ok(Some(Value::Int(source_done)));
    }
    Ok(Some(Value::Int(if done != 0 { 1 } else { 0 })))
}

/// `CompletableFuture.complete(value)` — handles BOTH the synthetic side-field
/// model and a real-JDK `java.util.concurrent.CompletableFuture`.
///
/// CRITICAL: the synthetic override only flips the side fields (`result`@0,
/// `done`@1) and historically never ran `postComplete()`. For a *real-JDK*
/// `CompletableFuture` (allocated by the genuine bytecode `<init>`, whose
/// instance layout is `result`@0 + the lock-free `stack`@1), a thread blocked in
/// the real `waitingGet()` (untimed `get()`/`join()`) pushes a `Signaller` onto
/// `stack`@1 and parks via `LockSupport.park`. The synthetic override (a) never
/// fired that Signaller (so the parked thread was never `unpark`ed → indefinite
/// hang on every cross-thread `complete()` after the waiter blocks — the timed
/// `get(...)` only "self-heals" because `parkNanos` re-polls `result`), and
/// (b) clobbered `stack`@1 with the `done` int. Distinguish the two layouts by
/// slot-1's value type: a real-JDK CF has an Object `stack` (null or a Signaller
/// chain), a synthetic CF has an Int `done`. For the real-JDK object delegate to
/// the genuine `completeValue` + `postComplete` so waiters are released.
pub(crate) fn native_cf_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    match ctx.get_field(this, FUT_FIELD_DONE) {
        // Synthetic CF model — `done` flag lives in slot 1.
        Value::Int(done) => {
            if done != 0 {
                return Ok(Some(Value::Int(0))); // already completed
            }
            ctx.set_field(this, FUT_FIELD_RESULT, val);
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        // Real-JDK CompletableFuture — slot 1 is the `stack` (Completion) field.
        // Run the genuine completion path so `postComplete()` fires any parked
        // `Signaller` (i.e. `LockSupport.unpark` the thread blocked in `get()`).
        _ => {
            let triggered = ctx
                .invoke_virtual(this, "completeValue", "(Ljava/lang/Object;)Z", &[val])?
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            // `invoke_virtual_bytecode_only`, not `invoke_virtual`:
            // `postComplete` is ordinary JDK bytecode and can never be a
            // native, so asking the by-NAME native resolver is two hashes of
            // the 53-byte triple plus the cold descriptor-quirk rewrite, all
            // of them misses, on every completion. Measured on the sibling
            // call in `native_cf_complete`: 1.154x of the whole composition
            // workload, and it takes the registry's own miss census from
            // 100 008 rows for this one triple to zero. This site is not on
            // that measured path, and is changed with it because a fix that
            // lands for one of five identical call sites is at the wrong
            // level.
            ctx.invoke_virtual_bytecode_only(this, "postComplete", "()V", &[])?;
            Ok(Some(Value::Int(triggered)))
        }
    }
}

pub(crate) fn native_cf_then_apply(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(_)) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniApplyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    let cf = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4)?;
    // RD.8: propagate exceptional/cancelled state without invoking the Function.
    if done == 2 || done == 3 {
        ctx.set_field(cf, FUT_FIELD_RESULT, ctx.get_field(this, FUT_FIELD_RESULT));
        ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(done));
        return Ok(Some(Value::Object(Some(cf))));
    }
    let val = ctx.get_field(this, FUT_FIELD_RESULT);
    match ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    ) {
        Ok(result) => {
            ctx.set_field(cf, FUT_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
            ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
        }
        Err(e) => {
            let throwable = cf_capture_throwable(ctx, &e);
            ctx.set_field(cf, FUT_FIELD_RESULT, throwable);
            ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(2));
        }
    }
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn native_cf_then_accept(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(_)) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniAcceptStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(consumer)),
            ],
        );
    }
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    let cf = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4)?;
    if done == 2 || done == 3 {
        ctx.set_field(cf, FUT_FIELD_RESULT, ctx.get_field(this, FUT_FIELD_RESULT));
        ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(done));
        return Ok(Some(Value::Object(Some(cf))));
    }
    let val = ctx.get_field(this, FUT_FIELD_RESULT);
    match ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val]) {
        Ok(_) => {
            ctx.set_field(cf, FUT_FIELD_RESULT, Value::Object(None));
            ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
        }
        Err(e) => {
            let throwable = cf_capture_throwable(ctx, &e);
            ctx.set_field(cf, FUT_FIELD_RESULT, throwable);
            ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(2));
        }
    }
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_get_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done != 0 {
        Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
    } else {
        Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
    }
}

pub(crate) fn transition_real_executor_to_shutdown(
    ctx: &mut dyn NativeContext,
    executor: ObjectRef,
) -> MethodCallResult {
    // This bridge calls into Java several times (AtomicInteger, the executor
    // hooks, and finally ThreadPoolExecutor.tryTerminate).  Keep both the
    // receiver and `ctl` rooted across those calls; otherwise a moving
    // collection can leave the final invokespecial targeting a stale
    // executor, observed as an intermittent `this.ctl == null` while JUnit
    // closes its ScheduledThreadPoolExecutor timeout resource.
    let executor_pin = ctx.pin_native_root(executor);
    let executor = ctx.read_native_pin(executor_pin, executor);
    let Value::Object(Some(ctl)) = ctx.get_field_by_name(executor, "ctl") else {
        ctx.unpin_native_roots(executor_pin);
        return Ok(None);
    };
    let ctl_pin = ctx.pin_native_root(ctl);
    let result = (|| {
        let ctl = ctx.read_native_pin(ctl_pin, ctl);
        let current = match ctx.invoke_virtual(ctl, "get", "()I", &[])? {
            Some(Value::Int(value)) => value,
            _ => return Ok(None),
        };
        // ThreadPoolExecutor packs run state in the high 3 bits and worker
        // count below. SHUTDOWN is run-state 0, so retain only the worker
        // count bits.
        let shutdown = current & 0x1fff_ffff;
        let ctl = ctx.read_native_pin(ctl_pin, ctl);
        let _ = ctx.invoke_virtual(ctl, "set", "(I)V", &[Value::Int(shutdown)]);
        // A graceful ThreadPoolExecutor shutdown must wake IDLE workers so
        // they observe SHUTDOWN and leave getTask().  Merely updating ctl
        // leaks every worker blocked in LinkedBlockingQueue.take().
        //
        // Only the idle ones: this is `shutdown()`, not `shutdownNow()`, and a
        // worker in the middle of a task must be allowed to finish it. See
        // `interrupt_executor_workers_filtered`.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ =
            crate::interrupt_executor_workers_filtered(ctx, executor, /* only_idle */ true);
        // A ScheduledThreadPoolExecutor owns delayed tasks in its work queue.
        // Its real `onShutdown()` removes cancelled delayed tasks (including
        // JUnit's cancelled timeout watchdog); without it, the queue stays
        // nonempty until the original timeout expires and awaitTermination()
        // cannot finish.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ = ctx.invoke_virtual_bytecode_only(executor, "onShutdown", "()V", &[])?;
        // A zero-worker pool has no worker-exit path to call tryTerminate(),
        // so finalize it explicitly after the shutdown hook.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ = ctx.invoke_special_bytecode_only(
            "java/util/concurrent/ThreadPoolExecutor",
            "tryTerminate",
            "()V",
            &[Value::Object(Some(executor))],
        )?;
        Ok(None)
    })();
    ctx.unpin_native_roots(executor_pin);
    result
}

fn native_sr_next_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int((seed >> 16) as i32)))
}

fn native_sr_next_int_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bound = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if bound <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int(
        ((seed >> 16) as i32).unsigned_abs() as i32 % bound,
    )))
}

pub(crate) fn native_sr_next_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Long(seed)))
}

fn native_sr_next_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    if len == 0 {
        return Ok(None);
    }
    // Fill the buffer in a single bulk call to the OS CSPRNG instead of
    // burning a 64-bit random draw per byte (which discarded 7/8 of each
    // call's entropy). If the CSPRNG is unavailable we fall back to
    // per-byte `sr_os_random_u64` mixing so we never silently produce
    // deterministic bytes.
    let mut buf = vec![0u8; len];
    if !sr_os_random_bytes(&mut buf) {
        for chunk in buf.chunks_mut(8) {
            let v = sr_os_random_u64().to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(v.iter()) {
                *dst = *src;
            }
        }
    }
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((b as i8) as i32));
    }
    // Zero the intermediate buffer — cheap defence in depth even though
    // the heap copy in `set_array_element` is already the authoritative
    // destination.
    for b in buf.iter_mut() {
        *b = 0;
    }
    Ok(None)
}

pub(crate) fn native_sr_next_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let seed = sr_next_seed(ctx, this);
    let d = ((seed >> 16) as u32 as f64) / (u32::MAX as f64);
    Ok(Some(Value::Double(d)))
}

pub(crate) fn native_sr_next_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int(if seed & 1 == 0 { 0 } else { 1 })))
}

fn native_sr_generate_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let num_bytes = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, num_bytes);
    // Use OS entropy directly (CSPRNG) for seed generation.
    let mut buf = vec![0u8; num_bytes];
    if !sr_os_random_bytes(&mut buf) {
        // Fallback: use system_entropy_seed per-byte
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (system_entropy_seed() >> (i % 8 * 8)) as u8;
        }
    }
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ===========================================================================
// Phase 34: ReadWriteLock, Atomic extras, LongAdder
// ===========================================================================

// ---------------------------------------------------------------------------
// Real AQS long state
// ---------------------------------------------------------------------------
//
// JDK 25's ReentrantReadWriteLock uses AbstractQueuedLongSynchronizer.  Its
// state word is only ever reached through these final protected methods by the
// real lock implementation, but the generic Unsafe CAS path stores a 64-bit
// value in CratonVM's 16-byte Value slot.  Keep the JDK queue and lock
// algorithm intact while placing that one scalar state word in an AtomicI64.
// The side table is keyed with the same moving-GC-stable identity protocol as
// the other native lock state tables.
fn aqls_state_table() -> &'static parking_lot::Mutex<
    std::collections::HashMap<usize, std::sync::Arc<std::sync::atomic::AtomicI64>>,
> {
    static TABLE: std::sync::OnceLock<
        parking_lot::Mutex<
            std::collections::HashMap<usize, std::sync::Arc<std::sync::atomic::AtomicI64>>,
        >,
    > = std::sync::OnceLock::new();
    TABLE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn aqls_state_slot(
    ctx: &mut dyn NativeContext,
    synchronizer: ObjectRef,
) -> Result<std::sync::Arc<std::sync::atomic::AtomicI64>, MethodCallFailed> {
    let key = gc_stable_lock_key(ctx, synchronizer);
    let mut table = aqls_state_table().lock();
    Ok(table
        .entry(key?)
        // AbstractQueuedLongSynchronizer initializes `state` to zero. Every
        // later Java-side transition goes through the three forced natives
        // registered below, including deserialization's `setState` path.
        .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)))
        .clone())
}

fn native_aqls_get_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(synchronizer))) = args.first() else {
        return Ok(Some(Value::Long(0)));
    };
    let state = aqls_state_slot(ctx, *synchronizer)?.load(std::sync::atomic::Ordering::SeqCst);
    Ok(Some(Value::Long(state)))
}

fn native_aqls_set_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (Some(Value::Object(Some(synchronizer))), Some(Value::Long(state))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    aqls_state_slot(ctx, *synchronizer)?.store(*state, std::sync::atomic::Ordering::SeqCst);
    Ok(None)
}

fn native_aqls_compare_and_set_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (
        Some(Value::Object(Some(synchronizer))),
        Some(Value::Long(expected)),
        Some(Value::Long(new_state)),
    ) = (args.first(), args.get(1), args.get(2))
    else {
        return Ok(Some(Value::Int(0)));
    };
    let swapped = aqls_state_slot(ctx, *synchronizer)?
        .compare_exchange(
            *expected,
            *new_state,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok();
    Ok(Some(Value::Int(i32::from(swapped))))
}

fn register_aqls_state_natives(registry: &mut NativeMethodRegistry) {
    let aqls = "java/util/concurrent/locks/AbstractQueuedLongSynchronizer";
    registry.register(aqls, "getState", "()J", native_aqls_get_state);
    registry.register(aqls, "setState", "(J)V", native_aqls_set_state);
    registry.register(
        aqls,
        "compareAndSetState",
        "(JJ)Z",
        native_aqls_compare_and_set_state,
    );
}

pub(crate) fn register_rwlock_natives(registry: &mut NativeMethodRegistry) {
    register_aqls_state_natives(registry);
    // Real AQS is the default (and is explicitly enabled by the Spring Boot
    // runner).  Its `ReentrantReadWriteLock` constructor creates real
    // ReadLock/WriteLock views whose `sync` field points at the real nested
    // `Sync`.  The legacy callbacks below instead manufacture a one-field
    // synthetic view with its parent RWL in slot zero.  That happens to work
    // for the callbacks they replace, but not for any unoverridden real-JDK
    // method: `WriteLock.newCondition()` reads that slot as `Sync` and then
    // tries to invoke `Sync.newCondition()` on the outer RWL, yielding the
    // misleading `ReentrantReadWriteLock.newCondition` NoSuchMethodError.
    // Keep the complete RWL family on real bytecode in this mode; the JIT
    // skip list already protects the AQS family.  StampedLock remains an
    // independent native implementation and must stay registered.
    let real_aqs = !crate::nbflags().synthetic_aqs || crate::nbflags().real_aqs;
    if real_aqs {
        register_stamped_lock_natives(registry);
        return;
    }
    register_synthetic_rwlock_natives(registry);
}

/// The legacy synthetic `ReentrantReadWriteLock` surface, split out of
/// [`register_rwlock_natives`] for the same reason as
/// [`register_synthetic_aqs_natives`]: under the default real-AQS build it is
/// never registered, and `cratonvm-vm`'s inline tests cannot drive the real
/// path either, so its tests had nothing to call.
///
/// Production behaviour is unchanged — the caller above still returns early
/// under real AQS without reaching this.
pub fn register_synthetic_rwlock_natives(registry: &mut NativeMethodRegistry) {
    let rwl = "java/util/concurrent/locks/ReentrantReadWriteLock";
    let rl = "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock";
    let wl = "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock";

    // Do not intercept the real-JDK constructors. Their three reference
    // fields are { readerLock, writerLock, sync }; the historical synthetic
    // initializer wrote integer state into those slots, so a method reference
    // such as ReentrantReadWriteLock::readLock received null. Let genuine
    // bytecode initialize the layout, while keeping the native lock-operation
    // backend below for the returned lock views.
    registry.register(
        rwl,
        "readLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$ReadLock;",
        native_rwl_read_lock,
    );
    registry.register(
        rwl,
        "writeLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$WriteLock;",
        native_rwl_write_lock,
    );

    // ReadLock = 1-field synthetic (parent=0 → ReentrantReadWriteLock).
    //
    // WP4.7: The lock/unlock/tryLock callbacks now delegate to the
    // process-wide RwLock backend in `crate::stamped_lock` instead of
    // toggling a per-object field. The old field-toggle was effectively
    // unsynchronized (two threads racing on `lock()` would each read
    // readers=0, both bump it to 1, neither would exclude a writer).
    // Real exclusion is now via a Mutex+Condvar slot keyed on the parent
    // RWL's address.
    // Pattern-A fix (bug nb-lib-gckeys §1): key the ReentrantReadWriteLock
    // state table by a GC-stable identity of the parent RWL object (see
    // `gc_stable_lock_key`) rather than its raw heap address. A read/write
    // view obtained before a GC must unlock against the SAME slot afterwards;
    // `parent.as_ptr()` shifts under a moving collector and the lock/unlock
    // pair would diverge, deadlocking the lock or corrupting a neighbour slot.
    fn rwl_parent_addr(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<Option<usize>, MethodCallFailed> {
        match ctx.get_field(this, 0) {
            Value::Object(Some(parent)) => Ok(Some(gc_stable_lock_key(ctx, parent)?)),
            _ => Ok(None),
        }
    }

    registry.register(rl, "lock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
            // rw_read_lock's internal contended wait is a raw
            // parking_lot::Condvar::wait with NO GC-blocking-region bracket
            // and no Java-heap touch inside — a thread contending for this
            // lock while a writer holds it stays counted in the STW
            // barrier's `expected` forever (not in JIT, never reaches a
            // safepoint), livelocking any cross-thread STW pause requested
            // while it waits.
            ctx.begin_blocking_region();
            crate::stamped_lock::rw_read_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    });
    registry.register(rl, "unlock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_read_unlock(addr, ctx.thread_id());
        }
        Ok(None)
    });
    registry.register(rl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_try_read_lock(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(ok))))
    });
    registry.register(
        rl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            // Timed variant — caller already pre-rounded the timeout, and
            // the contention ratio in real apps doesn't justify a pdqueue
            // here. Treat as fast-path tryLock.
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let ok = match rwl_parent_addr(ctx, this)? {
                Some(a) => crate::stamped_lock::rw_try_read_lock(a, ctx.thread_id()),
                None => false,
            };
            Ok(Some(Value::Int(i32::from(ok))))
        },
    );
    registry.register(rl, "lockInterruptibly", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit — see the plain `lock()` registration above.
            ctx.begin_blocking_region();
            crate::stamped_lock::rw_read_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    });

    // WriteLock = 1-field synthetic (parent=0). WP4.7: delegate to the
    // process-wide RwLock backend (see comment on the read lock above).
    registry.register(wl, "lock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
            // rw_write_lock's internal contended wait is a raw
            // parking_lot::Condvar::wait with NO GC-blocking-region bracket
            // and no Java-heap touch inside — see the read-lock `lock()`
            // registration above for the full rationale. Confirmed via
            // symbolicated cdb stacks: TestOrderInterceptor's stuck
            // ForkJoinPool worker threads were parked exactly here, not in
            // LockSupport.park (which IS correctly bracketed) — this was the
            // actual root cause of the STW takeover livelock, not a
            // ForkJoinPool/AQS-specific issue.
            ctx.begin_blocking_region();
            crate::stamped_lock::rw_write_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    });
    registry.register(wl, "unlock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_write_unlock(addr, ctx.thread_id());
        }
        Ok(None)
    });
    registry.register(wl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_try_write_lock(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(ok))))
    });
    registry.register(
        wl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let ok = match rwl_parent_addr(ctx, this)? {
                Some(a) => crate::stamped_lock::rw_try_write_lock(a, ctx.thread_id()),
                None => false,
            };
            Ok(Some(Value::Int(i32::from(ok))))
        },
    );
    registry.register(wl, "lockInterruptibly", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit — see the plain `lock()` registration above.
            ctx.begin_blocking_region();
            crate::stamped_lock::rw_write_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    });
    registry.register(wl, "isHeldByCurrentThread", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let held = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_write_is_held(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(held))))
    });

    // StampedLock — real optimistic read / exclusive write lock with stamp validation.
    // Uses a global state map keyed by object address for per-lock state.
    register_stamped_lock_natives(registry);
}

/// JDK-ONLY-CLASSIFY: stub — all 31 registrations, and the reason is a
/// measurement rather than a reading of the class.
///
/// This function sets its own category now. It did not, and the consequence was
/// the ambient-category defect in its purest form: **the same registration site
/// produced a `Bridge` row and a `SyntheticStub` row in one boot**, because
/// this function is called from three places and the callers disagreed about
/// what was in effect. `--dump-native-registry` on JDK 25 / linux, 2026-08-06:
/// every one of the 25 `StampedLock` triples appears three times, twice
/// `bridge` and once `synthetic-stub`, and registration is last-write-wins, so
/// what actually shipped was decided by call ORDER.
///
/// `SyntheticStub` is what shipped, and it is also the right tag on the merits:
/// `java.util.concurrent.locks.StampedLock` is pure Java and JDK 25 declares no
/// `ACC_NATIVE` method on it or on its two view classes, so contract §1.5
/// cannot call these bridges. Under `--jdk-only` the whole surface is refused
/// together and the real class runs — which is the shape
/// `stampedlock-surface-must-be-complete-not-partial` asks for; a *partial*
/// surface is the failure mode there, and an ambient tag that depends on call
/// order is exactly how you get one.
///
/// CALLED THREE TIMES PER BOOT, AND THAT IS FINE — verified W7-15, 2026-08-07.
/// The census will show every triple below repeated; the repeats are these:
///
///   1. `lib.rs::register_essential_natives_with_shims` calls it directly;
///   2. the very next line there calls [`register_rwlock_natives`], which
///      calls it again on BOTH of its arms (real-AQS returns early through it,
///      synthetic AQS reaches it at the end of `register_synthetic_rwlock_natives`);
///   3. `vm/src/vm/vm_init.rs` calls it directly, in both mode arms.
///
/// A `synthetic-jdk` feature build adds a fourth via
/// `register_synthetic_overrides` -> [`register_rwlock_natives`].
///
/// IDEMPOTENT FOR DISPATCH. Each repeat re-registers the identical triples with
/// the identical named-`fn` callbacks; this function states its own category, so
/// unlike the pre-2026-08-06 behaviour described above there is no ambient-kind
/// disagreement between callers, and `current_leaf` is `false` at all of them.
/// `NativeMethodRegistry::register` updates the EXISTING slot in place for a key
/// it has already seen — same callback, same `SyntheticStub` kind, same leaf
/// claim — and does not append a slot or a `slot_invocations` counter, so
/// already-issued `NativeMethodId`s stay valid and nothing observable changes.
///
/// NOT idempotent for the two per-registration LOGS, which is where the repeats
/// become visible and why they must not be read as a defect:
///
///   * `registrations` / `categories` / `kind_stated` / `category_chosen_log` /
///     `provenance` each gain a row per repeat, with
///     `provenance.overwrote == Some(SyntheticStub)` — i.e. this surface shadows
///     ITSELF. Those are the ~62 self-shadowed `StampedLock` rows in the
///     duplicate-registration census, not 62 lost registrations.
///   * under `CompatibilityMode::JdkOnly` the refusal arm returns *before*
///     inserting, so all 31 triples are pushed onto `refused` once per call.
///     A `--jdk-only` gate that counts refusal ROWS therefore counts this
///     surface three times; count distinct triples instead.
pub fn register_stamped_lock_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let sl = "java/util/concurrent/locks/StampedLock";
    registry.register(sl, "<init>", "()V", native_stamped_init);
    registry.register(sl, "readLock", "()J", native_stamped_read_lock);
    registry.register(sl, "writeLock", "()J", native_stamped_write_lock);
    registry.register(sl, "tryOptimisticRead", "()J", native_stamped_optimistic);
    registry.register(sl, "unlockRead", "(J)V", native_stamped_unlock_read);
    registry.register(sl, "unlockWrite", "(J)V", native_stamped_unlock_write);
    // See `native_stamped_unlock_by_stamp` — ported from the phase-62
    // imitation so that block can be deleted without losing `unlock(J)V`.
    registry.register(sl, "unlock", "(J)V", native_stamped_unlock_by_stamp);
    registry.register(
        sl,
        "unstampedUnlockRead",
        "()V",
        native_stamped_unstamped_unlock_read,
    );
    registry.register(
        sl,
        "unstampedUnlockWrite",
        "()V",
        native_stamped_unstamped_unlock_write,
    );
    registry.register(sl, "tryUnlockRead", "()Z", native_stamped_try_unlock_read);
    registry.register(sl, "tryUnlockWrite", "()Z", native_stamped_try_unlock_write);
    registry.register(sl, "validate", "(J)Z", native_stamped_validate);
    registry.register(sl, "tryReadLock", "()J", native_stamped_try_read_lock);
    registry.register(sl, "tryWriteLock", "()J", native_stamped_try_write_lock);
    registry.register(
        sl,
        "tryConvertToWriteLock",
        "(J)J",
        native_stamped_try_convert_to_write,
    );
    registry.register(
        sl,
        "tryConvertToReadLock",
        "(J)J",
        native_stamped_try_convert_to_read,
    );
    // The release path Agroal's `StampedCopyOnWriteArrayList` uses INSTEAD of
    // `unlockWrite` -- see `stamped_lock::stamped_try_convert_to_optimistic`.
    // While this was missing the call ran real JDK bytecode over a `state`
    // field this backend does not drive, released nothing, and deadlocked the
    // Keycloak boot.
    registry.register(
        sl,
        "tryConvertToOptimisticRead",
        "(J)J",
        native_stamped_try_convert_to_optimistic,
    );
    // The rest of the blocking surface. Leaving ANY of these to real JDK
    // bytecode reintroduces exactly the same class of bug: the bytecode spins
    // on a `state` word nothing maintains.
    registry.register(
        sl,
        "readLockInterruptibly",
        "()J",
        native_stamped_read_lock_interruptibly,
    );
    registry.register(
        sl,
        "writeLockInterruptibly",
        "()J",
        native_stamped_write_lock_interruptibly,
    );
    registry.register(
        sl,
        "tryReadLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        native_stamped_try_read_lock_timed,
    );
    registry.register(
        sl,
        "tryWriteLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        native_stamped_try_write_lock_timed,
    );
    // `isLocked()` is a DELIBERATE completion of the synthetic surface: JDK 25's
    // `StampedLock` declares `isReadLocked`/`isWriteLocked` and no `isLocked`,
    // so the dead-everywhere sweep scores this row "method nowhere on any
    // image" and listed it for deletion. It must NOT be deleted —
    // `native-builtins/tests/registry_contracts.rs` pins it, and under
    // `--synthetic-jdk` it is the only implementation there is. A row a contract
    // test pins is gated by that fact alone, which is what the sweep's new
    // `--pinned` input carries.
    registry.register(sl, "isLocked", "()Z", native_stamped_is_locked);
    registry.register(sl, "isWriteLocked", "()Z", native_stamped_is_write_locked);
    registry.register(sl, "isReadLocked", "()Z", native_stamped_is_read_locked);
    registry.register(
        sl,
        "getReadLockCount",
        "()I",
        native_stamped_get_read_lock_count,
    );

    let sl_wv = "java/util/concurrent/locks/StampedLock$WriteLockView";
    registry.register(sl_wv, "lock", "()V", native_stamped_write_view_lock);
    registry.register(sl_wv, "tryLock", "()Z", native_stamped_write_view_try_lock);
    registry.register(sl_wv, "unlock", "()V", native_stamped_write_view_unlock);

    let sl_rv = "java/util/concurrent/locks/StampedLock$ReadLockView";
    registry.register(sl_rv, "lock", "()V", native_stamped_read_view_lock);
    registry.register(sl_rv, "tryLock", "()Z", native_stamped_read_view_try_lock);
    registry.register(sl_rv, "unlock", "()V", native_stamped_read_view_unlock);
    registry.set_category(__prev_cat);
}

// The four `native_rwl_*` stubs that used to sit here were dead code: nothing
// registered them, they stored the reader/writer counts in the object's own
// slots (which the real class uses for its `Sync`/view references), and there
// was no `lock`/`unlock` for the views they handed out. The working
// implementation lives at the bottom of this file next to
// `register_synthetic_aqs_natives`, which registers it.

// Pattern-A fix (bug nb-lib-gckeys §1): key the StampedLock state table by a
// GC-stable identity (see `gc_stable_lock_key`) instead of the raw, moving
// heap address that the lock/unlock pair could disagree on across a GC.
fn stamped_addr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<usize>, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(Some(gc_stable_lock_key(ctx, *o)?)),
        _ => Ok(None),
    }
}

fn stamped_obj(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn stamped_addr_for_obj(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    Ok(gc_stable_lock_key(ctx, obj)?)
}

fn stamped_view_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    let view = stamped_obj(args)?;
    match ctx.get_field_by_name(view, "this$0") {
        Value::Object(Some(parent)) => Some(parent),
        _ => None,
    }
}

fn native_stamped_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        crate::stamped_lock::stamped_init(addr);
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

fn native_stamped_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
    // stamped_write_lock's contended wait is a raw parking_lot::Condvar::wait
    // with NO GC-blocking-region bracket — same missing-bracket bug as
    // ReentrantReadWriteLock's rw_write_lock (see that registration's
    // comment for the full rationale and how this was diagnosed).
    ctx.begin_blocking_region();
    let stamp = crate::stamped_lock::stamped_write_lock(addr);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, obj, addr);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STAMPED").is_some() {
        eprintln!("[SL-DBG] writeLock addr={addr:#x} stamp={stamp}");
    }
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    ctx.begin_blocking_region();
    let stamp = crate::stamped_lock::stamped_read_lock(addr);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_try_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let stamp = crate::stamped_lock::stamped_try_read_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_try_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let stamp = crate::stamped_lock::stamped_try_write_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_write_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    ctx.begin_blocking_region();
    crate::stamped_lock::stamped_write_lock(addr);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

fn native_stamped_write_view_try_lock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    let stamp = crate::stamped_lock::stamped_try_write_lock(addr);
    mirror_stamped_state(ctx, parent, addr);
    Ok(Some(Value::Int(i32::from(stamp != 0))))
}

fn native_stamped_write_view_unlock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    if !crate::stamped_lock::stamped_try_unstamped_unlock_write(addr) {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "StampedLock write lock not held".to_string(),
        }
        .into());
    }
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

fn native_stamped_read_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    ctx.begin_blocking_region();
    crate::stamped_lock::stamped_read_lock(addr);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

fn native_stamped_read_view_try_lock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    let stamp = crate::stamped_lock::stamped_try_read_lock(addr);
    mirror_stamped_state(ctx, parent, addr);
    Ok(Some(Value::Int(i32::from(stamp != 0))))
}

fn native_stamped_read_view_unlock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    if !crate::stamped_lock::stamped_try_unstamped_unlock_read(addr) {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "StampedLock read lock not held".to_string(),
        }
        .into());
    }
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

fn native_stamped_optimistic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Long(STAMPED_ORIGIN))),
    };
    Ok(Some(Value::Long(
        crate::stamped_lock::stamped_try_optimistic_read(addr),
    )))
}

fn native_stamped_validate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    let valid = crate::stamped_lock::stamped_validate(addr, stamp);
    Ok(Some(Value::Int(i32::from(valid))))
}

/// The stamp argument of a `(J)V` / `(J)Z` StampedLock native, or `None`
/// when the frame did not carry one.
fn stamped_arg(args: &[Value]) -> Option<i64> {
    match args.get(1) {
        Some(Value::Long(v)) => Some(*v),
        _ => None,
    }
}

/// `unlockRead(long stamp)`.
///
/// The stamp is CHECKED, not ignored: the JDK throws
/// `IllegalMonitorStateException` when the stamp is not a read stamp, when
/// its version has moved on, or when no read hold is outstanding. Verified
/// against HotSpot 25.0.3 (`SLBits.java` rows `J.unlockReadBogus = IMSE`).
fn native_stamped_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let stamp = stamped_arg(args).unwrap_or(0);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = crate::stamped_lock::stamped_unlock_read(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    Ok(None)
}

/// `unlockWrite(long stamp)`.
///
/// The JDK guard is `if (state != stamp || (stamp & WBIT) == 0L) throw` — an
/// EXACT state-word match, so a stale stamp cannot release a later writer's
/// hold. This used to ignore the stamp and release whatever was held, which
/// silently broke mutual exclusion whenever a caller passed the wrong stamp
/// (which the `isWriteLockStamp` encoding bug made routine). Verified
/// against HotSpot (`J.doubleUnlockWrite = IMSE`).
fn native_stamped_unlock_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let stamp = stamped_arg(args).unwrap_or(0);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = crate::stamped_lock::stamped_unlock_write(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    Ok(None)
}

/// `StampedLock.unlock(long stamp)` — the stamp-dispatching release.
///
/// Added 2026-07-28. `unlock(J)V` was the one method of the real
/// `StampedLock` surface that NO registrar provided — it threw
/// `NoSuchMethodError` in both run modes. Added here alongside disabling
/// `native-collections`' rival StampedLock block (see the comment at its
/// call site), which was silently destroying mutual exclusion.
///
/// For anyone tracing history: `register_p62_stamped_lock` in
/// `phases_late/concurrent.rs` looks like the culprit and is NOT — it is
/// dead code, never called from anywhere in the tree. The live shadower was
/// `native-collections`, which wins by running after `register_builtins`.
///
/// Spec, transcribed from JDK 25 `StampedLock.unlock`:
///
/// ```java
/// public void unlock(long stamp) {
///     if ((stamp & WBIT) != 0L) unlockWrite(stamp);
///     else                      unlockRead(stamp);
/// }
/// ```
///
/// so it is a pure re-dispatch, and the `IllegalMonitorStateException` comes
/// from the callee. Note this arm tests `(stamp & WBIT) != 0`, NOT
/// `isWriteLockStamp`'s `(stamp & ABITS) == WBIT`.
///
/// Until 2026-08-07 this read the write flag as `stamp & 1` and the read
/// flag as `stamp & 2` — a private encoding this backend no longer uses; see
/// the constant block in `stamped_lock` for why the JDK's own layout is now
/// mandatory. It also released the hold WITHOUT checking the stamp's
/// version, so a stale stamp released a live hold belonging to someone else.
fn native_stamped_unlock_by_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let Some(stamp) = stamped_arg(args) else {
        // The verifier guarantees a long here; anything else means the call
        // did not come through `unlock(J)V`, so refuse rather than guess.
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "unlock: missing stamp argument".to_string(),
        }
        .into());
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = if stamp & crate::stamped_lock::JDK_WBIT != 0 {
        crate::stamped_lock::stamped_unlock_write(addr, stamp)
    } else {
        // Everything else -- read stamps, optimistic stamps and the zero
        // sentinel -- goes to unlockRead, which rejects the last two.
        crate::stamped_lock::stamped_unlock_read(addr, stamp)
    };
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    mirror_stamped_state(ctx, obj, addr);
    Ok(None)
}

fn native_stamped_unstamped_unlock_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        if !crate::stamped_lock::stamped_try_unstamped_unlock_read(addr) {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "StampedLock read lock not held".to_string(),
            }
            .into());
        }
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

fn native_stamped_unstamped_unlock_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        if !crate::stamped_lock::stamped_try_unstamped_unlock_write(addr) {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "StampedLock write lock not held".to_string(),
            }
            .into());
        }
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

fn native_stamped_try_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let unlocked = crate::stamped_lock::stamped_try_unstamped_unlock_read(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Int(i32::from(unlocked))))
}

fn native_stamped_try_unlock_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let unlocked = crate::stamped_lock::stamped_try_unstamped_unlock_write(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Int(i32::from(unlocked))))
}

fn native_stamped_try_convert_to_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_write(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

fn native_stamped_try_convert_to_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_read(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

/// `tryConvertToOptimisticRead(long stamp)` -- releases the hold `stamp`
/// names and returns an observation stamp (0 when the stamp is stale).
fn native_stamped_try_convert_to_optimistic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_optimistic(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STAMPED").is_some() {
        eprintln!(
            "[SL-DBG] tryConvertToOptimisticRead addr={addr:#x} stamp={stamp} -> {converted}"
        );
    }
    Ok(Some(Value::Long(converted)))
}

/// `readLockInterruptibly()` / `writeLockInterruptibly()`.
///
/// This backend parks on a `parking_lot::Condvar`, which has no interrupt
/// channel, so these behave as the uninterruptible acquire. That is a
/// liveness-preserving approximation; leaving them to real JDK bytecode is
/// not, because that bytecode queues on a `state` word this backend never
/// writes and would never be released.
fn native_stamped_read_lock_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_stamped_read_lock(ctx, args)
}

fn native_stamped_write_lock_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_stamped_write_lock(ctx, args)
}

/// Convert a `(long time, TimeUnit unit)` argument pair to nanoseconds by
/// asking the unit itself, so any TimeUnit constant works without a table.
fn stamped_timeout_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> i64 {
    let time = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return 0,
    };
    let unit = match args.get(2) {
        Some(Value::Object(Some(u))) => *u,
        _ => return 0,
    };
    match ctx.invoke_virtual(unit, "toNanos", "(J)J", &[Value::Long(time)]) {
        Ok(Some(Value::Long(n))) => n,
        _ => 0,
    }
}

fn native_stamped_try_read_lock_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let nanos = stamped_timeout_nanos(ctx, args);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    ctx.begin_blocking_region();
    let stamp = crate::stamped_lock::stamped_try_read_lock_timed(addr, nanos);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_try_write_lock_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let nanos = stamped_timeout_nanos(ctx, args);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    ctx.begin_blocking_region();
    let stamp = crate::stamped_lock::stamped_try_write_lock_timed(addr, nanos);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_is_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(i32::from(
        crate::stamped_lock::stamped_is_locked(addr),
    ))))
}

fn native_stamped_is_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    let held = crate::stamped_lock::stamped_is_write_locked(addr);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STAMPED").is_some() {
        eprintln!("[SL-DBG] isWriteLocked addr={addr:#x} held={held}");
    }
    Ok(Some(Value::Int(i32::from(held))))
}

fn native_stamped_is_read_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(i32::from(
        crate::stamped_lock::stamped_is_read_locked(addr),
    ))))
}

fn native_stamped_get_read_lock_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        crate::stamped_lock::stamped_get_read_lock_count(addr),
    )))
}

pub(crate) fn register_atomic_extras_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: LongAdder / DoubleAdder / Atomic*Array are VM-internal atomic
    // primitives with spec-correct semantics → Bridge.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // LongAdder = 1-field synthetic (sum=0 Long)
    let la = "java/util/concurrent/atomic/LongAdder";
    registry.register(la, "<init>", "()V", native_long_adder_init);
    registry.register(la, "add", "(J)V", native_long_adder_add);
    registry.register(la, "increment", "()V", native_long_adder_increment);
    registry.register(la, "decrement", "()V", native_long_adder_decrement);
    registry.register(la, "sum", "()J", native_long_adder_sum);
    registry.register(la, "longValue", "()J", native_long_adder_sum);
    registry.register(la, "intValue", "()I", native_long_adder_int_value);
    registry.register(la, "reset", "()V", native_long_adder_reset);
    registry.register(la, "sumThenReset", "()J", native_long_adder_sum_then_reset);
    registry.register(
        la,
        "toString",
        "()Ljava/lang/String;",
        native_long_adder_to_string,
    );

    // DoubleAdder = 1-field synthetic (sum=0 Double)
    let da = "java/util/concurrent/atomic/DoubleAdder";
    registry.register(da, "<init>", "()V", native_double_adder_init);
    registry.register(da, "add", "(D)V", native_double_adder_add);
    registry.register(da, "sum", "()D", native_double_adder_sum);
    registry.register(da, "doubleValue", "()D", native_double_adder_sum);
    registry.register(da, "reset", "()V", native_double_adder_reset);

    // AtomicIntegerArray = 1-field synthetic (backing int[])
    let aia = "java/util/concurrent/atomic/AtomicIntegerArray";
    registry.register(aia, "<init>", "(I)V", native_aia_init);
    registry.register(aia, "get", "(I)I", native_aia_get);
    registry.register(aia, "set", "(II)V", native_aia_set);
    registry.register(aia, "getAndSet", "(II)I", native_aia_get_and_set);
    registry.register(aia, "compareAndSet", "(III)Z", native_aia_cas);
    registry.register(aia, "getAndIncrement", "(I)I", native_aia_get_and_inc);
    registry.register(aia, "getAndDecrement", "(I)I", native_aia_get_and_dec);
    registry.register(aia, "getAndAdd", "(II)I", native_aia_get_and_add);
    registry.register(aia, "incrementAndGet", "(I)I", native_aia_inc_and_get);
    registry.register(aia, "decrementAndGet", "(I)I", native_aia_dec_and_get);
    registry.register(aia, "length", "()I", native_aia_length);
    // The interpreter force-routes this whole method set to a native (see the
    // "C23" block in vm_exec.rs): the real JDK bodies go through
    // `VarHandles$Array$*`, which cannot CAS an array element here. Anything
    // on that list without a registration silently falls back to exactly the
    // bytecode the force-route exists to avoid, so register the full set.
    registry.register(aia, "addAndGet", "(II)I", native_aia_add_and_get);
    registry.register(aia, "lazySet", "(II)V", native_aia_set);
    registry.register(aia, "setPlain", "(II)V", native_aia_set);
    registry.register(aia, "setOpaque", "(II)V", native_aia_set);
    registry.register(aia, "setRelease", "(II)V", native_aia_set);
    registry.register(aia, "getPlain", "(I)I", native_aia_get);
    registry.register(aia, "getOpaque", "(I)I", native_aia_get);
    registry.register(aia, "getAcquire", "(I)I", native_aia_get);
    registry.register(aia, "weakCompareAndSet", "(III)Z", native_aia_cas);
    registry.register(aia, "weakCompareAndSetPlain", "(III)Z", native_aia_cas);
    registry.register(aia, "weakCompareAndSetAcquire", "(III)Z", native_aia_cas);
    registry.register(aia, "weakCompareAndSetRelease", "(III)Z", native_aia_cas);
    registry.register(aia, "compareAndExchange", "(III)I", native_aia_cae);
    registry.register(aia, "compareAndExchangeAcquire", "(III)I", native_aia_cae);
    registry.register(aia, "compareAndExchangeRelease", "(III)I", native_aia_cae);

    // AtomicLongArray = 1-field synthetic (backing long[])
    let ala = "java/util/concurrent/atomic/AtomicLongArray";
    registry.register(ala, "<init>", "(I)V", native_ala_init);
    registry.register(ala, "get", "(I)J", native_ala_get);
    registry.register(ala, "set", "(IJ)V", native_ala_set);
    registry.register(ala, "getAndSet", "(IJ)J", native_ala_get_and_set);
    registry.register(ala, "compareAndSet", "(IJJ)Z", native_ala_cas);
    registry.register(ala, "getAndIncrement", "(I)J", native_ala_get_and_inc);
    registry.register(ala, "incrementAndGet", "(I)J", native_ala_inc_and_get);
    registry.register(ala, "length", "()I", native_ala_length);
    registry.register(ala, "getAndAdd", "(IJ)J", native_ala_get_and_add);
    registry.register(ala, "addAndGet", "(IJ)J", native_ala_add_and_get);
    registry.register(ala, "getAndDecrement", "(I)J", native_ala_get_and_dec);
    registry.register(ala, "decrementAndGet", "(I)J", native_ala_dec_and_get);
    registry.register(ala, "lazySet", "(IJ)V", native_ala_set);
    registry.register(ala, "setPlain", "(IJ)V", native_ala_set);
    registry.register(ala, "setOpaque", "(IJ)V", native_ala_set);
    registry.register(ala, "setRelease", "(IJ)V", native_ala_set);
    registry.register(ala, "getPlain", "(I)J", native_ala_get);
    registry.register(ala, "getOpaque", "(I)J", native_ala_get);
    registry.register(ala, "getAcquire", "(I)J", native_ala_get);
    registry.register(ala, "weakCompareAndSet", "(IJJ)Z", native_ala_cas);
    registry.register(ala, "weakCompareAndSetPlain", "(IJJ)Z", native_ala_cas);
    registry.register(ala, "weakCompareAndSetAcquire", "(IJJ)Z", native_ala_cas);
    registry.register(ala, "weakCompareAndSetRelease", "(IJJ)Z", native_ala_cas);
    registry.register(ala, "compareAndExchange", "(IJJ)J", native_ala_cae);
    registry.register(ala, "compareAndExchangeAcquire", "(IJJ)J", native_ala_cae);
    registry.register(ala, "compareAndExchangeRelease", "(IJJ)J", native_ala_cae);
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Atomic array element read-modify-write
// ---------------------------------------------------------------------------
//
// `AtomicIntegerArray` / `AtomicLongArray` / `AtomicReferenceArray` are backed
// by a plain Java array here, and every read-modify-write native used to be a
// bare `get_array_element` + `set_array_element` pair with nothing in between.
// That is not a compare-and-swap: two threads can both read the expected value
// and both report success, so `compareAndSet` did not provide mutual exclusion
// at all.
//
// H2's `TestFileSystem.testConcurrent` uses exactly that idiom as a spin lock
// (`while (!locks.compareAndSet(pos, 0, 1)) {}`), so a writer and a reader
// could hold the same "lock" simultaneously and the reader observed a stale
// `expected` against freshly written file contents -- surfacing as
// `AssertionError: Expected: 3900 actual: 3897`. It was previously attributed
// to compiled `org/h2` code reordering across the atomic; the atomic simply
// was not one. (2026-07-27)
//
// `NativeContext::compare_and_swap_field` already special-cases array
// receivers and performs the read/compare/write under the per-object CAS lock,
// which is what the scalar `AtomicInteger` natives have always used. Route
// every array RMW through it.

/// Range-check an atomic-array element index, throwing HotSpot's
/// `ArrayIndexOutOfBoundsException` when it is out of range.
///
/// **Why this has to exist at all.** The heap DOES range-check --
/// `VmHeap::get_array_element` returns `Err(index)` past the end and
/// `set_array_element` writes nothing -- but `vm_exec.rs`'s `NativeContext`
/// impl deliberately swallows both results (`get` ends in a typed default,
/// `set` in `let _ = ...`), and its own doc comment says so: *"The caller
/// range-checks."* Every `AtomicIntegerArray` / `AtomicLongArray` /
/// `AtomicReferenceArray` native below was a caller that never did. Measured
/// on a 3-element array:
///
/// ```text
///                            HotSpot 25.0.3+9        CratonVM (before)
/// ARA.get(-1)                AIOOBE                  null
/// ARA.get(3) / get(100)      AIOOBE                  null
/// AIA.get(-1) / get(3)       AIOOBE                  0
/// ARA.set(-1, "x")           AIOOBE                  silently no-op
/// AIA.compareAndSet(-1,0,9)  AIOOBE                  returned true, wrote nothing
/// ```
///
/// This is a *missing-exception* defect, not a memory-safety one: the heap's
/// own check still suppresses the access, so no out-of-range read or write
/// reaches adjacent memory (verified with a sentinel neighbour swept over
/// -32..63). It is still serious -- an off-by-one gets a plausible `null`/`0`
/// and the program keeps running, and a *write* that silently does nothing is
/// worse than a read that answers wrong, because the loss surfaces arbitrarily
/// far away. A `compareAndSet` that reports success without storing is the
/// sharpest form: it is the same idiom H2's `TestFileSystem.testConcurrent`
/// uses as a spin lock.
///
/// The check lives on a shared funnel rather than on each accessor for the
/// reason `lang_invoke`'s `vh_array_index` gives for the identical decision on
/// the VarHandle side: there are ~26 registered triples per class here, and a
/// check added to twenty-five of them is a silent hole in the twenty-sixth.
/// `RuntimeError::aioobe` produces HotSpot's exact text
/// (`Index -1 out of bounds for length 3`).
///
/// The negative case is the one that mattered most: the old code did
/// `*v as usize`, so `-1` became `usize::MAX` and only the heap's own
/// `index >= len` test stopped it from being a wild read.
pub(crate) fn atomic_array_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: i32,
) -> Result<usize, MethodCallFailed> {
    let len = ctx.array_length(arr);
    if idx < 0 || (idx as usize) >= len {
        return Err(RuntimeError::aioobe(idx, len as i32).into());
    }
    Ok(idx as usize)
}

/// Read the `int` index argument of an atomic-array native WITHOUT widening it
/// to `usize`. The widening is what erased the sign; keep it `i32` until
/// [`atomic_array_index`] has passed judgement on it.
pub(crate) fn atomic_array_raw_index(args: &[Value]) -> i32 {
    match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

/// Validate the `length` argument of an atomic-array constructor.
///
/// `new AtomicIntegerArray(-1)` throws `NegativeArraySizeException: -1` on
/// HotSpot (measured; same for the `Long` and `Reference` twins). The old code
/// did `as usize`, handing `18446744073709551615` to `new_array` -- which is
/// either a catchable `OutOfMemoryError` or a hard abort depending on whether
/// a JIT frame is in between, and is the wrong exception either way.
pub(crate) fn atomic_array_new_length(len: i32) -> Result<usize, MethodCallFailed> {
    if len < 0 {
        return Err(RuntimeError::NegativeArraySizeException { size: len }.into());
    }
    Ok(len as usize)
}

/// Atomically apply `f` to element `idx` of `arr`, retrying until the CAS wins.
/// Returns `(previous, new)`. `f` must be side-effect free -- it can run more
/// than once.
pub(crate) fn atomic_array_rmw<F>(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    mut f: F,
) -> (Value, Value)
where
    F: FnMut(Value) -> Value,
{
    loop {
        let current = ctx.get_array_element(arr, idx);
        let updated = f(current);
        if ctx.compare_and_swap_field(arr, idx, current, updated) {
            return (current, updated);
        }
    }
}

/// Atomic `compareAndSet` on element `idx` of `arr`.
pub(crate) fn atomic_array_cas(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    expected: Value,
    update: Value,
) -> bool {
    ctx.compare_and_swap_field(arr, idx, expected, update)
}

/// Resolve `(backing array, RANGE-CHECKED index)` for an atomic-array element
/// native whose backing array lives in slot 0 (`AtomicIntegerArray` /
/// `AtomicLongArray`).
///
/// `Ok(None)` means the receiver or the backing array is missing, which keeps
/// each caller's historical typed default. `Err` means the index was out of
/// range and HotSpot would have thrown -- see [`atomic_array_index`].
fn atomic_array_slot(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(ObjectRef, usize)>, MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let raw = atomic_array_raw_index(args);
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    Ok(Some((arr, atomic_array_index(ctx, arr, raw)?)))
}

// --- AtomicIntegerArray ---
fn native_aia_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = atomic_array_new_length(atomic_array_raw_index(args))?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, len);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    Ok(None)
}

fn native_aia_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))
}

fn native_aia_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Int(val));
    Ok(None)
}

fn native_aia_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let new_val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| Value::Int(new_val));
    Ok(Some(old))
}

fn native_aia_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let ok = atomic_array_cas(ctx, arr, idx, Value::Int(expected), Value::Int(update));
    Ok(Some(Value::Int(i32::from(ok))))
}

fn native_aia_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(1))
    });
    let _ = (old, new_val);
    Ok(Some(old))
}

fn native_aia_get_and_dec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_sub(1))
    });
    let _ = (old, new_val);
    Ok(Some(old))
}

fn native_aia_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(old))
}

fn native_aia_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(1))
    });
    let _ = (old, new_val);
    Ok(Some(new_val))
}

fn native_aia_dec_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_sub(1))
    });
    let _ = (old, new_val);
    Ok(Some(new_val))
}

fn native_aia_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
}

fn native_aia_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let delta = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(new_val))
}

/// `compareAndExchange`: like `compareAndSet` but returns the WITNESS value
/// (the value actually found), not a boolean.
fn native_aia_cae(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let expected = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    loop {
        let current = ctx.get_array_element(arr, idx);
        if current.as_int().unwrap_or(0) != expected {
            return Ok(Some(current));
        }
        if atomic_array_cas(ctx, arr, idx, current, Value::Int(update)) {
            return Ok(Some(current));
        }
    }
}

// --- AtomicLongArray ---
fn native_ala_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = atomic_array_new_length(atomic_array_raw_index(args))?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, len);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    Ok(None)
}

fn native_ala_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))
}

fn native_ala_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Long(val));
    Ok(None)
}

fn native_ala_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let new_val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| Value::Long(new_val));
    Ok(Some(old))
}

fn native_ala_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let ok = atomic_array_cas(ctx, arr, idx, Value::Long(expected), Value::Long(update));
    Ok(Some(Value::Int(i32::from(ok))))
}

fn native_ala_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(1))
    });
    Ok(Some(old))
}

fn native_ala_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(1))
    });
    Ok(Some(new_val))
}

/// Resolve `(backing array, RANGE-CHECKED index)` for an `AtomicLongArray`
/// native call. Was an unchecked `Option`-returning helper that widened the
/// index with `as usize`; it is now a thin alias for [`atomic_array_slot`] so
/// the five callers below get the bounds check for free rather than each
/// needing its own.
fn ala_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(ObjectRef, usize)>, MethodCallFailed> {
    atomic_array_slot(ctx, args)
}

fn native_ala_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(old))
}

fn native_ala_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(new_val))
}

fn native_ala_get_and_dec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_sub(1))
    });
    Ok(Some(old))
}

fn native_ala_dec_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_sub(1))
    });
    Ok(Some(new_val))
}

fn native_ala_cae(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    loop {
        let current = ctx.get_array_element(arr, idx);
        if current.as_long().unwrap_or(0) != expected {
            return Ok(Some(current));
        }
        if atomic_array_cas(ctx, arr, idx, current, Value::Long(update)) {
            return Ok(Some(current));
        }
    }
}

fn native_ala_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
}

/// `Collections.synchronizedMap(m)` — return a REAL `Collections$SynchronizedMap`
/// wrapper, not the bare map. The historical identity stub left every method
/// unsynchronized, so `synchronizedMap(...).computeIfAbsent(...)` was not atomic
/// under concurrency: Spring's `ConcurrencyLimitBeanPostProcessor` keeps its
/// per-proxy throttle holders in a `synchronizedMap(IdentityHashMap)` and races
/// created DUPLICATE holders → duplicate throttles → the @ConcurrencyLimit cap
/// was exceeded (flaky `IllegalStateException` in ConcurrencyLimitTests). The
/// real wrapper's `computeIfAbsent` runs `synchronized (mutex) { m.compute… }`,
/// which CratonVM executes correctly. Null passes through.
pub(crate) fn native_synchronized_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedMap",
            "(Ljava/util/Map;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedList(l)` — wrap in the real `SynchronizedList` (or
/// `SynchronizedRandomAccessList` when the backing list implements `RandomAccess`,
/// matching `Collections.synchronizedList`), not the raw list. Same rationale as
/// [`native_synchronized_map`]: the identity stub left mutation unsynchronized.
pub(crate) fn native_synchronized_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(list))) => {
            let is_random_access = match ctx.class_id_by_name("java/util/RandomAccess") {
                Some(ra) => ctx.is_subclass(ctx.class_id_of_object(*list), ra),
                None => false,
            };
            let cls = if is_random_access {
                "java/util/Collections$SynchronizedRandomAccessList"
            } else {
                "java/util/Collections$SynchronizedList"
            };
            ctx.new_object_initialized(cls, "(Ljava/util/List;)V", &[v.clone()])
        }
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedSet(s)` — wrap in the real `SynchronizedSet`.
pub(crate) fn native_synchronized_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedCollection(c)` — wrap in the real `SynchronizedCollection`.
pub(crate) fn native_synchronized_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// Hold the wrapper's `mutex` across `body`, exactly as the JDK's
/// `Collections$Synchronized{Collection,Set,List,Map}` bytecode does
/// (`synchronized (mutex) { c.add(e); }`).
///
/// Every one of those methods was a bare forward to the backing collection --
/// the `mutex` field was written by the constructor and then read by nobody.
/// The wrapper's whole contract is the lock, so what
/// `Collections.synchronizedSet(new HashSet<>())` actually handed out was an
/// unsynchronized `HashSet` behind a class name that promises otherwise: eight
/// threads adding and then removing 4000 distinct elements each ended the run
/// at `size() == 3716` where HotSpot ends at `0` (`SyncSetProbe`, 2026-08-16;
/// the H2 `TestMultiThread` write-up saw the same shape as a NEGATIVE size).
/// A registered native shadows the class's own bytecode at every dispatch
/// site, so the real JDK implementation could not compensate -- including for
/// `SynchronizedList`, which has no natives of its own but inherits `add` /
/// `remove` / `size` from `SynchronizedCollection`, and so lost elements too
/// (26540 of 32000).
///
/// **Contended entry is GC-safe on purpose.** These wrappers are contended by
/// construction, and the owner is inside `HashMap.put`, which allocates and can
/// therefore be parked at a collection safepoint while it holds the mutex. A
/// plain `monitor_enter` leaves the waiter counted in the STW barrier's
/// `expected` set -- the three-way wedge `Monitor::block_enter`'s own doc names
/// (owner waits for GC, contender waits for owner, GC waits for contender). The
/// price of the GC-safe wait is that a moving collection CAN run inside it, so
/// `this`, the mutex and every reference argument are pinned across it and
/// re-read afterwards; nothing below may use a pre-wait `ObjectRef`.
pub(crate) fn with_sync_mutex<F>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
    body: F,
) -> MethodCallResult
where
    F: FnOnce(&mut dyn NativeContext, ObjectRef, &[Value]) -> MethodCallResult,
{
    let mutex = match ctx.get_field_by_name(this, "mutex") {
        Value::Object(Some(m)) => m,
        // A wrapper that reached us without going through `native_sync_*_init`
        // has no `mutex` yet; the JDK's one-argument constructor uses `this`,
        // which is also the only lock that can be correct for such an object.
        _ => this,
    };
    let base = ctx.pin_native_root(this);
    let mutex_h = ctx.pin_native_root(mutex);
    let arg_h: Vec<Option<usize>> = args
        .iter()
        .map(|a| match a {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        })
        .collect();
    ctx.monitor_enter_gc_safe(mutex);
    // Post-wait addresses only, for every reference we still name.
    let mutex = ctx.read_native_pin(mutex_h, mutex);
    let this = ctx.read_native_pin(base, this);
    let mut fixed: Vec<Value> = Vec::with_capacity(args.len());
    for (a, h) in args.iter().zip(arg_h.iter()) {
        fixed.push(match (a, h) {
            (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(*h, *o))),
            _ => *a,
        });
    }
    let out = body(ctx, this, &fixed);
    // `body` RE-ENTERS JAVA (`invoke_virtual` into `HashMap.get`,
    // `ArrayList.set`, ...), so a collection can run inside it and move the
    // mutex just as the contended entry above can. The refresh after
    // `monitor_enter_gc_safe` covers only the wait; the exit needs its own,
    // and it has to happen BEFORE `unpin_native_roots(base)` truncates the
    // stack that owns `mutex_h`.
    //
    // Without it `monitor_exit` dereferenced the PRE-BODY address:
    // `MonitorTable::exit` opens with `header_of(obj_ref)`, so a mutex whose
    // young slot the collection had reclaimed faulted there —
    // `EXCEPTION_ACCESS_VIOLATION` inside `native_sync_map_get` /
    // `sync_collection_delegate`, reproduced in seconds by
    // `probes/OldToYoungBarrierSweep.java` under
    // `--XX:UseGc Generational` with and without the JIT, and absent under
    // ZGC only because its wrapper objects had not been relocated yet.
    // The quiet face is worse than the loud one: a mutex that merely MOVED
    // exits a monitor at its old address, so the real one is never released
    // and every later waiter on that wrapper blocks forever.
    let mutex = ctx.read_native_pin(mutex_h, mutex);
    ctx.monitor_exit(mutex);
    // Unpin LAST: `monitor_exit` is the final use of a pinned reference, and
    // the previous order released the pins while one was still live.
    ctx.unpin_native_roots(base);
    out
}

pub(crate) fn native_sync_collection_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let backing = args.get(1).copied().unwrap_or(Value::Object(None));
    let mutex = match args.get(2).copied() {
        Some(Value::Object(Some(mutex))) => Value::Object(Some(mutex)),
        _ => Value::Object(Some(this)),
    };
    ctx.set_field_by_name(this, "c", backing);
    ctx.set_field(this, 0, backing);
    ctx.set_field_by_name(this, "mutex", mutex);
    Ok(None)
}

pub(crate) fn native_sync_collection_add(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "add", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub(crate) fn native_sync_collection_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "contains", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub(crate) fn native_sync_collection_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "remove", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub(crate) fn native_sync_collection_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "size", "()I", &[]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub(crate) fn native_sync_collection_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "isEmpty", "()Z", &[]),
            None => Ok(Some(Value::Int(1))),
        },
    )
}

/// NOT synchronized, deliberately: `SynchronizedCollection.iterator()` is the
/// one mutating-surface method the JDK forwards outside the lock, because the
/// returned iterator is used outside it too ("it is imperative that the user
/// manually synchronize on the returned collection when traversing"). Locking
/// here would diverge from the JDK without making traversal safe.
pub(crate) fn native_sync_collection_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "iterator", "()Ljava/util/Iterator;", &[]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_collection_to_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "toArray", "()[Ljava/lang/Object;", &[]),
            None => {
                let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                Ok(Some(Value::Object(Some(empty))))
            }
        },
    )
}

/// Forward one `Collections$Synchronized{Collection,Set}` method to the wrapped
/// collection, unchanged.
///
/// The named-method stubs above cover the surface the wrapper needed while
/// `Collections.synchronizedCollection(…)` was its only source. Since
/// 2026-08-13 `Hashtable`/`Properties` views are wrapped too — the JDK's own
/// `Hashtable.keySet()` is `Collections.synchronizedSet(new KeySet(), this)`, so
/// matching `getClass()` means returning the wrapper — and those views are asked
/// for the whole `Collection` contract (`toString`, `stream`, `forEach`,
/// `containsAll`, …). In real-JDK mode the class's own bytecode answers all of
/// it and these are dropped as `SyntheticStub`s; in synthetic-JDK mode they are
/// the only implementation there is, and a MISSING one is worse than a slow one:
/// the call falls through to an interface-level native that reads the wrapper as
/// if it were the collection and reports it EMPTY.
///
/// `fallback` is what to answer when the wrapper has no `c` — a shape that
/// cannot arise from either constructor, so it is chosen per method only to keep
/// the return type honest rather than to encode behaviour.
/// Forward one `Collections$Synchronized{Collection,Set,List}` method to the
/// wrapped collection, under the wrapper's `mutex`.
///
/// The named-method stubs above cover the surface the wrapper needed while
/// `Collections.synchronizedCollection(...)` was its only source. Since
/// 2026-08-13 `Hashtable`/`Properties` views are wrapped too -- the JDK's own
/// `Hashtable.keySet()` is `Collections.synchronizedSet(new KeySet(), this)`, so
/// matching `getClass()` means returning the wrapper -- and those views are asked
/// for the whole `Collection` contract (`toString`, `stream`, `forEach`,
/// `containsAll`, ...). In real-JDK mode the class's own bytecode answers all of
/// it and these are dropped as `SyntheticStub`s; in synthetic-JDK mode they are
/// the only implementation there is, and a MISSING one is worse than a slow one:
/// the call falls through to an interface-level native that reads the wrapper as
/// if it were the collection and reports it EMPTY.
///
/// `fallback` is what to answer when the wrapper has no `c` -- a shape that
/// cannot arise from either constructor, so it is chosen per method only to keep
/// the return type honest rather than to encode behaviour.
///
/// `stream` / `parallelStream` / `spliterator` are forwarded WITHOUT the lock,
/// matching the JDK, for the same reason `iterator()` is.
pub(crate) fn sync_collection_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
    fallback: Option<Value>,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(
        method,
        "iterator" | "stream" | "parallelStream" | "spliterator"
    ) {
        return match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, method, descriptor, &args[1..]),
            None => Ok(fallback),
        };
    }
    let rest: Vec<Value> = args[1..].to_vec();
    with_sync_mutex(
        ctx,
        this,
        &rest,
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, method, descriptor, a),
            None => Ok(fallback),
        },
    )
}

pub(crate) fn native_sync_map_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let backing = args.get(1).copied().unwrap_or(Value::Object(None));
    let mutex = match args.get(2).copied() {
        Some(Value::Object(Some(mutex))) => Value::Object(Some(mutex)),
        _ => Value::Object(Some(this)),
    };
    ctx.set_field_by_name(this, "m", backing);
    ctx.set_field(this, 0, backing);
    ctx.set_field_by_name(this, "mutex", mutex);
    Ok(None)
}

pub(crate) fn native_sync_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => {
                ctx.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &a[..1])
            }
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub(crate) fn native_sync_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[key, value],
        |ctx, this, a| match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(
                m,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &a[..2],
            ),
            None => Ok(Some(Value::Object(None))),
        },
    )
}

pub(crate) fn native_sync_map_contains_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "containsKey", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        }
    })
}

pub(crate) fn native_sync_map_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(
                m,
                "remove",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &a[..1],
            ),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub(crate) fn native_sync_map_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "size", "()I", &[]),
            None => Ok(Some(Value::Int(0))),
        }
    })
}

pub(crate) fn native_sync_map_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "isEmpty", "()Z", &[]),
            None => Ok(Some(Value::Int(1))),
        }
    })
}

pub(crate) fn native_sync_map_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "entrySet", "()Ljava/util/Set;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub(crate) fn native_sync_map_key_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "keySet", "()Ljava/util/Set;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub(crate) fn native_sync_map_values(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "values", "()Ljava/util/Collection;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub(crate) fn native_sync_map_clear(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        if let Some(m) = sync_map_backing(ctx, this) {
            ctx.invoke_virtual(m, "clear", "()V", &[])?;
        }
        Ok(None)
    })
}

pub(crate) fn native_sync_map_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapper_arg = args.get(2).copied().unwrap_or(Value::Object(None));
    // The get/apply/put trio is ONE critical section, exactly as the real
    // wrapper's `synchronized (mutex) { m.computeIfAbsent(...) }` is. Running it
    // unlocked is what let two threads both miss the `get` and both `put` --
    // the duplicate-holder race this method's own doc above describes.
    with_sync_mutex(ctx, this, &[key, mapper_arg], |ctx, this, a| {
        let key = a[0];
        let mapper = match a[1] {
            Value::Object(Some(f)) => Some(f),
            _ => None,
        };
        let Some(m) = sync_map_backing(ctx, this) else {
            return Ok(Some(Value::Object(None)));
        };
        if let Some(v @ Value::Object(Some(_))) =
            ctx.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key])?
        {
            return Ok(Some(v));
        }
        let Some(mapper) = mapper else {
            return Ok(Some(Value::Object(None)));
        };
        let computed = ctx
            .invoke_virtual(
                mapper,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?
            .unwrap_or(Value::Object(None));
        if matches!(computed, Value::Object(Some(_))) {
            let _ = ctx.invoke_virtual(
                m,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, computed],
            );
        }
        Ok(Some(computed))
    })
}

pub(crate) fn native_iss_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, ISS_FIELD_COUNT, Value::Long(0));
    ctx.set_field(this, ISS_FIELD_SUM, Value::Long(0));
    ctx.set_field(this, ISS_FIELD_MIN, Value::Int(i32::MAX));
    ctx.set_field(this, ISS_FIELD_MAX, Value::Int(i32::MIN));
    Ok(None)
}

pub(crate) fn native_iss_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    let min = match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => v,
        _ => i32::MAX,
    };
    let max = match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => v,
        _ => i32::MIN,
    };
    ctx.set_field(this, ISS_FIELD_COUNT, Value::Long(count + 1));
    ctx.set_field(this, ISS_FIELD_SUM, Value::Long(sum + val as i64));
    ctx.set_field(this, ISS_FIELD_MIN, Value::Int(min.min(val)));
    ctx.set_field(this, ISS_FIELD_MAX, Value::Int(max.max(val)));
    Ok(None)
}

pub(crate) fn native_iss_get_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub(crate) fn native_iss_get_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub(crate) fn native_iss_get_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(i32::MAX))),
    };
    match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(i32::MAX))),
    }
}

pub(crate) fn native_iss_get_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(i32::MIN))),
    };
    match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(i32::MIN))),
    }
}

pub(crate) fn native_iss_get_average(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    if count == 0 {
        Ok(Some(Value::Double(0.0)))
    } else {
        Ok(Some(Value::Double(sum as f64 / count as f64)))
    }
}

pub(crate) fn native_iss_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    let min = match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => v,
        _ => 0,
    };
    let max = match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => v,
        _ => 0,
    };
    let avg = if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    };
    let text = format!(
        "IntSummaryStatistics{{count={}, sum={}, min={}, average={:.6}, max={}}}",
        count, sum, min, avg, max
    );
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

pub(crate) fn register_atomic_boolean_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicBoolean atomic primitive (Unsafe-equivalent) → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicBoolean";
    r.register(c, "<init>", "()V", native_ab_init_default);
    r.register(c, "<init>", "(Z)V", native_ab_init_value);
    r.register(c, "get", "()Z", native_ab_get);
    r.register(c, "set", "(Z)V", native_ab_set);
    r.register(c, "lazySet", "(Z)V", native_ab_set);
    r.register(c, "compareAndSet", "(ZZ)Z", native_ab_cas);
    r.register(c, "getAndSet", "(Z)Z", native_ab_get_and_set);
    r.register(c, "toString", "()Ljava/lang/String;", native_ab_to_string);
    r.set_category(__prev_cat);
}

pub(crate) fn native_ab_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ab_init_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(val));
    Ok(None)
}

pub(crate) fn native_ab_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let v = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })))
}

pub(crate) fn native_ab_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(val));
    Ok(None)
}

pub(crate) fn native_ab_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    let update = match args.get(2) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    let current = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    if current == expected {
        ctx.set_field(this, AB_FIELD_VALUE, Value::Int(update));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub(crate) fn native_ab_get_and_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let new_val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    let old = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(new_val));
    Ok(Some(Value::Int(if old != 0 { 1 } else { 0 })))
}

pub(crate) fn native_ab_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i != 0,
        _ => false,
    };
    let s = ctx.create_string(if v { "true" } else { "false" });
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn register_atomic_stamped_ref_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicStampedReference atomic primitive → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicStampedReference";
    r.register(c, "<init>", "(Ljava/lang/Object;I)V", native_asr_init);
    r.register(
        c,
        "getReference",
        "()Ljava/lang/Object;",
        native_asr_get_ref,
    );
    r.register(c, "getStamp", "()I", native_asr_get_stamp);
    // get([I)Ljava/lang/Object; — reads stamp into stampHolder[0] and
    // returns the reference. Faithful to the JDK two-result accessor.
    r.register(
        c,
        "get",
        "([I)Ljava/lang/Object;",
        native_asr_get_with_holder,
    );
    r.register(c, "set", "(Ljava/lang/Object;I)V", native_asr_set);
    r.register(
        c,
        "attemptStamp",
        "(Ljava/lang/Object;I)Z",
        native_asr_attempt_stamp,
    );
    r.register(
        c,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        native_asr_cas,
    );
    r.register(
        c,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        native_asr_cas,
    );
    r.set_category(__prev_cat);
}

fn native_asr_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Store a real Pair object in the single `pair` field (slot 0). No write to
    // slot 1 — the ASR object itself has only one slot.
    let pair = asr_alloc_pair(ctx, r, stamp);
    ctx.set_field(this, 0, Value::Object(Some(pair?)));
    Ok(None)
}

fn native_asr_get_ref(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, _stamp) = asr_read_pair(ctx, this);
    Ok(Some(reference))
}

fn native_asr_get_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_reference, stamp) = asr_read_pair(ctx, this);
    Ok(Some(Value::Int(stamp)))
}

/// `V get(int[] stampHolder)` — store the current stamp into `stampHolder[0]`
/// and return the current reference (matches JDK semantics).
fn native_asr_get_with_holder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, stamp) = asr_read_pair(ctx, this);
    if let Some(Value::Object(Some(holder))) = args.get(1) {
        ctx.set_array_element(*holder, 0, Value::Int(stamp));
    }
    Ok(Some(reference))
}

fn native_asr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JDK `set` allocates a fresh Pair only when reference or stamp differ; we
    // always install a fresh Pair (semantically identical, slightly simpler).
    let pair = asr_alloc_pair(ctx, r, stamp);
    ctx.set_field(this, 0, Value::Object(Some(pair?)));
    Ok(None)
}

fn native_asr_attempt_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (current_ref, _current_stamp) = asr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) {
        // Swap in a new Pair carrying the (unchanged) reference + new stamp.
        let pair = asr_alloc_pair(ctx, current_ref, new_stamp);
        ctx.set_field(this, 0, Value::Object(Some(pair?)));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

fn native_asr_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_ref = args.get(2).copied().unwrap_or(Value::Object(None));
    let expected_stamp = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let new_stamp = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (current_ref, current_stamp) = asr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) && current_stamp == expected_stamp {
        // Install a new Pair only if reference or stamp actually changes (the
        // JDK fast-path: when both are identical it skips the casPair entirely).
        if !values_ref_equal(current_ref, new_ref) || current_stamp != new_stamp {
            let pair = asr_alloc_pair(ctx, new_ref, new_stamp);
            ctx.set_field(this, 0, Value::Object(Some(pair?)));
        }
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub(crate) fn register_atomic_markable_ref_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicMarkableReference atomic primitive → Bridge.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/atomic/AtomicMarkableReference";
    r.register(c, "<init>", "(Ljava/lang/Object;Z)V", native_amr_init);
    r.register(
        c,
        "getReference",
        "()Ljava/lang/Object;",
        native_amr_get_ref,
    );
    r.register(c, "isMarked", "()Z", native_amr_is_marked);
    // get(boolean[]) — reads mark into markHolder[0] and returns the
    // reference. Must be registered explicitly (see comment above): without
    // it, real bytecode ran unintercepted against a corrupted slot 0.
    r.register(
        c,
        "get",
        "([Z)Ljava/lang/Object;",
        native_amr_get_with_holder,
    );
    r.register(c, "set", "(Ljava/lang/Object;Z)V", native_amr_set);
    r.register(
        c,
        "attemptMark",
        "(Ljava/lang/Object;Z)Z",
        native_amr_attempt_mark,
    );
    r.register(
        c,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z",
        native_amr_cas,
    );
    r.register(
        c,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z",
        native_amr_cas,
    );
    r.set_category(__prev_cat);
}

/// RETIRED 2026-08-12 (W7-18 patch B). Registers nothing, on purpose, and the
/// function is kept only because its call site is in another lane's file
/// (`lib.rs`'s `register_phase_d_natives`).
///
/// This was the *third* registrar of `java/util/concurrent/StructuredTaskScope`,
/// and every triple it held was either dead or a landmine:
///
/// * **Provably inert.** All three registrars sit inside
///   `register_synthetic_overrides`, and the call order there is
///   `register_phase67_natives` (`phases_late/concurrent.rs`), then
///   `register_phase_d_natives` (this one), then
///   `register_jdk25_concurrency_natives`. `register()` is
///   last-registration-wins (docs/architecture/natives-over-real-jdk-classes.md
///   §3), and `jdk25_concurrency.rs` registers a superset of this function's
///   `StructuredTaskScope` and `$Subtask` triples — so every one of them was
///   overwritten before boot finished. The only registrations here that ever
///   *won* were the two covariant-return `join()`s,
///   `$ShutdownOnFailure.join()L…$ShutdownOnFailure;` and
///   `$ShutdownOnSuccess.join()L…$ShutdownOnSuccess;`, which `jdk25_concurrency`
///   spelled with the base `L…StructuredTaskScope;` return and therefore did not
///   collide with. Both are on classes **JEP 505 deleted**: `javap` and
///   `Class.forName` answer "not found" on Adoptium 25.0.3.9, `--real-jdk` and
///   `--jdk-only` alike (docs/known-issues/jdk-only/W7-18-structured-task-scope-jep505.md §3).
///
/// * **A landmine, which is why it is retired rather than left alone.** These
///   bodies used a DIFFERENT `$Subtask` slot convention from the registrar that
///   owns the readers. Here, `state` was read and written at **slot 3** (`1` =
///   success, `2` = failed) and the result at slot 1. In
///   `jdk25_concurrency.rs`, which wins `Subtask.get/state/exception`, the
///   layout is `SUBTASK_FIELD_STATE = 0`, `RESULT = 1`, `EXCEPTION = 2`,
///   **`CALLABLE = 3`** — so this file's `state` write landed on the callable
///   REFERENCE slot: an `Int` in a slot the collector scans as an oop, which is
///   heap corruption rather than a wrong answer
///   (docs/architecture/natives-over-real-jdk-classes.md §5). It never fired
///   only because every one of those triples was overwritten. The day anyone
///   deletes a JDK-21-shaped triple from the winning registrar, this becomes
///   live. `t3_impl.rs::register_t31_structured_concurrency` is already a
///   tombstone for exactly this defect ("registering them here caused the
///   canonical 8-field layout to be overridden with the earlier 2-field stubs,
///   silently breaking `close()`, `result()`, and `throwIfFailed()`"); this is
///   the copy that pass missed.
///
/// Scope of the change: **synthetic-JDK mode only**, and structurally so. All
/// three registrars are reachable only from `register_synthetic_overrides`,
/// which is `#[cfg(feature = "synthetic-jdk")]` and called only on the
/// `use_synthetic_jdk` arm of `vm_init`. In `--real-jdk` and `--jdk-only` real
/// JDK bytecode serves this API end to end, measured at
/// `compatibility_classes: 0` / `synthetic_stub_invocations: 0` with zero
/// StructuredTaskScope violations (W7-18 §5), so nothing here can move either
/// shipping mode by any amount, and nothing here can move a ratchet taken in
/// Compatible mode.
pub(crate) fn register_pd_structured_concurrency(_r: &mut NativeMethodRegistry) {}

// ===========================================================================
// Concurrency primitive tests & Unsafe.setMemory test
// ===========================================================================

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn make_ctx() -> MockNativeContext {
        MockNativeContext::new()
    }

    // Several concurrency primitives (StampedLock, and — since the
    // descriptor-collision fix — ReentrantLock/Condition) keep their shim
    // state in process-global per-object side maps keyed by the object's
    // identity (`ObjectRef.as_ptr()` / `identity_hash_code`).  Every fresh
    // `MockNativeContext` starts with `next_ptr = 8`, so the first object
    // each test allocates reuses address `0x8` — tests running in parallel
    // therefore share the same side-map state and race.  Serialize every
    // test that touches one of those global maps with this test-only Mutex.
    // Production code is unaffected because real `ObjectRef`s produced by the
    // heap allocator have unique, GC-stable identities per live object.
    fn stamped_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn default_real_aqs_does_not_override_semaphore() {
        // This test deliberately leaves the process environment untouched:
        // production's default is real AQS. Synthetic-AQS test runs exercise
        // the legacy registration surface instead.
        if crate::nbflags().synthetic_aqs && !crate::nbflags().real_aqs {
            return;
        }

        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        register_concurrent_natives(&mut registry);
        let sem = "java/util/concurrent/Semaphore";
        for (name, descriptor) in [
            ("<init>", "(I)V"),
            ("<init>", "(IZ)V"),
            ("acquire", "()V"),
            ("release", "()V"),
            ("availablePermits", "()I"),
        ] {
            assert!(
                registry.find(sem, name, descriptor).is_none(),
                "real AQS must execute Semaphore.{name}{descriptor} bytecode"
            );
        }
    }

    // -----------------------------------------------------------------------
    // ReentrantLock
    // -----------------------------------------------------------------------

    #[test]
    fn rl_lock_unlock_basic() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        // Lock: hold count should become 1
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);

        // Unlock: hold count should become 0, owner should become 0
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
    }

    #[test]
    fn rl_try_lock_succeeds_when_unlocked() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let result = native_rl_try_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(result, Some(Value::Int(1))); // true
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
    }

    #[test]
    fn rl_reentrant_locking() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        // Lock twice -- should increment hold count to 2
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 2);

        // First unlock decrements to 1 (still locked)
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
        assert_ne!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

        // Second unlock fully releases
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
    }

    #[test]
    fn rl_unlock_notifies_on_full_release() {
        // Unlock when hold count drops to 0 should call monitor_notify
        // In MockNativeContext, monitor_notify is a no-op but doesn't error
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        // This should succeed (calls monitor_notify internally)
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
    }

    // -----------------------------------------------------------------------
    // Condition
    // -----------------------------------------------------------------------

    #[test]
    fn cond_await_releases_lock() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        // Create condition associated with this lock
        let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        let cond = match cond_val {
            Value::Object(Some(c)) => c,
            _ => panic!("expected condition object"),
        };

        // await releases the lock (in mock, monitor_wait returns immediately
        // so the lock is released then reacquired within the same call)
        native_cond_await(&mut ctx, &[Value::Object(Some(cond))]).unwrap();

        // After await returns, lock should be reacquired
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
    }

    #[test]
    fn cond_signal_does_not_error() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        let cond = match cond_val {
            Value::Object(Some(c)) => c,
            _ => panic!("expected condition object"),
        };

        // signal and signalAll should complete without error
        native_cond_signal(&mut ctx, &[Value::Object(Some(cond))]).unwrap();
        native_cond_signal_all(&mut ctx, &[Value::Object(Some(cond))]).unwrap();
    }

    // -----------------------------------------------------------------------
    // CountDownLatch
    // -----------------------------------------------------------------------

    // NOTE: since the descriptor-coercion fix the count lives in an int[1]
    // holder object in slot 0 (the real CountDownLatch's only field is the
    // reference-typed `sync`, so a raw Int there was coerced to null in
    // real-JDK builds). Assert through `cdl_count` — the storage accessor —
    // rather than the raw slot.
    #[test]
    fn cdl_countdown_to_zero_triggers_notify() {
        let mut ctx = make_ctx();
        let cdl =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
                .unwrap();
        native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(2)]).unwrap();

        assert_eq!(cdl_count(&mut ctx, cdl), 2);

        native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
        assert_eq!(cdl_count(&mut ctx, cdl), 1);

        // Second countDown reaches zero -- triggers monitor_notify_all (no-op in mock)
        native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
        assert_eq!(cdl_count(&mut ctx, cdl), 0);
    }

    #[test]
    fn cdl_await_returns_when_count_is_zero() {
        let mut ctx = make_ctx();
        let cdl =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
                .unwrap();
        // Init with count=0 means await should return immediately
        native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(0)]).unwrap();

        // Should not block (count is already 0)
        native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
        assert_eq!(cdl_count(&mut ctx, cdl), 0);
    }

    #[test]
    fn cdl_countdown_below_zero_stays_at_zero() {
        let mut ctx = make_ctx();
        let cdl =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1)
                .unwrap();
        native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(1)]).unwrap();

        native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
        native_cdl_count_down(&mut ctx, &[Value::Object(Some(cdl))]).unwrap(); // extra
        assert_eq!(cdl_count(&mut ctx, cdl), 0);
    }

    // -----------------------------------------------------------------------
    // Semaphore
    // -----------------------------------------------------------------------

    // NOTE: since the descriptor-coercion fix the permits/fair pair lives in
    // an int[2] holder object in slot 0 (the real Semaphore's only field is
    // the reference-typed `sync`). Assert through `sem_permits`.
    #[test]
    fn sem_acquire_decrements_permits() {
        let mut ctx = make_ctx();
        let sem =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

        assert_eq!(sem_permits(&mut ctx, sem), 3);

        native_sem_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 2);
    }

    #[test]
    fn sem_acquire_uninterruptibly_n_decrements_requested_permits() {
        let mut ctx = make_ctx();
        let sem =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

        native_sem_acquire_n(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 0);
    }

    #[test]
    fn sem_release_increments_and_notifies() {
        let mut ctx = make_ctx();
        let sem =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(1)]).unwrap();

        // Release adds a permit (calls monitor_notify internally)
        native_sem_release(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 2);
    }

    #[test]
    fn sem_try_acquire_with_no_permits_returns_zero() {
        let mut ctx = make_ctx();
        let sem =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(0)]).unwrap();

        let result = native_sem_try_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(result, Some(Value::Int(0))); // false -- no permits available
        assert_eq!(sem_permits(&mut ctx, sem), 0);
    }

    #[test]
    fn sem_try_acquire_with_permits_succeeds() {
        let mut ctx = make_ctx();
        let sem =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2).unwrap();
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(5)]).unwrap();

        let result = native_sem_try_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(result, Some(Value::Int(1))); // true
        assert_eq!(sem_permits(&mut ctx, sem), 4);
    }

    // -----------------------------------------------------------------------
    // Unsafe.setMemory
    // -----------------------------------------------------------------------

    #[test]
    fn unsafe_set_memory_fills_fields() {
        let mut ctx = make_ctx();
        // Allocate an object with 5 fields, all initially 0
        let obj = ctx.alloc_object(ClassId::new(0), 5);
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0));

        // setMemory(unsafe_this, obj, offset=1, bytes=3, value=0x42)
        // Should fill fields [1], [2], [3] with Int(0x42)
        native_unsafe_set_memory(
            &mut ctx,
            &[
                Value::Object(None), // unsafe this (ignored)
                Value::Object(Some(obj)),
                Value::Long(1),   // offset
                Value::Long(3),   // bytes (field count)
                Value::Int(0x42), // fill value
            ],
        )
        .unwrap();

        // Fields 0 and 4 should be unchanged
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0));
        assert_eq!(ctx.get_field(obj, 4), Value::Int(0));
        // Fields 1, 2, 3 should be filled with 0x42
        assert_eq!(ctx.get_field(obj, 1), Value::Int(0x42));
        assert_eq!(ctx.get_field(obj, 2), Value::Int(0x42));
        assert_eq!(ctx.get_field(obj, 3), Value::Int(0x42));
    }

    // M3: oversized requests are rejected before any heap access, and array
    // targets that fail the bounds-checked path no longer fall through to the
    // unbounded slot loop.

    #[test]
    fn unsafe_set_memory_rejects_oversized_request() {
        let mut ctx = make_ctx();
        let obj = ctx.alloc_object(ClassId::new(0), 5);
        let huge = (MAX_UNSAFE_COPY_SIZE + 1) as i64;
        let res = native_unsafe_set_memory(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Long(huge),
                Value::Int(0x42),
            ],
        );
        assert!(res.is_err(), "oversized setMemory must be rejected");
        // Nothing was written despite the huge byte count.
        assert_eq!(ctx.get_field(obj, 0), Value::Int(0));
    }

    #[test]
    fn unsafe_copy_memory_rejects_oversized_request() {
        let mut ctx = make_ctx();
        let src = ctx.alloc_object(ClassId::new(0), 4);
        let dst = ctx.alloc_object(ClassId::new(0), 4);
        let huge = (MAX_UNSAFE_COPY_SIZE + 1) as i64;
        let res = native_unsafe_copy_memory(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(src)),
                Value::Long(0),
                Value::Object(Some(dst)),
                Value::Long(0),
                Value::Long(huge),
            ],
        );
        assert!(res.is_err());
        assert_eq!(ctx.get_field(dst, 0), Value::Int(0));
    }

    // ===================================================================
    // M18: Real StampedLock, monitor-based contention, condition timeouts
    // ===================================================================

    // ------- StampedLock -------

    fn stamped_lock_obj(ctx: &mut MockNativeContext) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("java/util/concurrent/locks/StampedLock")
            .expect("mock StampedLock class id");
        ctx.alloc_object(cid, 8)
    }

    #[test]
    fn m18_stamped_init_creates_state() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        // tryOptimisticRead should return STAMPED_ORIGIN (256) since no writer
        let stamp = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(stamp, Value::Long(STAMPED_ORIGIN));
    }

    // bug nb-lib-gckeys §1 regression: the StampedLock/RWL state tables must
    // be keyed by a GC-STABLE identity, not the raw moving heap address.
    // Here we verify the contract `gc_stable_lock_key` guarantees:
    //   (a) the SAME object yields the SAME key on every call (so a thread
    //       that locks before a GC unlocks the same slot afterwards), and
    //   (b) DISTINCT objects yield DISTINCT keys (no cross-lock aliasing).
    // We can't simulate a real relocation under MockNativeContext (its
    // `identity_hash_code` is derived from the address), but the stability +
    // distinctness invariants are exactly what the production key must hold.
    #[test]
    fn m18_gc_stable_lock_key_is_stable_and_distinct() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let a = ctx.alloc_object(ClassId::new(0), 1);
        let b = ctx.alloc_object(ClassId::new(0), 1);

        let ka1 = gc_stable_lock_key(&mut ctx, a).unwrap();
        let ka2 = gc_stable_lock_key(&mut ctx, a).unwrap();
        let kb = gc_stable_lock_key(&mut ctx, b).unwrap();

        // (a) stable for the same object across calls
        assert_eq!(ka1, ka2, "key must be stable for the same lock object");
        // (b) distinct objects → distinct keys
        assert_ne!(
            ka1, kb,
            "distinct lock objects must not alias the same slot"
        );
    }

    // bug nb-lib-gckeys §1 regression: a full write-lock / unlock round-trip
    // through the native entry points (which now route the key via `ctx` and
    // `gc_stable_lock_key`) must leave the lock un-held, proving lock and
    // unlock agree on the same GC-stable slot.
    #[test]
    fn m18_stamped_roundtrip_via_gc_stable_key() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        // Acquire the write lock, then release it.
        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        // Real JDK layout: a write stamp's mode field is WBIT (128). It used
        // to be bit 0, which made `StampedLock.isWriteLockStamp` — real JDK
        // bytecode, since nothing registers it — call this a READ stamp.
        assert!(
            matches!(stamp, Value::Long(v) if v & 255 == 128),
            "got {stamp:?}"
        );
        // `unlockWrite` now CHECKS its stamp, so it must be the real one.
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();

        // After unlock the lock must NOT report write-locked — i.e. the unlock
        // hit the SAME slot the lock established (would fail under the old
        // raw-address keying if lock and unlock disagreed on the key).
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0), "lock/unlock must agree on the slot");
    }

    #[test]
    fn m18_stamped_real_jdk_registry_keeps_write_and_unstamped_unlock_coherent() {
        let _guard = stamped_test_lock();
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        register_concurrent_natives(&mut registry);
        register_stamped_lock_natives(&mut registry);
        let sl_class = "java/util/concurrent/locks/StampedLock";
        let init = registry
            .find(sl_class, "<init>", "()V")
            .expect("StampedLock.<init> should be registered");
        let write_lock = registry
            .find(sl_class, "writeLock", "()J")
            .expect("StampedLock.writeLock should be registered");
        let unstamped_unlock_write = registry
            .find(sl_class, "unstampedUnlockWrite", "()V")
            .expect("StampedLock.unstampedUnlockWrite should be registered");
        let is_write_locked = registry
            .find(sl_class, "isWriteLocked", "()Z")
            .expect("StampedLock.isWriteLocked should be registered");

        let mut ctx = make_ctx();
        let sl = stamped_lock_obj(&mut ctx);
        init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        unstamped_unlock_write(&mut ctx, &[Value::Object(Some(sl))])
            .expect("writeLock and unstampedUnlockWrite must use the same state backend");

        let locked = is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0));
    }

    #[test]
    fn m18_stamped_write_view_lock_unlock_updates_parent_state() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = stamped_lock_obj(&mut ctx);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let view_cid = ctx
            .ensure_class_initialized("java/util/concurrent/locks/StampedLock$WriteLockView")
            .expect("mock StampedLock WriteLockView class id");
        let view = ctx.alloc_object(view_cid, 1);
        ctx.set_field_by_name(view, "this$0", Value::Object(Some(sl)));

        native_stamped_write_view_lock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
        let write_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_ne!(write_state & 128, 0);

        native_stamped_write_view_unlock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
        let post_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_eq!(post_state & 128, 0);
    }

    #[test]
    fn m18_stamped_read_view_lock_unlock_updates_parent_state() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = stamped_lock_obj(&mut ctx);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let view_cid = ctx
            .ensure_class_initialized("java/util/concurrent/locks/StampedLock$ReadLockView")
            .expect("mock StampedLock ReadLockView class id");
        let view = ctx.alloc_object(view_cid, 1);
        ctx.set_field_by_name(view, "this$0", Value::Object(Some(sl)));

        native_stamped_read_view_lock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
        let read_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_eq!(read_state & 127, 1);

        native_stamped_read_view_unlock(&mut ctx, &[Value::Object(Some(view))]).unwrap();
        let post_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_eq!(post_state & 127, 0);
    }

    #[test]
    fn m18_stamped_unstamped_write_unlock_updates_side_table_and_state() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = stamped_lock_obj(&mut ctx);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        assert_eq!(
            ctx.get_field_by_name(sl, "state"),
            Value::Long(STAMPED_ORIGIN)
        );

        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let write_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_ne!(write_state & 128, 0, "JDK WBIT must be visible");

        native_stamped_unstamped_unlock_write(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0));
        let post_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_eq!(post_state & 128, 0, "JDK WBIT must clear after unlock");
    }

    #[test]
    fn m18_stamped_unstamped_read_unlock_updates_side_table_and_state() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = stamped_lock_obj(&mut ctx);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(1));
        let read_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_ne!(read_state & 127, 0, "read count must be visible");

        native_stamped_unstamped_unlock_read(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(0));
        let post_state = match ctx.get_field_by_name(sl, "state") {
            Value::Long(v) => v,
            other => panic!("expected mirrored StampedLock.state Long, got {other:?}"),
        };
        assert_eq!(post_state & 127, 0, "read count must clear after unlock");
    }

    #[test]
    fn m18_stamped_write_lock_returns_wbit_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        match stamp {
            Value::Long(v) => {
                // JDK `isWriteLockStamp`: `(stamp & ABITS) == WBIT`.
                assert_eq!(v & 255, 128, "write stamp mode field must be WBIT, got {v}");
            }
            _ => panic!("expected Long stamp"),
        }
    }

    #[test]
    fn m18_stamped_write_lock_blocks_optimistic_read() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        // Take write lock
        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        // tryOptimisticRead should return 0 while writer is active
        let opt = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(
            opt,
            Value::Long(0),
            "optimistic read should fail during write lock"
        );
    }

    #[test]
    fn m18_stamped_unlock_write_advances_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let before = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
        let after = native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();

        match (before, after) {
            (Value::Long(b), Value::Long(a)) => {
                assert!(
                    a > b,
                    "stamp should advance after write: before={}, after={}",
                    b,
                    a
                );
                // JDK `isOptimisticReadStamp`: `(stamp & ABITS) == 0`.
                assert_eq!(a & 255, 0, "post-unlock stamp must be optimistic, got {a}");
            }
            _ => panic!("expected Long stamps"),
        }
    }

    #[test]
    fn m18_stamped_validate_succeeds_when_no_write() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            _ => panic!("expected Long"),
        };
        // No writes happened — validate should succeed
        let valid =
            native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
                .unwrap()
                .unwrap();
        assert_eq!(valid, Value::Int(1));
    }

    #[test]
    fn m18_stamped_validate_fails_after_write() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            _ => panic!("expected Long"),
        };
        // Perform a write lock/unlock cycle — stamp advances
        let wstamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), wstamp]).unwrap();
        // Old stamp should now be invalid
        let valid =
            native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
                .unwrap()
                .unwrap();
        assert_eq!(
            valid,
            Value::Int(0),
            "stamp should be invalid after a write"
        );
    }

    #[test]
    fn m18_stamped_validate_zero_always_invalid() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let valid = native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)])
            .unwrap()
            .unwrap();
        assert_eq!(valid, Value::Int(0));
    }

    #[test]
    fn m18_stamped_read_lock_returns_reader_count_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        match stamp {
            // JDK `isReadLockStamp`: `(stamp & RBITS) != 0`. The low 7 bits
            // are the READER COUNT, so the sole reader's stamp ends in 1.
            Value::Long(v) => assert_eq!(v & 255, 1, "read stamp mode field must be 1, got {v}"),
            _ => panic!("expected Long"),
        }
    }

    #[test]
    fn m18_stamped_read_lock_count() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        // No readers initially
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(0));

        // One read lock
        let stamp1 = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(1));

        // Two read locks
        let _stamp2 = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(2));

        // Unlock one — back to 1
        native_stamped_unlock_read(&mut ctx, &[Value::Object(Some(sl)), stamp1]).unwrap();
        let count = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(count, Value::Int(1));
    }

    #[test]
    fn m18_stamped_is_write_locked() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0));

        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(1));

        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0));
    }

    #[test]
    fn m18_stamped_try_write_lock_succeeds_when_free() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = native_stamped_try_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        match stamp {
            Value::Long(v) => assert!(v != 0, "tryWriteLock should succeed when free"),
            _ => panic!("expected Long"),
        }
    }

    #[test]
    fn m18_stamped_try_write_lock_fails_when_read_locked() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let stamp = native_stamped_try_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(
            stamp,
            Value::Long(0),
            "tryWriteLock should fail when readers active"
        );
    }

    #[test]
    fn m18_stamped_try_read_lock_fails_when_write_locked() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let stamp = native_stamped_try_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(
            stamp,
            Value::Long(0),
            "tryReadLock should fail when writer active"
        );
    }

    #[test]
    fn m18_stamped_convert_read_to_write() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let read_stamp = match native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            _ => panic!("expected Long"),
        };
        // Only one reader — conversion should succeed
        let write_stamp = native_stamped_try_convert_to_write(
            &mut ctx,
            &[Value::Object(Some(sl)), Value::Long(read_stamp)],
        )
        .unwrap()
        .unwrap();
        match write_stamp {
            Value::Long(v) => assert!(
                v != 0,
                "conversion to write should succeed with single reader"
            ),
            _ => panic!("expected Long"),
        }
        // Should now be write-locked
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(1));
        // Reader count should be 0
        let readers = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(readers, Value::Int(0));
    }

    #[test]
    fn m18_stamped_convert_write_to_read() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        // `tryConvertToReadLock` is stamp-checked: it must be handed the stamp
        // `writeLock()` actually returned (stamp 0 is the JDK's "no hold"
        // sentinel and correctly converts to nothing).
        let write_stamp = match native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            other => panic!("expected Long write stamp, got {other:?}"),
        };
        let read_stamp = native_stamped_try_convert_to_read(
            &mut ctx,
            &[Value::Object(Some(sl)), Value::Long(write_stamp)],
        )
        .unwrap()
        .unwrap();
        match read_stamp {
            Value::Long(v) => {
                assert!(v != 0, "conversion to read should succeed");
                // Downgrade leaves exactly one reader, so `& ABITS == 1`.
                assert_eq!(
                    v & 255,
                    1,
                    "converted read stamp must be a read stamp, got {v}"
                );
            }
            _ => panic!("expected Long"),
        }
        // Should not be write-locked anymore
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(0));
        // Should have 1 reader
        let readers = native_stamped_get_read_lock_count(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(readers, Value::Int(1));
    }

    // ------- ReentrantLock with monitor-based contention -------

    #[test]
    fn m18_rl_lock_uses_monitor_parking() {
        // Verify that ReentrantLock lock/unlock cycle works with monitor-based contention
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
        assert_ne!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);
    }

    // ------- Condition with real timeout -------

    #[test]
    fn m18_cond_await_timeout_returns_true_on_signal() {
        // In single-threaded mock, monitor_wait returns immediately → not timed out
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        let cond = match cond_val {
            Value::Object(Some(c)) => c,
            _ => panic!("expected condition object"),
        };

        // await with timeout should return true (not timed out) since mock wakes immediately
        let result =
            native_cond_await_timeout(&mut ctx, &[Value::Object(Some(cond)), Value::Long(1000)])
                .unwrap()
                .unwrap();
        // In mock, returns immediately so not timed out
        assert_eq!(result, Value::Int(1));
        // Lock should be reacquired
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
    }

    #[test]
    fn m18_cond_await_nanos_returns_remaining() {
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let cond_val = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        let cond = match cond_val {
            Value::Object(Some(c)) => c,
            _ => panic!("expected condition object"),
        };

        // awaitNanos returns remaining nanoseconds (>= 0)
        let result = native_cond_await_nanos(
            &mut ctx,
            &[Value::Object(Some(cond)), Value::Long(5_000_000_000)],
        )
        .unwrap()
        .unwrap();
        match result {
            Value::Long(v) => assert!(v >= 0, "remaining nanos should be non-negative"),
            _ => panic!("expected Long"),
        }
        // Lock should be reacquired
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
    }

    // ------- CopyOnWriteArrayList (already real) -------

    #[test]
    fn m18_cowal_iterator_snapshot_isolation() {
        // CopyOnWriteArrayList iterator should see a snapshot, not live data
        let mut ctx = make_ctx();
        let cowal = try_alloc_concurrent_synthetic(
            &mut ctx,
            "java/util/concurrent/CopyOnWriteArrayList",
            2,
        )
        .unwrap();
        cratonvm_native_collections::native_al_init(&mut ctx, &[Value::Object(Some(cowal))])
            .unwrap();

        // Add two elements
        cratonvm_native_collections::native_al_add(
            &mut ctx,
            &[Value::Object(Some(cowal)), Value::Int(10)],
        )
        .unwrap();
        cratonvm_native_collections::native_al_add(
            &mut ctx,
            &[Value::Object(Some(cowal)), Value::Int(20)],
        )
        .unwrap();

        // Size should be 2
        let size =
            cratonvm_native_collections::native_al_size(&mut ctx, &[Value::Object(Some(cowal))])
                .unwrap()
                .unwrap();
        assert_eq!(size, Value::Int(2));

        // Verify elements
        let elem0 = cratonvm_native_collections::native_al_get(
            &mut ctx,
            &[Value::Object(Some(cowal)), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(elem0, Value::Int(10));
        let elem1 = cratonvm_native_collections::native_al_get(
            &mut ctx,
            &[Value::Object(Some(cowal)), Value::Int(1)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(elem1, Value::Int(20));
    }

    #[test]
    fn m18_stamped_optimistic_read_validate_cycle() {
        let _guard = stamped_test_lock();
        // Full optimistic read cycle: tryOptimisticRead → validate (no intervening write)
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap()
        {
            Value::Long(v) => v,
            _ => panic!("expected Long"),
        };
        assert!(stamp != 0, "optimistic read should succeed when no writer");

        // Simulate reading shared data (no actual fields to read in mock, just validate)
        let valid =
            native_stamped_validate(&mut ctx, &[Value::Object(Some(sl)), Value::Long(stamp)])
                .unwrap()
                .unwrap();
        assert_eq!(
            valid,
            Value::Int(1),
            "validate should succeed when no write occurred"
        );
    }

    #[test]
    fn m18_stamped_multiple_write_cycles_advance_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let mut prev_stamp = STAMPED_ORIGIN;
        for _ in 0..5 {
            let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
                .unwrap()
                .unwrap();
            native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), stamp]).unwrap();
            let cur = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
                .unwrap()
                .unwrap()
            {
                Value::Long(v) => v,
                _ => panic!("expected Long"),
            };
            assert!(cur > prev_stamp, "stamp should monotonically increase");
            assert_eq!(
                cur & 255,
                0,
                "stamp must be optimistic after unlock, got {cur}"
            );
            prev_stamp = cur;
        }
    }

    // =======================================================================
    // Phase D (RD.1 .. RD.10) — concurrency primitives
    // =======================================================================

    use crate::phases_early;

    fn register_atomics() -> cratonvm_native_api::NativeMethodRegistry {
        let mut r = cratonvm_native_api::NativeMethodRegistry::new();
        phases_early::register_phase54_atomics(&mut r);
        r
    }

    /// RD.1 — AtomicInteger CAS semantics (single-threaded mock).
    #[test]
    fn rd1_atomic_integer_cas_and_rmw() {
        let reg = register_atomics();
        let ai = "java/util/concurrent/atomic/AtomicInteger";
        let mut ctx = make_ctx();
        let obj = try_alloc_concurrent_synthetic(&mut ctx, ai, 1).unwrap();

        let init = reg.find(ai, "<init>", "(I)V").unwrap();
        init(&mut ctx, &[Value::Object(Some(obj)), Value::Int(10)]).unwrap();
        assert_eq!(ctx.get_field(obj, 0), Value::Int(10));

        let cas = reg.find(ai, "compareAndSet", "(II)Z").unwrap();
        let ok = cas(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Int(10), Value::Int(42)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok, Value::Int(1));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(42));

        let ok2 = cas(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Int(10), Value::Int(99)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok2, Value::Int(0));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(42));

        let gaa = reg.find(ai, "getAndAdd", "(I)I").unwrap();
        let prev = gaa(&mut ctx, &[Value::Object(Some(obj)), Value::Int(8)])
            .unwrap()
            .unwrap();
        assert_eq!(prev, Value::Int(42));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(50));

        let iag = reg.find(ai, "incrementAndGet", "()I").unwrap();
        let got = iag(&mut ctx, &[Value::Object(Some(obj))]).unwrap().unwrap();
        assert_eq!(got, Value::Int(51));

        let caex = reg.find(ai, "compareAndExchange", "(II)I").unwrap();
        let old = caex(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Int(51), Value::Int(7)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(old, Value::Int(51));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(7));

        let old2 = caex(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Int(99), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(old2, Value::Int(7));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(7));
    }

    /// RD.1 — AtomicLong wraps and CAS behaves like AtomicInteger.
    #[test]
    fn rd1_atomic_long_rmw() {
        let reg = register_atomics();
        let al = "java/util/concurrent/atomic/AtomicLong";
        let mut ctx = make_ctx();
        let obj = try_alloc_concurrent_synthetic(&mut ctx, al, 1).unwrap();

        let init = reg.find(al, "<init>", "(J)V").unwrap();
        init(&mut ctx, &[Value::Object(Some(obj)), Value::Long(i64::MAX)]).unwrap();

        let iag = reg.find(al, "incrementAndGet", "()J").unwrap();
        let wrapped = iag(&mut ctx, &[Value::Object(Some(obj))]).unwrap().unwrap();
        assert_eq!(wrapped, Value::Long(i64::MIN));

        let cas = reg.find(al, "compareAndSet", "(JJ)Z").unwrap();
        let ok = cas(
            &mut ctx,
            &[
                Value::Object(Some(obj)),
                Value::Long(i64::MIN),
                Value::Long(0),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok, Value::Int(1));
        assert_eq!(ctx.get_field(obj, 0), Value::Long(0));

        let gai = reg.find(al, "getAndIncrement", "()J").unwrap();
        for _ in 0..100 {
            let _ = gai(&mut ctx, &[Value::Object(Some(obj))]).unwrap();
        }
        assert_eq!(ctx.get_field(obj, 0), Value::Long(100));
    }

    // -----------------------------------------------------------------------
    // AtomicLong.VMSupportsCS8 — static <clinit>-time query (T19 N4)
    // -----------------------------------------------------------------------

    #[test]
    fn t19_n4_atomic_long_vm_supports_cs8_on_64bit_returns_true() {
        let mut ctx = make_ctx();
        let result = super::native_atomic_long_vm_supports_cs8(&mut ctx, &[])
            .unwrap()
            .unwrap();
        // Our CI runs on x86_64 / aarch64 — both 64-bit.  Assert true (1).
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn t19_n4_atomic_long_vm_supports_cs8_ignores_extra_args() {
        // Spec: no params.  The Java verifier guarantees correct arity, so
        // trailing junk arguments must be silently ignored (no panic, no
        // validation error).
        let mut ctx = make_ctx();
        let result = super::native_atomic_long_vm_supports_cs8(&mut ctx, &[Value::Int(42)])
            .unwrap()
            .unwrap();
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn t19_n4_atomic_long_vm_supports_cs8_registered() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        super::register_atomic_long_natives(&mut registry);
        assert!(registry
            .find(
                "java/util/concurrent/atomic/AtomicLong",
                "VMSupportsCS8",
                "()Z",
            )
            .is_some());
    }

    #[test]
    fn t19_n4_atomic_long_vm_supports_cs8_registered_in_phase54() {
        // The synthetic-JDK atomics bundle also wires VMSupportsCS8 so that
        // real-JDK bytecode can initialize AtomicLong without hitting
        // UnsatisfiedLinkError during <clinit>.
        let reg = register_atomics();
        assert!(reg
            .find(
                "java/util/concurrent/atomic/AtomicLong",
                "VMSupportsCS8",
                "()Z",
            )
            .is_some());
    }

    /// RD.2 — AtomicReference CAS uses reference identity, not .equals().
    #[test]
    fn atomic_reference_to_string_delegates_to_value() {
        let reg = register_atomics();
        let ar = "java/util/concurrent/atomic/AtomicReference";
        let mut ctx = make_ctx();
        let obj = try_alloc_concurrent_synthetic(&mut ctx, ar, 1).unwrap();
        let value = ctx.create_string("CLOSED");

        let init = reg.find(ar, "<init>", "(Ljava/lang/Object;)V").unwrap();
        init(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Object(Some(value))],
        )
        .unwrap();

        let to_string = reg.find(ar, "toString", "()Ljava/lang/String;").unwrap();
        let result = to_string(&mut ctx, &[Value::Object(Some(obj))]).unwrap();
        let rendered = match result {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap(),
            other => panic!("AtomicReference.toString returned {other:?}"),
        };
        assert_eq!(rendered, "CLOSED");
    }

    #[test]
    fn rd2_atomic_reference_identity_cas() {
        let reg = register_atomics();
        let ar = "java/util/concurrent/atomic/AtomicReference";
        let mut ctx = make_ctx();
        let obj = try_alloc_concurrent_synthetic(&mut ctx, ar, 1).unwrap();

        let s_a1 = ctx.create_string("hello");
        let s_a2 = ctx.create_string("hello");

        let init = reg.find(ar, "<init>", "(Ljava/lang/Object;)V").unwrap();
        init(
            &mut ctx,
            &[Value::Object(Some(obj)), Value::Object(Some(s_a1))],
        )
        .unwrap();

        let cas = reg
            .find(
                ar,
                "compareAndSet",
                "(Ljava/lang/Object;Ljava/lang/Object;)Z",
            )
            .unwrap();
        let ok = cas(
            &mut ctx,
            &[
                Value::Object(Some(obj)),
                Value::Object(Some(s_a2)),
                Value::Object(Some(s_a2)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            ok,
            Value::Int(0),
            "reference-identity CAS must not treat .equals() as =="
        );
        assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(s_a1)));

        let ok2 = cas(
            &mut ctx,
            &[
                Value::Object(Some(obj)),
                Value::Object(Some(s_a1)),
                Value::Object(Some(s_a2)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok2, Value::Int(1));
        assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(s_a2)));
    }

    /// RD.4 — ReentrantLock reentrance counter increments and decrements.
    #[test]
    fn rd4_reentrant_lock_depth_tracking() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let held = native_rl_is_held_by_current_thread(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        assert_eq!(held, Value::Int(1));
        let hc = native_rl_get_hold_count(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        assert_eq!(hc, Value::Int(3));

        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 2);
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 1);
        native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).hold, 0);
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, RL_UNOWNED);

        // Unlock without holding must raise IllegalMonitorStateException.
        let err = native_rl_unlock(&mut ctx, &[Value::Object(Some(lock))]);
        assert!(err.is_err());
    }

    /// RD.5 — Condition.await(long, TimeUnit) uses TimeUnit conversion and
    /// reacquires the lock on return.
    #[test]
    fn rd5_condition_await_timeunit_conversion() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let lock =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3)
                .unwrap();
        native_rl_init(&mut ctx, &[Value::Object(Some(lock))]).unwrap();
        native_rl_lock(&mut ctx, &[Value::Object(Some(lock))]).unwrap();

        let cond_v = native_rl_new_condition(&mut ctx, &[Value::Object(Some(lock))])
            .unwrap()
            .unwrap();
        let cond = match cond_v {
            Value::Object(Some(c)) => c,
            _ => panic!("condition"),
        };

        // TimeUnit.SECONDS ordinal = 3; field 0 carries the ordinal.
        let tu = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        ctx.set_field(tu, 0, Value::Int(3));
        let res = native_cond_await_timeout(
            &mut ctx,
            &[
                Value::Object(Some(cond)),
                Value::Long(1),
                Value::Object(Some(tu)),
            ],
        )
        .unwrap()
        .unwrap();
        match res {
            Value::Int(v) => assert!(v == 0 || v == 1, "result must be boolean (got {v})"),
            _ => panic!("expected Int"),
        }
        // Lock reacquired on return.
        assert_eq!(rl_get(rl_key(&mut ctx, lock)).owner, ctx.thread_id() as i64);
    }

    /// RD.8 — CompletableFuture.thenApply propagates exceptional state
    /// downstream without invoking the Function.
    #[test]
    fn rd8_completable_future_exceptional_passthrough() {
        let mut ctx = make_ctx();
        let cf =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CompletableFuture", 4)
                .unwrap();
        let err_msg = ctx.create_string("boom");
        ctx.set_field(cf, FUT_FIELD_RESULT, Value::Object(Some(err_msg)));
        ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(2));

        let func = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let result = native_cf_then_apply(
            &mut ctx,
            &[Value::Object(Some(cf)), Value::Object(Some(func))],
        )
        .unwrap()
        .unwrap();
        let downstream = match result {
            Value::Object(Some(o)) => o,
            _ => panic!("expected downstream CF"),
        };
        assert_eq!(ctx.get_field(downstream, FUT_FIELD_DONE), Value::Int(2));
        assert_eq!(
            ctx.get_field(downstream, FUT_FIELD_RESULT),
            Value::Object(Some(err_msg))
        );
    }

    /// RD.9 — Thread.sleep(0, 500_000) honours sub-millisecond precision.
    #[test]
    fn rd9_thread_sleep_sub_millisecond_precision() {
        let mut ctx = make_ctx();
        let start = std::time::Instant::now();
        crate::lang_system::native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(500_000)],
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_nanos() >= 500_000,
            "sleep(0, 500_000) too short: {}ns",
            elapsed.as_nanos()
        );
    }

    /// RD.9 — argument validation: negative millis and out-of-range nanos.
    #[test]
    fn rd9_thread_sleep_argument_validation() {
        let mut ctx = make_ctx();
        let neg = crate::lang_system::native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(-1), Value::Int(0)],
        );
        assert!(neg.is_err(), "negative millis must raise");
        let bad_nanos = crate::lang_system::native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(1_000_000)],
        );
        assert!(bad_nanos.is_err(), "nanos >= 1_000_000 must raise");
    }

    /// RD.10 — Thread.join(long) rejects negative timeouts.
    #[test]
    fn rd10_thread_join_rejects_negative_timeout() {
        let mut ctx = make_ctx();
        let t = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
        let err = crate::lang_system::native_thread_join_timed(
            &mut ctx,
            &[Value::Object(Some(t)), Value::Long(-5)],
        );
        assert!(err.is_err());
    }

    /// RD.10 — Thread.join(long, int) validates nanosecond range.
    #[test]
    fn rd10_thread_join_nanos_range() {
        let mut ctx = make_ctx();
        let t = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
        let err = crate::lang_system::native_thread_join_millis_nanos(
            &mut ctx,
            &[
                Value::Object(Some(t)),
                Value::Long(10),
                Value::Int(2_000_000),
            ],
        );
        assert!(err.is_err());
        let ok = crate::lang_system::native_thread_join_millis_nanos(
            &mut ctx,
            &[Value::Object(Some(t)), Value::Long(0), Value::Int(1)],
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn aqls_state_natives_preserve_atomic_long_transitions() {
        let mut ctx = make_ctx();
        let synchronizer = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

        assert_eq!(
            native_aqls_get_state(&mut ctx, &[Value::Object(Some(synchronizer))])
                .unwrap()
                .unwrap(),
            Value::Long(0)
        );
        native_aqls_set_state(
            &mut ctx,
            &[Value::Object(Some(synchronizer)), Value::Long(4)],
        )
        .unwrap();
        assert_eq!(
            native_aqls_compare_and_set_state(
                &mut ctx,
                &[
                    Value::Object(Some(synchronizer)),
                    Value::Long(4),
                    Value::Long(8),
                ],
            )
            .unwrap()
            .unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            native_aqls_compare_and_set_state(
                &mut ctx,
                &[
                    Value::Object(Some(synchronizer)),
                    Value::Long(4),
                    Value::Long(12),
                ],
            )
            .unwrap()
            .unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            native_aqls_get_state(&mut ctx, &[Value::Object(Some(synchronizer))])
                .unwrap()
                .unwrap(),
            Value::Long(8)
        );
    }
}

/// The legacy synthetic `ReentrantLock` / `Lock` / `Condition` / `Semaphore`
/// natives, split out of [`register_concurrent_natives`] so they have a name.
///
/// Production behaviour is unchanged: `register_concurrent_natives` calls this
/// under exactly the `!real_aqs` condition the two inline blocks used to test,
/// so the default real-AQS build still registers none of them.
///
/// It is public because otherwise this code is untestable. Real AQS became the
/// default, which is right for the VM, but it left the synthetic implementation
/// shipped and unreachable from any test: `cratonvm-vm`'s inline suite builds
/// hand-rolled receivers with no real `java.util.concurrent` bytecode behind
/// them, so it cannot exercise the real path, and the synthetic path was no
/// longer registered for it to exercise either. Its tests were left asserting
/// natives that the default build deliberately omits, and simply failed. A
/// test registry can now opt in the same way `CRATONVM_SYNTHETIC_AQS=1` does
/// at runtime.
pub fn register_synthetic_aqs_natives(registry: &mut NativeMethodRegistry) {
    // --- ReentrantLock ---
    let rl = "java/util/concurrent/locks/ReentrantLock";
    registry.register(rl, "<init>", "()V", native_rl_init);
    registry.register(rl, "<init>", "(Z)V", native_rl_init_fair);
    registry.register(rl, "lock", "()V", native_rl_lock);
    registry.register(rl, "lockInterruptibly", "()V", native_rl_lock); // simplified
    registry.register(rl, "unlock", "()V", native_rl_unlock);
    registry.register(rl, "tryLock", "()Z", native_rl_try_lock);
    registry.register(
        rl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_rl_try_lock_timeout,
    );
    registry.register(rl, "isLocked", "()Z", native_rl_is_locked);
    registry.register(
        rl,
        "isHeldByCurrentThread",
        "()Z",
        native_rl_is_held_by_current_thread,
    );
    registry.register(rl, "getHoldCount", "()I", native_rl_get_hold_count);
    registry.register(rl, "isFair", "()Z", native_rl_is_fair);
    registry.register(
        rl,
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        native_rl_new_condition,
    );
    registry.register(rl, "toString", "()Ljava/lang/String;", native_rl_to_string);

    // Also register under Lock interface
    let lock = "java/util/concurrent/locks/Lock";
    registry.register(lock, "lock", "()V", native_rl_lock);
    registry.register(lock, "unlock", "()V", native_rl_unlock);
    registry.register(lock, "tryLock", "()Z", native_rl_try_lock);
    registry.register(
        lock,
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        native_rl_new_condition,
    );

    // --- Condition ---
    let cond = "java/util/concurrent/locks/Condition";
    registry.register(cond, "await", "()V", native_cond_await);
    registry.register(cond, "awaitUninterruptibly", "()V", native_cond_await);
    registry.register(
        cond,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_cond_await_timeout,
    );
    registry.register(cond, "awaitNanos", "(J)J", native_cond_await_nanos);
    registry.register(
        cond,
        "awaitUntil",
        "(Ljava/util/Date;)Z",
        native_cond_await_until,
    );
    registry.register(cond, "signal", "()V", native_cond_signal);
    registry.register(cond, "signalAll", "()V", native_cond_signal_all);

    let sem = "java/util/concurrent/Semaphore";
    registry.register(sem, "<init>", "(I)V", native_sem_init);
    registry.register(sem, "<init>", "(IZ)V", native_sem_init_fair);
    registry.register(sem, "acquire", "()V", native_sem_acquire);
    registry.register(sem, "acquire", "(I)V", native_sem_acquire_n);
    registry.register(sem, "acquireUninterruptibly", "()V", native_sem_acquire);
    // HikariCP's SuspendResumeLock may acquire all 10,000 permits at
    // once; retain this synthetic-mode overload.
    registry.register(sem, "acquireUninterruptibly", "(I)V", native_sem_acquire_n);
    registry.register(sem, "release", "()V", native_sem_release);
    registry.register(sem, "release", "(I)V", native_sem_release_n);
    registry.register(sem, "tryAcquire", "()Z", native_sem_try_acquire);
    registry.register(sem, "tryAcquire", "(I)Z", native_sem_try_acquire_n);
    registry.register(
        sem,
        "tryAcquire",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_sem_try_acquire_timeout,
    );
    registry.register(sem, "availablePermits", "()I", native_sem_available_permits);
    registry.register(sem, "drainPermits", "()I", native_sem_drain_permits);
    registry.register(sem, "isFair", "()Z", native_sem_is_fair);
    registry.register(
        sem,
        "toString",
        "()Ljava/lang/String;",
        native_sem_to_string,
    );

    // --- ReentrantReadWriteLock ---
    //
    // Never had natives at all: `ClassManager` declares the layout
    // (`ReentrantReadWriteLock` = 3 fields, `$ReadLock`/`$WriteLock` = 1)
    // but nothing implemented it, so synthetic mode answered
    // `NoSuchMethodError: ReentrantReadWriteLock.readLock()` — the last of
    // the `JucComplete` lock failures.
    let rwl = RWL_CLASS;
    registry.register(rwl, "<init>", "()V", native_rwl_init);
    registry.register(rwl, "<init>", "(Z)V", native_rwl_init_fair);
    registry.register(
        rwl,
        "readLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$ReadLock;",
        native_rwl_read_lock,
    );
    registry.register(
        rwl,
        "writeLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$WriteLock;",
        native_rwl_write_lock,
    );
    registry.register(rwl, "isFair", "()Z", native_rwl_is_fair);
    registry.register(rwl, "getReadLockCount", "()I", native_rwl_read_count);
    registry.register(rwl, "isWriteLocked", "()Z", native_rwl_is_write_locked);
    registry.register(rwl, "getWriteHoldCount", "()I", native_rwl_write_hold_count);
    for view in [RWL_READ_VIEW, RWL_WRITE_VIEW] {
        registry.register(view, "lock", "()V", native_rwl_view_lock);
        registry.register(view, "lockInterruptibly", "()V", native_rwl_view_lock);
        registry.register(view, "unlock", "()V", native_rwl_view_unlock);
        registry.register(view, "tryLock", "()Z", native_rwl_view_try_lock);
    }
}

// ---------------------------------------------------------------------------
// ReentrantReadWriteLock — a reader/writer lock over the same monitor-parking
// scheme the `ReentrantLock` natives above use.
//
// State lives in a side table keyed by the lock's identity hash, not in the
// object's fields, for the reason spelled out on `rl_state_table`: the real
// class's slots hold `Sync` references and writing scalars there does not
// round-trip. The two *view* objects (`$ReadLock` / `$WriteLock`) ARE cached
// in the owner's slots 0 and 1 — `rwl.readLock()` must return the same
// instance every time, as HotSpot's final fields do — with slot 2 the `fair`
// flag, matching `ClassManager`'s declared 3-field layout.
//
// Semantics implemented: multiple concurrent readers, one writer excluding all
// readers, reentrant write holds, and write-while-holding-read refused (real
// AQS throws no error but deadlocks; refusing is the honest approximation and
// no fixture depends on it). Lock downgrading (write → read) works because a
// writer may always take a read lock.
// ---------------------------------------------------------------------------

const RWL_CLASS: &str = "java/util/concurrent/locks/ReentrantReadWriteLock";
const RWL_READ_VIEW: &str = "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock";
const RWL_WRITE_VIEW: &str = "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock";

/// Slot 0/1 cache the `$ReadLock` / `$WriteLock` views; slot 2 is `fair`.
const RWL_FIELD_READ_VIEW: usize = 0;
const RWL_FIELD_WRITE_VIEW: usize = 1;
const RWL_FIELD_FAIR: usize = 2;
/// A view's single slot points back at the owning `ReentrantReadWriteLock`.
const RWL_VIEW_FIELD_OWNER: usize = 0;

#[derive(Clone, Copy, Default)]
struct RwlState {
    /// Total read holds across all threads.
    readers: i32,
    /// Owning thread of the write lock, or `RL_UNOWNED`.
    writer: i64,
    /// Reentrant write hold count.
    write_hold: i32,
}

fn rwl_state_table() -> &'static std::sync::Mutex<std::collections::HashMap<RlKey, RwlState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<RlKey, RwlState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Drop every read/write-lock row belonging to `vm_identity`; the companion of
/// `forget_vm_lock_state`, which calls it.
pub fn forget_vm_rwl_state(vm_identity: usize) {
    rwl_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _), _| *vm != vm_identity);
}

fn rwl_with<R>(key: RlKey, f: impl FnOnce(&mut RwlState) -> R) -> R {
    let mut table = rwl_state_table().lock().unwrap_or_else(|e| e.into_inner());
    f(table.entry(key).or_default())
}

fn rwl_get(key: RlKey) -> RwlState {
    rwl_state_table()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).copied())
        .unwrap_or_default()
}

fn native_rwl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    rwl_init_common(ctx, args, false)
}

fn native_rwl_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fair = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    rwl_init_common(ctx, args, fair)
}

fn rwl_init_common(ctx: &mut dyn NativeContext, args: &[Value], fair: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = rl_key(ctx, this);
    rwl_with(key, |st| *st = RwlState::default());
    if ctx.object_num_fields(this) > RWL_FIELD_FAIR {
        ctx.set_field(this, RWL_FIELD_READ_VIEW, Value::Object(None));
        ctx.set_field(this, RWL_FIELD_WRITE_VIEW, Value::Object(None));
        ctx.set_field(this, RWL_FIELD_FAIR, Value::Int(i32::from(fair)));
    }
    Ok(None)
}

/// Return (creating on first call) the cached view object for `slot`.
fn rwl_view(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
    class_name: &str,
) -> MethodCallResult {
    if ctx.object_num_fields(this) > slot {
        if let Value::Object(Some(existing)) = ctx.get_field(this, slot) {
            return Ok(Some(Value::Object(Some(existing))));
        }
    }
    // Allocate first, publish afterwards: `alloc_concurrent_synthetic` can
    // trigger a collection that moves `this`, and `this` is pinned as a native
    // ARG — the pin is remapped, this Rust local is not. Re-read it through a
    // pin around the allocation.
    let this_pin = ctx.pin_native_root(this);
    let view = try_alloc_concurrent_synthetic(ctx, class_name, 1)?;
    let view_pin = ctx.pin_native_root(view);
    let this = ctx.read_native_pin(this_pin, this);
    let view = ctx.read_native_pin(view_pin, view);
    ctx.set_field(view, RWL_VIEW_FIELD_OWNER, Value::Object(Some(this)));
    if ctx.object_num_fields(this) > slot {
        ctx.set_field(this, slot, Value::Object(Some(view)));
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(view))))
}

fn native_rwl_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    rwl_view(ctx, this, RWL_FIELD_READ_VIEW, RWL_READ_VIEW)
}

fn native_rwl_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    rwl_view(ctx, this, RWL_FIELD_WRITE_VIEW, RWL_WRITE_VIEW)
}

fn native_rwl_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if ctx.object_num_fields(this) > RWL_FIELD_FAIR {
        if let Value::Int(v) = ctx.get_field(this, RWL_FIELD_FAIR) {
            return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_rwl_read_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(rwl_get(key).readers)))
}

fn native_rwl_is_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(i32::from(rwl_get(key).write_hold > 0))))
}

fn native_rwl_write_hold_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let st = rwl_get(key);
    Ok(Some(Value::Int(if st.writer == tid {
        st.write_hold
    } else {
        0
    })))
}

/// Resolve a `$ReadLock`/`$WriteLock` view to `(owner, is_write)`.
fn rwl_view_target(ctx: &mut dyn NativeContext, view: ObjectRef) -> Option<(ObjectRef, bool)> {
    let owner = match ctx.get_field(view, RWL_VIEW_FIELD_OWNER) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let is_write = ctx
        .class_name_of_id(ctx.class_id_of_object(view))
        .is_some_and(|name| name == RWL_WRITE_VIEW);
    Some((owner, is_write))
}

/// Try to take the lock once. `None` means "would block".
fn rwl_try_acquire(key: RlKey, tid: i64, is_write: bool) -> bool {
    rwl_with(key, |st| {
        if is_write {
            let free = st.writer == RL_UNOWNED && st.readers == 0;
            let reentrant = st.writer == tid;
            if free || reentrant {
                st.writer = tid;
                st.write_hold += 1;
                return true;
            }
            false
        } else {
            // A writer excludes readers, except itself (lock downgrading).
            if st.writer == RL_UNOWNED || st.writer == tid {
                st.readers += 1;
                return true;
            }
            false
        }
    })
}

fn native_rwl_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((mut owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(None);
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    loop {
        if rwl_try_acquire(key, tid, is_write) {
            return Ok(None);
        }
        // Park on the owner lock's monitor until a releaser notifies. The 5ms
        // timeout re-checks defensively, exactly as `native_rl_lock` does.
        ctx.monitor_enter(owner);
        let still_blocked = {
            let st = rwl_get(key);
            if is_write {
                (st.writer != RL_UNOWNED && st.writer != tid) || st.readers > 0
            } else {
                st.writer != RL_UNOWNED && st.writer != tid
            }
        };
        if still_blocked {
            if let Err(e) = monitor_wait_release(ctx, &mut owner, Some(5)) {
                if is_interrupted_exception(&e) {
                    let self_thread = ctx.current_thread_object();
                    ctx.thread_interrupt(self_thread);
                    continue;
                }
                return Err(e);
            }
        } else {
            ctx.monitor_exit(owner);
        }
    }
}

fn native_rwl_view_try_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some((owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(Some(Value::Int(0)));
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    Ok(Some(Value::Int(i32::from(rwl_try_acquire(
        key, tid, is_write,
    )))))
}

fn native_rwl_view_unlock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(None);
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    let released = rwl_with(key, |st| {
        if is_write {
            if st.writer != tid {
                return false;
            }
            st.write_hold -= 1;
            if st.write_hold <= 0 {
                st.write_hold = 0;
                st.writer = RL_UNOWNED;
                return true;
            }
            false
        } else {
            if st.readers > 0 {
                st.readers -= 1;
            }
            st.readers == 0
        }
    });
    if released {
        // Wake anyone parked in `native_rwl_view_lock`.
        ctx.monitor_enter(owner);
        let _ = ctx.monitor_notify_all(owner);
        ctx.monitor_exit(owner);
    }
    Ok(None)
}
