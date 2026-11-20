#[allow(unused_imports)]
use super::*;


pub fn register_queue_bridge_natives(registry: &mut NativeMethodRegistry) {
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
            native_netty_mpsc_offer, NativeKind::SyntheticStub,
        );
        registry.register(
            class_name,
            "relaxedOffer",
            "(Ljava/lang/Object;)Z",
            native_netty_mpsc_offer, NativeKind::SyntheticStub,
        );
        registry.register(
            class_name,
            "poll",
            "()Ljava/lang/Object;",
            native_netty_mpsc_poll, NativeKind::SyntheticStub,
        );
        registry.register(
            class_name,
            "relaxedPoll",
            "()Ljava/lang/Object;",
            native_netty_mpsc_poll, NativeKind::SyntheticStub,
        );
        registry.register(
            class_name,
            "peek",
            "()Ljava/lang/Object;",
            native_netty_mpsc_peek, NativeKind::SyntheticStub,
        );
        registry.register(
            class_name,
            "relaxedPeek",
            "()Ljava/lang/Object;",
            native_netty_mpsc_peek, NativeKind::SyntheticStub,
        );
        registry.register(class_name, "size", "()I", native_netty_mpsc_size, NativeKind::SyntheticStub);
        registry.register(class_name, "isEmpty", "()Z", native_netty_mpsc_is_empty, NativeKind::SyntheticStub);
        registry.register(class_name, "clear", "()V", native_netty_mpsc_clear, NativeKind::SyntheticStub);
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

pub fn register_atomic_integer_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicInteger get/set/CAS are VM-internal atomic primitives
    // (Unsafe-equivalent) with spec-exact semantics → Bridge.
            let c = "java/util/concurrent/atomic/AtomicInteger";
    r.register(c, "<init>", "()V", native_atomic_int_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(I)V", native_atomic_int_init_value, cratonvm_native_api::NativeKind::Bridge);
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
    r.register(c, "get", "()I", native_atomic_int_get, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "set", "(I)V", native_atomic_int_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "lazySet", "(I)V", native_atomic_int_set, cratonvm_native_api::NativeKind::Bridge);
    r.set_leaf(false);
    r.register(c, "getAndSet", "(I)I", native_atomic_int_get_and_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "compareAndSet", "(II)Z", native_atomic_int_cas, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "weakCompareAndSet", "(II)Z", native_atomic_int_cas, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "getAndIncrement",
        "()I",
        native_atomic_int_get_and_increment, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "getAndDecrement",
        "()I",
        native_atomic_int_get_and_decrement, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "getAndAdd", "(I)I", native_atomic_int_get_and_add, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "incrementAndGet",
        "()I",
        native_atomic_int_increment_and_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "decrementAndGet",
        "()I",
        native_atomic_int_decrement_and_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "addAndGet", "(I)I", native_atomic_int_add_and_get, cratonvm_native_api::NativeKind::Bridge);
    // LEAF, same argument as `get`/`set` above: both are plain reads of field 0.
    r.set_leaf(true);
    r.register(c, "intValue", "()I", native_atomic_int_get, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "longValue", "()J", native_atomic_int_long_value, cratonvm_native_api::NativeKind::Bridge);
    r.set_leaf(false);
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_atomic_int_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_atomic_int_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Int(0));
    }
    Ok(None)
}

pub fn native_atomic_int_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let val = args.get(1).copied().unwrap_or(Value::Int(0));
        ctx.set_field_volatile(*this, 0, val);
    }
    Ok(None)
}

pub fn native_atomic_int_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_volatile(this, 0)))
}

pub fn native_atomic_int_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = args.get(1).copied().unwrap_or(Value::Int(0));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

pub fn native_atomic_int_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_val = args.get(1).copied().unwrap_or(Value::Int(0));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

pub fn native_atomic_int_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub fn native_atomic_int_get_and_increment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, 1)?;
    Ok(Some(Value::Int(old)))
}

pub fn native_atomic_int_get_and_decrement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, -1)?;
    Ok(Some(Value::Int(old)))
}

pub fn native_atomic_int_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delta = match args.get(1) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_int(this, 0, delta)?;
    Ok(Some(Value::Int(old)))
}

pub fn native_atomic_int_increment_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, 1)?;
    Ok(Some(Value::Int(old.wrapping_add(1))))
}

pub fn native_atomic_int_decrement_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let old = ctx.atomic_fetch_add_int(this, 0, -1)?;
    Ok(Some(Value::Int(old.wrapping_sub(1))))
}

pub fn native_atomic_int_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let delta = match args.get(1) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_int(this, 0, delta)?;
    Ok(Some(Value::Int(old.wrapping_add(delta))))
}

pub fn native_atomic_int_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = ctx.get_field_volatile(this, 0);
    match val {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub fn native_atomic_int_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub fn register_atomic_long_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicLong atomic primitives (Unsafe-equivalent) → Bridge.
            let c = "java/util/concurrent/atomic/AtomicLong";
    // VMSupportsCS8 is consulted by AtomicLong.<clinit> to choose between
    // a lock-free 64-bit CAS implementation and a synchronized fallback.
    // See `native_atomic_long_vm_supports_cs8` below for the cfg rules.
    r.register(
        c,
        "VMSupportsCS8",
        "()Z",
        native_atomic_long_vm_supports_cs8, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "<init>", "()V", native_atomic_long_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(J)V", native_atomic_long_init_value, cratonvm_native_api::NativeKind::Bridge);
    // LEAF, for the same reason as the `AtomicInteger` accessors above: a
    // descriptor-typed volatile read/write of field 0, and nothing else. The
    // CAS / fetch-add members stay on the funnel because they take the CAS lock.
    r.set_leaf(true);
    r.register(c, "get", "()J", native_atomic_long_get, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "set", "(J)V", native_atomic_long_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "lazySet", "(J)V", native_atomic_long_set, cratonvm_native_api::NativeKind::Bridge);
    r.set_leaf(false);
    r.register(c, "getAndSet", "(J)J", native_atomic_long_get_and_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "compareAndSet", "(JJ)Z", native_atomic_long_cas, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "weakCompareAndSet", "(JJ)Z", native_atomic_long_cas, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "getAndIncrement",
        "()J",
        native_atomic_long_get_and_increment, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "getAndDecrement",
        "()J",
        native_atomic_long_get_and_decrement, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "getAndAdd", "(J)J", native_atomic_long_get_and_add, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "incrementAndGet",
        "()J",
        native_atomic_long_increment_and_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "decrementAndGet",
        "()J",
        native_atomic_long_decrement_and_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "addAndGet", "(J)J", native_atomic_long_add_and_get, cratonvm_native_api::NativeKind::Bridge);
    r.set_leaf(true);
    r.register(c, "intValue", "()I", native_atomic_long_int_value, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "longValue", "()J", native_atomic_long_get, cratonvm_native_api::NativeKind::Bridge);
    r.set_leaf(false);
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
pub fn native_atomic_long_vm_supports_cs8(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let supports = cfg!(target_pointer_width = "64") || cfg!(target_feature = "cmpxchg16b");
    Ok(Some(Value::Int(if supports { 1 } else { 0 })))
}

pub fn native_atomic_long_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Long(0));
    }
    Ok(None)
}

pub fn native_atomic_long_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub fn atomic_receiver(
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
pub fn report_lost_atomic_receiver(
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
        for entry in ctx.capture_stack_trace().iter().rev() {
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

pub fn native_atomic_long_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(ctx, args, "java.util.concurrent.atomic.AtomicLong.get()")?;
    Ok(Some(ctx.get_field_volatile(this, 0)))
}

pub fn native_atomic_long_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.set(long)",
    )?;
    let val = args.get(1).copied().unwrap_or(Value::Long(0));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

pub fn native_atomic_long_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.getAndSet(long)",
    )?;
    let new_val = args.get(1).copied().unwrap_or(Value::Long(0));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

pub fn native_atomic_long_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.compareAndSet(long, long)",
    )?;
    let expected = args.get(1).copied().unwrap_or(Value::Long(0));
    let new_val = args.get(2).copied().unwrap_or(Value::Long(0));
    let result = ctx.compare_and_swap_field(this, 0, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

// audit-round5 fix #9 (HIGH): AtomicLong analogue of the AtomicInteger
// fetch_add migration — single trait dispatch per call, VM override can
// emit a single `LOCK XADDQ`.
pub fn native_atomic_long_get_and_increment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.getAndIncrement()",
    )?;
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old)))
}

pub fn native_atomic_long_get_and_decrement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.getAndDecrement()",
    )?;
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old)))
}

pub fn native_atomic_long_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.getAndAdd(long)",
    )?;
    let delta = match args.get(1) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_long(this, 0, delta)?;
    Ok(Some(Value::Long(old)))
}

pub fn native_atomic_long_increment_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.incrementAndGet()",
    )?;
    let old = ctx.atomic_fetch_add_long(this, 0, 1)?;
    Ok(Some(Value::Long(old.wrapping_add(1))))
}

pub fn native_atomic_long_decrement_and_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.decrementAndGet()",
    )?;
    let old = ctx.atomic_fetch_add_long(this, 0, -1)?;
    Ok(Some(Value::Long(old.wrapping_sub(1))))
}

pub fn native_atomic_long_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.addAndGet(long)",
    )?;
    let delta = match args.get(1) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let old = ctx.atomic_fetch_add_long(this, 0, delta)?;
    Ok(Some(Value::Long(old.wrapping_add(delta))))
}

pub fn native_atomic_long_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicLong.intValue()",
    )?;
    let val = ctx.get_field_volatile(this, 0);
    match val {
        Value::Long(v) => Ok(Some(Value::Int(v as i32))),
        _ => Ok(Some(Value::Int(0))),
    }
}

// ===========================================================================
// java.util.concurrent.atomic.AtomicReference
// ===========================================================================

pub fn register_atomic_reference_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicReference atomic primitives (Unsafe-equivalent) → Bridge.
            let c = "java/util/concurrent/atomic/AtomicReference";
    r.register(c, "<init>", "()V", native_atomic_ref_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "<init>",
        "(Ljava/lang/Object;)V",
        native_atomic_ref_init_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "get", "()Ljava/lang/Object;", native_atomic_ref_get, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "set", "(Ljava/lang/Object;)V", native_atomic_ref_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "lazySet", "(Ljava/lang/Object;)V", native_atomic_ref_set, cratonvm_native_api::NativeKind::Bridge);
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
        native_atomic_ref_get_and_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_atomic_ref_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_atomic_ref_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field_volatile(*this, 0, Value::Object(None));
    }
    Ok(None)
}

pub fn native_atomic_ref_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field_volatile(*this, 0, val);
    }
    Ok(None)
}

pub fn native_atomic_ref_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicReference.get()",
    )?;
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

pub fn native_atomic_ref_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicReference.set(Object)",
    )?;
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field_volatile(this, 0, val);
    Ok(None)
}

pub fn native_atomic_ref_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicReference.getAndSet(Object)",
    )?;
    let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
    loop {
        let current = ctx.get_field_volatile(this, 0);
        if ctx.compare_and_swap_field(this, 0, current, new_val) {
            return Ok(Some(current));
        }
    }
}

pub fn native_atomic_ref_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = atomic_receiver(
        ctx,
        args,
        "java.util.concurrent.atomic.AtomicReference.toString()",
    )?;
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

