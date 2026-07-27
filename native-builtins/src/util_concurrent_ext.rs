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
    r.register(c, "get", "()I", native_atomic_int_get);
    r.register(c, "set", "(I)V", native_atomic_int_set);
    r.register(c, "lazySet", "(I)V", native_atomic_int_set);
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
    r.register(c, "intValue", "()I", native_atomic_int_get);
    r.register(c, "longValue", "()J", native_atomic_int_long_value);
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
    r.register(c, "get", "()J", native_atomic_long_get);
    r.register(c, "set", "(J)V", native_atomic_long_set);
    r.register(c, "lazySet", "(J)V", native_atomic_long_set);
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
    r.register(c, "intValue", "()I", native_atomic_long_int_value);
    r.register(c, "longValue", "()J", native_atomic_long_get);
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

fn native_atomic_long_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    Ok(Some(ctx.get_field_volatile(this, 0)))
}

fn native_atomic_long_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let val = args.get(1).copied().unwrap_or(Value::Long(0));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

fn native_atomic_long_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let new_val = args.get(1).copied().unwrap_or(Value::Long(0));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_atomic_long_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
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
    let this = unsafe_obj(args, 0).unwrap();
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old)))
}

fn native_atomic_long_get_and_decrement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old)))
}

fn native_atomic_long_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
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
    let this = unsafe_obj(args, 0).unwrap();
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old.wrapping_add(1))))
}

fn native_atomic_long_decrement_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old.wrapping_sub(1))))
}

fn native_atomic_long_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let delta = match args.get(1) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_long(this, 0, delta)?;
    Ok(Some(Value::Long(old.wrapping_add(delta))))
}

fn native_atomic_long_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
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
    r.register(
        c,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_atomic_ref_cas,
    );
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
    let this = unsafe_obj(args, 0).unwrap();
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
    let this = unsafe_obj(args, 0).unwrap();
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

