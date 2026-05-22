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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::ArrayElementType;
use cratonvm_types::{ObjectRef, Value};
use cratonvm_types::error::MethodCallResult;

use crate::alloc_concurrent_synthetic;

/// Allocate a wrapper object with class name `cls` (e.g. `java/lang/Integer`).
fn alloc_letsgo_wrapper(ctx: &mut dyn NativeContext, cls: &str) -> Option<ObjectRef> {
    ctx.allocate_instance(cls)
}

pub fn register_letsgo_compat_natives(registry: &mut NativeMethodRegistry) {
    register_wrapper_value_of(registry);
    register_wrapper_unbox(registry);
    register_security_fallbacks(registry);
    // `drainTo` synthetic overrides assume slot 0=array, 1=size — that's the
    // synthetic-jdk LBQ/ABQ layout.  On real JDK 25 LBQ those slots are
    // head/last (Node refs), so the synthetic native silently misreads.  More
    // importantly, registering this override is paired with synthetic LBQ
    // <init> overrides elsewhere (m18, native-collections) that leave real
    // putLock/takeLock null.  Gate so real-JDK bytecode (which works) runs.
    #[cfg(feature = "synthetic-jdk")]
    register_blocking_queue_drain_to(registry);
    register_atomic_compat(registry);
}

/// `java.util.concurrent.atomic.{AtomicBoolean, AtomicInteger, AtomicLong}`
/// constructors / accessors. Real-JDK bytecode for these reads internal
/// `Unsafe` field offsets we don't model.  The registrations live in
/// `register_atomic_boolean_natives` etc. but those are only wired into
/// `register_synthetic_overrides`. Re-register the layout-safe versions
/// here so real-JDK boot can allocate Logback's
/// `COWArrayList(_AtomicBoolean field)` and Spring's `AtomicInteger`
/// counters without NSME.
fn register_atomic_compat(r: &mut NativeMethodRegistry) {
    register_atomic_boolean_compat(r);
    register_atomic_integer_compat(r);
    register_atomic_long_compat(r);
    register_atomic_reference_compat(r);
}

fn register_atomic_boolean_compat(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/atomic/AtomicBoolean";
    r.register(c, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, 0, Value::Int(0));
        }
        Ok(None)
    });
    r.register(c, "<init>", "(Z)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "get", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })))
    });
    r.register(c, "set", "(Z)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "lazySet", "(Z)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "compareAndSet", "(ZZ)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let expected = match args.get(1) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
        let update = match args.get(2) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        if current == expected {
            ctx.set_field(this, 0, Value::Int(update));
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(c, "getAndSet", "(Z)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let new_val = match args.get(1) { Some(Value::Int(n)) if *n != 0 => 1, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Int(new_val));
        Ok(Some(Value::Int(if current != 0 { 1 } else { 0 })))
    });
}

fn register_atomic_integer_compat(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/atomic/AtomicInteger";
    r.register(c, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, 0, Value::Int(0));
        }
        Ok(None)
    });
    r.register(c, "<init>", "(I)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "get", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(match ctx.get_field(this, 0) {
            Value::Int(n) => Value::Int(n),
            _ => Value::Int(0),
        }))
    });
    r.register(c, "set", "(I)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "lazySet", "(I)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
            ctx.set_field(*this, 0, Value::Int(v));
        }
        Ok(None)
    });
    r.register(c, "incrementAndGet", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        let nv = v.wrapping_add(1);
        ctx.set_field(this, 0, Value::Int(nv));
        Ok(Some(Value::Int(nv)))
    });
    r.register(c, "decrementAndGet", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        let nv = v.wrapping_sub(1);
        ctx.set_field(this, 0, Value::Int(nv));
        Ok(Some(Value::Int(nv)))
    });
    r.register(c, "getAndIncrement", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Int(v.wrapping_add(1)));
        Ok(Some(Value::Int(v)))
    });
    r.register(c, "getAndDecrement", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Int(v.wrapping_sub(1)));
        Ok(Some(Value::Int(v)))
    });
    r.register(c, "getAndAdd", "(I)I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let d = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Int(v.wrapping_add(d)));
        Ok(Some(Value::Int(v)))
    });
    r.register(c, "addAndGet", "(I)I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let d = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        let nv = v.wrapping_add(d);
        ctx.set_field(this, 0, Value::Int(nv));
        Ok(Some(Value::Int(nv)))
    });
    r.register(c, "compareAndSet", "(II)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let expected = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
        let update = match args.get(2) { Some(Value::Int(n)) => *n, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        if current == expected {
            ctx.set_field(this, 0, Value::Int(update));
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(c, "getAndSet", "(I)I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let new_val = match args.get(1) { Some(Value::Int(n)) => *n, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Int(new_val));
        Ok(Some(Value::Int(current)))
    });
    r.register(c, "intValue", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(match ctx.get_field(this, 0) {
            Value::Int(n) => Value::Int(n),
            _ => Value::Int(0),
        }))
    });
    r.register(c, "longValue", "()J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Int(n) => n, _ => 0 };
        Ok(Some(Value::Long(v as i64)))
    });
}