pub fn register_lock_support_natives(r: &mut NativeMethodRegistry) {
    // census-tag: Bridge, but NOT because these are `ACC_NATIVE` (in JDK 25
    // none of them is: each is a short body over `Unsafe.park` / `unpark` and
    // the `parkBlocker` field). Every row but `parkUntil(J)V` (never
    // registered) is a retired shadow, so `--real-jdk` refuses them and runs
    // the bodies; `--compatible` over a real JDK does too since interpreter
    // round i2 wave 69, lane L1 (the `JdkLockSupport` HotSpot-alignment row,
    // `CRATONVM_COMPAT_JDK_LOCK_SUPPORT`, default on: `register` drops them).
    // What is left for these stand-ins is the synthetic class library and that
    // row's `=0` arm.
            let c = "java/util/concurrent/locks/LockSupport";
    r.register(c, "park", "(Ljava/lang/Object;)V", native_lock_support_park, cratonvm_native_api::NativeKind::Bridge);
    // Also register no-arg versions (Java public API uses no-arg park())
    r.register(c, "park", "()V", native_lock_support_park, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "parkNanos",
        "(Ljava/lang/Object;J)V",
        native_lock_support_park_nanos, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "parkNanos", "(J)V", |ctx, args| {
        // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
        // tag-loss for category-2 longs (CompactValue::long stores raw i64 untagged,
        // and `to_value()` decodes untagged bits as Double). Recover via bit-reinterpret.
        let nanos: i64 = match args.first() {
            Some(Value::Long(n)) => *n,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(n)) => *n as i64,
            _ => 0,
        };
        // gci i27/vt: JDK 25's `if (nanos > 0)` comes before `U.park`, so a
        // non-positive time keeps the permit even when interrupted.
        match lock_support_relative_entry(nanos, ctx.is_interrupted(false)) {
            LockSupportEntry::NoPark => return Ok(None),
            LockSupportEntry::AtOnce => {
                lock_support_park_at_once(ctx, None, None, nanos);
                return Ok(None);
            }
            LockSupportEntry::Park => {}
        }
        if nanos > 0 {
            let timeout = Some(std::time::Duration::from_nanos(nanos as u64));
            if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
                return yielded;
            }
            ctx.park_reporting_timeout(timeout);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "parkUntil",
        "(Ljava/lang/Object;J)V",
        native_lock_support_park_until, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "unpark",
        "(Ljava/lang/Thread;)V",
        native_lock_support_unpark, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "getBlocker",
        "(Ljava/lang/Thread;)Ljava/lang/Object;",
        native_lock_support_get_blocker, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_lock_support_park(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker]
    // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
    if ctx.is_interrupted(false) {
        // gci i23/pk: `Parker::park` takes the permit before its interrupt
        // test; the VM's Java-level park (`timed`) does that on its
        // interrupted return, and posts the `jdk.ThreadPark` HotSpot posts
        // (gci i27/vt: as `U.park(false, 0L)`'s -- untimed, `timeout =
        // Long.MIN_VALUE` -- with the blocker set, as the JDK body sets it).
        lock_support_park_at_once(ctx, args.first(), None, i64::MIN);
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
    // which path is live. Mirrors `monitor_wait_keepalive_and_exit_as`.
    let pin = match args.first() {
        Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    // Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
    // the JDK body's `setBlocker(t, blocker)` / `setBlocker(t, null)` around
    // the park, so JMX names the blocker (`ThreadInfo.getLockName()`).
    let blocker_set = lock_support_set_blocker(ctx, args.first());
    // JIT round 21 lane refs (REFS21-6): `park` with the Java-level marker;
    // identical otherwise (WAITING, no park event).
    ctx.park_java_untimed(None);
    lock_support_clear_blocker(ctx, blocker_set);
    if let Some(pin) = pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(None)
}

/// `LockSupport.park*(Object blocker, ..)`'s `setBlocker(t, blocker)`
/// (interpreter round i2 wave 68, lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`,
/// default on;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// JDK 25's three blocker overloads are
/// `setBlocker(t, blocker); try { U.park(..); } finally { setBlocker(t, null); }`,
/// and HotSpot's `ThreadInfo.getLockName()` for a parked thread is that
/// `parkBlocker` (the `Semaphore$NonfairSync`, `ConditionObject` or
/// `StampedLock` an AQS acquire, `awaitNanos` or `StampedLock` acquire hands
/// it). These bridges parked without it, so under `--compatible` every such
/// wait named no lock (`i65-orch-compatible-served-waits-name-no-jmx-lock-20261029.md`).
/// Stores `blocker` in the current thread's `parkBlocker` when the switch is
/// on, a blocker was given and the thread has a mirror; `true` when it did,
/// for [`lock_support_clear_blocker`]. `blocker` is pinned by the caller
/// across the park; the field itself is a reference the mirror keeps.
pub fn lock_support_set_blocker(ctx: &mut dyn NativeContext, blocker: Option<&Value>) -> bool {
    let Some(Value::Object(Some(blocker))) = blocker else {
        return false;
    };
    if !ctx.stand_in_wait_objects() {
        return false;
    }
    let Some(thread) = ctx.current_thread_object_if_any() else {
        return false;
    };
    crate::lang_misc::set_thread_field(ctx, thread, "parkBlocker", Value::Object(Some(*blocker)));
    true
}

/// The `finally { setBlocker(t, null); }` of [`lock_support_set_blocker`]:
/// re-reads the mirror (the park may have spanned a moving collection).
pub fn lock_support_clear_blocker(ctx: &mut dyn NativeContext, was_set: bool) {
    if !was_set {
        return;
    }
    if let Some(thread) = ctx.current_thread_object_if_any() {
        crate::lang_misc::set_thread_field(ctx, thread, "parkBlocker", Value::Object(None));
    }
}

pub fn native_lock_support_park_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker, nanos(long)]
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs. Recover via bit-reinterpret.
    let nanos: i64 = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    };
    // JDK spec: if interrupted, park returns immediately (no exception, flag
    // NOT cleared). gci i23/pk: `Parker::park` takes the permit before its
    // interrupt test. gci i27/vt: but JDK 25's `if (nanos > 0)` comes before
    // `U.park`, so a non-positive time parks not at all (permit kept, no
    // `jdk.ThreadPark`), interrupted or not.
    match lock_support_relative_entry(nanos, ctx.is_interrupted(false)) {
        LockSupportEntry::NoPark => return Ok(None),
        LockSupportEntry::AtOnce => {
            lock_support_park_at_once(ctx, args.first(), None, nanos);
            return Ok(None);
        }
        LockSupportEntry::Park => {}
    }
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
        // Wave 68 (lane L1): the JDK body's `setBlocker`; see
        // [`lock_support_set_blocker`].
        let blocker_set = lock_support_set_blocker(ctx, args.first());
        ctx.park_reporting_timeout(timeout);
        lock_support_clear_blocker(ctx, blocker_set);
        if let Some(pin) = pin {
            ctx.unpin_native_roots(pin);
        }
    }
    Ok(None)
}

pub fn native_lock_support_park_until(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [blocker, deadline(long)] — deadline in millis since epoch
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs. Recover via bit-reinterpret.
    let deadline: i64 = match args.get(1) {
        Some(Value::Long(d)) => *d,
        Some(Value::Double(x)) => x.to_bits() as i64,
        Some(Value::Int(x)) => *x as i64,
        _ => 0,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let remaining = deadline.saturating_sub(now_ms).max(0) as u64;
    // JDK spec: if interrupted, park returns immediately (no exception, flag
    // NOT cleared). gci i23/pk: `Parker::park` takes the permit before its
    // interrupt test. gci i27/vt: JDK 25's `parkUntil` calls `U.park(true,
    // deadline)` with no guard, so a non-positive or elapsed deadline takes
    // the permit and posts the event too (the stand-in returned with neither
    // for `deadline <= 0`); the event is the absolute one (`until`).
    match lock_support_absolute_entry(deadline, remaining, ctx.is_interrupted(false)) {
        LockSupportEntry::Park => {}
        LockSupportEntry::NoPark | LockSupportEntry::AtOnce => {
            let until = crate::unsafe_natives_ext::unsafe_park_absolute_until_field(deadline);
            lock_support_park_at_once(ctx, args.first(), Some(until), 0);
            return Ok(None);
        }
    }
    {
        let timeout = Some(std::time::Duration::from_millis(remaining));
        // gci i22/rv (finding 3 of
        // `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): a deadline
        // already passed never yields -- `try_yield_virtual_park` reads
        // `Some(ZERO)` as the UNTIMED yield; the park below returns at once.
        if remaining > 0 {
            if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
                return yielded;
            }
        }
        // AQS-PARK-PIN — see `native_lock_support_park`.
        let pin = match args.first() {
            Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
            _ => None,
        };
        // Wave 68 (lane L1): the JDK body's `setBlocker`; see
        // [`lock_support_set_blocker`].
        let blocker_set = lock_support_set_blocker(ctx, args.first());
        // gci i27/vt: the absolute park's event (`until = deadline`,
        // `timeout = Long.MIN_VALUE`), as `U.park(true, deadline)` posts it.
        ctx.park_until_reporting(timeout, deadline);
        lock_support_clear_blocker(ctx, blocker_set);
        if let Some(pin) = pin {
            ctx.unpin_native_roots(pin);
        }
    }
    Ok(None)
}

/// gci i27/vt (item B of
/// `gci-i23pk-park-permit-residuals-windows-and-stand-ins-FIXED-20261017`): what a
/// `LockSupport` stand-in does at entry, in the order JDK 25's body and
/// HotSpot 25's `Parker::park` (os_posix.cpp: `_counter` taken first, then
/// the interrupt test, then the time decode) give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockSupportEntry {
    /// No `U.park` at all (`parkNanos`'s `if (nanos > 0)` is false): the
    /// permit is kept and no `jdk.ThreadPark` is posted, interrupted or not.
    NoPark,
    /// `U.park` returns at once: the permit is taken, the event posted.
    AtOnce,
    /// A park that may block.
    Park,
}

/// `parkNanos(blocker, nanos)` / `parkNanos(nanos)`.
pub fn lock_support_relative_entry(nanos: i64, interrupted: bool) -> LockSupportEntry {
    if nanos <= 0 {
        LockSupportEntry::NoPark
    } else if interrupted {
        LockSupportEntry::AtOnce
    } else {
        LockSupportEntry::Park
    }
}

/// `parkUntil(blocker, deadline)`: always a `U.park(true, deadline)`; it
/// returns at once when interrupted, for `deadline <= 0` (`time < 0 ||
/// time == 0` arms) and for an elapsed deadline (`remaining_ms == 0`).
pub fn lock_support_absolute_entry(
    deadline_ms: i64,
    remaining_ms: u64,
    interrupted: bool,
) -> LockSupportEntry {
    if interrupted || deadline_ms <= 0 || remaining_ms == 0 {
        LockSupportEntry::AtOnce
    } else {
        LockSupportEntry::Park
    }
}