fn native_atomic_ref_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let expected = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    if crate::nbflags().dbg_loader_trace {
        if let Value::Object(Some(o)) = new_val {
            let val_cid = ctx.class_id_of_object(o);
            let val_cn = ctx.class_name_of_id(val_cid).unwrap_or_default();
            if val_cn.contains("RootReference") {
                let frames = ctx.frame_class_ids();
                let caller_cid = frames.first().copied();
                let caller_cn = caller_cid
                    .and_then(|c| ctx.class_name_of_id(c))
                    .unwrap_or_default();
                eprintln!(
                    "[LOADER-TRACE] native_atomic_ref_cas PRE thread={} holder_obj={:p} new_obj={:p} new_class={} new_cid={} caller_class_id={:?} caller_class={}",
                    ctx.thread_id(), this.as_ptr(), o.as_ptr(), val_cn, val_cid.as_u32(), caller_cid, caller_cn
                );
            }
        }
    }
    let result = ctx.compare_and_swap_field(this, 0, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

fn native_atomic_ref_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
    let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

fn native_atomic_ref_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();
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

pub(crate) fn alloc_concurrent_synthetic(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> ObjectRef {
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
                ctx.class_id_by_name(class_name)
                    .unwrap_or_else(|| ctx.ensure_synthetic_class(class_name, num_fields))
            };
            // In real-JDK mode the loaded class's actual instance-field
            // count often exceeds the synthetic-mode hard-coded number.
            // Allocating with too few slots causes out-of-bounds field
            // access later (KC16 bootstrap tripped this on ClassId
            // 355/359 with index=4 vs num_slots=3).  Use the larger of
            // the two so both paths have enough room.  0 means the class
            // isn't loaded yet — keep the caller's requested size.
            let real = ctx.class_num_total_fields(cid);
            let n = num_fields.max(real);
            // `try_alloc_object_gc_safe` first (proactively collects, then
            // walks young -> old gen without aborting): this is the shared
            // allocator behind `java.net.URI`, `HttpURLConnection`, and many
            // other synthetic native objects -- a gdb backtrace confirmed
            // TestResponsePerformance's doUri() hot loop (`new URI(...)` x
            // 1,000,000) hard-aborted the whole process here on young-gen
            // exhaustion. Falling back to the aborting `alloc_object` only
            // if the GC-safe path still reports genuine exhaustion (both
            // generations full even after a fresh collection) preserves
            // today's behavior for that now much narrower case, with no
            // signature change for this function's many other callers. See
            // docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md.
            ctx.try_alloc_object_gc_safe(cid, n)
                .unwrap_or_else(|| ctx.alloc_object(cid, n))
        }
        Err(_) => {
            // The real `.class` file could not be loaded. Allocating with
            // `ClassId::new(0)` (`java/lang/Object`, zero declared fields)
            // but a non-zero slot count produces an "undersized object
            // layout" object — the GC's `get_field` bounds guard rejects
            // every field access on it (class declares 0 fields, object has
            // `num_fields` slots). Register a synthetic class declaring
            // `num_fields` instance fields so the header's `class_id`
            // matches the allocated slot count.
            let cid = ctx.ensure_synthetic_class(class_name, num_fields);
            ctx.alloc_object(cid, num_fields)
        }
    }
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
    } // end if !real_aqs

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
    if !real_aqs {
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
    }

    // --- CyclicBarrier ---
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
            let idx = match args.get(1) {
                Some(Value::Int(i)) => *i as usize,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (data, size) = cowal_read_state(ctx, this);
            if idx >= size {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(
                data.map(|a| ctx.get_array_element(a, idx))
                    .unwrap_or(Value::Object(None)),
            ))
        });
        registry.register(cowal, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (data, size) = cowal_read_state(ctx, this);
            if let Some(arr) = data {
                for i in 0..size {
                    let elem = ctx.get_array_element(arr, i);
                    if cowal_element_matches(ctx, elem, needle) {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
            Ok(Some(Value::Int(0)))
        });
        registry.register(cowal, "indexOf", "(Ljava/lang/Object;)I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (data, size) = cowal_read_state(ctx, this);
            if let Some(arr) = data {
                for i in 0..size {
                    let elem = ctx.get_array_element(arr, i);
                    if cowal_element_matches(ctx, elem, needle) {
                        return Ok(Some(Value::Int(i as i32)));
                    }
                }
            }
            Ok(Some(Value::Int(-1)))
        });
        registry.register(cowal, "toArray", "()[Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (data, size) = cowal_read_state(ctx, this);
            let out = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (data_opt, size) = cowal_read_state(ctx, this);
            // Copy into snapshot array (matches COWAL semantics: writes after
            // iterator creation do not affect what the iterator sees).
            let snap = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
            if let Some(data) = data_opt {
                for i in 0..size {
                    let elem = ctx.get_array_element(data, i);
                    ctx.set_array_element(snap, i, elem);
                }
            }
            // Return a self-contained snapshot iterator (3-field `HashMap$KeyItr`
            // model: keys/cursor/total).  Unlike the previous `ArrayList$Itr`
            // wrapper, this iterator's `remove()` throws
            // `UnsupportedOperationException` — matching real COWAL's `COWIterator`,
            // which never supports removal (MutablePropertySourcesTests
            // `iteratorContainsPropertySource`).  The earlier wrapper reused the
            // mutating `ArrayList$Itr.remove` native, so `it.remove()` silently
            // succeeded instead of throwing.
            cratonvm_native_collections::make_iterator_from_array(ctx, snap, size)
        });
        // Writes — true copy-on-write: copy array, mutate copy, swap reference.
        // Uses `cowal_read_state` / `cowal_write_array` so both real and
        // synthetic COWAL layouts work.
        registry.register(
            cowal,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let idx = match args.get(1) {
                    Some(Value::Int(i)) => *i as usize,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let (old_arr, size) = cowal_read_state(ctx, this);
                if idx >= size {
                    ctx.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (old_arr, size) = cowal_read_state(ctx, this);
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size + 1);
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
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let elem = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let (old_arr, size) = cowal_read_state(ctx, this);
                if let Some(old) = old_arr {
                    for i in 0..size {
                        let cur = ctx.get_array_element(old, i);
                        if cowal_element_matches(ctx, cur, elem) {
                            ctx.monitor_exit(this);
                            return Ok(Some(Value::Int(0)));
                        }
                    }
                }
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size + 1);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let idx = match args.get(1) {
                Some(Value::Int(i)) => *i as usize,
                _ => return Ok(None),
            };
            let elem = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (old_arr, size) = cowal_read_state(ctx, this);
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size + 1);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let idx = match args.get(1) {
                Some(Value::Int(i)) => *i as usize,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let (old_arr, size) = cowal_read_state(ctx, this);
            if idx >= size {
                ctx.monitor_exit(this);
                return Ok(Some(Value::Object(None)));
            }
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size - 1);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let needle = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (old_arr, size) = cowal_read_state(ctx, this);
            let mut found_idx: Option<usize> = None;
            if let Some(old) = old_arr {
                for i in 0..size {
                    let elem = ctx.get_array_element(old, i);
                    if cowal_element_matches(ctx, elem, needle) {
                        found_idx = Some(i);
                        break;
                    }
                }
            }
            if let Some(idx) = found_idx {
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size - 1);
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            ctx.monitor_enter(this);
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
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
            for i in 0..arr_len {
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
                    loop {
                        let has = ctx.invoke_virtual(iter, "hasNext", "()Z", &[])?;
                        if has != Some(Value::Int(1)) {
                            break;
                        }
                        let elem = ctx
                            .invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[])?
                            .unwrap_or(Value::Object(None));
                        m18_lbq_add_internal(ctx, this, elem);
                    }
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
                    Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
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
                    let iter = alloc_concurrent_synthetic(ctx, "java/util/ArrayList$Itr", 3);
                    ctx.set_field(iter, 0, Value::Int(0));
                    ctx.set_field(iter, 1, Value::Int(0));
                    return Ok(Some(Value::Object(Some(iter))));
                }
            };
            let snap = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
            for i in 0..size {
                ctx.set_array_element(snap, i, ctx.get_array_element(arr, i));
            }
            let iter = alloc_concurrent_synthetic(ctx, "java/util/ArrayList$Itr", 3);
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
            for i in 0..size as usize {
                let elem = ctx.get_array_element(arr, i);
                ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
            }
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
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            loop {
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
                monitor_wait_release(ctx, this, Some(10))?;
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
            let this = match args.first() {
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
                    let result = m18_abq_remove_head(ctx, this);
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(Some(result));
                }
                monitor_wait_release(ctx, this, Some(10))?;
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
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
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
                    monitor_wait_release(ctx, this, Some(wait_ms))?;
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
            for i in 0..size {
                let idx = ((head + i) % cap) as usize;
                let elem = ctx.get_array_element(arr, idx);
                ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
            }
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
        let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        ctx.set_field(this, 0, Value::Object(Some(keys)));
        ctx.set_field(this, 1, Value::Object(Some(vals)));
        ctx.set_field(this, 2, Value::Int(0));
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
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
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

            // Find insertion point via linear scan (compareTo)
            let mut pos = size;
            for i in 0..size {
                let existing_key = ctx.get_array_element(keys_arr, i);
                if let Value::Object(Some(ek)) = existing_key {
                    if let Ok(Some(Value::Int(cmp))) =
                        ctx.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[key])
                    {
                        if cmp == 0 {
                            // Key exists — replace value
                            let old = ctx.get_array_element(vals_arr, i);
                            ctx.set_array_element(vals_arr, i, val);
                            ctx.monitor_exit(this);
                            return Ok(Some(old));
                        } else if cmp > 0 {
                            pos = i;
                            break;
                        }
                    }
                }
            }

            // Grow if needed
            let cap = ctx.array_length(keys_arr);
            if size >= cap {
                let new_cap = cap * 2;
                let new_keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let new_vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                for i in 0..size {
                    ctx.set_array_element(new_keys, i, ctx.get_array_element(keys_arr, i));
                    ctx.set_array_element(new_vals, i, ctx.get_array_element(vals_arr, i));
                }
                ctx.set_field(this, 0, Value::Object(Some(new_keys)));
                ctx.set_field(this, 1, Value::Object(Some(new_vals)));
                // Re-fetch after resize
                let keys_arr = new_keys;
                let vals_arr = new_vals;
                // Shift right from pos
                for i in (pos..size).rev() {
                    ctx.set_array_element(keys_arr, i + 1, ctx.get_array_element(keys_arr, i));
                    ctx.set_array_element(vals_arr, i + 1, ctx.get_array_element(vals_arr, i));
                }
                ctx.set_array_element(keys_arr, pos, key);
                ctx.set_array_element(vals_arr, pos, val);
            } else {
                // Shift right from pos
                for i in (pos..size).rev() {
                    ctx.set_array_element(keys_arr, i + 1, ctx.get_array_element(keys_arr, i));
                    ctx.set_array_element(vals_arr, i + 1, ctx.get_array_element(vals_arr, i));
                }
                ctx.set_array_element(keys_arr, pos, key);
                ctx.set_array_element(vals_arr, pos, val);
            }
            ctx.set_field(this, 2, Value::Int((size + 1) as i32));
            ctx.monitor_exit(this);
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
            let this = match args.first() {
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
            loop {
                let size_now = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size_now < size_before {
                    return Ok(None);
                }
                ctx.monitor_enter(this);
                monitor_wait_release(ctx, this, Some(5))?;
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
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
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
                        return Ok(Some(Value::Object(None)));
                    }
                    let wait_ms = bounded_monitor_wait_ms(remaining, 10);
                    monitor_wait_release(ctx, this, Some(wait_ms))?;
                }
            },
        );
        // take() — blocking
        registry.register(ltq, "take", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
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
                monitor_wait_release(ctx, this, Some(10))?;
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
                let sub =
                    alloc_concurrent_synthetic(ctx, "java/util/concurrent/Flow$Subscription", 1);
                ctx.set_field(sub, 0, Value::Int(0)); // cancelled flag
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
    registry.register(flow_sub, "request", "(J)V", |_ctx, _args| {
        // No-op for default subscription
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
    loop {
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
            if let Err(e) = monitor_wait_release(ctx, this, Some(5)) {
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
        Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord);
    if timeout_ms <= 0 {
        return native_rl_try_lock(ctx, args);
    }
    let key = rl_key(ctx, this);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    loop {
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
        monitor_wait_release(ctx, this, Some(wait_ms))?;
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
    let cond = alloc_concurrent_synthetic(ctx, "java/util/concurrent/locks/Condition", 1);
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
        Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
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
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2, // MILLISECONDS
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
    loop {
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
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
        _ => 2, // MILLISECONDS
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
    let aes = "java/util/concurrent/AbstractExecutorService";
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        exec,
        "defaultThreadFactory",
        "()Ljava/util/concurrent/ThreadFactory;",
        |ctx, _args| {
            let factory = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadFactory", 1);
            ctx.set_field(factory, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(factory))))
        },
    );
    registry.register(
        tf,
        "newThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            // BUG FIX (2026-07-10, ES executors-factory mainlock NPE, layer 2
            // residual): this used to allocate a real-shaped Thread object via
            // alloc_concurrent_synthetic and then poke 5 legacy synthetic
            // slots — the exact same half-real pattern that made
            // ThreadPoolExecutor's mainLock/ctl/workQueue null (see
            // initialize_real_thread_pool_executor in phases_early.rs). A
            // Thread built this way never runs the real constructor, so
            // start()/start0() operate on an uninitialized `holder` and the
            // worker never actually runs — real ThreadPoolExecutor.execute()
            // silently never executes submitted tasks. Drive the real
            // Thread(Runnable) constructor instead so start() works.
            // See docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md.
            let runnable = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.new_object_initialized("java/lang/Thread", "(Ljava/lang/Runnable;)V", &[runnable])
        },
    );
    registry.set_category(__prev_cat);

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
    registry.register(
        aes,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_es_submit_runnable,
    );
    registry.register(
        aes,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_es_submit_callable,
    );
    registry.register(es, "execute", "(Ljava/lang/Runnable;)V", native_es_execute);
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
    registry.register(tp, "execute", "(Ljava/lang/Runnable;)V", native_es_execute);
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
    registry.register(
        aes,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        submit_rt_closure,
    );

    // RD.6: invokeAll(Collection<Callable>) -> List<Future> — run sequentially.
    let invoke_all_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let coll = match args.get(1) {
            Some(Value::Object(Some(c))) => *c,
            _ => return Ok(Some(Value::Object(None))),
        };
        let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
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
    registry.register(
        aes,
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        invoke_any_closure,
    );
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
        let submitted = ctx.invoke_virtual(
            pool,
            "execute",
            "(Ljava/lang/Runnable;)V",
            &[Value::Object(Some(runnable_cur))],
        );
        ctx.unpin_native_roots(pin);
        submitted?;
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
    registry.register(fut, "cancel", "(Z)Z", |_ctx, _args| Ok(Some(Value::Int(0))));
    registry.register(fut, "isCancelled", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // FutureTask
    registry.register(ft, "get", "()Ljava/lang/Object;", native_fut_get);
    registry.register(
        ft,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed,
    );
    registry.register(ft, "isDone", "()Z", native_fut_is_done);
    registry.register(ft, "cancel", "(Z)Z", |_ctx, _args| Ok(Some(Value::Int(0))));
    registry.register(ft, "isCancelled", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // CompletableFuture timed get
    registry.register(
        cf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed,
    );
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
        Some(Value::Object(Some(u))) => ctx.get_field(*u, 0).as_int().unwrap_or(2),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord).max(0) as u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
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
        monitor_wait_release(ctx, this, Some(wait_ms))?;
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
    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, FUT_FIELD_RESULT, val);
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_supply_async(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
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
            ctx.invoke_virtual(this, "postComplete", "()V", &[])?;
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
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
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
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
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
        let _ = crate::interrupt_executor_workers_filtered(ctx, executor, /* only_idle */ true);
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

pub(crate) fn register_rwlock_natives(registry: &mut NativeMethodRegistry) {
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
    fn rwl_parent_addr(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<usize> {
        match ctx.get_field(this, 0) {
            Value::Object(Some(parent)) => Some(gc_stable_lock_key(ctx, parent)),
            _ => None,
        }
    }

    registry.register(rl, "lock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Some(addr) = rwl_parent_addr(ctx, this) {
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
        if let Some(addr) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_read_unlock(addr, ctx.thread_id());
        }
        Ok(None)
    });
    registry.register(rl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this) {
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
            let ok = match rwl_parent_addr(ctx, this) {
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
        if let Some(addr) = rwl_parent_addr(ctx, this) {
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
        if let Some(addr) = rwl_parent_addr(ctx, this) {
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
        if let Some(addr) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_write_unlock(addr, ctx.thread_id());
        }
        Ok(None)
    });
    registry.register(wl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this) {
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
            let ok = match rwl_parent_addr(ctx, this) {
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
        if let Some(addr) = rwl_parent_addr(ctx, this) {
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
        let held = match rwl_parent_addr(ctx, this) {
            Some(a) => crate::stamped_lock::rw_write_is_held(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(held))))
    });

    // StampedLock — real optimistic read / exclusive write lock with stamp validation.
    // Uses a global state map keyed by object address for per-lock state.
    register_stamped_lock_natives(registry);
}

pub fn register_stamped_lock_natives(registry: &mut NativeMethodRegistry) {
    let sl = "java/util/concurrent/locks/StampedLock";
    registry.register(sl, "<init>", "()V", native_stamped_init);
    registry.register(sl, "readLock", "()J", native_stamped_read_lock);
    registry.register(sl, "writeLock", "()J", native_stamped_write_lock);
    registry.register(sl, "tryOptimisticRead", "()J", native_stamped_optimistic);
    registry.register(sl, "unlockRead", "(J)V", native_stamped_unlock_read);
    registry.register(sl, "unlockWrite", "(J)V", native_stamped_unlock_write);
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
}

fn native_rwl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, Value::Int(0)); // readers
    ctx.set_field(this, 1, Value::Int(0)); // writer
    ctx.set_field(this, 2, Value::Int(0)); // fair
    Ok(None)
}

fn native_rwl_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let fair = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, 0, Value::Int(0));
    ctx.set_field(this, 1, Value::Int(0));
    ctx.set_field(this, 2, Value::Int(fair));
    Ok(None)
}

