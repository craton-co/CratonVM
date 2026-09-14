// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! LETSGO_S2 — broader real-JDK / synthetic-stub compatibility layer.
//!
//! This module registers Rust-side native fallbacks for JDK methods that
//! commonly fail with `NoSuchMethodError` during real-JDK boot of medium
//! Spring/Logback/SLF4J apps when CratonVM's class store has gaps in the
//! resolved bytecode (nested fat-jars, partial JDK images, synthetic-stub
//! fallbacks for inner classes, etc).
//!
//! Add new compat fallbacks here rather than deeper in the class-specific
//! files so the broader compatibility surface stays auditable.

use cratonvm_native_api::NativeMethodRegistry;
// `NativeContext`, `Value`, and `MethodCallResult` are now used only by the
// `synthetic-jdk`-gated `drainTo` helpers below; gate the imports so non-synthetic
// builds don't warn on unused imports.
#[cfg(feature = "synthetic-jdk")]
use cratonvm_native_api::NativeContext;
#[cfg(feature = "synthetic-jdk")]
use cratonvm_types::error::MethodCallResult;
#[cfg(feature = "synthetic-jdk")]
use cratonvm_types::Value;
// synthetic-stub removed: `ArrayElementType`, `alloc_concurrent_synthetic`, and
// the `alloc_letsgo_wrapper` helper were only used by the deleted wrapper-boxing
// and hardcoded-`Security.getAlgorithms` stubs.

pub fn register_letsgo_compat_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // synthetic-stub removed: `register_wrapper_value_of` — java.lang wrapper
    // `valueOf(prim)` has BOTH real JDK bytecode AND real natives
    // (`lang_math::native_{integer,long,boolean}_value_of` + the wrapper
    // intrinsics in `intrinsics/{integer,long}.rs`). The letsgo duplicate was
    // redundant; the real cache-preserving path now wins.
    // synthetic-stub removed: `register_wrapper_unbox` — wrapper `primValue()`
    // unboxers have real JDK bytecode + real natives; letsgo duplicate removed.
    // synthetic-stub removed: `register_security_fallbacks` — the hardcoded
    // algorithm lists + no-op AccessController.checkPermission were fakes.
    // W4-3 correction: the claim that used to follow — that
    // `Security.getAlgorithms` was "covered by the identical essential-path
    // native in `phases_early::register_real_jdk_forkjoin_essentials`" — was
    // FALSE. No such registration exists. The only `getAlgorithms` native lived
    // in `phases_early::register_phase53_security`, which only
    // `register_synthetic_overrides` reaches, so real-JDK and `--jdk-only` ran
    // the JDK's own bytecode over empty synthetic Providers and answered the
    // EMPTY SET for every engine type. It is now registered from
    // `jca::provider_chain::register` (live in every mode) and answers from the
    // provider service registry.
    // `drainTo` synthetic overrides assume slot 0=array, 1=size — that's the
    // synthetic-jdk LBQ/ABQ layout.  On real JDK 25 LBQ those slots are
    // head/last (Node refs), so the synthetic native silently misreads.  More
    // importantly, registering this override is paired with synthetic LBQ
    // <init> overrides elsewhere (m18, native-collections) that leave real
    // putLock/takeLock null.  Gate so real-JDK bytecode (which works) runs.
    #[cfg(feature = "synthetic-jdk")]
    register_blocking_queue_drain_to(registry);
    // synthetic-stub removed: `register_atomic_compat` — all four
    // `java.util.concurrent.atomic.*` classes have BOTH real JDK bytecode AND
    // real natives (`register_atomic_{boolean,integer,long,reference}_natives`
    // in lib.rs, wired into the real-JDK path at lib.rs:9368-9371). The letsgo
    // layout-safe duplicates were redundant; the real natives now win.
    registry.set_category(__prev_cat);
}