/// The `U.park` that returns at once ([`LockSupportEntry::AtOnce`]): the
/// VM's Java-level park with `Some(ZERO)` takes the permit (also on its
/// interrupted return, gci i23/pk) and posts the `jdk.ThreadPark` with the
/// caller's fields -- `absolute_until` for `parkUntil`, else
/// `relative_raw_ns` (`Long.MIN_VALUE` for the untimed `park`) -- with the
/// blocker set around it as the JDK body's `setBlocker` does, so the event's
/// `parkedClass` is HotSpot's.
pub fn lock_support_park_at_once(
    ctx: &mut dyn NativeContext,
    blocker: Option<&Value>,
    absolute_until: Option<i64>,
    relative_raw_ns: i64,
) {
    let pin = match blocker {
        Some(Value::Object(Some(o))) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    let blocker_set = lock_support_set_blocker(ctx, blocker);
    let zero = Some(std::time::Duration::ZERO);
    match absolute_until {
        Some(until) => ctx.park_until_reporting(zero, until),
        None => ctx.park_relative_reporting_raw(zero, relative_raw_ns),
    }
    lock_support_clear_blocker(ctx, blocker_set);
    if let Some(pin) = pin {
        ctx.unpin_native_roots(pin);
    }
}

pub fn native_lock_support_unpark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [thread_obj]
    if let Some(Value::Object(Some(thread_obj))) = args.first() {
        ctx.unpark(*thread_obj);
    } else {
    }
    Ok(None)
}

pub fn native_lock_support_get_blocker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
    // the JDK body, `if (t == null) throw new NullPointerException(); return
    // U.getReferenceOpaque(t, PARKBLOCKER);`, now that the park bridges set
    // the field. `=0`: null, as before.
    if !ctx.stand_in_wait_objects() {
        return Ok(Some(Value::Object(None)));
    }
    let Some(Value::Object(Some(thread))) = args.first() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    Ok(Some(crate::lang_misc::thread_field(
        ctx,
        *thread,
        "parkBlocker",
    )))
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
pub fn report_uninstantiable_receiver(
    ctx: &dyn NativeContext,
    class_name: &str,
    class_id: cratonvm_types::ClassId,
) {
    let flags = ctx.class_access_flags(class_id);
    let _ = cratonvm_native_api::instantiable::observe_uninstantiable_receiver(class_name, flags);
}

#[track_caller]
pub fn report_layout_alias(class_name: &str, num_fields: usize, real: usize) {
    // `observe_from_rust` is `#[track_caller]` and so is this function, so the
    // location that reaches the census is the NATIVE that asked for the shape,
    // not this forwarding line and not `alloc_concurrent_synthetic` in between.
    let _ = cratonvm_native_api::layout_alias::observe_from_rust(class_name, num_fields, real);
}

// The infallible `alloc_concurrent_synthetic` twin is DELETED (Real-JDK wave 2,
// step 3, 2026-08-10). Its 1,904 call sites moved to the fallible spelling on
// 2026-08-07 (attempt 5), leaving one straggler in `lookup_define.rs`; with that
// migrated it survived only as a way back to `ensure_synthetic_class`, which is
// the entry point step 3 removes.

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
pub fn build_rooted_ref_array<F>(
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

/// Allocate a synthetic object through the workspace's busiest fabrication
/// funnel, with the refusal `--real-jdk` requires.
///
/// The deleted infallible twin reached `ensure_synthetic_class`, whose signature
/// had no error channel, so under `--real-jdk` it recorded a
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
///
/// **It never collects** (callers hold bare references). A caller that has
/// pinned everything it holds can take
/// [`try_alloc_concurrent_synthetic_reclaiming`] instead, which reclaims and
/// retries once before raising `OutOfMemoryError`.
///
/// (gc-common w8-b: this doc had drifted onto `build_rooted_ref_array` above
/// when that function was inserted between it and this item.)
#[track_caller]
pub fn try_alloc_concurrent_synthetic(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let layout = concurrent_synthetic_layout(ctx, class_name, num_fields)?;
    Ok(alloc_with_concurrent_synthetic_layout(ctx, &layout))
}

/// [`try_alloc_concurrent_synthetic`] for a caller that allocates MANY objects
/// of one class in one call (a `StackTraceElement[]`'s elements): the class
/// resolution and layout decision are made once, in [`Self::new`], and every
/// [`Self::alloc_reclaiming`] is the funnel's allocation alone. Interpreter round i1 wave
/// 49, lane L7: the per-element resolution (a class lookup and initialization
/// check, a `String` of the class name, the field count, the layout-alias
/// census) was a fifth of an element's cost in `Throwable.getStackTrace()`.
pub struct ConcurrentSyntheticAllocator {
    pub layout: ConcurrentSyntheticLayout,
}

impl ConcurrentSyntheticAllocator {
    /// Resolve `class_name`'s layout as [`try_alloc_concurrent_synthetic`]
    /// does (the same refusals, the same census).
    #[track_caller]
    pub fn new(
        ctx: &mut dyn NativeContext,
        class_name: &str,
        num_fields: usize,
    ) -> Result<Self, MethodCallFailed> {
        Ok(Self {
            layout: concurrent_synthetic_layout(ctx, class_name, num_fields)?,
        })
    }

    /// gcg g6/oo (door 4): one object of the resolved layout, for a caller
    /// that holds no unpinned `ObjectRef` in a Rust local (the
    /// `StackTraceElement[]` builders in `lang_misc.rs`). It replaced the
    /// one-attempt `alloc` (never collected), removed with its last caller.
    /// A refusal reclaims and retries, as
    /// [`try_alloc_concurrent_synthetic_reclaiming`] does, and only then answers
    /// the catchable `OutOfMemoryError`. `what` names the allocation.
    pub fn alloc_reclaiming(
        &self,
        ctx: &mut dyn NativeContext,
        what: &str,
    ) -> Result<ObjectRef, MethodCallFailed> {
        alloc_reclaiming_with_layout(ctx, &self.layout, what)
    }

    /// One object of the resolved layout through exactly the allocation half
    /// of [`try_alloc_concurrent_synthetic`]: it never collects, so a caller
    /// that held bare references across that funnel may hold them across this
    /// one. JIT round 19 wave 6, lane throws6 (THROWS6-1): a `StackWalker`
    /// walk's per-frame `StackFrameInfo` / `ResolvedMethodName` allocations.
    pub fn alloc(&self, ctx: &mut dyn NativeContext) -> ObjectRef {
        alloc_with_concurrent_synthetic_layout(ctx, &self.layout)
    }
}

/// The allocation half of [`try_alloc_concurrent_synthetic`].
pub fn alloc_with_concurrent_synthetic_layout(
    ctx: &mut dyn NativeContext,
    layout: &ConcurrentSyntheticLayout,
) -> ObjectRef {
    if !layout.resolved {
        return ctx.alloc_object(layout.class_id, layout.slots);
    }
    // This is the shared allocator behind `java.net.URI`,
    // `HttpURLConnection`, and many other synthetic native objects (a
    // gdb backtrace once showed TestResponsePerformance's doUri() hot
    // loop, `new URI(...)` x 1,000,000, hard-aborting here on young-gen
    // exhaustion).
    //
    // gc-common w7-b: what this actually does TODAY, for the record.
    // `try_alloc_object_gc_safe` has no VM override (pinned by
    // `native-api/tests/layout_alias_coverage.rs`), so its trait
    // default forwards to `alloc_object`, and NEITHER half collects.
    // The VM's `alloc_object` walks young -> old gen without moving
    // anything; inside a native callback, exhaustion unwinds to the
    // native funnel as a catchable `OutOfMemoryError`
    // (`runtime::native_oom`), and outside one it keeps the historical
    // infallible entry. The `unwrap_or_else` arm below is therefore
    // unreachable in the VM. A collecting retry needs a caller that
    // holds no unpinned reference: that is
    // `try_alloc_concurrent_synthetic_reclaiming` (gc-common w8-b).
    ctx.try_alloc_object_gc_safe(layout.class_id, layout.slots)
        .unwrap_or_else(|| ctx.alloc_object(layout.class_id, layout.slots))
}

/// [`try_alloc_concurrent_synthetic`] for a caller that holds **no unpinned
/// `ObjectRef` in a Rust local**: on a refusal it reclaims the heap and
/// retries once, and only then raises a catchable `OutOfMemoryError`.
///
/// gc-common w8-b (`handoff-w7b-native-fallible-object-alloc-and-may-collect`,
/// `common-w3c-native-callback-allocations-cannot-collect`). Same class
/// resolution, same layout decision and same census as the funnel
/// ([`concurrent_synthetic_layout`]); the allocation is
/// `NativeContext::try_alloc_object`, which answers `None` instead of
/// unwinding, so the refusal is seen here.
///
/// # The precondition, which the CALLER owns
///
/// The reclaim is a real collection: it may move or free anything that is
/// not a root. Every reference the caller still uses after this call must
/// be pinned (`pin_native_root`) and re-read from its pin afterwards, or be
/// one the funnel already remaps for it (its own return value). Class
/// resolution here can already run `<clinit>` (Java, which can collect), so
/// a caller that is correct for [`try_alloc_concurrent_synthetic`] with its
/// references pinned is correct for this one; a caller with a bare
/// reference is correct for neither, and this one makes the window certain
/// on a full heap rather than rare.
#[track_caller]
pub fn try_alloc_concurrent_synthetic_reclaiming(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let layout = concurrent_synthetic_layout(ctx, class_name, num_fields)?;
    alloc_reclaiming_with_layout(ctx, &layout, class_name)
}

/// The reclaim-and-retry ladder of [`try_alloc_concurrent_synthetic_reclaiming`]
/// for an already resolved layout (shared with
/// [`ConcurrentSyntheticAllocator::alloc_reclaiming`]).
pub fn alloc_reclaiming_with_layout(
    ctx: &mut dyn NativeContext,
    layout: &ConcurrentSyntheticLayout,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
        return Ok(obj);
    }
    // gcg g3/o (opt-in): try after the young collection before the full rungs.
    if ctx.reclaim_young_before_alloc_retry() {
        if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
            return Ok(obj);
        }
        if ctx.reclaim_before_alloc_retry_after_young() {
            if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
                return Ok(obj);
            }
        }
    } else if ctx.reclaim_before_alloc_retry() {
        if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
            return Ok(obj);
        }
    }
    // zr z8/nr: a ZGC ladder that skipped its clear-soft rung (its collection
    // had made room) pays it now that the retry failed, then one more try.
    if ctx.reclaim_after_failed_alloc_retry() {
        if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
            return Ok(obj);
        }
    }
    // The first full G1 reclaim can expose fragmentation only after it has
    // returned aggregate capacity in separate holes.  This hook answers true
    // exclusively for that proved G1 shape and performs one target-window
    // compaction; it is not a generic third collection attempt.  The caller's
    // native-root precondition is the same as for the first reclaim above.
    if ctx.reclaim_after_fragmented_humongous_retry() {
        if let Some(obj) = ctx.try_alloc_object(layout.class_id, layout.slots) {
            return Ok(obj);
        }
    }
    Err(native_heap_oom(class_name))
}