fn native_rwl_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let lock = alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock",
        1,
    );
    ctx.set_field(lock, 0, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(lock))))
}

fn native_rwl_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let lock = alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock",
        1,
    );
    ctx.set_field(lock, 0, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(lock))))
}

// Pattern-A fix (bug nb-lib-gckeys §1): key the StampedLock state table by a
// GC-stable identity (see `gc_stable_lock_key`) instead of the raw, moving
// heap address that the lock/unlock pair could disagree on across a GC.
fn stamped_addr(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<usize> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(gc_stable_lock_key(ctx, *o)),
        _ => None,
    }
}

fn stamped_obj(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn stamped_addr_for_obj(ctx: &mut dyn NativeContext, obj: ObjectRef) -> usize {
    gc_stable_lock_key(ctx, obj)
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
        let addr = stamped_addr_for_obj(ctx, obj);
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
    let addr = stamped_addr_for_obj(ctx, obj);
    // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
    // stamped_write_lock's contended wait is a raw parking_lot::Condvar::wait
    // with NO GC-blocking-region bracket — same missing-bracket bug as
    // ReentrantReadWriteLock's rw_write_lock (see that registration's
    // comment for the full rationale and how this was diagnosed).
    ctx.begin_blocking_region();
    let stamp = crate::stamped_lock::stamped_write_lock(addr);
    ctx.end_blocking_region();
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj);
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
    let addr = stamped_addr_for_obj(ctx, obj);
    let stamp = crate::stamped_lock::stamped_try_read_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_try_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj);
    let stamp = crate::stamped_lock::stamped_try_write_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