// synthetic-stub removed: register_atomic_compat + register_atomic_{boolean,integer,
// long,reference}_compat deleted. All four java.util.concurrent.atomic.* classes
// have real JDK bytecode AND real natives (register_atomic_*_natives in lib.rs,
// wired into the real-JDK path at lib.rs:9368-9371), so these layout-safe
// duplicates were redundant.

/// `Wrapper.primValue()` unboxers. Kept as a no-op (`pub`, still called from
/// `register_essential_natives` in lib.rs) — but no longer registers anything.
///
/// synthetic-stub removed: every java.lang wrapper `booleanValue/intValue/...`
/// has BOTH real JDK bytecode AND a real native (the wrapper intrinsics in
/// `intrinsics/{integer,long}.rs` + `lang_*` unbox paths), so these layout-safe
/// duplicates were redundant. Letting the real path win avoids the
/// `field 0`-only assumption breaking the JDK-25 wrapper layout.
pub fn register_wrapper_unbox(_r: &mut NativeMethodRegistry) {
    // intentionally empty — real bytecode/natives cover wrapper unboxing.
}

// synthetic-stub removed: unbox_*_field + box_* + boxed_int_field helpers
// deleted along with their register fns. Wrapper boxing/unboxing is covered by
// real bytecode + lang_math/intrinsics natives.

/// `Wrapper.valueOf(prim)`. Kept as a no-op (`pub`, still called from
/// `register_essential_natives` in lib.rs) — but no longer registers anything.
///
/// synthetic-stub removed: every java.lang wrapper `valueOf(prim)` has BOTH
/// real JDK bytecode AND real natives. The canonical cache-preserving
/// implementations live in `lang_math::native_{integer,long,boolean}_value_of`
/// (plus the wrapper intrinsics in `intrinsics/{integer,long}.rs`). Those
/// already honour `IntegerCache`/`LongCache`/`Boolean.TRUE` identity, so the
/// letsgo duplicates were redundant — letting the real path win.
pub fn register_wrapper_value_of(_r: &mut NativeMethodRegistry) {
    // intentionally empty — real bytecode/natives cover wrapper boxing.
}

/// Security fallbacks. Kept as a no-op (`pub`, still called from
/// `register_essential_natives` in lib.rs) — but no longer registers anything.
///
/// synthetic-stub removed: the no-op `AccessController.checkPermission` and the
/// hardcoded `Security.getAlgorithms` algorithm lists were fakes. Real crypto is
/// wired elsewhere (jca/* + phases_early JCA block).
///
/// W4-3 correction: this comment used to say `getAlgorithms` was "covered by the
/// identical essential-path native in
/// `phases_early::register_real_jdk_forkjoin_essentials`", and that `vm_exec`'s
/// forced-native dispatch targeted it. Neither is true — there is no
/// `getAlgorithms` registration in that function, and no `java/security/*`
/// triple in `force_native_over_real_jdk_bytecode`. The live registration is now
/// `jca::provider_chain::register`, reached from
/// `register_essential_natives_with_shims` in every mode.
pub fn register_security_fallbacks(_r: &mut NativeMethodRegistry) {
    // intentionally empty — `jca::provider_chain` owns the live
    // `java.security.Security` surface, including `getAlgorithms`.
}

/// `BlockingQueue.drainTo(Collection, int)` overload — required by SLF4J's
/// `LoggerFactory.replayEvents` and by Spring's `TaskExecutor` pools.
#[cfg(feature = "synthetic-jdk")]
fn register_blocking_queue_drain_to(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let lbq = "java/util/concurrent/LinkedBlockingQueue";
    r.register(
        lbq,
        "drainTo",
        "(Ljava/util/Collection;I)I",
        drain_to_lbq_bounded,
    );

    let abq = "java/util/concurrent/ArrayBlockingQueue";
    r.register(
        abq,
        "drainTo",
        "(Ljava/util/Collection;I)I",
        drain_to_abq_bounded,
    );

    let bq = "java/util/concurrent/BlockingQueue";
    r.register(
        bq,
        "drainTo",
        "(Ljava/util/Collection;)I",
        drain_to_iface_unbounded,
    );
    r.register(
        bq,
        "drainTo",
        "(Ljava/util/Collection;I)I",
        drain_to_iface_bounded,
    );
    r.set_category(__prev_cat);
}

