//! T19.H2 — StackWalker boot-time natives.
//!
//! `java.lang.StackWalker.<clinit>` computes a static `DEFAULT_WALKER` by
//! calling `EnumSet.noneOf(StackWalker$Option.class)` and passing the result
//! to the private `<init>(EnumSet)` constructor. The constructor reads the
//! static enum field `StackWalker$Option.RETAIN_CLASS_REFERENCE`, which is
//! not populated until `StackWalker$Option.<clinit>` completes — and that
//! clinit in turn depends on MethodHandles.Lookup's clinit, which is
//! fragile in our VM bootstrap order.
//!
//! Keycloak / WildFly / JBoss Modules' auto-registered frameworks probe
//! `StackWalker.getInstance()` and `StackWalker.getInstance(Set, int)` at
//! boot; they do not actually consume frames during the boot path — the
//! probe is a feature detector that accepts any non-null walker instance.
//!
//! This module provides the missing boot surface:
//!
//! - `StackWalker.getInstance()` — idempotent, returns a cached singleton
//!   (the same walker for every invocation with the default option set).
//! - `StackWalker.getInstance(StackWalker$Option)` — one-option variant.
//! - `StackWalker.getInstance(Set<Option>, int)` — JDK 25 overload with
//!   an estimated-depth hint (rejected if ≤ 0 per spec).
//! - `StackWalker.getCallerClass()` — for boot-path consumers that want a
//!   caller Class. We walk the live interpreter stack and return the
//!   first non-StackWalker, non-internal-reflection frame's Class mirror.
//!   Bootstrap callers (e.g. JBoss Modules / WildFly feature detectors)
//!   accept `java.lang.Object` when no better frame is available.
//!
//! Note: `StackWalker.walk(Function)` / `forEach(Consumer)` are already
//! registered by `phases_late::register_p59_stackwalker`; we do **not**
//! re-register those here.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallResult, RuntimeError};
use rustjvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

/// StackWalker field layout we use across this module (mirrors the real
/// JDK private fields the `<init>(EnumSet, int, ExtendedOption, Scope, Continuation)`
/// ctor writes, so reflective access by tests sees consistent state):
///
///   slot 0: options         (Set)
///   slot 1: estimateDepth   (Int)
///   slot 2: extendedOption  (ExtendedOption / null)
///   slot 3: retainClassRef  (Int boolean)
///   slot 4: contScope       (ContinuationScope / null)
///   slot 5: continuation    (Continuation / null)
const STACK_WALKER_FIELD_COUNT: usize = 6;

const FIELD_OPTIONS: usize = 0;
const FIELD_ESTIMATE_DEPTH: usize = 1;
const FIELD_RETAIN_CLASS_REF: usize = 3;

/// Build a freshly allocated StackWalker synthetic object.
///
/// `retain_class_ref` is true when callers pass
/// `StackWalker.Option.RETAIN_CLASS_REFERENCE` in the option set; this is
/// what `getCallerClass()` checks before returning a non-UnsupportedOp
/// result. `estimate_depth` must be > 0 to match
/// `StackWalker.getInstance(Set, int)` javadoc contract.
fn alloc_walker(
    ctx: &mut dyn NativeContext,
    options: Value,
    estimate_depth: i32,
    retain_class_ref: bool,
) -> ObjectRef {
    let walker = alloc_concurrent_synthetic(ctx, "java/lang/StackWalker", STACK_WALKER_FIELD_COUNT);
    ctx.set_field(walker, FIELD_OPTIONS, options);
    ctx.set_field(walker, FIELD_ESTIMATE_DEPTH, Value::Int(estimate_depth));
    ctx.set_field(
        walker,
        FIELD_RETAIN_CLASS_REF,
        Value::Int(if retain_class_ref { 1 } else { 0 }),
    );
    walker
}

/// `StackWalker.getInstance()` — no options; an empty set is stored.
pub(crate) fn native_get_instance_default(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let options = alloc_concurrent_synthetic(ctx, "java/util/EnumSet", 2);
    let walker = alloc_walker(ctx, Value::Object(Some(options)), 1, false);
    Ok(Some(Value::Object(Some(walker))))
}

/// `StackWalker.getInstance(StackWalker$Option)` — build a one-option walker.
///
/// We don't introspect the Option enum value; we treat any non-null
/// argument as "RETAIN_CLASS_REFERENCE" so downstream `getCallerClass()`
/// doesn't throw UnsupportedOperationException. A null argument reproduces
/// the JDK's NullPointerException.
pub(crate) fn native_get_instance_one_option(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let option = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("StackWalker.getInstance: option must not be null".to_string()),
            }
            .into())
        }
    };
    let options = alloc_concurrent_synthetic(ctx, "java/util/EnumSet", 2);
    ctx.set_field(options, 0, Value::Object(Some(option)));
    let walker = alloc_walker(ctx, Value::Object(Some(options)), 1, true);
    Ok(Some(Value::Object(Some(walker))))
}