fn native_stamped_write_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = stamped_addr_for_obj(ctx, parent);
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
    let addr = match stamped_addr(ctx, args) {
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
    let addr = match stamped_addr(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    let valid = crate::stamped_lock::stamped_validate(addr, stamp);
    Ok(Some(Value::Int(i32::from(valid))))
}

fn native_stamped_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj);
        crate::stamped_lock::stamped_unlock_read(addr);
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

fn native_stamped_unlock_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj);
        crate::stamped_lock::stamped_unlock_write(addr);
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

fn native_stamped_unstamped_unlock_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj);
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
        let addr = stamped_addr_for_obj(ctx, obj);
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
    let addr = stamped_addr_for_obj(ctx, obj);
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
    let addr = stamped_addr_for_obj(ctx, obj);
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
    let addr = stamped_addr_for_obj(ctx, obj);
    let converted = crate::stamped_lock::stamped_try_convert_to_write(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

fn native_stamped_try_convert_to_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj);
    let converted = crate::stamped_lock::stamped_try_convert_to_read(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

fn native_stamped_is_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(i32::from(
        crate::stamped_lock::stamped_is_write_locked(addr),
    ))))
}

fn native_stamped_is_read_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args) {
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
    let addr = match stamped_addr(ctx, args) {
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
    registry.set_category(__prev_cat);
}

// --- AtomicIntegerArray ---
fn native_aia_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, len);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    Ok(None)
}