fn register_atomic_long_compat(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/atomic/AtomicLong";
    r.register(c, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, 0, Value::Long(0));
        }
        Ok(None)
    });
    r.register(c, "<init>", "(J)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
            ctx.set_field(*this, 0, Value::Long(v));
        }
        Ok(None)
    });
    r.register(c, "get", "()J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        Ok(Some(match ctx.get_field(this, 0) {
            Value::Long(n) => Value::Long(n),
            _ => Value::Long(0),
        }))
    });
    r.register(c, "set", "(J)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
            ctx.set_field(*this, 0, Value::Long(v));
        }
        Ok(None)
    });
    r.register(c, "incrementAndGet", "()J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        let nv = v.wrapping_add(1);
        ctx.set_field(this, 0, Value::Long(nv));
        Ok(Some(Value::Long(nv)))
    });
    r.register(c, "decrementAndGet", "()J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let v = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        let nv = v.wrapping_sub(1);
        ctx.set_field(this, 0, Value::Long(nv));
        Ok(Some(Value::Long(nv)))
    });
    r.register(c, "compareAndSet", "(JJ)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let expected = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
        let update = match args.get(2) { Some(Value::Long(n)) => *n, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        if current == expected {
            ctx.set_field(this, 0, Value::Long(update));
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(c, "getAndSet", "(J)J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let new_val = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
        let current = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Long(new_val));
        Ok(Some(Value::Long(current)))
    });
    r.register(c, "addAndGet", "(J)J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let d = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
        let v = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        let nv = v.wrapping_add(d);
        ctx.set_field(this, 0, Value::Long(nv));
        Ok(Some(Value::Long(nv)))
    });
    r.register(c, "getAndAdd", "(J)J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Long(0))),
        };
        let d = match args.get(1) { Some(Value::Long(n)) => *n, _ => 0 };
        let v = match ctx.get_field(this, 0) { Value::Long(n) => n, _ => 0 };
        ctx.set_field(this, 0, Value::Long(v.wrapping_add(d)));
        Ok(Some(Value::Long(v)))
    });
}

fn register_atomic_reference_compat(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/atomic/AtomicReference";
    r.register(c, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, 0, Value::Object(None));
        }
        Ok(None)
    });
    r.register(c, "<init>", "(Ljava/lang/Object;)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(*this, 0, v);
        }
        Ok(None)
    });
    r.register(c, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(c, "set", "(Ljava/lang/Object;)V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            let v = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(*this, 0, v);
        }
        Ok(None)
    });
    r.register(c, "compareAndSet", "(Ljava/lang/Object;Ljava/lang/Object;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let expected = args.get(1).copied().unwrap_or(Value::Object(None));
        let update = args.get(2).copied().unwrap_or(Value::Object(None));
        let current = ctx.get_field(this, 0);
        let same = match (current, expected) {
            (Value::Object(a), Value::Object(b)) => a == b,
            _ => false,
        };
        if same {
            ctx.set_field(this, 0, update);
            Ok(Some(Value::Int(1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(c, "getAndSet", "(Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
        let current = ctx.get_field(this, 0);
        ctx.set_field(this, 0, new_val);
        Ok(Some(current))
    });
}

/// Wrapper unboxing methods (`Wrapper.primValue()`).  Logback's
/// `Loader.<clinit>` calls `Boolean.booleanValue()` on the constant
/// `Boolean.TRUE`. When real-JDK bytecode for the wrapper class fails to
/// resolve `booleanValue/intValue/etc.`, we fall through to NSME.
/// Provide layout-safe getters that read the boxed primitive from
/// `field 0` (matching our wrapper convention).
pub fn register_wrapper_unbox(r: &mut NativeMethodRegistry) {
    r.register("java/lang/Boolean", "booleanValue", "()Z", unbox_int_field);
    r.register("java/lang/Byte", "byteValue", "()B", unbox_int_field);
    r.register("java/lang/Short", "shortValue", "()S", unbox_int_field);
    r.register("java/lang/Character", "charValue", "()C", unbox_int_field);
    r.register("java/lang/Integer", "intValue", "()I", unbox_int_field);
    r.register("java/lang/Long", "longValue", "()J", unbox_long_field);
    r.register("java/lang/Float", "floatValue", "()F", unbox_float_field);
    r.register("java/lang/Double", "doubleValue", "()D", unbox_double_field);
}

fn unbox_int_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(match ctx.get_field(this, 0) {
        Value::Int(n) => Value::Int(n),
        _ => Value::Int(0),
    }))
}

fn unbox_long_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(match ctx.get_field(this, 0) {
        Value::Long(n) => Value::Long(n),
        Value::Int(n) => Value::Long(n as i64),
        _ => Value::Long(0),
    }))
}