#[cfg(feature = "synthetic-jdk")]
fn drain_to_lbq_bounded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let max = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => i32::MAX,
    };
    if max <= 0 {
        return Ok(Some(Value::Int(0)));
    }
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
    let n = size.min(max);
    // PINNED ACROSS THE LOOP, and re-read INSIDE it.
    // `invoke_virtual(coll, "add", ..)` runs Java on every iteration, so from
    // the second iteration on `coll` and `arr` are pre-GC addresses -- and
    // `this` is used after the loop as well. The re-reads shadow within the
    // body, so the outer bindings are untouched. No unpin: this is a native
    // entry point and `safe_native_call` truncates the pin stack to its entry
    // floor on return.
    let this_pin = ctx.pin_native_root(this);
    let coll_pin = ctx.pin_native_root(coll);
    let arr_pin = ctx.pin_native_root(arr);
    for i in 0..n as usize {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let coll = ctx.read_native_pin(coll_pin, coll);
        let elem = ctx.get_array_element(arr, i);
        ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    }
    let this = ctx.read_native_pin(this_pin, this);
    let arr = ctx.read_native_pin(arr_pin, arr);
    if n < size {
        for i in 0..(size - n) as usize {
            let elem = ctx.get_array_element(arr, n as usize + i);
            ctx.set_array_element(arr, i, elem);
        }
    }
    ctx.set_field(this, 1, Value::Int(size - n));
    ctx.monitor_notify_all(this)?;
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(n)))
}

#[cfg(feature = "synthetic-jdk")]
fn drain_to_abq_bounded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let max = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => i32::MAX,
    };
    if max <= 0 {
        return Ok(Some(Value::Int(0)));
    }
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
    let n = size.min(max);
    // PINNED ACROSS THE LOOP, and re-read INSIDE it.
    // `invoke_virtual(coll, "add", ..)` runs Java on every iteration, so from
    // the second iteration on `coll` and `arr` are pre-GC addresses -- and
    // `this` is used after the loop as well. The re-reads shadow within the
    // body, so the outer bindings are untouched. No unpin: this is a native
    // entry point and `safe_native_call` truncates the pin stack to its entry
    // floor on return.
    let this_pin = ctx.pin_native_root(this);
    let coll_pin = ctx.pin_native_root(coll);
    let arr_pin = ctx.pin_native_root(arr);
    for i in 0..n {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let coll = ctx.read_native_pin(coll_pin, coll);
        let idx = ((head + i) % cap.max(1)) as usize;
        let elem = ctx.get_array_element(arr, idx);
        ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    }
    let this = ctx.read_native_pin(this_pin, this);
    let _ = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(this, 1, Value::Int(size - n));
    if cap > 0 {
        ctx.set_field(this, 2, Value::Int((head + n) % cap));
    }
    ctx.monitor_notify_all(this)?;
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(n)))
}

#[cfg(feature = "synthetic-jdk")]
fn drain_to_iface_unbounded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => Value::Object(Some(*c)),
        _ => return Ok(Some(Value::Int(0))),
    };
    let r = ctx.invoke_virtual(this, "drainTo", "(Ljava/util/Collection;)I", &[coll])?;
    Ok(r.or(Some(Value::Int(0))))
}

#[cfg(feature = "synthetic-jdk")]
fn drain_to_iface_bounded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => Value::Object(Some(*c)),
        _ => return Ok(Some(Value::Int(0))),
    };
    let max = match args.get(2) {
        Some(Value::Int(n)) => Value::Int(*n),
        _ => Value::Int(i32::MAX),
    };
    let r = ctx.invoke_virtual(this, "drainTo", "(Ljava/util/Collection;I)I", &[coll, max])?;
    Ok(r.or(Some(Value::Int(0))))
}