fn native_aia_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))
}

fn native_aia_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Int(val));
    Ok(None)
}

fn native_aia_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let new_val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let old = ctx.get_array_element(arr, idx);
    ctx.set_array_element(arr, idx, Value::Int(new_val));
    Ok(Some(old))
}

fn native_aia_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let expected = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    if cur == expected {
        ctx.set_array_element(arr, idx, Value::Int(update));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

fn native_aia_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Int(cur + 1));
    Ok(Some(Value::Int(cur)))
}

fn native_aia_get_and_dec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Int(cur - 1));
    Ok(Some(Value::Int(cur)))
}

fn native_aia_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let delta = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Int(cur + delta));
    Ok(Some(Value::Int(cur)))
}

fn native_aia_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Int(cur + 1));
    Ok(Some(Value::Int(cur + 1)))
}

fn native_aia_dec_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Int(cur - 1));
    Ok(Some(Value::Int(cur - 1)))
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

// --- AtomicLongArray ---
fn native_ala_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, len);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    Ok(None)
}

fn native_ala_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))
}

fn native_ala_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Long(val));
    Ok(None)
}

fn native_ala_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let new_val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let old = ctx.get_array_element(arr, idx);
    ctx.set_array_element(arr, idx, Value::Long(new_val));
    Ok(Some(old))
}