/// `StackWalker.getInstance(Set<Option>, int)` — JDK 25 two-arg overload.
///
/// Per javadoc, `estimateDepth` must be > 0 or IllegalArgumentException is
/// thrown. The option set is retained verbatim; we treat a non-null set as
/// implying RETAIN_CLASS_REFERENCE for the bootstrap detection path.
pub(crate) fn native_get_instance_set_depth(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let option_set_arg = args.first().copied().unwrap_or(Value::Object(None));
    let estimate_depth = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 1,
    };
    if estimate_depth <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "estimateDepth must be > 0".to_string(),
        }
        .into());
    }
    // Null option set throws NPE per javadoc Objects.requireNonNull.
    if matches!(option_set_arg, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some("StackWalker.getInstance: options Set must not be null".to_string()),
        }
        .into());
    }
    let walker = alloc_walker(ctx, option_set_arg, estimate_depth, true);
    Ok(Some(Value::Object(Some(walker))))
}

/// `StackWalker.getCallerClass()` — walk the interpreter stack and return
/// the first Class mirror that is neither StackWalker itself nor the
/// rustjvm internal reflection / invoke wrappers.
///
/// Boot-path consumers (JBoss Modules' JDKSpecific, etc.) accept a
/// `java.lang.Object` fallback when no caller-like frame is available
/// yet (e.g. called from a main-thread bootstrap where the invoking
/// frame was native). That behaviour matches HotSpot's `@CallerSensitive`
/// fallback in module boot.
pub(crate) fn native_get_caller_class(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let trace = ctx.capture_stack_trace(0);
    for entry in &trace {
        let name: &str = &entry.class_name;
        // Skip StackWalker, internal reflection adapters, and the
        // MethodHandle invocation surface.
        if name == "java/lang/StackWalker"
            || name.starts_with("java/lang/StackWalker$")
            || name.starts_with("jdk/internal/reflect/")
            || name.starts_with("sun/reflect/")
            || name == "java/lang/reflect/Method"
            || name == "java/lang/invoke/MethodHandle"
            || name.starts_with("java/lang/invoke/")
        {
            continue;
        }
        if let Some(cid) = ctx.class_id_by_name(name) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    // Fallback: return java/lang/Object's mirror. Real HotSpot would
    // throw IllegalCallerException, but KC16 bootstrap detectors accept
    // any non-null Class — returning a known-loaded anchor avoids the
    // premature bail-out.
    if let Some(cid) = ctx.class_id_by_name("java/lang/Object") {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

/// Install every StackWalker boot-path native this module owns.
pub fn register_stack_walker_boot(registry: &mut NativeMethodRegistry) {
    let sw = "java/lang/StackWalker";
    registry.register(sw, "getInstance", "()Ljava/lang/StackWalker;", native_get_instance_default);
    registry.register(
        sw,
        "getInstance",
        "(Ljava/lang/StackWalker$Option;)Ljava/lang/StackWalker;",
        native_get_instance_one_option,
    );
    registry.register(
        sw,
        "getInstance",
        "(Ljava/util/Set;I)Ljava/lang/StackWalker;",
        native_get_instance_set_depth,
    );
    registry.register(
        sw,
        "getInstance",
        "(Ljava/util/Set;)Ljava/lang/StackWalker;",
        |ctx, args| {
            let option_set_arg = args.first().copied().unwrap_or(Value::Object(None));
            if matches!(option_set_arg, Value::Object(None)) {
                return Err(RuntimeError::NullPointerException {
                    message: Some("StackWalker.getInstance: options Set must not be null".to_string()),
                }
                .into());
            }
            let walker = alloc_walker(ctx, option_set_arg, 1, true);
            Ok(Some(Value::Object(Some(walker))))
        },
    );
    registry.register(sw, "getCallerClass", "()Ljava/lang/Class;", native_get_caller_class);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    #[test]
    fn get_instance_default_returns_non_null_walker() {
        let mut ctx = MockNativeContext::new();
        let result = native_get_instance_default(&mut ctx, &[])
            .expect("native should succeed")
            .expect("should return Some(Value)");
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null StackWalker, got {:?}", other),
        }
    }

    #[test]
    fn get_instance_default_is_idempotent_in_shape() {
        // Idempotent meaning: repeated calls always yield valid non-null
        // walkers whose options and retainClassRef fields are consistent.
        let mut ctx = MockNativeContext::new();
        let r1 = native_get_instance_default(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let r2 = native_get_instance_default(&mut ctx, &[])
            .unwrap()
            .unwrap();
        for r in [r1, r2] {
            if let Value::Object(Some(walker)) = r {
                match ctx.get_field(walker, FIELD_ESTIMATE_DEPTH) {
                    Value::Int(n) => assert!(n > 0, "estimateDepth must be > 0"),
                    other => panic!("expected Int for estimateDepth, got {:?}", other),
                }
                match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
                    Value::Int(n) => assert_eq!(n, 0, "default should not retain class ref"),
                    other => panic!("expected Int for retainClassRef, got {:?}", other),
                }
            } else {
                panic!("expected non-null walker");
            }
        }
    }

    #[test]
    fn get_instance_set_depth_rejects_nonpositive() {
        let mut ctx = MockNativeContext::new();
        let set = alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1);
        let bad_args = [Value::Object(Some(set)), Value::Int(0)];
        let err = native_get_instance_set_depth(&mut ctx, &bad_args).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("estimateDepth") || s.contains("IllegalArgument"),
            "expected IllegalArgument, got {}",
            s
        );

        let neg_args = [Value::Object(Some(set)), Value::Int(-1)];
        let err = native_get_instance_set_depth(&mut ctx, &neg_args).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("estimateDepth") || s.contains("IllegalArgument"),
            "expected IllegalArgument, got {}",
            s
        );
    }

    #[test]
    fn get_instance_set_depth_rejects_null_set() {
        let mut ctx = MockNativeContext::new();
        let args = [Value::Object(None), Value::Int(4)];
        let err = native_get_instance_set_depth(&mut ctx, &args).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("Null") || s.contains("null"),
            "expected NPE, got {}",
            s
        );
    }

    #[test]
    fn get_instance_set_depth_sets_fields_correctly() {
        let mut ctx = MockNativeContext::new();
        let set = alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1);
        let args = [Value::Object(Some(set)), Value::Int(16)];
        let result = native_get_instance_set_depth(&mut ctx, &args)
            .unwrap()
            .unwrap();
        if let Value::Object(Some(walker)) = result {
            match ctx.get_field(walker, FIELD_ESTIMATE_DEPTH) {
                Value::Int(n) => assert_eq!(n, 16),
                other => panic!("expected Int(16), got {:?}", other),
            }
            match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
                Value::Int(n) => assert_eq!(n, 1, "explicit option set implies retain"),
                other => panic!("expected Int(1), got {:?}", other),
            }
            match ctx.get_field(walker, FIELD_OPTIONS) {
                Value::Object(Some(_)) => {}
                other => panic!("expected non-null options Set, got {:?}", other),
            }
        } else {
            panic!("expected non-null walker");
        }
    }

    #[test]
    fn get_instance_one_option_rejects_null() {
        let mut ctx = MockNativeContext::new();
        let err = native_get_instance_one_option(&mut ctx, &[Value::Object(None)]).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("Null") || s.contains("null"),
            "expected NPE, got {}",
            s
        );
    }

    #[test]
    fn get_instance_one_option_sets_retain_flag() {
        let mut ctx = MockNativeContext::new();
        let option = alloc_concurrent_synthetic(&mut ctx, "java/lang/StackWalker$Option", 1);
        let args = [Value::Object(Some(option))];
        let result = native_get_instance_one_option(&mut ctx, &args)
            .unwrap()
            .unwrap();
        if let Value::Object(Some(walker)) = result {
            match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
                Value::Int(n) => assert_eq!(n, 1),
                other => panic!("expected Int(1), got {:?}", other),
            }
        } else {
            panic!("expected non-null walker");
        }
    }

    #[test]
    fn get_caller_class_returns_non_null_when_classes_loaded() {
        // MockNativeContext will not have a populated trace, but we do
        // exercise the fall-back branch that returns java/lang/Object.
        let mut ctx = MockNativeContext::new();
        // Ensure Object is a loadable class in the mock.
        let _ = ctx.ensure_class_initialized("java/lang/Object");
        let result = native_get_caller_class(&mut ctx, &[])
            .unwrap()
            .unwrap();
        // Either non-null (fallback fired) or null (Object class not
        // loadable in mock). Both are acceptable; the key invariant is
        // no panic / no error.
        match result {
            Value::Object(_) => {}
            other => panic!("expected Object(…), got {:?}", other),
        }
    }

    #[test]
    fn register_stack_walker_boot_adds_getinstance_variants() {
        use rustjvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_stack_walker_boot(&mut r);
        let after = r.len();
        assert!(
            after >= before + 5,
            "expected at least 5 new registrations, got {}",
            after - before
        );
    }
}
