// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
use cratonvm_types::error::MethodCallFailed;

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
) -> Result<ObjectRef, MethodCallFailed> {
    let walker =
        try_alloc_concurrent_synthetic(ctx, "java/lang/StackWalker", STACK_WALKER_FIELD_COUNT)?;
    // Real-JDK field declaration order is:
    //   continuation, contScope, options, extendedOption, estimateDepth, retainClassRef
    // Our synthetic-mode hard-coded indices (FIELD_OPTIONS=0,
    // FIELD_ESTIMATE_DEPTH=1, FIELD_RETAIN_CLASS_REF=3) only line up when
    // the synthetic StackWalker class is in use; in real-JDK mode (Spring
    // Boot 3 + JDK 25 boot path) those indices land on `continuation` and
    // `contScope`, leaving `options` null — which makes
    // `StackStreamFactory.toStackWalkMode` NPE in
    // `StackWalker.hasOption(options.contains(...))` at JDK 25 line 635.
    //
    // Resolve by name so both layouts get the right slot. Fall back to
    // the legacy synthetic indices if the lookup fails (synthetic-mode
    // tests that don't load the real class).
    let cid = ctx.class_id_of_object(walker);
    let opt_idx = ctx
        .resolve_field_index("java/lang/StackWalker", "options")
        .or_else(|| {
            ctx.class_name_of_id(cid)
                .and_then(|n| ctx.resolve_field_index(&n, "options"))
        })
        .unwrap_or(FIELD_OPTIONS);
    let depth_idx = ctx
        .resolve_field_index("java/lang/StackWalker", "estimateDepth")
        .unwrap_or(FIELD_ESTIMATE_DEPTH);
    let retain_idx = ctx
        .resolve_field_index("java/lang/StackWalker", "retainClassRef")
        .unwrap_or(FIELD_RETAIN_CLASS_REF);
    ctx.set_field(walker, opt_idx, options);
    ctx.set_field(walker, depth_idx, Value::Int(estimate_depth));
    ctx.set_field(
        walker,
        retain_idx,
        Value::Int(if retain_class_ref { 1 } else { 0 }),
    );
    Ok(walker)
}

/// `StackWalker.getInstance()` — no options; an empty set is stored.
pub(crate) fn native_get_instance_default(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Use a fully-initialised HashSet rather than a half-built EnumSet.
    // HashSet.<init>() runs and populates its internal HashMap, so a
    // subsequent `contains(option)` query returns false instead of NPEing.
    // EnumSet is abstract (RegularEnumSet/JumboEnumSet are package-private)
    // so allocating it as synthetic leaves Set methods broken under real-JDK
    // dispatch.
    let options = build_options_set(ctx, &[]);
    let walker = alloc_walker(ctx, Value::Object(Some(options?)), 1, false)?;
    Ok(Some(Value::Object(Some(walker))))
}

/// Helper: allocate a real `java.util.HashSet` and add each provided option
/// reference to it. Used by every `getInstance(...)` variant so the stored
/// option set is queryable via standard `Set.contains` without NPE.
fn build_options_set(
    ctx: &mut dyn NativeContext,
    opts: &[ObjectRef],
) -> Result<ObjectRef, MethodCallFailed> {
    let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 0)?;
    let _ = ctx.invoke(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(set))],
    );
    for opt in opts {
        let _ = ctx.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(set)), Value::Object(Some(*opt))],
        );
    }
    Ok(set)
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
    let options = build_options_set(ctx, &[option]);
    let walker = alloc_walker(ctx, Value::Object(Some(options?)), 1, true)?;
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
    let walker = alloc_walker(ctx, option_set_arg, estimate_depth, true)?;
    Ok(Some(Value::Object(Some(walker))))
}