fn native_ala_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let expected = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Long(v) => v,
        _ => 0,
    };
    if cur == expected {
        ctx.set_array_element(arr, idx, Value::Long(update));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

fn native_ala_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Long(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Long(cur + 1));
    Ok(Some(Value::Long(cur)))
}

fn native_ala_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let cur = match ctx.get_array_element(arr, idx) {
        Value::Long(v) => v,
        _ => 0,
    };
    ctx.set_array_element(arr, idx, Value::Long(cur + 1));
    Ok(Some(Value::Long(cur + 1)))
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
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "add", "(Ljava/lang/Object;)Z", &[elem]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_collection_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "contains", "(Ljava/lang/Object;)Z", &[elem]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_collection_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "remove", "(Ljava/lang/Object;)Z", &[elem]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_collection_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "size", "()I", &[]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_collection_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "isEmpty", "()Z", &[]),
        None => Ok(Some(Value::Int(1))),
    }
}

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
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "toArray", "()[Ljava/lang/Object;", &[]),
        None => {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(empty))))
        }
    }
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
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(
            m,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[key, value],
        ),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_contains_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "containsKey", "(Ljava/lang/Object;)Z", &[key]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_map_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(
            m,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key],
        ),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "size", "()I", &[]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_sync_map_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "isEmpty", "()Z", &[]),
        None => Ok(Some(Value::Int(1))),
    }
}