fn unbox_float_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    Ok(Some(match ctx.get_field(this, 0) {
        Value::Float(f) => Value::Float(f),
        _ => Value::Float(0.0),
    }))
}

fn unbox_double_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    Ok(Some(match ctx.get_field(this, 0) {
        Value::Double(d) => Value::Double(d),
        _ => Value::Double(0.0),
    }))
}

fn boxed_int_field(ctx: &mut dyn NativeContext, args: &[Value], cls: &'static str) -> MethodCallResult {
    let v = match args.first() { Some(Value::Int(n)) => *n, _ => 0 };
    let Some(w) = alloc_letsgo_wrapper(ctx, cls) else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.set_field(w, 0, Value::Int(v));
    Ok(Some(Value::Object(Some(w))))
}

fn box_integer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `Integer.valueOf(int)` MUST return the canonical cached instance for
    // values in [-128, 127] (JLS §5.1.7 / `IntegerCache`); real JDK code
    // does `return IntegerCache.cache[...]`. Reference-identity checks
    // (`Integer.valueOf(x) == Integer.valueOf(x)`) depend on this. Allocating
    // a fresh wrapper here broke that invariant — delegate to the
    // cache-preserving implementation in `lang_math`, exactly as
    // `box_boolean` does for the canonical `Boolean.TRUE`/`FALSE`.
    crate::lang_math::native_integer_value_of(ctx, args)
        .or_else(|_| boxed_int_field(ctx, args, "java/lang/Integer"))
}
fn box_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    boxed_int_field(ctx, args, "java/lang/Short")
}
fn box_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    boxed_int_field(ctx, args, "java/lang/Byte")
}
fn box_character(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    boxed_int_field(ctx, args, "java/lang/Character")
}
fn box_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() { Some(Value::Int(n)) => *n, _ => 0 };
    // `Boolean.valueOf(boolean)` MUST return the canonical `Boolean.TRUE` /
    // `Boolean.FALSE` static-field instances (real JDK: `return b ? TRUE :
    // FALSE`). Reference-identity checks against `Boolean.TRUE` depend on
    // this — notably Xerces' `XML11Configuration.configurePipeline()`, which
    // selects the namespace-aware scanner via
    // `fFeatures.get(".../namespaces") == Boolean.TRUE`. Allocating a fresh
    // wrapper here broke that comparison and silently disabled namespace
    // processing. Delegate to the canonical implementation in `lang_math`.
    crate::lang_math::native_boolean_value_of(ctx, args).or_else(|_| {
        // Defensive fallback: if delegation somehow fails, preserve the old
        // allocate-a-wrapper behaviour rather than propagating an error.
        let Some(w) = alloc_letsgo_wrapper(ctx, "java/lang/Boolean") else {
            return Ok(Some(Value::Object(None)));
        };
        ctx.set_field(w, 0, Value::Int(if v != 0 { 1 } else { 0 }));
        Ok(Some(Value::Object(Some(w))))
    })
}

fn box_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() { Some(Value::Long(n)) => *n, _ => 0 };
    let Some(w) = alloc_letsgo_wrapper(ctx, "java/lang/Long") else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.set_field(w, 0, Value::Long(v));
    Ok(Some(Value::Object(Some(w))))
}

fn box_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() { Some(Value::Float(n)) => *n, _ => 0.0 };
    let Some(w) = alloc_letsgo_wrapper(ctx, "java/lang/Float") else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.set_field(w, 0, Value::Float(v));
    Ok(Some(Value::Object(Some(w))))
}

fn box_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() { Some(Value::Double(n)) => *n, _ => 0.0 };
    let Some(w) = alloc_letsgo_wrapper(ctx, "java/lang/Double") else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.set_field(w, 0, Value::Double(v));
    Ok(Some(Value::Object(Some(w))))
}

/// `Wrapper.valueOf(prim)` for every boxing static. Layout matches our
/// wrapper convention (`field 0 = primitive value`).
pub fn register_wrapper_value_of(r: &mut NativeMethodRegistry) {
    r.register("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", box_integer);
    r.register("java/lang/Short", "valueOf", "(S)Ljava/lang/Short;", box_short);
    r.register("java/lang/Byte", "valueOf", "(B)Ljava/lang/Byte;", box_byte);
    r.register("java/lang/Character", "valueOf", "(C)Ljava/lang/Character;", box_character);
    r.register("java/lang/Boolean", "valueOf", "(Z)Ljava/lang/Boolean;", box_boolean);
    r.register("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;", box_long);
    r.register("java/lang/Float", "valueOf", "(F)Ljava/lang/Float;", box_float);
    r.register("java/lang/Double", "valueOf", "(D)Ljava/lang/Double;", box_double);
}