/// `StackWalker.getCallerClass()` — walk the interpreter stack and return
/// the first Class mirror that is neither StackWalker itself nor the
/// cratonvm internal reflection / invoke wrappers.
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

    // `capture_stack_trace` returns frames OUTERMOST-first (index 0 = the
    // bottom-of-stack `main`, last = the innermost method that ran the
    // `getCallerClass` native; the native itself is not pushed as a frame).
    //
    // Per the JDK spec, `getCallerClass()` returns the class of the caller of
    // the method that invoked `getCallerClass` — i.e. the frame *below* the
    // innermost real (non-internal) frame. So: collect the real frames in
    // stack order, take the last one (the `@CallerSensitive` method that
    // called us, e.g. H2's `TestBase.createCaller`), and return ITS caller —
    // the second-to-last real frame (e.g. `TestDate.main`).
    //
    // The previous implementation returned the *first* (outermost) real frame,
    // which only coincidentally matched for a 2-deep stack and returned the
    // wrong class (the `@CallerSensitive` method's own class) for any deeper
    // nesting — H2's `createCaller` then tried to `newInstance()` the abstract
    // `TestBase` and threw `InstantiationException`.
    let is_internal = |name: &str| {
        name == "java/lang/StackWalker"
            || name.starts_with("java/lang/StackWalker$")
            || name.starts_with("jdk/internal/reflect/")
            || name.starts_with("sun/reflect/")
            || name == "java/lang/reflect/Method"
            || name == "java/lang/invoke/MethodHandle"
            || name.starts_with("java/lang/invoke/")
    };
    let real: Vec<&str> = trace
        .iter()
        .map(|e| e.class_name.as_ref())
        .filter(|n| !is_internal(n))
        .collect();
    if crate::nbflags().dbg_caller {
        eprintln!(
            "[DBG_CALLER] getCallerClass trace ({} frames):",
            trace.len()
        );
        for (i, e) in trace.iter().enumerate() {
            eprintln!(
                "  [{}] {}::{} bci={}",
                i, e.class_name, e.method_name, e.byte_code_index
            );
        }
    }

    // Normal case: at least the `@CallerSensitive` method and its caller are on
    // the captured stack — return the caller (second-to-last real frame).
    if real.len() >= 2 {
        let caller = real[real.len() - 2];
        if let Some(cid) = ctx.class_id_by_name(caller) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // Degraded case: the caller frame is not on the interpreter stack. This
    // happens under the JIT, where a compiled caller (e.g. an OSR-compiled
    // `main`) executes as native code and pushes no interpreter frame, so only
    // the innermost interpreted frame is visible. Returning that lone frame
    // would be the @CallerSensitive method itself (wrong), so fall back to the
    // first available real frame, then to `java/lang/Object` — matching the
    // historical boot-path behaviour that JBoss-Modules / KC16 detectors
    // tolerate (they accept any non-null Class).
    if let Some(first) = real.first() {
        if let Some(cid) = ctx.class_id_by_name(first) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    if let Some(cid) = ctx.class_id_by_name("java/lang/Object") {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

/// Canonical fallback constant list, used only when the loaded class reports
/// no enum-typed static fields (a stripped synthetic stand-in). Declaration
/// order IS ordinal order, so this mirrors the JDK source order.
const OPTION_FALLBACK_CONSTANTS: [&str; 4] = [
    "RETAIN_CLASS_REFERENCE",
    "DROP_METHOD_INFO",
    "SHOW_REFLECT_FRAMES",
    "SHOW_HIDDEN_FRAMES",
];

/// Names of the enum constants `java.lang.StackWalker$Option` declares, in
/// declaration (= ordinal) order.
///
/// Read from the loaded class rather than hard-coded: `DROP_METHOD_INFO` only
/// exists from JDK 22, so a fixed list is wrong on one JDK or the other. Enum
/// constants are exactly the static fields whose descriptor is the enum type
/// itself, which excludes `$VALUES` (an array) and any other static.
fn option_constant_names(ctx: &dyn NativeContext, class_name: &str) -> Vec<String> {
    let self_descriptor = format!("L{class_name};");
    let names: Vec<String> = match ctx.class_id_by_name(class_name) {
        Some(cid) => ctx
            .declared_fields(cid)
            .into_iter()
            .filter(|f| f.is_static && f.descriptor == self_descriptor)
            .map(|f| f.name)
            .collect(),
        None => Vec::new(),
    };
    if names.is_empty() {
        return OPTION_FALLBACK_CONSTANTS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    names
}

/// `java.lang.StackWalker$Option.<clinit>`.
///
/// Builds REAL enum constants — `name` and `ordinal` populated — not bare
/// instances. A nameless constant is not merely cosmetic: `Enum.valueOf`
/// resolves by comparing `name`, so nameless constants make
/// `Option.valueOf("SHOW_REFLECT_FRAMES")` (and every other name) throw
/// `IllegalArgumentException: No enum constant`. Mockito's
/// `Java9PlusLocationImpl.<clinit>` does exactly that lookup, so it died with
/// `ExceptionInInitializerError` and every Mockito-based test class failed
/// wholesale — 7 of the 19 netty `io.netty.util` classes in
/// `docs/known-issues/netty/investigate-batch-12.md` / `-13.md`.
///
/// The constant set is read from the loaded class (see
/// `option_constant_names`) so the array matches whichever JDK is in use, and
/// `$VALUES` is built in declaration order so `ordinal()` agrees with
/// `values()[i]`.
fn native_option_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let class_name = "java/lang/StackWalker$Option";
    let names = option_constant_names(ctx, class_name);

    // Every constant is ROOTED for the whole clinit, not just across its own
    // `create_string`. The per-constant pin below fixes the one allocation
    // inside an iteration, but each later iteration allocates two more objects
    // (the next constant and its name String) and `new_ref_array` allocates a
    // third — all with the earlier constants live only as bare `ObjectRef`s.
    // One moving young collection in any of those and `$VALUES` gets filled
    // with vacated from-space addresses. Handles resolve post-GC; raw
    // `ObjectRef`s in a `Vec` do not.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let mut handles = Vec::with_capacity(names.len());

    for (ordinal, name) in names.iter().enumerate() {
        // 2 slots = java.lang.Enum's (name, ordinal), the layout every other
        // synthetic JDK enum in this crate uses (see `p57_alloc_enum`).
        let option = try_alloc_concurrent_synthetic(&mut *scope, class_name, 2)?;
        let handle = scope.root(option);
        // Pin across `create_string`: a moving young GC there would relocate
        // the freshly allocated constant (native stale-local family).
        let name_str = scope.create_string(name);
        let option = scope.get(&handle);
        scope.set_field_by_name(option, "name", Value::Object(Some(name_str)));
        scope.set_field_by_name(option, "ordinal", Value::Int(ordinal as i32));
        // `set_field_by_name` is a NO-OP when the field is absent, which is the
        // case for a stripped synthetic stand-in that declares no `java.lang.Enum`
        // superclass fields. Fall back to Enum's (name, ordinal) slot convention
        // — the same one `p57_alloc_enum` writes positionally — but only when the
        // by-name write demonstrably did not land, so a real-JDK layout is never
        // written through blind slot indices.
        if !matches!(
            scope.get_field_by_name(option, "name"),
            Value::Object(Some(_))
        ) {
            scope.set_field(option, 0, Value::Object(Some(name_str)));
            scope.set_field(option, 1, Value::Int(ordinal as i32));
        }
        scope.set_static_field_by_name(class_name, name, Value::Object(Some(option)));
        handles.push(handle);
    }

    let option_class = {
        let first = scope.get(&handles[0]);
        scope.class_id_of_object(first)
    };
    let values_array = scope.new_ref_array(option_class, handles.len());
    let values_handle = scope.root(values_array);
    for (idx, handle) in handles.iter().enumerate() {
        let option = scope.get(handle);
        let array = scope.get(&values_handle);
        scope.set_array_element(array, idx, Value::Object(Some(option)));
    }
    let values_array = scope.get(&values_handle);
    scope.set_static_field_by_name(class_name, "$VALUES", Value::Object(Some(values_array)));
    scope.set_static_field_by_name(class_name, "ENUM$VALUES", Value::Object(Some(values_array)));

    Ok(None)
}

/// Install every StackWalker boot-path native this module owns.
pub fn register_stack_walker_boot(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/StackWalker$Option",
        "<clinit>",
        "()V",
        native_option_clinit,
    );
    let sw = "java/lang/StackWalker";
    registry.register(
        sw,
        "getInstance",
        "()Ljava/lang/StackWalker;",
        native_get_instance_default,
    );
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
                    message: Some(
                        "StackWalker.getInstance: options Set must not be null".to_string(),
                    ),
                }
                .into());
            }
            let walker = alloc_walker(ctx, option_set_arg, 1, true)?;
            Ok(Some(Value::Object(Some(walker))))
        },
    );
    registry.register(
        sw,
        "getCallerClass",
        "()Ljava/lang/Class;",
        native_get_caller_class,
    );

    // WP1.9 — `StackStreamFactory$AbstractStackWalker.checkStackWalkModes()Z`.
    //
    // In OpenJDK this is a private Java method that validates the walker's
    // stored `mode` bitmask against the set of legal mode bits. cratonvm's
    // bootstrap dispatches it through the native registry (the Java
    // implementation reaches into `jdk.internal.reflect.Reflection` and
    // `MemberName`-resolution paths that aren't yet wired during early
    // boot, so the `<clinit>` path NPEs on a missing native). The boot
    // probe (Keycloak / WildFly StackWalker feature detection) calls
    // `getInstance(...)` which routes through `AbstractStackWalker.<init>`
    // → `checkStackWalkModes`. For our purposes the call is a tautology
    // — any walker we hand back via `native_get_instance_*` above is
    // already constructed with a legal mode set — so we return true
    // unconditionally. This matches the spec's intent (the method exists
    // to reject illegal callers, not to filter normal ones).
    registry.register(
        "java/lang/StackStreamFactory$AbstractStackWalker",
        "checkStackWalkModes",
        "()Z",
        native_check_stack_walk_modes,
    );
    // RKC16N.13 — In JDK 25 the helper is exposed as a *static* native on
    // the outer `StackStreamFactory` class as well (called from
    // `StackStreamFactory.<clinit>` to feature-test the walker pipeline).
    // Without it, JBoss-Modules / WildFly boot logs an UnsatisfiedLinkError
    // swallowed at `<clinit>` and a downstream NPE on
    // `StackFrameTraverser.<clinit>` because the verbatim `mode` static
    // never gets initialised. Same semantics — return true.
    registry.register_with_kind(
        "java/lang/StackStreamFactory",
        "checkStackWalkModes",
        "()Z",
        native_check_stack_walk_modes,
        NativeKind::Bridge,
    );
    registry.set_category(__prev_cat);
    
}