/// zr z8/nr: the reclaim ladder of [`new_array_reclaiming`] for any fallible
/// `attempt` whose FIRST try was already refused: the staged young door (or
/// the whole door), the skipped clear-soft rung after a failed retry, and
/// G1's fragmented-humongous step, retrying after each. `None` = every rung
/// spent; the caller raises. The caller holds no unpinned `ObjectRef` (its
/// references are pinned or in a `NativeHandleScope`).
pub fn retry_after_reclaim<T>(
    ctx: &mut dyn NativeContext,
    mut attempt: impl FnMut(&mut dyn NativeContext) -> Option<T>,
) -> Option<T> {
    if ctx.reclaim_young_before_alloc_retry() {
        if let Some(v) = attempt(ctx) {
            return Some(v);
        }
        if ctx.reclaim_before_alloc_retry_after_young() {
            if let Some(v) = attempt(ctx) {
                return Some(v);
            }
        }
    } else if ctx.reclaim_before_alloc_retry() {
        if let Some(v) = attempt(ctx) {
            return Some(v);
        }
    }
    if ctx.reclaim_after_failed_alloc_retry() {
        if let Some(v) = attempt(ctx) {
            return Some(v);
        }
    }
    if ctx.reclaim_after_fragmented_humongous_retry() {
        return attempt(ctx);
    }
    None
}

/// The catchable `OutOfMemoryError` a native raises once its reclaim-and-retry
/// ladder is spent. `what` names the allocation (a class or an array shape).
pub fn native_heap_oom(what: &str) -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: format!("Java heap space (allocating {what})"),
    }
    .into()
}

/// gci i13/oa item 4: the footprint (header plus 8-byte-aligned data) of an
/// array of `length` `element_type` elements, which the sized reclaim doors
/// (`NativeContext::reclaim_before_alloc_retry_for`) judge humongous by.
/// `usize::MAX` when the data size overflows (the request is refused anyway).
pub fn array_footprint(element_type: cratonvm_types::ArrayElementType, length: usize) -> usize {
    cratonvm_types::array_data_size_checked(length, element_type)
        .map_or(usize::MAX, |data| data.saturating_add(cratonvm_types::ARRAY_DATA_OFFSET))
}

/// HotSpot's refusal of an array LENGTH (`arrayOopDesc::max_array_length`,
/// `Integer.MAX_VALUE - 2` for every element type on 64-bit): `OutOfMemoryError:
/// Requested array size exceeds VM limit`, at once and without collecting, where
/// the reclaim ladder below ended in `Java heap space`.
/// `Array.newInstance(long.class, Integer.MAX_VALUE)` (interpreter round i4 wave
/// 98, lane L9; `L9W98VmErrorTexts` row `vm-limit-reflect`).
pub fn array_length_past_vm_limit(length: usize) -> Option<MethodCallFailed> {
    const VM_MAX_ARRAY_LENGTH: usize = i32::MAX as usize - 2;
    (length > VM_MAX_ARRAY_LENGTH).then(|| {
        cratonvm_types::error::RuntimeError::OutOfMemoryError {
            message: "Requested array size exceeds VM limit".to_string(),
        }
        .into()
    })
}

/// Reclaim the heap on behalf of a native whose live references are exactly
/// `live`: pin each, run `reclaim_before_alloc_retry`, write the post-GC
/// address of each back into `live`, and unpin. Returns whether a retry is
/// worth making.
///
/// The pins are taken AFTER the refused allocation, which is sound: nothing
/// between the refusal and the pin can collect. Slow path only; the `Vec`
/// is the handle list, not a Java allocation.
pub fn reclaim_holding(
    ctx: &mut dyn NativeContext,
    live: &mut [ObjectRef],
    reclaim: impl FnOnce(&mut dyn NativeContext) -> bool,
) -> bool {
    let handles: Vec<usize> = live.iter().map(|&o| ctx.pin_native_root(o)).collect();
    let retry = reclaim(ctx);
    for (slot, &handle) in live.iter_mut().zip(handles.iter()) {
        *slot = ctx.read_native_pin(handle, *slot);
    }
    if let Some(&base) = handles.first() {
        ctx.unpin_native_roots(base);
    }
    retry
}

/// A primitive array for a native array factory (`Arrays.copyOf*`,
/// `Array.newInstance`), with one reclaim-and-retry before the catchable
/// `OutOfMemoryError` ([`native_heap_oom`], naming `what`).
///
/// **Every `ObjectRef` the caller still uses afterwards must be in `live`.**
/// On the slow path those are pinned across the collection and refreshed in
/// place, so the caller reads `live[i]` after this call, never a copy taken
/// before it. A caller that cannot list everything it holds must not use
/// this; it keeps the single-attempt `new_array`.
///
/// gc-common w8-b, route 1 of
/// `common-w3c-native-callback-allocations-cannot-collect`: HotSpot's
/// `Arrays.copyOf` is Java and collects before it throws, while these natives
/// got one no-GC attempt and threw on a heap a collection would have freed.
pub fn new_array_reclaiming(
    ctx: &mut dyn NativeContext,
    element_type: cratonvm_types::ArrayElementType,
    length: usize,
    live: &mut [ObjectRef],
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(refused) = array_length_past_vm_limit(length) {
        return Err(refused);
    }
    if let Some(arr) = ctx.try_new_array(element_type, length) {
        return Ok(arr);
    }
    // gci i13/oa item 4: the doors take the request's size, so a G1 humongous
    // request collects under HotSpot's `G1 Humongous Allocation`.
    let bytes = array_footprint(element_type, length);
    // gcg g3/o (opt-in): try after the young collection before the full rungs.
    if reclaim_holding(ctx, live, |ctx| ctx.reclaim_young_before_alloc_retry_for(bytes)) {
        if let Some(arr) = ctx.try_new_array(element_type, length) {
            return Ok(arr);
        }
        if reclaim_holding(ctx, live, |ctx| {
            ctx.reclaim_before_alloc_retry_after_young()
        }) {
            if let Some(arr) = ctx.try_new_array(element_type, length) {
                return Ok(arr);
            }
        }
    } else if reclaim_holding(ctx, live, |ctx| ctx.reclaim_before_alloc_retry_for(bytes)) {
        if let Some(arr) = ctx.try_new_array(element_type, length) {
            return Ok(arr);
        }
    }
    // zr z8/nr: the skipped clear-soft rung, paid after a failed retry.
    if reclaim_holding(ctx, live, |ctx| ctx.reclaim_after_failed_alloc_retry()) {
        if let Some(arr) = ctx.try_new_array(element_type, length) {
            return Ok(arr);
        }
    }
    // A full G1 cycle can reclaim enough *total* capacity only for this
    // second refusal to reveal that it is split by live Old regions.  Give
    // that proved shape one target-window compaction, never a generic third
    // GC attempt.  Every live native reference is rooted again because the
    // first collection may have relocated it.
    if reclaim_holding(ctx, live, |ctx| {
        ctx.reclaim_after_fragmented_humongous_retry()
    }) {
        if let Some(arr) = ctx.try_new_array(element_type, length) {
            return Ok(arr);
        }
    }
    Err(native_heap_oom(what))
}

/// [`new_array_reclaiming`] for a reference array of component `class_id`.
pub fn new_ref_array_reclaiming(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    length: usize,
    live: &mut [ObjectRef],
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(refused) = array_length_past_vm_limit(length) {
        return Err(refused);
    }
    let copy_diag = length >= 30_000
        && what == "Arrays.copyOf"
        && false;
    if copy_diag {
        eprintln!(
            "[REF-COPY-PRE] len={length} live={:?}",
            live.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>()
        );
        for &src in live.iter() {
            for i in (0..ctx.array_length(src)).step_by(1024) {
                let _ = ctx.get_array_element(src, i);
            }
        }
    }
    if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
        if copy_diag {
            eprintln!(
                "[REF-COPY-POST] immediate dest={:#x} live={:?}",
                arr.as_ptr() as usize,
                live.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>()
            );
        }
        return Ok(arr);
    }
    // gci i13/oa item 4: as in `new_array_reclaiming`.
    let bytes = array_footprint(cratonvm_types::ArrayElementType::Reference, length);
    // gcg g3/o (opt-in): try after the young collection before the full rungs.
    if reclaim_holding(ctx, live, |ctx| ctx.reclaim_young_before_alloc_retry_for(bytes)) {
        if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
            if copy_diag {
                eprintln!(
                    "[REF-COPY-POST] reclaimed dest={:#x} live={:?}",
                    arr.as_ptr() as usize,
                    live.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>()
                );
            }
            return Ok(arr);
        }
        if reclaim_holding(ctx, live, |ctx| {
            ctx.reclaim_before_alloc_retry_after_young()
        }) {
            if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
                if copy_diag {
                    eprintln!(
                        "[REF-COPY-POST] reclaimed dest={:#x} live={:?}",
                        arr.as_ptr() as usize,
                        live.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>()
                    );
                }
                return Ok(arr);
            }
        }
    } else if reclaim_holding(ctx, live, |ctx| ctx.reclaim_before_alloc_retry_for(bytes)) {
        if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
            if copy_diag {
                eprintln!(
                    "[REF-COPY-POST] reclaimed dest={:#x} live={:?}",
                    arr.as_ptr() as usize,
                    live.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>()
                );
            }
            return Ok(arr);
        }
    }
    // zr z8/nr: the skipped clear-soft rung, paid after a failed retry.
    if reclaim_holding(ctx, live, |ctx| ctx.reclaim_after_failed_alloc_retry()) {
        if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
            return Ok(arr);
        }
    }
    if reclaim_holding(ctx, live, |ctx| {
        ctx.reclaim_after_fragmented_humongous_retry()
    }) {
        if let Some(arr) = ctx.try_new_ref_array(class_id, length) {
            return Ok(arr);
        }
    }
    Err(native_heap_oom(what))
}

/// What [`concurrent_synthetic_layout`] resolved for one synthetic
/// allocation.
pub struct ConcurrentSyntheticLayout {
    pub class_id: cratonvm_types::ClassId,
    /// The slot count to allocate: the caller's request widened to the real
    /// class's field count when the class resolved.
    pub slots: usize,
    /// `false` when the class did not load and the policy fabricated
    /// (`refused_class`) a synthetic class declaring exactly `slots`.
    pub resolved: bool,
}