pub(crate) fn native_sync_map_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "entrySet", "()Ljava/util/Set;", &[]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_key_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "keySet", "()Ljava/util/Set;", &[]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_values(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "values", "()Ljava/util/Collection;", &[]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_sync_map_clear(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(m) = sync_map_backing(ctx, this) {
        ctx.invoke_virtual(m, "clear", "()V", &[])?;
    }
    Ok(None)
}

pub(crate) fn native_sync_map_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapper = match args.get(2) {
        Some(Value::Object(Some(f))) => Some(*f),
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
    ctx.set_field(this, 0, Value::Object(Some(pair)));
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
    ctx.set_field(this, 0, Value::Object(Some(pair)));
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
        ctx.set_field(this, 0, Value::Object(Some(pair)));
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
            ctx.set_field(this, 0, Value::Object(Some(pair)));
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

pub(crate) fn register_pd_structured_concurrency(r: &mut NativeMethodRegistry) {
    let scope = "java/util/concurrent/StructuredTaskScope";

    r.register(scope, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        pd_init_scope(ctx, this);
        Ok(None)
    });
    r.register(
        scope,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            pd_init_scope(ctx, this);
            Ok(None)
        },
    );

    r.register(
        scope,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let callable = args.get(1).copied().unwrap_or(Value::Object(None));
            pd_fork_callable(ctx, this, callable)
        },
    );

    r.register(
        scope,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        scope,
        "joinUntil",
        "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(scope, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(2));
        Ok(None)
    });
    r.register(scope, "shutdown", "()V", |_ctx, _args| Ok(None));
    r.register(scope, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            if matches!(ctx.get_field(this, 1), Value::Int(2)) {
                1
            } else {
                0
            },
        )))
    });

    let subtask = "java/util/concurrent/StructuredTaskScope$Subtask";
    r.register(subtask, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, 3) {
            Value::Int(1) => Ok(Some(ctx.get_field(this, 1))),
            Value::Int(2) => Err(RuntimeError::IllegalStateException {
                message: "Subtask failed".into(),
            }
            .into()),
            _ => Err(RuntimeError::IllegalStateException {
                message: "Subtask result is unavailable".into(),
            }
            .into()),
        }
    });
    r.register(
        subtask,
        "state",
        "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(
        subtask,
        "exception",
        "()Ljava/lang/Throwable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );

    // --- ShutdownOnFailure ---
    let sof = "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure";
    r.register(sof, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        pd_init_scope(ctx, this);
        Ok(None)
    });
    r.register(
        sof,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let c = args.get(1).copied().unwrap_or(Value::Object(None));
            pd_fork_callable(ctx, this, c)
        },
    );
    r.register(
        sof,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sof, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(2));
        Ok(None)
    });
    r.register(sof, "throwIfFailed", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if pd_has_failure(ctx, this) {
            return Err(RuntimeError::IllegalStateException {
                message: "Subtask failed".into(),
            }
            .into());
        }
        Ok(None)
    });
    r.register(
        sof,
        "throwIfFailed",
        "(Ljava/util/function/Function;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if pd_has_failure(ctx, this) {
                return Err(RuntimeError::IllegalStateException {
                    message: "Subtask failed".into(),
                }
                .into());
            }
            Ok(None)
        },
    );

    // --- ShutdownOnSuccess ---
    let sos = "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess";
    r.register(sos, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        pd_init_scope(ctx, this);
        Ok(None)
    });
    r.register(
        sos,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let c = args.get(1).copied().unwrap_or(Value::Object(None));
            pd_fork_callable(ctx, this, c)
        },
    );
    r.register(
        sos,
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sos, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(2));
        Ok(None)
    });
    r.register(sos, "result", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        pd_first_success(ctx, this)
            .map(|v| Ok(Some(v)))
            .unwrap_or_else(|| {
                Err(RuntimeError::IllegalStateException {
                    message: "No successful subtask".into(),
                }
                .into())
            })
    });
    r.register(
        sos,
        "result",
        "(Ljava/util/function/Function;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            pd_first_success(ctx, this)
                .map(|v| Ok(Some(v)))
                .unwrap_or_else(|| {
                    Err(RuntimeError::IllegalStateException {
                        message: "No successful subtask".into(),
                    }
                    .into())
                })
        },
    );
}