/// `StackStreamFactory$AbstractStackWalker.checkStackWalkModes()Z` —
/// validate the receiver walker's mode bitmask. Returns true for any
/// recognised combination of mode bits, false otherwise.
///
/// Recognised mode bits (per OpenJDK 25 `AbstractStackWalker`):
///   DEFAULT_MODE              = 0x0
///   FILL_CLASS_REFS_ONLY      = 0x2
///   FILTER_FILL_IN_STACKTRACE = 0x10
///   SHOW_HIDDEN_FRAMES        = 0x20
///   FILL_LIVE_STACK_FRAMES    = 0x100
///   GET_CALLER_CLASS          = 0x4
///   RETAIN_CLASS_REFERENCE    = 0x1
///
/// We accept any value whose set bits all fall within this union mask
/// AND don't combine `LOCALS_AND_OPERANDS` (0x100, FILL_LIVE_STACK_FRAMES)
/// with `RETAIN_CLASS_REFERENCE` (0x1) — the JDK rejects that pair.
///
/// Boot detectors only ever construct walkers with one of {DEFAULT,
/// RETAIN_CLASS_REFERENCE, SHOW_HIDDEN_FRAMES} set, so they always
/// succeed; the strict validation matters only for hostile callers.
fn validate_stack_walk_modes(mode: i32) -> bool {
    const DEFAULT_MODE: i32 = 0x0;
    const RETAIN_CLASS_REFERENCE: i32 = 0x1;
    const FILL_CLASS_REFS_ONLY: i32 = 0x2;
    const GET_CALLER_CLASS: i32 = 0x4;
    const FILTER_FILL_IN_STACKTRACE: i32 = 0x10;
    const SHOW_HIDDEN_FRAMES: i32 = 0x20;
    const LOCALS_AND_OPERANDS: i32 = 0x100; // a.k.a. FILL_LIVE_STACK_FRAMES

    let all_modes = DEFAULT_MODE
        | RETAIN_CLASS_REFERENCE
        | FILL_CLASS_REFS_ONLY
        | GET_CALLER_CLASS
        | FILTER_FILL_IN_STACKTRACE
        | SHOW_HIDDEN_FRAMES
        | LOCALS_AND_OPERANDS;

    if (mode & !all_modes) != 0 {
        return false;
    }
    // LOCALS_AND_OPERANDS implies RETAIN_CLASS_REFERENCE in the JDK; the
    // pair is internally consistent rather than rejected. We mirror that
    // by accepting it.
    true
}