fn no_op(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `AccessController.checkPermission` — no-op (no SecurityManager
/// installed).  Matches HotSpot's behaviour since JDK 9 when the
/// SecurityManager has been removed.
pub fn register_security_fallbacks(r: &mut NativeMethodRegistry) {
    r.register(
        "java/security/AccessController",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        no_op,
    );
    // Real-JDK mode never runs `register_builtins` → `phases_early` JCA block,
    // but Tomcat `SessionIdGeneratorBase.<clinit>` needs a non-empty
    // `Security.getAlgorithms("SecureRandom")`. `vm_exec` can force native
    // dispatch only when this registration exists (see Security.getAlgorithms
    // allow-list there).
    r.register(
        "java/security/Security",
        "getAlgorithms",
        "(Ljava/lang/String;)Ljava/util/Set;",
        |ctx, args| {
            let type_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let algos: &[&str] = match type_name.as_str() {
                "MessageDigest" => &["MD5", "SHA-1", "SHA-256", "SHA-384", "SHA-512"],
                "Cipher" => {
                    &[
                        "AES",
                        "AES/CBC/PKCS5Padding",
                        "AES/CBC/NoPadding",
                        "AES/ECB/PKCS5Padding",
                        "AES/GCM/NoPadding",
                    ]
                }
                "Mac" => {
                    &[
                        "HmacSHA1",
                        "HmacSHA256",
                        "HmacSHA384",
                        "HmacSHA512",
                        "HmacMD5",
                    ]
                }
                "Signature" => {
                    &[
                        "SHA256withRSA",
                        "SHA384withRSA",
                        "SHA512withRSA",
                        "SHA256withECDSA",
                    ]
                }
                "KeyPairGenerator" => &["RSA", "EC", "DSA"],
                "KeyGenerator" => &["AES", "DESede", "HmacSHA256"],
                "SecureRandom" => {
                    &[
                        "NativePRNGNonBlocking",
                        "NativePRNGBlocking",
                        "SHA1PRNG",
                        "Windows-PRNG",
                    ]
                }
                _ => &[],
            };
            let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 1);
            let arr = ctx.new_array(ArrayElementType::Reference, algos.len());
            for (i, &algo) in algos.iter().enumerate() {
                let s = ctx.create_string(algo);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            ctx.set_field(set, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(set))))
        },
    );
}

/// `BlockingQueue.drainTo(Collection, int)` overload — required by SLF4J's
/// `LoggerFactory.replayEvents` and by Spring's `TaskExecutor` pools.
#[cfg(feature = "synthetic-jdk")]
fn register_blocking_queue_drain_to(r: &mut NativeMethodRegistry) {
    let lbq = "java/util/concurrent/LinkedBlockingQueue";
    r.register(lbq, "drainTo", "(Ljava/util/Collection;I)I", drain_to_lbq_bounded);

    let abq = "java/util/concurrent/ArrayBlockingQueue";
    r.register(abq, "drainTo", "(Ljava/util/Collection;I)I", drain_to_abq_bounded);

    let bq = "java/util/concurrent/BlockingQueue";
    r.register(bq, "drainTo", "(Ljava/util/Collection;)I", drain_to_iface_unbounded);
    r.register(bq, "drainTo", "(Ljava/util/Collection;I)I", drain_to_iface_bounded);
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
    let max = match args.get(2) { Some(Value::Int(n)) => *n, _ => i32::MAX };
    if max <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, 1) { Value::Int(n) => n, _ => 0 };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => { ctx.monitor_exit(this); return Ok(Some(Value::Int(0))); }
    };
    let n = size.min(max);
    for i in 0..n as usize {
        let elem = ctx.get_array_element(arr, i);
        ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    }
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
    let max = match args.get(2) { Some(Value::Int(n)) => *n, _ => i32::MAX };
    if max <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, 1) { Value::Int(n) => n, _ => 0 };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => { ctx.monitor_exit(this); return Ok(Some(Value::Int(0))); }
    };
    let cap = ctx.array_length(arr) as i32;
    let head = match ctx.get_field(this, 2) { Value::Int(n) => n, _ => 0 };
    let n = size.min(max);
    for i in 0..n {
        let idx = ((head + i) % cap.max(1)) as usize;
        let elem = ctx.get_array_element(arr, idx);
        ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    }
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
    let max = match args.get(2) { Some(Value::Int(n)) => Value::Int(*n), _ => Value::Int(i32::MAX) };
    let r = ctx.invoke_virtual(this, "drainTo", "(Ljava/util/Collection;I)I", &[coll, max])?;
    Ok(r.or(Some(Value::Int(0))))
}