// ===========================================================================
// Concurrency primitive tests & Unsafe.setMemory test
// ===========================================================================

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
        let cdl = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1);
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
        let cdl = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1);
        // Init with count=0 means await should return immediately
        native_cdl_init(&mut ctx, &[Value::Object(Some(cdl)), Value::Int(0)]).unwrap();

        // Should not block (count is already 0)
        native_cdl_await(&mut ctx, &[Value::Object(Some(cdl))]).unwrap();
        assert_eq!(cdl_count(&mut ctx, cdl), 0);
    }

    #[test]
    fn cdl_countdown_below_zero_stays_at_zero() {
        let mut ctx = make_ctx();
        let cdl = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CountDownLatch", 1);
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
        let sem = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2);
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

        assert_eq!(sem_permits(&mut ctx, sem), 3);

        native_sem_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 2);
    }

    #[test]
    fn sem_acquire_uninterruptibly_n_decrements_requested_permits() {
        let mut ctx = make_ctx();
        let sem = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2);
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();

        native_sem_acquire_n(&mut ctx, &[Value::Object(Some(sem)), Value::Int(3)]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 0);
    }

    #[test]
    fn sem_release_increments_and_notifies() {
        let mut ctx = make_ctx();
        let sem = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2);
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(1)]).unwrap();

        // Release adds a permit (calls monitor_notify internally)
        native_sem_release(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(sem_permits(&mut ctx, sem), 2);
    }

    #[test]
    fn sem_try_acquire_with_no_permits_returns_zero() {
        let mut ctx = make_ctx();
        let sem = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2);
        native_sem_init(&mut ctx, &[Value::Object(Some(sem)), Value::Int(0)]).unwrap();

        let result = native_sem_try_acquire(&mut ctx, &[Value::Object(Some(sem))]).unwrap();
        assert_eq!(result, Some(Value::Int(0))); // false -- no permits available
        assert_eq!(sem_permits(&mut ctx, sem), 0);
    }

    #[test]
    fn sem_try_acquire_with_permits_succeeds() {
        let mut ctx = make_ctx();
        let sem = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/Semaphore", 2);
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

        let ka1 = gc_stable_lock_key(&mut ctx, a);
        let ka2 = gc_stable_lock_key(&mut ctx, a);
        let kb = gc_stable_lock_key(&mut ctx, b);

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
        assert!(matches!(stamp, Value::Long(v) if v & 1 != 0));
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)]).unwrap();

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
    fn m18_stamped_write_lock_returns_odd_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        match stamp {
            Value::Long(v) => {
                assert!(v & 1 != 0, "write stamp should be odd, got {}", v);
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
        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)]).unwrap();
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
                assert!(a & 1 == 0, "stamp should be even after unlock, got {}", a);
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
        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)]).unwrap();
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
    fn m18_stamped_read_lock_returns_even_stamp() {
        let _guard = stamped_test_lock();
        let mut ctx = make_ctx();
        let sl = ctx.alloc_object(ClassId::new(0), 1);
        native_stamped_init(&mut ctx, &[Value::Object(Some(sl))]).unwrap();

        let stamp = native_stamped_read_lock(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        match stamp {
            Value::Long(v) => assert!(v & 1 == 0, "read stamp should be even, got {}", v),
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

        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let locked = native_stamped_is_write_locked(&mut ctx, &[Value::Object(Some(sl))])
            .unwrap()
            .unwrap();
        assert_eq!(locked, Value::Int(1));

        native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)]).unwrap();
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

        native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
        let read_stamp = native_stamped_try_convert_to_read(
            &mut ctx,
            &[Value::Object(Some(sl)), Value::Long(0)],
        )
        .unwrap()
        .unwrap();
        match read_stamp {
            Value::Long(v) => {
                assert!(v != 0, "conversion to read should succeed");
                assert!(v & 1 == 0, "read stamp should be even after conversion");
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
        let cowal =
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CopyOnWriteArrayList", 2);
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
            native_stamped_write_lock(&mut ctx, &[Value::Object(Some(sl))]).unwrap();
            native_stamped_unlock_write(&mut ctx, &[Value::Object(Some(sl)), Value::Long(0)])
                .unwrap();
            let cur = match native_stamped_optimistic(&mut ctx, &[Value::Object(Some(sl))])
                .unwrap()
                .unwrap()
            {
                Value::Long(v) => v,
                _ => panic!("expected Long"),
            };
            assert!(cur > prev_stamp, "stamp should monotonically increase");
            assert!(cur & 1 == 0, "stamp should be even after unlock");
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
        let obj = alloc_concurrent_synthetic(&mut ctx, ai, 1);

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
        let obj = alloc_concurrent_synthetic(&mut ctx, al, 1);

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
        let obj = alloc_concurrent_synthetic(&mut ctx, ar, 1);
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
        let obj = alloc_concurrent_synthetic(&mut ctx, ar, 1);

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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
            alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/locks/ReentrantLock", 3);
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
        let cf = alloc_concurrent_synthetic(&mut ctx, "java/util/concurrent/CompletableFuture", 4);
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
}