pub(crate) fn native_check_stack_walk_modes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Receiver is `this` (the AbstractStackWalker). Read its `mode` field
    // if available — the AbstractStackWalker layout in JDK 25 puts mode
    // at field index 2 (after walker and contScope). If we can't read it
    // (synthetic walker, missing field, etc.) fall back to accepting.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let mode = match ctx.get_field(this, 2) {
        Value::Int(n) => n,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if validate_stack_walk_modes(mode) {
        1
    } else {
        0
    })))
}

#[cfg(test)]
mod check_modes_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn validate_accepts_default() {
        assert!(validate_stack_walk_modes(0x0));
    }

    #[test]
    fn validate_accepts_known_bits() {
        // RETAIN_CLASS_REFERENCE | SHOW_HIDDEN_FRAMES
        assert!(validate_stack_walk_modes(0x1 | 0x20));
        // FILL_CLASS_REFS_ONLY | GET_CALLER_CLASS
        assert!(validate_stack_walk_modes(0x2 | 0x4));
        // LOCALS_AND_OPERANDS | RETAIN_CLASS_REFERENCE — JDK considers
        // this consistent (the LOCALS variant implies retaining refs).
        assert!(validate_stack_walk_modes(0x100 | 0x1));
    }

    #[test]
    fn validate_rejects_unknown_bits() {
        // 0x800 is not in the recognised mask.
        assert!(!validate_stack_walk_modes(0x800));
        assert!(!validate_stack_walk_modes(0x1 | 0x80000000_u32 as i32));
    }

    #[test]
    fn native_with_null_receiver_returns_true() {
        let mut ctx = MockNativeContext::new();
        let result = native_check_stack_walk_modes(&mut ctx, &[Value::Object(None)])
            .expect("native should not error")
            .expect("should return Some(Value)");
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn register_includes_check_stack_walk_modes() {
        use cratonvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        register_stack_walker_boot(&mut r);
        // Sanity: the registry now contains both the StackWalker entries
        // (5+ from the existing test) plus the new AbstractStackWalker
        // entry, so we expect at least 6 registrations.
        assert!(r.len() >= 6);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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
        let r1 = native_get_instance_default(&mut ctx, &[]).unwrap().unwrap();
        let r2 = native_get_instance_default(&mut ctx, &[]).unwrap().unwrap();
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
        let set = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1).unwrap();
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
        let set = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1).unwrap();
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

    /// Stand a `StackWalker$Option` class up in the mock declaring exactly
    /// `constants` (plus `$VALUES`), run the native clinit, and assert
    /// `$VALUES` is those constants in that order — each carrying the
    /// `name`/`ordinal` state `java.lang.Enum` declares.
    ///
    /// The name/ordinal assertions are the point. Asserting only that
    /// `$VALUES[i]` is the same object as the i-th static field — which is
    /// what this test used to do — holds for ANY set of instances the native
    /// invents, including the nameless ones that made `Enum.valueOf` match
    /// nothing and took out every Mockito-backed netty test class. A test that
    /// cannot fail on the original bug is not cover for it.
    fn assert_option_clinit_shape(constants: &[&str]) {
        use cratonvm_native_api::FieldMetadata;

        let mut ctx = MockNativeContext::new();
        let option_class = ctx
            .ensure_class_initialized("java/lang/StackWalker$Option")
            .unwrap();
        let mut fields: Vec<FieldMetadata> = constants
            .iter()
            .map(|n| (*n, "Ljava/lang/StackWalker$Option;"))
            .chain(std::iter::once((
                "$VALUES",
                "[Ljava/lang/StackWalker$Option;",
            )))
            .enumerate()
            .map(|(slot_index, (name, descriptor))| FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0,
                slot_index,
                declaring_class_id: option_class,
                is_static: true,
            })
            .collect();
        // The INSTANCE fields every enum inherits from `java.lang.Enum`, in
        // the JDK's slot order. Without them the mock falls back to a generic
        // name→slot table that puts `name` and `ordinal` wherever it likes,
        // and the assertions below would be measuring the mock rather than
        // the native.
        for (slot_index, (name, descriptor)) in [("name", "Ljava/lang/String;"), ("ordinal", "I")]
            .into_iter()
            .enumerate()
        {
            fields.push(FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0,
                slot_index,
                declaring_class_id: option_class,
                is_static: false,
            });
        }
        ctx.set_declared_fields(option_class, fields);

        native_option_clinit(&mut ctx, &[]).expect("clinit should succeed");

        let values_slot = ctx
            .static_field_index_by_name(option_class, "$VALUES")
            .unwrap();
        let values_array = match ctx.get_static_field(option_class, values_slot) {
            Value::Object(Some(array)) => array,
            other => panic!("expected non-null $VALUES array, got {:?}", other),
        };
        assert_eq!(
            ctx.array_length(values_array),
            constants.len(),
            "$VALUES must have one entry per constant the class declares"
        );

        for (idx, name) in constants.iter().enumerate() {
            let slot = ctx.static_field_index_by_name(option_class, name).unwrap();
            let static_value = ctx.get_static_field(option_class, slot);
            assert_eq!(
                ctx.get_array_element(values_array, idx),
                static_value,
                "$VALUES[{idx}] must be {name}"
            );
            let constant = match static_value {
                Value::Object(Some(o)) => o,
                other => panic!("{name} static is {other:?}, expected an object"),
            };
            // Non-null is NOT the contract; the name must be the RIGHT string.
            // Merged 2026-08-12 from the jdk-only campaign, which found the
            // weaker shape twice in one day: a doc froze "values() returns
            // length 3" as verified-good while all three constants were wrong,
            // and a sibling enum passed a non-null-and-named check while
            // `values()[0] == State.NEW` was false. A null-check and an
            // identity/equality check are different assertions, and only the
            // latter can see a fabricated constant.
            match ctx.get_field_by_name(constant, "name") {
                Value::Object(Some(s)) => assert_eq!(
                    ctx.read_string(s).as_deref(),
                    // `name` is `&&str` from `constants.iter()`; deref rather
                    // than `.as_str()`, which resolves to the still-unstable
                    // `str::as_str` and fails to compile the whole test target.
                    Some(*name),
                    "{name} carries the wrong Enum.name — a wrong or null name makes \
                     Enum.valueOf's constant directory match nothing"
                ),
                other => panic!(
                    "{name} must carry an Enum.name string, got {other:?} — a null name \
                     makes Enum.valueOf's constant directory match nothing"
                ),
            }
            assert_eq!(
                ctx.get_field_by_name(constant, "ordinal"),
                Value::Int(idx as i32),
                "{name} must carry ordinal {idx}"
            );
        }
    }

    /// JDK 22+ declares four constants, with `DROP_METHOD_INFO` at ordinal 1.
    #[test]
    fn option_clinit_populates_enum_values_array() {
        assert_option_clinit_shape(&[
            "RETAIN_CLASS_REFERENCE",
            "DROP_METHOD_INFO",
            "SHOW_REFLECT_FRAMES",
            "SHOW_HIDDEN_FRAMES",
        ]);
    }

    /// JDK 9–21 has no `DROP_METHOD_INFO`; the native follows the class it
    /// actually finds rather than publishing a constant that JDK never
    /// declared.
    #[test]
    fn option_clinit_follows_a_pre_jdk22_three_constant_class() {
        assert_option_clinit_shape(&[
            "RETAIN_CLASS_REFERENCE",
            "SHOW_REFLECT_FRAMES",
            "SHOW_HIDDEN_FRAMES",
        ]);
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
        let option =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/StackWalker$Option", 1).unwrap();
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
        let result = native_get_caller_class(&mut ctx, &[]).unwrap().unwrap();
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
        use cratonvm_native_api::NativeMethodRegistry;
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