/// The class resolution and layout decision shared by
/// [`try_alloc_concurrent_synthetic`] and
/// [`try_alloc_concurrent_synthetic_reclaiming`]: one implementation, so the
/// two cannot drift on which class or how many slots (and the census in
/// `native-api/tests/layout_alias_coverage.rs` reads this body).
///
/// `#[track_caller]`, like the funnels, so the class-origin and layout-alias
/// censuses still name the native that asked.
#[track_caller]
pub fn concurrent_synthetic_layout(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ConcurrentSyntheticLayout, MethodCallFailed> {
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
            // `!=`, not `<`, since 2026-08-11 (Real-JDK lane W4-4).
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
            Ok(ConcurrentSyntheticLayout {
                class_id: cid,
                slots: n,
                resolved: true,
            })
        }
        Err(_) => {
            // The real `.class` file could not be loaded. Allocating with
            // `ClassId::new(0)` (`java/lang/Object`, zero declared fields) but a
            // non-zero slot count produces an "undersized object layout" object
            // — the GC's `get_field` bounds guard rejects every field access on
            // it. Ask the policy for a synthetic class declaring `num_fields`
            // instance fields, so the header's `class_id` matches the allocated
            // slot count; under `--real-jdk` that ask is refused instead.
            let cid = refused_class(ctx, class_name, num_fields)?;
            Ok(ConcurrentSyntheticLayout {
                class_id: cid,
                slots: num_fields,
                resolved: false,
            })
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
/// reach a `pub` helper in this crate; copying it here would have made
/// it the sixteenth private re-implementation of a primitive
/// W7-59-layout-detector-coverage.md §2.1 already counted fifteen copies of.
/// Kept as a name because this crate's call sites read better with it and
/// because deleting it would churn them for nothing.
pub fn appended_slot_base_for_class(ctx: &mut dyn NativeContext, class_name: &str) -> usize {
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
pub fn try_alloc_with_appended_slots(
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
/// `pub` since 2026-08-10 (Real-JDK wave 2, step 3): the crate's
/// remaining direct `ensure_synthetic_class` callers — the enterprise-shim
/// `alloc_object_for` helpers, `alloc_impl`, the array-element class lookups in
/// `lang_string`/`keystore`/`regex_matcher` — each needed the same three lines,
/// and three per-site copies of a conversion is how the two spellings drift
/// apart. One idiom, one place.
#[track_caller]
pub fn refused_class(
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
            let cb = "java/util/concurrent/CyclicBarrier";
    registry.register(cb, "<init>", "(I)V", native_cb_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        cb,
        "<init>",
        "(ILjava/lang/Runnable;)V",
        native_cb_init_action, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(cb, "await", "()I", native_cb_await, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        cb,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)I",
        native_cb_await_timeout, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(cb, "getParties", "()I", native_cb_get_parties, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(cb, "getNumberWaiting", "()I", native_cb_get_number_waiting, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(cb, "isBroken", "()Z", native_cb_is_broken, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(cb, "reset", "()V", native_cb_reset, cratonvm_native_api::NativeKind::SyntheticStub);
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
        
    // --- CountDownLatch ---
    //
    // OVER A REAL JDK, `--compatible` REGISTERS NONE OF THESE by default since
    // interpreter round i3 wave 71, lane L1: the `JdkCountDownLatch`
    // HotSpot-alignment row (`CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH`, `=0` kill
    // switch) names the class in a `retired_shadow::COMPAT_FAMILIES` entry's
    // `stub_classes`, so `NativeMethodRegistry::register` drops each of these
    // `SyntheticStub`s (and `lib.rs`'s `surefire_cdl` copy), as `--real-jdk`
    // refuses them, and the JDK 25 bodies run (AQS `Sync`, parked with the
    // `Sync` as blocker). What is left for them is the synthetic class library
    // and that row's `=0` arm.
    let cdl = "java/util/concurrent/CountDownLatch";
    registry.register(cdl, "<init>", "(I)V", native_cdl_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(cdl, "countDown", "()V", native_cdl_count_down, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(cdl, "await", "()V", native_cdl_await, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        cdl,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_cdl_await_timeout, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(cdl, "getCount", "()J", native_cdl_get_count, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        cdl,
        "toString",
        "()Ljava/lang/String;",
        native_cdl_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
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
    
    // `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST` (default ON since its flip,
    // kill `0`, `runtime_flag_default_on` in `NativeMethodRegistry::new`;
    // interpreter round i3 wave 77, lane L1): two reviewed `Intrinsic`s over the JDK's
    // `ReentrantLock.lock()` / `unlock()` bodies, kind stated at the site.
    // Both modes, real JDK and real AQS only. See the section comment above
    // `register_reentrant_lock_fast`.
    register_reentrant_lock_fast(registry, real_aqs);

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
    // The helpers below resolved `array` / `size` by field name first
    // (real-JDK path) and fell back to slot 0/1 when the real classfile
    // didn't define those fields. Since wave 76 the block runs only on the
    // synthetic library, so wave 77 (lane L1) deleted the by-name arm: the
    // helpers read and write the synthetic slots 0/1 only.
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
    //
    // (Interpreter round i3 wave 74, lane L1: the JDK bodies were not the
    // cause. The constructor had set `lock`; a stand-in that STAYED cleared
    // it -- `native-collections`' `addAll` / `addIfAbsent` / `bulkRemove`
    // called `cowal_bump_mod_count`, which wrote `AbstractList.modCount` into
    // slot 0, `lock` on a real COWAL. Fixed there on 2026-08-12. The record:
    // `i73-L1-proposal-decide-jdk-copy-on-write-without-the-spring-sample-20261106`,
    // "Progress (wave 74)"; probe `L1W74CopyOnWriteLockShapes`.)
    //
    // Interpreter round i3 wave 72, lane L1: every triple below is a retired
    // shadow (`--real-jdk` runs the JDK's bodies), and the `JdkCopyOnWrite`
    // HotSpot-alignment row (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE`, default ON
    // since the wave-74 triage, `=0` reverts) drops all of them over a real JDK, so the JDK's
    // mutators lock `lock` as its bulk methods already did (these lock the
    // LIST's monitor:
    // `i71-L1-compatible-copyonwritearraylist-bridges-lock-the-list-not-its-lock-20261104`).
    //
    // Interpreter round i3 wave 76, lane L1: the row GRADUATED for this
    // block. The wave-75 battery (252 probes) ran with it on by default and
    // nothing regressed, so over a real JDK the stand-ins are no longer
    // registered at all -- the `=0` arm that brought them back is gone -- and
    // they serve the synthetic class library only (where the two-slot layout
    // below is the implementation). Stated `Bridge` at the site, as before.
    // `i74-L1-proposal-retire-the-cowal-stand-ins-from-the-real-jdk-boot-20261107`.
    if registry.real_jdk() {
        note_graduated_copy_on_write_row(registry);
        return;
    }
    {
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

        /// Read (array, size) from a synthetic-library COWAL receiver
        /// (slot 0 = backing `Object[]`, slot 1 = `int` size).
        ///
        /// Interpreter round i3 wave 77, lane L1: the real-JDK arm (`array`
        /// resolved by name) is gone. These stand-ins are registered only
        /// when `!registry.real_jdk()` (wave 76), and the synthetic class
        /// declares `_f0` / `_f1` (`class_manager`'s `instance_fields(2)`),
        /// so `resolve_field_index(.., "array")` could only answer `None`
        /// here; no mock resolves it either.
        fn cowal_read_state(
            ctx: &dyn NativeContext,
            this: ObjectRef,
        ) -> (Option<ObjectRef>, usize) {
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

        /// Write a new backing array into a synthetic-library COWAL receiver:
        /// slot 0, and the `int` size in slot 1 (wave 77: the real-JDK arm is
        /// gone, see `cowal_read_state`).
        fn cowal_write_array(ctx: &mut dyn NativeContext, this: ObjectRef, new_arr: ObjectRef) {
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
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(cowal, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let (_, size) = cowal_read_state(ctx, this);
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(cowal, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (mut data, size) = cowal_read_state(ctx, this);
            let found = cowal_find_pinned(ctx, &mut this, &mut data, &mut needle, size);
            Ok(Some(Value::Int(i32::from(found.is_some()))))
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(cowal, "indexOf", "(Ljava/lang/Object;)I", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let mut needle = args.get(1).copied().unwrap_or(Value::Object(None));
            let (mut data, size) = cowal_read_state(ctx, this);
            let found = cowal_find_pinned(ctx, &mut this, &mut data, &mut needle, size);
            Ok(Some(Value::Int(found.map_or(-1, |i| i as i32))))
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(cowal, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let (mut old_arr, size) = cowal_read_state(ctx, this);
            let new_arr = cowal_pinned_new_array(ctx, size + 1, &mut this, &mut old_arr, &mut elem);
            if let Some(old) = old_arr {
                for i in 0..size {
                    ctx.set_array_element(new_arr, i, ctx.get_array_element(old, i));
                }
            }
            ctx.set_array_element(new_arr, size, elem);
            cowal_write_array(ctx, this, new_arr);
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, cratonvm_native_api::NativeKind::Bridge);
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            let new_arr = cowal_pinned_new_array(ctx, size + 1, &mut this, &mut old_arr, &mut elem);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
    };
}

/// Interpreter round i3 wave 76, lane L1: over a real JDK the
/// `CopyOnWriteArrayList` stand-ins are no longer registered whatever the
/// `JdkCopyOnWrite` row says (the row graduated: see the block in
/// [`register_concurrent_natives`]). A `--compatible` VM that turned the row
/// off (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE=0`, `CRATONVM_COMPAT_ALIGN=
/// -jdk-copy-on-write` or `none`) gets one stderr line saying the revert no
/// longer brings them back, so a host A/B does not read an unchanged run as
/// "the revert changed nothing". The variable stays one wave as this no-op,
/// then goes with the row. Cold: registration only.
#[cold]
pub fn note_graduated_copy_on_write_row(registry: &NativeMethodRegistry) {
    if !registry.aligned(cratonvm_types::HotSpotAlignment::JdkCopyOnWrite) {
        eprintln!(
            "[cratonvm] jdk-copy-on-write is off, but over a real JDK the \
             CopyOnWriteArrayList stand-ins are no longer registered (the row \
             graduated in interpreter round i3 wave 76); the JDK's own bodies run"
        );
    }
}

// ===========================================================================
// `ReentrantLock.lock()` / `unlock()`: the uncontended fast path
// ===========================================================================
//
// Interpreter round i3 wave 77, lane L1
// (`CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`, landed default OFF, since
// flipped default ON with kill `0` (`runtime_flag_default_on`; gci i18/vp
// corrected this note); page
// `i76-L1-proposal-an-uncontended-reentrantlock-fast-path-20261109`).
//
// One uncontended JDK 25 `lock()` / `unlock()` pair is a 17-invocation call
// tree (`ReentrantLock.lock` -> `Sync.lock` -> `NonfairSync.initialTryLock` ->
// `compareAndSetState` -> `Unsafe.compareAndSetInt`,
// `setExclusiveOwnerThread`; `unlock` -> `AQS.release` -> `Sync.tryRelease`
// -> the getters, `setExclusiveOwnerThread(null)`, `setState`,
// `signalNext(head)`), three of them through the native funnel
// (`setExclusiveOwnerThread` x2 is a reviewed `Intrinsic` in `jmx.rs`, the
// CAS is `Unsafe`). With the switch on, `lock()V` and `unlock()V` are each ONE
// reviewed `Intrinsic` (AGENTS.md: the only kind that may win over the class
// bytes) that does what those bodies do to the JDK's own fields, in the
// bodies' order, for the shapes listed below, and re-enters the JDK's own
// body (`invoke_special_bytecode_only_hinted`, as the `MethodType.makeImpl`
// memo does) for everything else, having written nothing:
//
// * `lock()`, `NonfairSync`: `Thread.currentThread()`, CAS `state` 0 -> 1, then
//   `exclusiveOwnerThread = current` (the `setExclusiveOwnerThread` store and
//   its ThreadMXBean ownership record); else, owner == current: `state + 1`
//   (a volatile write; an overflow to a negative count declines, so the
//   body throws its `Error("Maximum lock count exceeded")`); else declines
//   (contended: the body CASes again, queues and parks).
// * `lock()`, `FairSync`: `state` read; 0 and `hasQueuedThreads()` false at
//   once (`tail == head` or `tail == null`, where the JDK's loop does not
//   iterate): the same CAS and owner store; reentry as above; else declines.
// * `unlock()`: owner != current declines (the body throws
//   `IllegalMonitorStateException` with its own frames). `state - 1 != 0`:
//   the volatile write. `state - 1 == 0`: if `head.next != null` (a queued
//   successor) it declines BEFORE writing, so the body's `signalNext` wakes
//   it; else `exclusiveOwnerThread = null` (plus the ownership record), the
//   releasing volatile write `state = 0`, then -- AFTER that write, where the
//   JDK's `signalNext(head)` reads -- `head` / `head.next` again: a waiter
//   that queued in between gets the JDK's own `signalNext(head)` (a static
//   call), so no wake-up is lost.
//
// Interpreter round i3 wave 80, lane L1
// (`CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE`, landed default OFF, since
// flipped default ON with kill `0`, meaningful with the switch above; page
// `i77-L1-proposal-serve-lockinterruptibly-and-trylock-on-the-reentrantlock-fast-path-20261110`)
// adds two more reviewed `Intrinsic`s, same rules:
//
// * `lockInterruptibly()V` (JDK 25: `Sync.lockInterruptibly`, `if
//   (Thread.interrupted()) throw new InterruptedException(); if
//   (!initialTryLock()) acquireInterruptibly(1);`): the current thread's
//   interrupt status set -- the Java `Thread.interrupted` field the JDK reads,
//   or the VM's own flag -- DECLINES without clearing anything, so the body
//   clears it and throws with its own frames; else `lock()`'s shapes
//   (`initialTryLock` is the same method); else declines.
// * `tryLock()Z` (`Sync.tryLock`, which barges for BOTH fairness kinds):
//   `state == 0`: CAS 0 -> 1 and the owner store, `true`; a lost CAS answers
//   `false` (the body returns `false` there too, having written nothing);
//   `state != 0` and owner == current: `state + 1` (an overflow declines, so
//   the body throws its `Error`), `true`; owner != current: `false` natively
//   (no queueing, nothing to wake).
//
// Both registration waves follow `CRATONVM_INTRINSIC_BODY_COMPILES` (wave 79,
// `NativeMethodRegistry::interpreted_intrinsic_registers`): with it, a JIT VM
// registers the rows too and marks them body-compilable, so the JIT compiles
// the JDK bodies behind them and a decline reaches the compiled body.
//
// Interrupts, `Condition`s, `getHoldCount`,
// `isLocked`, `getQueuedThreads` and `toString` are untouched JDK bytecode
// reading the same fields the bodies would have left. Everything else -- a
// `null` or foreign `sync`, a sync class that is not exactly the JDK's
// `NonfairSync` / `FairSync`, a thread with no mirror yet, a field that does
// not resolve -- declines. Debugger events inside the two methods are not
// reported while they are served (the reviewed-`Intrinsic` rule of
// `interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004`);
// a declined call runs the body in a frame as usual.
//
// Memos: the `sync` slot and the `ReentrantLock` class id per VM, each sync
// class's slots per (VM, class id), and the two bodies' declared-method hints
// per (VM, class id) -- thread-local, keyed by `vm_identity()` (a VM of
// identity 0, a mock, recomputes them every call). Field slots are per-class
// constants and boot classes never unload.

pub const RL_FAST_CLASS: &str = "java/util/concurrent/locks/ReentrantLock";
pub const RL_FAST_NONFAIR_SYNC: &str = "java/util/concurrent/locks/ReentrantLock$NonfairSync";
pub const RL_FAST_FAIR_SYNC: &str = "java/util/concurrent/locks/ReentrantLock$FairSync";
pub const RL_FAST_AQS: &str = "java/util/concurrent/locks/AbstractQueuedSynchronizer";
pub const RL_FAST_AQS_NODE: &str = "java/util/concurrent/locks/AbstractQueuedSynchronizer$Node";
pub const RL_FAST_SIGNAL_NEXT_DESC: &str =
    "(Ljava/util/concurrent/locks/AbstractQueuedSynchronizer$Node;)V";
pub const RL_FAST_THREAD: &str = "java/lang/Thread";

/// The JDK bodies the fast path re-enters, by index (`rl_fast_jdk_body`'s
/// `which`, and the slot of its declared-method hint).
pub const RL_FAST_BODIES: [(&str, &str); 4] = [
    ("lock", "()V"),
    ("unlock", "()V"),
    ("lockInterruptibly", "()V"),
    ("tryLock", "()Z"),
];
pub const RL_FAST_BODY_LOCK: usize = 0;
pub const RL_FAST_BODY_UNLOCK: usize = 1;
pub const RL_FAST_BODY_LOCK_INTERRUPTIBLY: usize = 2;
pub const RL_FAST_BODY_TRY_LOCK: usize = 3;

/// `ReentrantLock`'s class id and `sync` slot, and `AQS$Node.next`'s slot, in
/// the VM of identity `vm`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlFastBase {
    pub vm: usize,
    pub rl_class: ClassId,
    pub sync: usize,
    pub node_next: usize,
}

/// The slots of one `ReentrantLock$NonfairSync` / `$FairSync` class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlFastSync {
    pub vm: usize,
    pub class: ClassId,
    pub fair: bool,
    pub state: usize,
    pub owner: usize,
    pub head: usize,
    pub tail: usize,
}

thread_local! {
    pub static RL_FAST_BASE: std::cell::Cell<Option<RlFastBase>> =
        const { std::cell::Cell::new(None) };
    /// The two sync classes of the last VM seen (fair and non-fair).
    pub static RL_FAST_SYNCS: std::cell::Cell<[Option<RlFastSync>; 2]> =
        const { std::cell::Cell::new([None, None]) };
    /// The `RL_FAST_BODIES`' declared-method hints: (VM, class id, index).
    pub static RL_FAST_BODY_HINTS: std::cell::Cell<[Option<(usize, ClassId, u32)>; 4]> =
        const { std::cell::Cell::new([None, None, None, None]) };
    /// The slot of `java.lang.Thread.interrupted` in the last VM seen: (VM,
    /// slot).
    pub static RL_FAST_INTERRUPTED_SLOT: std::cell::Cell<Option<(usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// What the `tryLock()` fast path did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RlFastTry {
    /// Served: the JDK body's answer, with the fields as it would leave them.
    Answer(bool),
    /// Not a served shape; nothing was written: run the JDK body.
    Decline,
}

/// What the `lock()` fast path did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RlFastLock {
    /// Served: the fields hold what the JDK body would have left.
    Served,
    /// Not a served shape; nothing was written: run the JDK body.
    Decline,
}

/// What the `unlock()` fast path did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RlFastUnlock {
    /// Served: the fields hold what the JDK body would have left.
    Served,
    /// Released, and a successor queued behind `head` after the check: the
    /// caller runs the JDK's `signalNext(head)`.
    ServedWake(ObjectRef),
    /// Not a served shape; nothing was written: run the JDK body.
    Decline,
}

/// A reference field's value; `None` when the slot does not hold a reference
/// (a layout this path does not know).
pub fn rl_fast_ref(v: Value) -> Option<Option<ObjectRef>> {
    match v {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

pub fn rl_fast_base(ctx: &dyn NativeContext, vm: usize) -> Option<RlFastBase> {
    if vm != 0 {
        if let Some(b) = RL_FAST_BASE
            .with(std::cell::Cell::get)
            .filter(|b| b.vm == vm)
        {
            return Some(b);
        }
    }
    let rl_class = ctx.class_id_by_name(RL_FAST_CLASS)?;
    let sync = ctx.resolve_field_index_by_class_id(rl_class, "sync")?;
    let node = ctx.class_id_by_name(RL_FAST_AQS_NODE)?;
    let node_next = ctx.resolve_field_index_by_class_id(node, "next")?;
    let b = RlFastBase {
        vm,
        rl_class,
        sync,
        node_next,
    };
    if vm != 0 {
        RL_FAST_BASE.with(|c| c.set(Some(b)));
    }
    Some(b)
}

pub fn rl_fast_sync_layout(ctx: &dyn NativeContext, vm: usize, class: ClassId) -> Option<RlFastSync> {
    if vm != 0 {
        let hit = RL_FAST_SYNCS.with(|c| {
            c.get()
                .into_iter()
                .flatten()
                .find(|s| s.vm == vm && s.class == class)
        });
        if hit.is_some() {
            return hit;
        }
    }
    let name = ctx.class_name_of_id(class)?;
    let fair = match name.as_str() {
        RL_FAST_NONFAIR_SYNC => false,
        RL_FAST_FAIR_SYNC => true,
        _ => return None,
    };
    let s = RlFastSync {
        vm,
        class,
        fair,
        state: ctx.resolve_field_index_by_class_id(class, "state")?,
        owner: ctx.resolve_field_index_by_class_id(class, "exclusiveOwnerThread")?,
        head: ctx.resolve_field_index_by_class_id(class, "head")?,
        tail: ctx.resolve_field_index_by_class_id(class, "tail")?,
    };
    if vm != 0 {
        RL_FAST_SYNCS.with(|c| {
            let mut rows = c.get();
            // A row of another VM is dropped first; else the row of the other
            // fairness is kept.
            let at = match rows.iter().position(|r| r.is_none_or(|r| r.vm != vm)) {
                Some(at) => at,
                None => usize::from(fair),
            };
            rows[at] = Some(s);
            c.set(rows);
        });
    }
    Some(s)
}

/// The receiver's `sync` and its class's slots, or `None` (decline).
pub fn rl_fast_sync_of(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, RlFastSync, RlFastBase)> {
    let vm = ctx.vm_identity();
    let base = rl_fast_base(ctx, vm)?;
    let sync = rl_fast_ref(ctx.get_field(this, base.sync))??;
    let layout = rl_fast_sync_layout(ctx, vm, ctx.class_id_of_object(sync))?;
    Some((sync, layout, base))
}

/// `setExclusiveOwnerThread(owner)`: the store and the ThreadMXBean
/// ownership record the `jmx.rs` intrinsic on that method makes.
pub fn rl_fast_set_owner(
    ctx: &mut dyn NativeContext,
    sync: ObjectRef,
    l: &RlFastSync,
    owner: Option<ObjectRef>,
) {
    ctx.set_field(sync, l.owner, Value::Object(owner));
    ctx.record_jmx_owned_synchronizer(sync, owner);
}

/// The reentrant arm of both `initialTryLock`s: `c` is the state read.
pub fn rl_fast_reenter(
    ctx: &mut dyn NativeContext,
    sync: ObjectRef,
    l: &RlFastSync,
    current: ObjectRef,
    c: i32,
) -> RlFastLock {
    if rl_fast_ref(ctx.get_field(sync, l.owner)) != Some(Some(current)) {
        return RlFastLock::Decline;
    }
    let next = c.wrapping_add(1);
    if next < 0 {
        // The body throws `Error("Maximum lock count exceeded")`.
        return RlFastLock::Decline;
    }
    ctx.set_field_volatile(sync, l.state, Value::Int(next));
    RlFastLock::Served
}

/// `ReentrantLock.lock()`'s served shapes (see the section comment).
pub fn rl_fast_lock(ctx: &mut dyn NativeContext, this: ObjectRef) -> RlFastLock {
    let Some(current) = ctx.current_thread_object_if_any() else {
        return RlFastLock::Decline;
    };
    let Some((sync, l, _)) = rl_fast_sync_of(&*ctx, this) else {
        return RlFastLock::Decline;
    };
    if l.fair {
        let Value::Int(c) = ctx.get_field_volatile(sync, l.state) else {
            return RlFastLock::Decline;
        };
        if c != 0 {
            return rl_fast_reenter(ctx, sync, &l, current, c);
        }
        // `hasQueuedThreads()`: `for (p = tail, h = head; p != h && p != null; ..)`.
        let Some(tail) = rl_fast_ref(ctx.get_field_volatile(sync, l.tail)) else {
            return RlFastLock::Decline;
        };
        let Some(head) = rl_fast_ref(ctx.get_field_volatile(sync, l.head)) else {
            return RlFastLock::Decline;
        };
        if tail.is_some() && tail != head {
            return RlFastLock::Decline;
        }
        if !ctx.compare_and_swap_field(sync, l.state, Value::Int(0), Value::Int(1)) {
            return RlFastLock::Decline;
        }
        rl_fast_set_owner(ctx, sync, &l, Some(current));
        return RlFastLock::Served;
    }
    if ctx.compare_and_swap_field(sync, l.state, Value::Int(0), Value::Int(1)) {
        rl_fast_set_owner(ctx, sync, &l, Some(current));
        return RlFastLock::Served;
    }
    let Value::Int(c) = ctx.get_field_volatile(sync, l.state) else {
        return RlFastLock::Decline;
    };
    rl_fast_reenter(ctx, sync, &l, current, c)}

/// `ReentrantLock.tryLock()`'s served shapes: `Sync.tryLock`, which barges
/// for both fairness kinds (see the section comment).
pub fn rl_fast_try_lock(ctx: &mut dyn NativeContext, this: ObjectRef) -> RlFastTry {
    let Some(current) = ctx.current_thread_object_if_any() else {
        return RlFastTry::Decline;
    };
    let Some((sync, l, _)) = rl_fast_sync_of(&*ctx, this) else {
        return RlFastTry::Decline;
    };
    let Value::Int(c) = ctx.get_field_volatile(sync, l.state) else {
        return RlFastTry::Decline;
    };
    if c == 0 {
        if ctx.compare_and_swap_field(sync, l.state, Value::Int(0), Value::Int(1)) {
            rl_fast_set_owner(ctx, sync, &l, Some(current));
            return RlFastTry::Answer(true);
        }
        // The body's CAS lost too: `return false`, nothing written.
        return RlFastTry::Answer(false);
    }
    match rl_fast_ref(ctx.get_field(sync, l.owner)) {
        Some(Some(owner)) if owner == current => {
            let next = c.wrapping_add(1);
            if next < 0 {
                // The body throws `Error("Maximum lock count exceeded")`.
                return RlFastTry::Decline;
            }
            ctx.set_field_volatile(sync, l.state, Value::Int(next));
            RlFastTry::Answer(true)
        }
        Some(_) => RlFastTry::Answer(false),
        None => RlFastTry::Decline,
    }
}

/// Would `Thread.interrupted()` answer `true` on this thread (or can this
/// path not tell)? Reads, never clears: the Java `Thread.interrupted` field of
/// the current thread's mirror (what the JDK body reads) and the VM's own
/// flag. A `true` sends `lockInterruptibly()` to its JDK body, which clears
/// the status and throws with its own frames.
pub fn rl_fast_interrupt_pending(ctx: &mut dyn NativeContext) -> bool {
    if ctx.is_interrupted(false) {
        return true;
    }
    let Some(current) = ctx.current_thread_object_if_any() else {
        return true;
    };
    let vm = ctx.vm_identity();
    let memo = if vm != 0 {
        RL_FAST_INTERRUPTED_SLOT
            .with(std::cell::Cell::get)
            .filter(|(v, _)| *v == vm)
    } else {
        None
    };
    let slot = match memo {
        Some((_, slot)) => slot,
        None => {
            let Some(slot) = ctx
                .class_id_by_name(RL_FAST_THREAD)
                .and_then(|t| ctx.resolve_field_index_by_class_id(t, "interrupted"))
            else {
                return true;
            };
            if vm != 0 {
                RL_FAST_INTERRUPTED_SLOT.with(|c| c.set(Some((vm, slot))));
            }
            slot
        }
    };
    !matches!(ctx.get_field_volatile(current, slot), Value::Int(0))
}

/// `head.next` is a node: `Some(true)`; `None` when a slot is not a reference.
pub fn rl_fast_has_successor(
    ctx: &dyn NativeContext,
    head: Option<ObjectRef>,
    base: &RlFastBase,
) -> Option<bool> {
    match head {
        None => Some(false),
        Some(h) => Some(rl_fast_ref(ctx.get_field_volatile(h, base.node_next))?.is_some()),
    }
}

/// `ReentrantLock.unlock()`'s served shapes (see the section comment).
pub fn rl_fast_unlock(ctx: &mut dyn NativeContext, this: ObjectRef) -> RlFastUnlock {
    let Some(current) = ctx.current_thread_object_if_any() else {
        return RlFastUnlock::Decline;
    };
    let Some((sync, l, base)) = rl_fast_sync_of(&*ctx, this) else {
        return RlFastUnlock::Decline;
    };
    // `tryRelease`: `c = getState() - releases`, then the owner check.
    let Value::Int(state) = ctx.get_field_volatile(sync, l.state) else {
        return RlFastUnlock::Decline;
    };
    let c = state.wrapping_sub(1);
    if rl_fast_ref(ctx.get_field(sync, l.owner)) != Some(Some(current)) {
        // The body throws `IllegalMonitorStateException`.
        return RlFastUnlock::Decline;
    }
    if c != 0 {
        ctx.set_field_volatile(sync, l.state, Value::Int(c));
        return RlFastUnlock::Served;
    }
    // A successor already queued: the body releases and signals it.
    let Some(head) = rl_fast_ref(ctx.get_field_volatile(sync, l.head)) else {
        return RlFastUnlock::Decline;
    };
    if rl_fast_has_successor(&*ctx, head, &base) != Some(false) {
        return RlFastUnlock::Decline;
    }
    rl_fast_set_owner(ctx, sync, &l, None);
    ctx.set_field_volatile(sync, l.state, Value::Int(0));
    // `signalNext(head)` reads `head` and `head.next` after the releasing
    // write; a waiter that queued since the check above is woken by the
    // JDK's own `signalNext`.
    match rl_fast_ref(ctx.get_field_volatile(sync, l.head)) {
        Some(Some(h)) if rl_fast_has_successor(&*ctx, Some(h), &base) != Some(false) => {
            RlFastUnlock::ServedWake(h)
        }
        _ => RlFastUnlock::Served,
    }
}

/// Re-enter `ReentrantLock`'s own JDK body `RL_FAST_BODIES[which]`, by its
/// declared-method hint when the VM keeps one.
///
/// gci i17/vt (`gci-i15orch-virtual-threads-never-start-under-nojit-so-300-thread-probes-hang-FIXED-20261015`):
/// the intrinsic's frame is TRANSPARENT for this nested call
/// (`continuation_pin::transparent`). Its only post-call work is handing the
/// body's answer back unchanged (the same method and descriptor: `lock()V`,
/// `unlock()V`, `lockInterruptibly()V`, `tryLock()Z`) and refreshing a
/// thread-local method-index hint, which a skipped store only costs a
/// re-resolution. HotSpot has no native frame here at all (`ReentrantLock`
/// is pure Java), so a virtual thread that parks inside the body -- a
/// contended `lock()` -- must unmount. Counted as an opaque native it pinned
/// instead (`native_callee_memo::continuation_pinned_by_native`: the
/// intrinsic plus `doYield` is depth 2): `VirtualThread.parkOnCarrierThread`
/// held the carrier, and under `--nojit` (the only VM that registers this
/// intrinsic by default) 32 virtual threads blocked on a lock held by the
/// main thread took every carrier of a 32-way scheduler, so the queued ones
/// never ran their first statement.
pub fn rl_fast_jdk_body(ctx: &mut dyn NativeContext, which: usize, args: &[Value]) -> MethodCallResult {
    let (name, desc) = RL_FAST_BODIES[which];
    let vm = ctx.vm_identity();
    let _transparent = cratonvm_native_api::continuation_pin::transparent();
    let Some(base) = rl_fast_base(&*ctx, vm) else {
        return ctx.invoke_special_bytecode_only(RL_FAST_CLASS, name, desc, args);
    };
    let cid = base.rl_class;
    let mut hint = RL_FAST_BODY_HINTS
        .with(std::cell::Cell::get)
        .get(which)
        .copied()
        .flatten()
        .filter(|(v, c, _)| vm != 0 && *v == vm && *c == cid)
        .map(|(_, _, i)| i);
    let answer =
        ctx.invoke_special_bytecode_only_hinted(cid, &mut hint, RL_FAST_CLASS, name, desc, args);
    if vm != 0 {
        RL_FAST_BODY_HINTS.with(|h| {
            let mut rows = h.get();
            rows[which] = hint.map(|i| (vm, cid, i));
            h.set(rows);
        });
    }
    answer
}

/// The receiver of a no-argument instance call.
pub fn rl_fast_receiver(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(this))) => Some(*this),
        _ => None,
    }
}

/// `java/util/concurrent/locks/ReentrantLock.lock()V` under
/// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`.
pub fn native_rl_fast_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = rl_fast_receiver(args) {
        if rl_fast_lock(ctx, this) == RlFastLock::Served {
            return Ok(None);
        }
    }
    rl_fast_jdk_body(ctx, RL_FAST_BODY_LOCK, args)
}

/// `java/util/concurrent/locks/ReentrantLock.unlock()V` under
/// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`.
pub fn native_rl_fast_unlock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = rl_fast_receiver(args) {
        match rl_fast_unlock(ctx, this) {
            RlFastUnlock::Served => return Ok(None),
            RlFastUnlock::ServedWake(head) => {
                ctx.invoke_static(
                    RL_FAST_AQS,
                    "signalNext",
                    RL_FAST_SIGNAL_NEXT_DESC,
                    &[Value::Object(Some(head))],
                )?;
                return Ok(None);
            }
            RlFastUnlock::Decline => {}
        }
    }
    rl_fast_jdk_body(ctx, RL_FAST_BODY_UNLOCK, args)
}

/// `java/util/concurrent/locks/ReentrantLock.lockInterruptibly()V` under
/// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE`: an interrupted caller
/// runs the JDK body (which clears the status and throws); else `lock()`'s
/// shapes.
pub fn native_rl_fast_lock_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(this) = rl_fast_receiver(args) {
        if !rl_fast_interrupt_pending(ctx) && rl_fast_lock(ctx, this) == RlFastLock::Served {
            return Ok(None);
        }
    }
    rl_fast_jdk_body(ctx, RL_FAST_BODY_LOCK_INTERRUPTIBLY, args)
}

/// `java/util/concurrent/locks/ReentrantLock.tryLock()Z` under
/// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE`.
pub fn native_rl_fast_try_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = rl_fast_receiver(args) {
        if let RlFastTry::Answer(taken) = rl_fast_try_lock(ctx, this) {
            return Ok(Some(Value::Int(i32::from(taken))));
        }
    }
    rl_fast_jdk_body(ctx, RL_FAST_BODY_TRY_LOCK, args)
}

/// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL` (diagnostic): `lock()` always
/// re-enters the JDK body, pricing the fallback.
pub fn native_rl_fast_lock_decline_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    rl_fast_jdk_body(ctx, RL_FAST_BODY_LOCK, args)
}

/// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL` (diagnostic): `unlock()` always
/// re-enters the JDK body.
pub fn native_rl_fast_unlock_decline_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    rl_fast_jdk_body(ctx, RL_FAST_BODY_UNLOCK, args)
}

/// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL` (diagnostic) with the wide
/// rows: `lockInterruptibly()` always re-enters the JDK body.
pub fn native_rl_fast_lock_interruptibly_decline_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    rl_fast_jdk_body(ctx, RL_FAST_BODY_LOCK_INTERRUPTIBLY, args)
}

/// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL` (diagnostic) with the wide
/// rows: `tryLock()` always re-enters the JDK body.
pub fn native_rl_fast_try_lock_decline_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    rl_fast_jdk_body(ctx, RL_FAST_BODY_TRY_LOCK, args)
}

/// Does this registry take the `ReentrantLock` fast path? The switch over a
/// real JDK of a known-new-enough AQS (JDK 17's rewrite: `initialTryLock`,
/// `signalNext`; an unknown version counts as the newest) with the real AQS
/// in force (`CRATONVM_SYNTHETIC_AQS` registers the legacy stand-ins on the
/// same triples instead), in a VM where an interpreted-only intrinsic
/// registers.
pub fn reentrant_lock_fast_applies(registry: &NativeMethodRegistry, real_aqs: bool) -> bool {
    // Default on since the wave-77 triage, and only in a VM without a JIT:
    // `--nojit` lock-unlock 2819 -> 867 ns, but with the JIT the compiled
    // JDK body is close (785 -> 619) and a declined call (fair queue,
    // hand-off) pays a ~5 us re-entry (fair 459 -> 760, handoff 1977 -> 2258).
    // Wave 80: `interpreted_intrinsic_registers` is `!jit_enabled()` unless
    // `CRATONVM_INTRINSIC_BODY_COMPILES` (wave 79, default off) lets a JIT VM
    // register the rows too, marked body-compilable below.
    registry.reentrant_lock_uncontended_fast()
        && registry.interpreted_intrinsic_registers()
        && real_aqs
        && !registry.real_jdk_older_than(17)
}

/// Register the `ReentrantLock` intrinsics when
/// [`reentrant_lock_fast_applies`]: `lock()` / `unlock()`, and with
/// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE` `lockInterruptibly()` /
/// `tryLock()`; the kind is stated here (AGENTS.md). Each row is marked
/// body-compilable (a no-op without `CRATONVM_INTRINSIC_BODY_COMPILES`).
pub fn register_reentrant_lock_fast(registry: &mut NativeMethodRegistry, real_aqs: bool) {
    if !reentrant_lock_fast_applies(registry, real_aqs) {
        return;
    }
    let decline_all = registry.reentrant_lock_fast_decline_all();
    let (lock, unlock) = if decline_all {
        (
            native_rl_fast_lock_decline_all as NativeCallback,
            native_rl_fast_unlock_decline_all as NativeCallback,
        )
    } else {
        (
            native_rl_fast_lock as NativeCallback,
            native_rl_fast_unlock as NativeCallback,
        )
    };
    let mut rows = vec![(RL_FAST_BODY_LOCK, lock), (RL_FAST_BODY_UNLOCK, unlock)];
    if registry.reentrant_lock_fast_wide() {
        let (lock_interruptibly, try_lock) = if decline_all {
            (
                native_rl_fast_lock_interruptibly_decline_all as NativeCallback,
                native_rl_fast_try_lock_decline_all as NativeCallback,
            )
        } else {
            (
                native_rl_fast_lock_interruptibly as NativeCallback,
                native_rl_fast_try_lock as NativeCallback,
            )
        };
        rows.push((RL_FAST_BODY_LOCK_INTERRUPTIBLY, lock_interruptibly));
        rows.push((RL_FAST_BODY_TRY_LOCK, try_lock));
    }
    for (which, callback) in rows {
        let (name, desc) = RL_FAST_BODIES[which];
        registry.register(RL_FAST_CLASS, name, desc, callback, NativeKind::Intrinsic);
        registry.mark_body_compilable_intrinsic(RL_FAST_CLASS, name, desc);
    }
}


// ===========================================================================
// M18: ConcurrentHashMap atomic ops, LinkedBlockingQueue, ArrayBlockingQueue
// ===========================================================================

// Interpreter round i3 wave 79, lane L1
// (`i76-L1-m18-chm-natives-are-overwritten-and-their-foreach-reads-no-real-layout`):
// the six `ConcurrentHashMap` rows that opened this registrar (`putIfAbsent`,
// `computeIfAbsent`, `compute`, `merge`, `getOrDefault`, `forEach`) are
// deleted. Its only caller is `register_synthetic_overrides` (synthetic-jdk
// builds, the synthetic class library's `register_builtins`), never the
// real-JDK boot path, and there `register_collections_natives` runs after it
// and registers the same six triples (`native-collections`
// `register_concurrent_hashmap_natives`), so last-write-wins overwrote every
// one; their `forEach` read slot 0 as an entries array no library has. What is
// left is the synthetic-layout LBQ / ABQ block, gated like its caller.
#[cfg(feature = "synthetic-jdk")]
pub fn register_m18_concurrent_fixes(registry: &mut NativeMethodRegistry) {
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
            let this = uce_store_fresh_ref_array(ctx, this, 16);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX));
            Ok(None)
        }, NativeKind::SyntheticStub);
        registry.register(lbq, "<init>", "(I)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cap = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => i32::MAX,
            };
            let this = uce_store_fresh_ref_array(ctx, this, cap.min(64) as usize);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(cap));
            Ok(None)
        }, NativeKind::SyntheticStub);
        registry.register(lbq, "<init>", "(Ljava/util/Collection;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // gc-common w19-b: the queue and the source collection, raw copies
            // out of `args`, are pinned from the start. The backing array's
            // allocation and `iterator()` can both collect, and both objects
            // were used after them through their earlier addresses.
            let this_pin = ctx.pin_native_root(this);
            let coll = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            let coll_pin = coll.map(|c| ctx.pin_native_root(c));
            let this = uce_store_fresh_ref_array(ctx, this, 16);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX));
            if let (Some(coll), Some(coll_pin)) = (coll, coll_pin) {
                let coll = ctx.read_native_pin(coll_pin, coll);
                if let Some(Value::Object(Some(iter))) =
                    ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[])?
                {
                    // GC-safety: `hasNext`/`next` are real bytecode and
                    // `m18_lbq_add_internal` grows a backing array; the queue
                    // being filled and the iterator driving it are both carried
                    // in from outside.
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
                        // The next turn re-reads the pin.
                        let _ = m18_lbq_add_internal(ctx, this, elem);
                    }
                }
            }
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        }, NativeKind::SyntheticStub);
        registry.register(lbq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        }, NativeKind::SyntheticStub);
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
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, NativeKind::SyntheticStub);
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
                let this = m18_lbq_add_internal(ctx, this, elem);
                ctx.monitor_notify_all(this)?;
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            }, NativeKind::SyntheticStub,
        );
        registry.register(lbq, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, NativeKind::SyntheticStub);
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
                // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
                // the JDK's `take` parks in `notEmpty.await()`: WAITING, PARKED.
                this = monitor_wait_keepalive_and_exit_as(
                    ctx,
                    this,
                    Some(10),
                    cratonvm_native_api::BlockKind::Parked,
                )?;
            }
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
                    // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
                    // the JDK's timed `poll` parks in `notEmpty.awaitNanos`:
                    // TIMED_WAITING, PARKED.
                    this = monitor_wait_keepalive_and_exit_as(
                        ctx,
                        this,
                        Some(wait_ms),
                        cratonvm_native_api::BlockKind::TimedParked,
                    )?;
                }
            }, NativeKind::SyntheticStub,
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
            Ok(Some(ctx.get_array_element(arr, 0)))        }, NativeKind::SyntheticStub);
        registry.register(lbq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
            // gc-common w19-b: the backing array is pinned across the copy's
            // allocation (it was read through its earlier address).
            let arr_pin = ctx.pin_native_root(arr);
            let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            for i in 0..size {
                ctx.set_array_element(result, i, ctx.get_array_element(arr, i));
            }
            Ok(Some(Value::Object(Some(result))))
        }, NativeKind::SyntheticStub);
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
            // gc-common w19-b: the backing array is rooted across the
            // snapshot's allocation and the snapshot across the iterator's;
            // each was used after the next allocation through its earlier
            // address.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr_h = scope.root(arr);
            let snap = scope.new_array(cratonvm_types::ArrayElementType::Reference, size);
            let arr = scope.get(&arr_h);
            for i in 0..size {
                let elem = scope.get_array_element(arr, i);
                scope.set_array_element(snap, i, elem);
            }
            let snap_h = scope.root(snap);
            let iter = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList$Itr", 3)?;
            let snap = scope.get(&snap_h);
            let ctx = &mut *scope;
            ctx.set_field(iter, 0, Value::Object(Some(snap)));
            ctx.set_field(iter, 1, Value::Int(size as i32));
            ctx.set_field(iter, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(iter))))
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);

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
            let this = uce_store_fresh_ref_array(ctx, this, cap);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        }, NativeKind::SyntheticStub);
        registry.register(abq, "<init>", "(IZ)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cap = match args.get(1) {
                Some(Value::Int(v)) => (*v).max(1) as usize,
                _ => 16,
            };
            let this = uce_store_fresh_ref_array(ctx, this, cap);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
            }, NativeKind::SyntheticStub,
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
            Ok(Some(ctx.get_array_element(arr, head)))        }, NativeKind::SyntheticStub);
        registry.register(abq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
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
            // gc-common w19-b: the backing array is pinned across the copy's
            // allocation (it was read through its earlier address).
            let arr_pin = ctx.pin_native_root(arr);
            let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size as usize);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            for i in 0..size {
                let idx = ((head + i) % cap) as usize;
                ctx.set_array_element(result, i as usize, ctx.get_array_element(arr, idx));
            }
            Ok(Some(Value::Object(Some(result))))
        }, NativeKind::SyntheticStub);
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
        }, NativeKind::SyntheticStub);
    } // end of #[cfg(feature = "synthetic-jdk")] block for LBQ/ABQ
}

// ===========================================================================
// T3.1: ConcurrentSkipListMap, LinkedTransferQueue, Flow
// ===========================================================================

/// gc-common w13-f: root a `ConcurrentSkipListMap` lookup key — when it is an
/// object — across the `compareTo` probes, each of which runs Java.
pub fn cslm_root_key(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    key: Value,
) -> Option<cratonvm_native_api::NativeHandle> {
    match key {
        Value::Object(Some(k)) => Some(scope.root(k)),
        _ => None,
    }
}

/// The lookup key as it is NOW: re-read off its handle, or the non-object
/// value exactly as it was passed.
pub fn cslm_key_now(
    scope: &cratonvm_native_api::NativeHandleScope<'_>,
    key_h: Option<&cratonvm_native_api::NativeHandle>,
    key: Value,
) -> Value {
    match key_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => key,
    }
}

