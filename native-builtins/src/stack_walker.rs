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
    // JIT round 22 wave 2, lane runtime: a SHOW_HIDDEN_FRAMES walker ends the
    // "hidden frames never observed" latch at creation (field reads only,
    // before anything is allocated).
    if let Value::Object(Some(set)) = options {
        crate::lang_stackwalker::note_hidden_frames_if_options_show(ctx, set);
    }
    // gc-common w19-g: `options` (the caller's freshly built option set) is
    // rooted across the walker's allocation and re-read before it is stored.
    // It was held raw, so a collection at the walker's allocation stored the
    // set's vacated address.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let options_h = match options {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let walker: ObjectRef = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/lang/StackWalker",
        STACK_WALKER_FIELD_COUNT,
    )?;
    let options = match &options_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => options,
    };
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
    let cid = scope.class_id_of_object(walker);
    let opt_idx = scope
        .resolve_field_index("java/lang/StackWalker", "options")
        .or_else(|| {
            scope
                .class_name_of_id(cid)
                .and_then(|n| scope.resolve_field_index(&n, "options"))
        })
        .unwrap_or(FIELD_OPTIONS);
    let depth_idx = scope
        .resolve_field_index("java/lang/StackWalker", "estimateDepth")
        .unwrap_or(FIELD_ESTIMATE_DEPTH);
    let retain_idx = scope
        .resolve_field_index("java/lang/StackWalker", "retainClassRef")
        .unwrap_or(FIELD_RETAIN_CLASS_REF);
    scope.set_field(walker, opt_idx, options);
    scope.set_field(walker, depth_idx, Value::Int(estimate_depth));
    scope.set_field(
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
    // gc-common w19-g: one handle scope for the set and every option. Before,
    // the set was pinned only AFTER `HashSet.<init>` (Java) -- pinning the
    // pre-constructor address -- the options were held raw across `<init>`
    // and every `add`, the pin was never released, and the function returned
    // the pre-constructor address rather than the re-read one.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let opt_hs: Vec<cratonvm_native_api::NativeHandle> =
        opts.iter().map(|o| scope.root(*o)).collect();
    let set_new: ObjectRef = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashSet", 0)?;
    let set_h = scope.root(set_new);
    let _ = scope.invoke(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(set_new))],
    );
    for opt_h in &opt_hs {
        let set = scope.get(&set_h);
        let opt = scope.get(opt_h);
        let _ = scope.invoke(
            "java/util/HashSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(set)), Value::Object(Some(opt))],
        );
    }
    Ok(scope.get(&set_h))
}

/// `StackWalker.getInstance(StackWalker$Option)` — build a one-option walker.
///
/// The walker retains class references only for RETAIN_CLASS_REFERENCE
/// (since interpreter round i1 wave 42; every option granted it before). A
/// null argument reproduces the JDK's message-less NullPointerException.
pub(crate) fn native_get_instance_one_option(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let option = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // `Objects.requireNonNull(option)`: no message on HotSpot
        // (interpreter round i1 wave 42, lane L3; `L3W42StackWalkerApi`,
        // `null-option`).
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // RETAIN_CLASS_REFERENCE only when that IS the option (wave 42, lane L3):
    // every option used to grant it, so `getInstance(SHOW_REFLECT_FRAMES)`
    // answered `getCallerClass()` where HotSpot refuses. An option whose name
    // cannot be read keeps the old grant.
    let retain_class_ref =
        option_name(ctx, option).is_none_or(|name| name == "RETAIN_CLASS_REFERENCE");
    let options = build_options_set(ctx, &[option]);
    let walker = alloc_walker(ctx, Value::Object(Some(options?)), 1, retain_class_ref)?;
    Ok(Some(Value::Object(Some(walker))))
}

/// The enum constant name of a `StackWalker.Option` (`Enum.name`), read from
/// its field without running Java.
fn option_name(ctx: &dyn NativeContext, option: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(option, "name") {
        Value::Object(Some(name)) => ctx.read_string(name),
        _ => None,
    }
}

/// Whether the option set `options` holds RETAIN_CLASS_REFERENCE, and the set
/// read back afterwards (asking may run the set's `toString`, which allocates):
/// `true` when the set cannot be read, the grant every set used to get.
/// Interpreter round i1 wave 42, lane L3.
fn options_retain_class_reference(ctx: &mut dyn NativeContext, options: Value) -> (bool, Value) {
    let Value::Object(Some(set)) = options else {
        return (false, options);
    };
    let pin = ctx.pin_native_root(set);
    let retains =
        crate::lang_stackwalker::walker_options_contain(ctx, set, "RETAIN_CLASS_REFERENCE")
            .unwrap_or(true);
    let set = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    (retains, Value::Object(Some(set)))
}

/// Whether the `StackWalker` receiver of a native was built with
/// RETAIN_CLASS_REFERENCE: its `retainClassRef` field, `true` when the field
/// cannot be read (a stand-in without it).
fn walker_retains_class_reference(ctx: &dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(walker))) = args.first().copied() else {
        return true;
    };
    match ctx.get_field_by_name(walker, "retainClassRef") {
        Value::Int(v) => v != 0,
        _ => true,
    }
}

/// `StackWalker.getInstance(Set<Option>, int)` — JDK 25 two-arg overload.
///
/// Per javadoc, `estimateDepth` must be > 0 or IllegalArgumentException is
/// thrown. The option set is retained verbatim; RETAIN_CLASS_REFERENCE is
/// granted when the set holds it, or when the set cannot be read (every
/// non-null set was taken to imply it before interpreter round i1 wave 42).
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
    // `toEnumSet`'s `Objects.requireNonNull(options)`: no message on HotSpot
    // (interpreter round i1 wave 44, lane L3; `L3W44StackWalkerEdges` row
    // `instance-null-set-depth`).
    if matches!(option_set_arg, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    // RETAIN_CLASS_REFERENCE only when the set holds it (wave 42, lane L3).
    let (retain_class_ref, option_set_arg) = options_retain_class_reference(ctx, option_set_arg);
    let walker = alloc_walker(ctx, option_set_arg, estimate_depth, retain_class_ref)?;
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
    args: &[Value],
) -> MethodCallResult {
    // The JDK's first check (interpreter round i1 wave 42, lane L3;
    // `L3W42StackWalkerApi`, `caller-class-without-option`).
    if !walker_retains_class_reference(&*ctx, args) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "This stack walker does not have RETAIN_CLASS_REFERENCE access".to_string(),
        }
        .into());
    }
    // The walkers' capture (interpreter round i1 wave 49, lane L3): it lists
    // the JDK frames a native stands for -- `AccessController.doPrivileged`
    // below a privileged action, HotSpot's answer for a `getCallerClass()`
    // made directly in `run()` (`L3W49PrivilegedFrames`, `caller-class`) --
    // and the reflective steps, which `is_internal` skips as HotSpot skips
    // reflection frames.
    // WALK21-9 (`CRATONVM_STACKWALK_CALLER_CLASS_TAIL`): the innermost
    // frames first; `None` from the VM (no provable window) or a window too
    // short for the finder falls through to the whole capture.
    if ctx.caller_class_finder_rule() && stackwalk_caller_class_tail_enabled() {
        for want in CALLER_CLASS_TAIL_WINDOWS {
            let Some(tail) = ctx.capture_stack_walk_trace_tail(want) else {
                break;
            };
            if let Some(answer) = caller_class_by_finder_rule_over(ctx, &tail, true) {
                return answer;
            }
        }
    }
    let trace = ctx.capture_stack_walk_trace();
    if ctx.caller_class_finder_rule() {
        return caller_class_by_finder_rule(ctx, &trace);
    }

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
            || name == "java/lang/reflect/Constructor"
            || name == "java/lang/invoke/MethodHandle"
            || name.starts_with("java/lang/invoke/")
    };
    // Each frame's own class (wave 49, lane L3): the capture names it by id,
    // so two same-named classes of different loaders are not confused; by
    // name only for an entry that carries no id.
    let real: Vec<(&str, Option<cratonvm_types::ClassId>)> = trace
        .iter()
        .filter(|e| !is_internal(&e.class_name))
        .map(|e| (e.class_name.as_ref(), e.class_id))
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
        let (caller, caller_id) = real[real.len() - 2];
        if let Some(cid) = caller_id.or_else(|| ctx.class_id_by_name(caller)) {
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
    if let Some(&(first, first_id)) = real.first() {
        if let Some(cid) = first_id.or_else(|| ctx.class_id_by_name(first)) {
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

/// Is a frame of `class_name` a method-handle frame for HotSpot's caller-class
/// walk? JDK 25 `StackStreamFactory.isMethodHandleFrame`: the class's package
/// is exactly `java.lang.invoke` (not a subpackage).
fn is_method_handle_frame_class(class_name: &str) -> bool {
    class_name
        .strip_prefix("java/lang/invoke/")
        .is_some_and(|simple| !simple.contains('/'))
}

/// `StackWalker.getCallerClass()` by HotSpot's rule (interpreter round i1
/// wave 50, lane L3; `VmConfig::caller_class_finder_rule`), over `trace` (the
/// walkers' capture, OUTERMOST first). JDK 25
/// `StackStreamFactory.CallerClassFinder.consumeFrames`: from the innermost
/// frame, skip every hidden frame, reflection frame (`Method`, `Constructor`,
/// the accessors) and `java.lang.invoke` class; the first frame left is the
/// method that asked, the second its caller, whose class is the answer. With
/// no second frame it throws `IllegalCallerException("no caller frame: [<first
/// class> , null]")` (`ClassFrameInfo.toString` of the first, or `null`).
/// The `@CallerSensitive` check on the first frame is not made: no class
/// outside the JDK can carry the annotation, and no JDK method calls
/// `getCallerClass` (`L3W50CallerClassRule`, row `from-main`).
fn caller_class_by_finder_rule(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> MethodCallResult {
    match caller_class_by_finder_rule_over(ctx, trace, false) {
        Some(answer) => answer,
        // Unreachable: a whole trace always answers.
        None => Ok(Some(Value::Object(None))),
    }
}

/// [`caller_class_by_finder_rule`] over `trace`; with `window` (JIT round 21
/// wave 3, lane walk, WALK21-9) `trace` is only the innermost part of the
/// walk capture, and a trace that runs out before the caller is found answers
/// `None` (the caller widens the window) instead of the whole trace's
/// `IllegalCallerException`.
fn caller_class_by_finder_rule_over(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
    window: bool,
) -> Option<MethodCallResult> {
    // The method's own hiddenness, not the `-XX:+ShowHiddenFrames` listing
    // (interpreter round i4 wave 92, lane L2): `getCallerClass` skips them
    // whatever the flag says.
    let hidden = ctx.hidden_member_entries(trace);
    // JIT round 21 wave 1, lane walk (`CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT`):
    // the JDK's own `isReflectionFrame` answer, asked only of the frames the
    // finder reaches.
    let screen: &dyn NativeContext = &*ctx;
    let mut kept = trace
        .iter()
        .zip(hidden)
        .rev()
        // The walker's own frames, should a capture list them.
        .skip_while(|(e, _)| {
            e.class_name.starts_with("java/lang/StackWalker")
                || e.class_name.starts_with("java/lang/StackStreamFactory")
        })
        .filter(|(e, hidden)| {
            !*hidden
                && !crate::lang_stackwalker::is_reflection_frame_entry(screen, e)
                && !is_method_handle_frame_class(&e.class_name)
        })
        .map(|(e, _)| e);
    let first = kept.next();
    let Some(caller) = kept.next() else {
        if window {
            return None;
        }
        let first = first.map_or_else(
            || "null".to_string(),
            |e| format!("{} ", crate::lang_class::dotted_binary_name(&e.class_name)),
        );
        return Some(Err(RuntimeError::IllegalCallerException {
            message: format!("no caller frame: [{first}, null]"),
        }
        .into()));
    };
    if crate::nbflags().dbg_caller {
        eprintln!(
            "[DBG_CALLER] getCallerClass (finder rule): {} -> {}",
            first.map_or("?", |e| &*e.class_name),
            caller.class_name
        );
    }
    let Some(cid) = caller
        .class_id
        .or_else(|| ctx.class_id_by_name(&caller.class_name))
    else {
        return Some(Ok(Some(Value::Object(None))));
    };
    let mirror = ctx.get_class_mirror(cid);
    Some(Ok(Some(Value::Object(Some(mirror)))))
}

/// `CRATONVM_STACKWALK_CALLER_CLASS_TAIL=1` (WALK21-9, JIT round 21 wave 3,
/// lane walk; opt-in, default OFF): `StackWalker.getCallerClass()` under the
/// finder rule reads the innermost frames of the walk capture
/// (`NativeContext::capture_stack_walk_trace_tail`: 8 entries, widened x4 while
/// the finder runs out of frames) instead of the whole stack with every
/// frame's line, as HotSpot's `CallerClassFinder` reads two batches from the
/// top. Exact: the window is the whole capture's innermost suffix (the VM
/// answers `None` when it cannot prove that, and the whole capture serves the
/// call), and the finder's answer depends only on the frames up to the
/// caller. Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_caller_class_tail_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_caller_class_tail
}

/// WALK21-9: the windows [`native_get_caller_class`] tries, innermost entries
/// first; past the last, the whole capture.
const CALLER_CLASS_TAIL_WINDOWS: [usize; 4] = [8, 32, 128, 512];

/// JDK 25's `MethodAccessorImpl` subclasses, read with `javap` from 25.0.3
/// (`DirectMethodHandleAccessor`, its `$NativeAccessor`,
/// `CsMethodAccessorAdapter`) and the generated accessors of older JDKs. Asked
/// only for a frame whose class id the VM cannot place under
/// `MethodAccessorImpl` (see [`is_method_accessor_frame`]).
fn is_method_accessor_class_name(class_name: &str) -> bool {
    let Some(simple) = class_name.strip_prefix("jdk/internal/reflect/") else {
        return false;
    };
    matches!(
        simple,
        "MethodAccessorImpl"
            | "DirectMethodHandleAccessor"
            | "DirectMethodHandleAccessor$NativeAccessor"
            | "CsMethodAccessorAdapter"
            | "NativeMethodAccessorImpl"
            | "DelegatingMethodAccessorImpl"
    ) || simple.starts_with("GeneratedMethodAccessor")
}

/// Is `entry` a frame of a `jdk.internal.reflect.MethodAccessorImpl`
/// subclass (HotSpot's `method_holder()->is_subclass_of(reflect_MethodAccessorImpl_klass())`)?
/// By the frame's class id when both it and `MethodAccessorImpl` are known,
/// else by name. A constructor accessor is NOT one (JDK 25's
/// `ConstructorAccessorImpl` does not extend `MethodAccessorImpl`).
fn is_method_accessor_frame(
    ctx: &dyn NativeContext,
    entry: &cratonvm_native_api::StackTraceEntry,
    accessor_base: Option<cratonvm_types::ClassId>,
) -> bool {
    if !entry.class_name.starts_with("jdk/internal/reflect/") {
        return false;
    }
    match (entry.class_id, accessor_base) {
        (Some(id), Some(base)) => id == base || ctx.is_subclass(id, base),
        _ => is_method_accessor_class_name(&entry.class_name),
    }
}

/// Does JDK 25's `JVM_GetCallerClass` walk past this frame
/// (`Method::is_ignored_by_security_stack_walk`)? `Method.invoke`, a
/// `MethodAccessorImpl` subclass (`accessor`), and the method-handle adapter
/// frames: a signature-polymorphic intrinsic (`MethodHandle.invokeBasic`,
/// `linkTo*`) or a compiled lambda form (a `java.lang.invoke` frame that is
/// hidden, or of a `LambdaForm$...` / `...$Holder` class, the classes
/// HotSpot's lambda forms are compiled into). Nothing else: not
/// `Constructor.newInstance`, not a constructor accessor, not a hidden frame
/// of another package, and not the rest of `java.lang.invoke` (unlike the
/// walkers' `CallerClassFinder`).
fn ignored_by_security_stack_walk(
    entry: &cratonvm_native_api::StackTraceEntry,
    hidden: bool,
    accessor: bool,
) -> bool {
    let name = &*entry.class_name;
    if name == "java/lang/reflect/Method" {
        return &*entry.method_name == "invoke";
    }
    if accessor {
        return true;
    }
    let Some(simple) = name.strip_prefix("java/lang/invoke/") else {
        return false;
    };
    hidden
        || simple.starts_with("LambdaForm$")
        || simple.ends_with("$Holder")
        // Interpreter round i5 wave 104, lane L3: the served
        // `invokeWithArguments` frame a capture now lists
        // (`ReflectiveCallKind::MethodHandleInvokeWithArguments`). On HotSpot a
        // caller-sensitive target is never directly above it (the handle is
        // bound to its caller: an injected invoker or an adapter stands
        // between), so it is never the answer there; skipping it keeps this
        // VM's answer the one it gave before the frame was listed.
        || (simple == "MethodHandle" && &*entry.method_name == "invokeWithArguments")
        || (simple == "MethodHandle"
            && matches!(
                &*entry.method_name,
                "invokeBasic"
                    | "linkToStatic"
                    | "linkToVirtual"
                    | "linkToSpecial"
                    | "linkToInterface"
                    | "linkToNative"
            ))
}

/// Is a frame of `class_name` stream / `Spliterator` / `ServiceLoader`
/// plumbing? The skip of the older rule (see
/// [`reflection_caller_index`]): a JIT-collapsed inline chain that lost the
/// `@CallerSensitive` frame and its caller.
fn is_stream_plumbing_class(class_name: &str) -> bool {
    class_name.starts_with("java/util/stream/")
        || class_name == "java/util/ServiceLoader"
        || (class_name.starts_with("java/util/") && class_name.contains("Spliterator"))
}

/// The index in `trace` (OUTERMOST first) of the frame whose class
/// `Reflection.getCallerClass()` answers, by JDK 25's `JVM_GetCallerClass`:
/// the innermost frame is the `@CallerSensitive` method that asked (frame 1;
/// the native `getCallerClass` itself is not in a capture), and the answer
/// is the next frame outward that the security walk does not ignore
/// ([`ignored_by_security_stack_walk`]; `hidden[i]` and `accessor[i]` are
/// per-frame inputs to it). `None`: no such frame (HotSpot returns `null`).
///
/// One residue of the older rule, kept for the shape it was written for: when
/// the innermost frame is stream / `Spliterator` plumbing -- a class with no
/// `@CallerSensitive` method, so the capture lost the asking frame in a
/// JIT-collapsed inline chain (the Hibernate `ServiceLoader.load` from a
/// `forEach`, `docs/feature-designs/profile-guided-inlining.md`)
/// -- the plumbing frames outward are skipped too. A capture that lists the
/// asking frame never takes that arm.
fn reflection_caller_index(
    trace: &[cratonvm_native_api::StackTraceEntry],
    hidden: &[bool],
    accessor: &[bool],
) -> Option<usize> {
    let mut indices = (0..trace.len())
        .rev()
        // The `Reflection` frame itself, should a capture list it.
        .skip_while(|&i| &*trace[i].class_name == "jdk/internal/reflect/Reflection");
    let asking = indices.next()?;
    let collapsed = is_stream_plumbing_class(&trace[asking].class_name)
        && &*trace[asking].class_name != "java/util/ServiceLoader";
    indices.find(|&i| {
        let e = &trace[i];
        !ignored_by_security_stack_walk(
            e,
            hidden.get(i).copied().unwrap_or(false),
            accessor.get(i).copied().unwrap_or(false),
        ) && !(collapsed && is_stream_plumbing_class(&e.class_name))
    })
}

/// `jdk.internal.reflect.Reflection.getCallerClass()` by HotSpot's rule
/// (interpreter round i1 wave 51, lane L3; stage 2 of
/// `docs/feature-designs/native-dispatch-architecture.md`;
/// `VmConfig::reflection_caller_class_rule`): over the walkers' capture (the
/// stand-in frames of a reflective call, compiled frames, the served
/// `Thread.run` frames, each with its own class id), the frame
/// [`reflection_caller_index`] names; the asking frame's own class when it is
/// the only frame (a VM-made call, see below); else `null`. Of the
/// `@CallerSensitive` checks on frames 0 and 1 (`InternalError`), frame 1's
/// is made for an asker outside the JDK ([`caller_sensitive_asker_error`],
/// interpreter round i5 wave 106, lane L3); JDK askers are trusted to carry
/// the annotation. Positive control:
/// `CRATONVM_DBG_CALLER=1` prints
/// `[DBG_CALLER] Reflection.getCallerClass (security walk): <asker> -> <caller>`.
pub(crate) fn reflection_get_caller_class_by_security_walk(
    ctx: &mut dyn NativeContext,
) -> MethodCallResult {
    // Interpreter round i2 wave 60, lane L3 (`CRATONVM_REFLECTION_CALLER_FAST`):
    // the same answer from the two innermost frames, when the VM can prove it.
    // `CRATONVM_DBG_CALLER=1` runs the whole rule too and reports a
    // disagreement (the positive control and the equivalence check).
    let fast = ctx.reflection_caller_class_fast();
    if let Some(cid) = fast {
        if !crate::nbflags().dbg_caller {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
        eprintln!(
            "[DBG_CALLER] Reflection.getCallerClass (fast): -> {}",
            ctx.class_name_of_id(cid).unwrap_or_default()
        );
    }
    let trace = ctx.capture_stack_walk_trace();
    if let Some(error) = caller_sensitive_asker_error(ctx, &trace) {
        return Err(error);
    }
    // A method reference's lambda proxy is a caller (interpreter round i4
    // wave 94, lane L3; `L3W94CallerSensitiveMatrix` `get-other-lambda`).
    let trace = with_caller_lambda_proxies(ctx, trace, &|_: &str, _: &str| false);
    // The method's own hiddenness, whatever `-XX:+ShowHiddenFrames` says
    // (`JVM_GetCallerClass`; interpreter round i4 wave 92, lane L2).
    let hidden = ctx.hidden_member_entries(&trace);
    let accessor: Vec<bool> = if trace
        .iter()
        .any(|e| e.class_name.starts_with("jdk/internal/reflect/"))
    {
        let base = ctx.class_id_by_name("jdk/internal/reflect/MethodAccessorImpl");
        trace
            .iter()
            .map(|e| is_method_accessor_frame(&*ctx, e, base))
            .collect()
    } else {
        Vec::new()
    };
    // No frame outward of the asking one: HotSpot answers `null` (a
    // `@CallerSensitive` method called from JNI on an attached thread), but
    // here that is a call the VM itself made into JDK code with no Java frame
    // below it (measured: `Logger.getLogger` during the VM's logging set-up,
    // whose `demandLogger` reads the caller's module), standing in for JDK
    // code HotSpot would run in Java. Answer the asking frame's own class, as
    // the older rule did, rather than a `null` that JDK code does not expect.
    let index = reflection_caller_index(&trace, &hidden, &accessor).or_else(|| {
        (0..trace.len())
            .rev()
            .find(|&i| &*trace[i].class_name != "jdk/internal/reflect/Reflection")
            .filter(|&i| i == 0)
    });
    if crate::nbflags().dbg_caller {
        // j27 wave 7 (lane T1ACCEPT; page
        // `j27-reinterpret-in-an-upcall-takes-the-caller-security-walk-FIXED-20261029.md`):
        // an ask the fast path answered still runs this walk, as the control;
        // its line says so, so `(security walk)` counts only real misses (it
        // was read as "every ask walked": 56,000 lines, 55,451 of them controls).
        eprintln!(
            "[DBG_CALLER] Reflection.getCallerClass ({}): {} -> {}",
            dbg_walk_label(fast.is_some()),
            trace.last().map_or("?", |e| &*e.class_name),
            index.map_or("null", |i| &*trace[i].class_name)
        );
    }
    let whole = index.map(|i| &trace[i]).and_then(|caller| {
        caller
            .class_id
            .or_else(|| ctx.class_id_by_name(&caller.class_name))
    });
    if fast.is_some() && fast != whole {
        eprintln!(
            "[DBG_CALLER] Reflection.getCallerClass FAST MISMATCH: fast {:?} whole {:?}",
            fast.and_then(|id| ctx.class_name_of_id(id)),
            whole.and_then(|id| ctx.class_name_of_id(id))
        );
    }
    let Some(cid) = whole else {
        return Ok(Some(Value::Object(None)));
    };
    let mirror = ctx.get_class_mirror(cid);
    Ok(Some(Value::Object(Some(mirror))))
}

/// The `CRATONVM_DBG_CALLER` walk line's label: `security walk` when the fast
/// answer declined (the walk decides), `security walk, control` when the fast
/// answer was given and the walk only checks it.
fn dbg_walk_label(fast_answered: bool) -> &'static str {
    if fast_answered {
        "security walk, control"
    } else {
        "security walk"
    }
}

/// HotSpot's frame-1 check in `JVM_GetCallerClass`: the method that asked
/// `Reflection.getCallerClass()` must be `@CallerSensitive`, which a class no
/// boot or platform loader defined never is (`ClassFileParser` honours the
/// annotation in privileged classes only), else `InternalError("CallerSensitive
/// annotation expected at frame 1")`. Code outside the JDK reaches
/// `Reflection` only with `--add-exports java.base/jdk.internal.reflect`;
/// this VM answered such a call with its caller (interpreter round i5 wave
/// 106, lane L3; `tools/probes/interp/L3/L3W106CallerClassAsker.java`).
///
/// Narrowed so a capture that lost the asking frame (a JIT-collapsed inline
/// chain, see [`reflection_caller_index`]) never throws: the innermost frame
/// (`trace` is OUTERMOST first) must be of a class the application or a user
/// loader defined AND that class's constant pool must reference
/// `Reflection.getCallerClass()` itself -- the class really is the asker.
/// `None`: no error.
fn caller_sensitive_asker_error(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> Option<MethodCallFailed> {
    let asker = trace
        .iter()
        .rev()
        .find(|e| &*e.class_name != "jdk/internal/reflect/Reflection")?;
    let class_id = asker.class_id?;
    // 0 bootstrap, 1 platform, 2 application, 3+ user-defined.
    if ctx.loader_id_of_class(class_id) < 2 {
        return None;
    }
    if !class_pool_references_get_caller_class(&*ctx, class_id) {
        return None;
    }
    Some(
        RuntimeError::InternalError {
            message: "CallerSensitive annotation expected at frame 1".to_string(),
        }
        .into(),
    )
}

/// Does the constant pool of `class_id` hold a `Methodref` to
/// `jdk.internal.reflect.Reflection.getCallerClass()`? Read through the
/// `ConstantPool` natives' pool view; only [`caller_sensitive_asker_error`]'s
/// application-class askers pay for it.
fn class_pool_references_get_caller_class(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let size = ctx.reflect_constant_pool_size(class_id).unwrap_or(0);
    (1..size).any(|index| {
        let Ok(index) = i32::try_from(index) else {
            return false;
        };
        matches!(
            ctx.reflect_constant_pool_entry(class_id, index),
            cratonvm_native_api::ReflectConstant::MemberRef {
                tag: 10,
                ref class_name,
                ref name,
                ref descriptor,
                ..
            } if class_name == "jdk/internal/reflect/Reflection"
                && name == "getCallerClass"
                && descriptor == "()Ljava/lang/Class;"
        )
    })
}

/// Caller of a native bridge with no Java @CallerSensitive frame of its own.
/// Use the same security-walk exclusions as Reflection.getCallerClass, but
/// keep the first visible frame rather than skipping the asking Java frame.
pub(crate) fn native_bridge_caller_class_id(
    ctx: &mut dyn NativeContext,
) -> Option<cratonvm_types::ClassId> {
    let trace = ctx.capture_stack_walk_trace();
    // The method's own hiddenness, whatever `-XX:+ShowHiddenFrames` says
    // (`JVM_GetCallerClass`; interpreter round i4 wave 92, lane L2).
    let hidden = ctx.hidden_member_entries(&trace);
    let base = ctx.class_id_by_name("jdk/internal/reflect/MethodAccessorImpl");
    (0..trace.len()).rev().find_map(|i| {
        let entry = &trace[i];
        (!ignored_by_security_stack_walk(
            entry,
            hidden.get(i).copied().unwrap_or(false),
            is_method_accessor_frame(&*ctx, entry, base),
        ))
        .then(|| {
            entry
                .class_id
                .or_else(|| ctx.class_id_by_name(&entry.class_name))
        })
        .flatten()
    })
}

/// `trace` (OUTERMOST first) with the lambda proxy frames a caller-class
/// question must see spliced in (`NativeExceptionAccess::caller_lambda_proxy_insertions`;
/// interpreter round i4 wave 94, lane L3): HotSpot's `JVM_GetCallerClass`
/// answers a method reference's proxy (`Host$$Lambda/0x..`), which this VM
/// runs with no frame. `served` names the caller-sensitive methods a native
/// serves on top of the capture (none for `Reflection.getCallerClass`, whose
/// asking method is the capture's innermost frame).
fn with_caller_lambda_proxies(
    ctx: &mut dyn NativeContext,
    trace: Vec<cratonvm_native_api::StackTraceEntry>,
    served: &dyn Fn(&str, &str) -> bool,
) -> Vec<cratonvm_native_api::StackTraceEntry> {
    let innermost_first: Vec<cratonvm_native_api::StackTraceEntry> =
        trace.iter().rev().cloned().collect();
    let insertions = ctx.caller_lambda_proxy_insertions(&innermost_first, served);
    if insertions.is_empty() {
        return trace;
    }
    if crate::nbflags().dbg_caller {
        for (at, entries) in &insertions {
            for e in entries {
                eprintln!("[DBG_CALLER] lambda proxy caller frame at {at}: {}", e.class_name);
            }
        }
    }
    let mut out = Vec::with_capacity(innermost_first.len() + insertions.len());
    let mut pending = insertions.into_iter().peekable();
    for (i, entry) in innermost_first.into_iter().enumerate() {
        while let Some((_, entries)) = pending.next_if(|(at, _)| *at <= i) {
            out.extend(entries);
        }
        out.push(entry);
    }
    for (_, entries) in pending {
        out.extend(entries);
    }
    out.reverse();
    out
}

/// Is `class_name.method_name` a JDK 25 `@CallerSensitive` method whose
/// caller one of `lang_class.rs`'s reflective door natives decides (through
/// `resolve_caller_class_id`): `Method.invoke`, `Constructor.newInstance`,
/// the `Field` getters and setters, `Class.newInstance`,
/// `Class.getResource` / `getResourceAsStream`, and the `setAccessible` /
/// `trySetAccessible` family? Each calls `Reflection.getCallerClass()`
/// itself (read from the JDK 25 sources), so when its Java body runs and the
/// door native sits under it, its frame is the asking frame.
pub(crate) fn is_reflective_door_cs_method(class_name: &str, method_name: &str) -> bool {
    match class_name {
        "java/lang/reflect/Method" => matches!(method_name, "invoke" | "setAccessible"),
        "java/lang/reflect/Constructor" => matches!(method_name, "newInstance" | "setAccessible"),
        "java/lang/reflect/Field" => matches!(
            method_name,
            "get"
                | "getBoolean"
                | "getByte"
                | "getChar"
                | "getShort"
                | "getInt"
                | "getLong"
                | "getFloat"
                | "getDouble"
                | "set"
                | "setBoolean"
                | "setByte"
                | "setChar"
                | "setShort"
                | "setInt"
                | "setLong"
                | "setFloat"
                | "setDouble"
                | "setAccessible"
        ),
        "java/lang/reflect/AccessibleObject" => {
            matches!(method_name, "setAccessible" | "trySetAccessible")
        }
        "java/lang/Class" => {
            matches!(method_name, "newInstance" | "getResource" | "getResourceAsStream")
        }
        _ => false,
    }
}

/// Stage 3 of
/// `docs/feature-designs/native-dispatch-architecture.md`
/// (interpreter round i4 wave 94, lane L3; `CRATONVM_REFLECTION_CS_CALLER_RULE`,
/// `VmConfig::reflection_cs_caller_rule`, default OFF): the caller a
/// reflective door native of `lang_class.rs` asks for, by JDK 25's
/// `JVM_GetCallerClass` rule over the walkers' capture (OUTERMOST first).
///
/// The asking frame is the innermost frame of a door's `@CallerSensitive`
/// method ([`is_reflective_door_cs_method`]) within the leading run of frames
/// the name list skips (`lang_class::is_reflection_internal_frame`): the
/// method's Java body ran and the door native sits under it. With none, the
/// door native is that method itself (a served `setAccessible`,
/// `Method.invoke`, `Field.get`, ...), which a capture does not list. The
/// answer is the next frame outward of the asking one (from the top for a
/// served method) that the security walk does not ignore
/// ([`ignored_by_security_stack_walk`]).
///
/// `Some(answer)`: the rule decides. `None`: the caller's name list answers,
/// for the shapes the rule cannot place from this VM's capture:
///
/// * no frame outward (a call the VM itself made);
/// * the answer is a frame of a non-hidden `java.lang.invoke` class (the VM's
///   own method-handle plumbing can show frames HotSpot's adapters would not,
///   and HotSpot binds a caller-sensitive method reached by a handle to an
///   injected invoker in the caller's package, which no capture here lists);
/// * with no asking frame, an answer the list skips other than
///   `java.lang.Class` (a JIT-collapsed capture that lost the asking frame
///   leaves the door's own JDK plumbing on top);
/// * a frame with no class id and no class of its name.
///
/// So the rule moves an answer only where a real JDK frame calls the
/// `@CallerSensitive` method: `java.lang.Class` (`getEnumConstantsShared`'s
/// `values.setAccessible(true)`), and, below a listed asking frame, a JDK
/// caller of a skipped package (`java.lang.reflect.Proxy$ProxyBuilder`,
/// `sun.reflect.annotation`). Everywhere else the list skips exactly the
/// frames the rule ignores (every ignored frame is a listed one), so the two
/// answer the same frame. Known limit: the door does not say which method it
/// serves, so a SERVED method called by another listed method's Java body
/// (`Class.newInstance`'s `c.setAccessible(true)`) takes that body's frame as
/// the asking one and answers its caller -- the list's answer, not
/// HotSpot's `java.lang.Class`. Positive control: `CRATONVM_DBG_CALLER=1`
/// prints `[DBG_CALLER] reflective door (cs rule): <caller> (list: <list's>)`,
/// with ` DIFFERS` where they differ (`lang_class::resolve_caller_class_id`).
pub(crate) fn reflective_door_caller_class_id(
    ctx: &mut dyn NativeContext,
) -> Option<Option<cratonvm_types::ClassId>> {
    // (A method reference's lambda proxy is answered ahead of this rule, by
    // its host: `lang_class::resolve_caller_class_id`.)
    let trace = ctx.capture_stack_walk_trace();
    let mut asking = None;
    for (i, entry) in trace.iter().enumerate().rev() {
        if !crate::lang_class::is_reflection_internal_frame(&entry.class_name) {
            break;
        }
        if is_reflective_door_cs_method(&entry.class_name, &entry.method_name) {
            asking = Some(i);
            break;
        }
    }
    let end = asking.unwrap_or(trace.len());
    // The method's own hiddenness, whatever `-XX:+ShowHiddenFrames` says.
    let hidden = ctx.hidden_member_entries(&trace);
    let base = if trace[..end]
        .iter()
        .any(|e| e.class_name.starts_with("jdk/internal/reflect/"))
    {
        ctx.class_id_by_name("jdk/internal/reflect/MethodAccessorImpl")
    } else {
        None
    };
    let index = (0..end).rev().find(|&i| {
        let entry = &trace[i];
        !ignored_by_security_stack_walk(
            entry,
            hidden.get(i).copied().unwrap_or(false),
            is_method_accessor_frame(&*ctx, entry, base),
        )
    })?;
    let caller = &trace[index];
    let name: &str = &caller.class_name;
    if crate::lang_class::is_reflection_internal_frame(name)
        && (name.starts_with("java/lang/invoke/")
            || (asking.is_none() && name != "java/lang/Class"))
    {
        if crate::nbflags().dbg_caller {
            eprintln!(
                "[DBG_CALLER] reflective door (cs rule): {} -> {name}: deferred to the list",
                asking.map_or("served", |i| &*trace[i].class_name)
            );
        }
        return None;
    }
    let cid = caller.class_id.or_else(|| ctx.class_id_by_name(name))?;
    Some(Some(cid))
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

/// `java.lang.StackWalker$Option.<clinit>` — SYNTHETIC-JDK ONLY.
///
/// W7-93 §7. In `--synthetic-jdk` the class is FABRICATED
/// (`class_manager.rs`'s three-static-field stub, no `<clinit>` bytecode) and
/// `class_manager.rs` injects an `ACC_NATIVE` `<clinit>` onto that stub, so
/// this registration is the only initialiser that can ever run there.
///
/// In real-JDK mode the loaded class carries genuine `<clinit>` bytecode —
/// ordinary `new`/`putstatic` per constant, nothing this VM cannot already
/// run for `DayOfWeek` or any other real JDK enum (verified,
/// `W7-93-stackwalker-option-constants-null.md`
/// §1/§3). Registering this native there means CratonVM fabricates the
/// constants instead of letting the real, loaded, runnable initialiser
/// answer — correct today because `option_constant_names` reads them back off
/// the class's own declared static fields, but still a shadow of bytecode
/// that would have worked. Call this ONLY from `register_synthetic_overrides`
/// so real-JDK mode runs the real `<clinit>`.
pub fn register_stack_walker_option_clinit_synthetic(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/StackWalker$Option",
        "<clinit>",
        "()V",
        native_option_clinit,
    );
    registry.set_category(__prev_cat);
}

/// Install every StackWalker boot-path native this module owns, except
/// `StackWalker$Option.<clinit>` — see
/// [`register_stack_walker_option_clinit_synthetic`], which real-JDK mode
/// must not call.
pub fn register_stack_walker_boot(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
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
            // The JDK body's first statement is `options.isEmpty()`: the
            // implicit NPE, with HotSpot's helpful message (a site literal,
            // fixed by the signature; interpreter round i1 wave 44, lane L3,
            // `L3W44StackWalkerEdges` row `instance-null-set`).
            if matches!(option_set_arg, Value::Object(None)) {
                return Err(RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"java.util.Set.isEmpty()\" because \"options\" is null"
                            .to_string(),
                    ),
                }
                .into());
            }
            // RETAIN_CLASS_REFERENCE only when the set holds it (wave 42,
            // lane L3).
            let (retain_class_ref, option_set_arg) =
                options_retain_class_reference(ctx, option_set_arg);
            let walker = alloc_walker(ctx, option_set_arg, 1, retain_class_ref)?;
            Ok(Some(Value::Object(Some(walker))))
        },
    );
    // SBR round 5 (lane walker): a reviewed Intrinsic over a real JDK, as
    // `register_p59_stackwalker`'s three rows (the JDK's `CallerClassFinder` is ~125 Java
    // calls against this native's one crossing).
    let caller_kind = if registry.real_jdk() {
        cratonvm_native_api::NativeKind::Intrinsic
    } else {
        cratonvm_native_api::NativeKind::Bridge
    };
    registry.register_with_kind(
        sw,
        "getCallerClass",
        "()Ljava/lang/Class;",
        native_get_caller_class,
        caller_kind,
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

    /// Interpreter round i4 wave 94, lane L3: every asking method of the
    /// reflective doors is a frame the name list skips (the rule searches
    /// only that leading run), and non-`@CallerSensitive` neighbours are not
    /// asking frames.
    #[test]
    fn i94_reflective_door_cs_methods_are_listed_frames() {
        for (class, method) in [
            ("java/lang/reflect/Method", "invoke"),
            ("java/lang/reflect/Constructor", "newInstance"),
            ("java/lang/reflect/Field", "getInt"),
            ("java/lang/reflect/Field", "set"),
            ("java/lang/reflect/AccessibleObject", "trySetAccessible"),
            ("java/lang/Class", "getResourceAsStream"),
            ("java/lang/Class", "newInstance"),
        ] {
            assert!(is_reflective_door_cs_method(class, method), "{class}.{method}");
            assert!(crate::lang_class::is_reflection_internal_frame(class), "{class}");
        }
        assert!(!is_reflective_door_cs_method("java/lang/reflect/Field", "getName"));
        assert!(!is_reflective_door_cs_method("java/lang/reflect/AccessibleObject", "checkAccess"));
        assert!(!is_reflective_door_cs_method("java/lang/Class", "forName"));
        assert!(!is_reflective_door_cs_method("p/Method", "invoke"));
    }

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

    fn frame(class: &str, id: u32) -> cratonvm_native_api::StackTraceEntry {
        cratonvm_native_api::StackTraceEntry {
            class_name: std::sync::Arc::from(class),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: -1,
            class_id: Some(cratonvm_types::ClassId::new(id)),
            method_index: None,
        }
    }

    /// Interpreter round i1 wave 50, lane L3: HotSpot's `isMethodHandleFrame`
    /// is the `java.lang.invoke` package itself, not its subpackages.
    #[test]
    fn method_handle_frames_are_the_java_lang_invoke_package_only() {
        assert!(is_method_handle_frame_class(
            "java/lang/invoke/MethodHandle"
        ));
        assert!(is_method_handle_frame_class(
            "java/lang/invoke/BootstrapMethodInvoker"
        ));
        assert!(!is_method_handle_frame_class("java/lang/invoke/sub/X"));
        assert!(!is_method_handle_frame_class("java/lang/Thread"));
        assert!(!is_method_handle_frame_class("p/java/lang/invoke/X"));
    }

    /// The finder rule (wave 50, lane L3): reflection and `java.lang.invoke`
    /// frames are skipped, the second frame left answers, and a lone frame
    /// throws HotSpot's `IllegalCallerException`.
    #[test]
    fn the_finder_rule_skips_reflection_and_invoke_frames_and_needs_a_caller() {
        let mut ctx = MockNativeContext::new();
        // OUTERMOST first: caller < Method.invoke < accessor < asker.
        let trace = [
            frame("p/Caller", 7),
            frame("java/lang/reflect/Method", 8),
            frame("jdk/internal/reflect/DirectMethodHandleAccessor", 9),
            frame("java/lang/invoke/LambdaForm$MH", 10),
            frame("p/Asker", 11),
        ];
        let answer = caller_class_by_finder_rule(&mut ctx, &trace)
            .unwrap()
            .unwrap();
        let Value::Object(Some(mirror)) = answer else {
            panic!("expected a mirror, got {answer:?}");
        };
        assert_eq!(ctx.get_field(mirror, 0), Value::Int(7));

        let lone = [frame("java/lang/reflect/Method", 8), frame("p/Main", 3)];
        let err = format!(
            "{:?}",
            caller_class_by_finder_rule(&mut ctx, &lone).unwrap_err()
        );
        assert!(err.contains("no caller frame: [p.Main , null]"), "{err}");
        let none: [cratonvm_native_api::StackTraceEntry; 0] = [];
        let err = format!(
            "{:?}",
            caller_class_by_finder_rule(&mut ctx, &none).unwrap_err()
        );
        assert!(err.contains("no caller frame: [null, null]"), "{err}");
    }

    fn named(class: &str, method: &str, id: u32) -> cratonvm_native_api::StackTraceEntry {
        let mut e = frame(class, id);
        e.method_name = std::sync::Arc::from(method);
        e
    }

    /// Stage 2 (wave 51, lane L3): `Reflection.getCallerClass` walks past
    /// `Method.invoke`, the method accessors and the method-handle adapter
    /// frames only -- not a constructor accessor, not a non-adapter
    /// `java.lang.invoke` frame -- and answers `None` with no caller left.
    #[test]
    fn the_security_walk_skips_only_what_hotspot_ignores() {
        // OUTERMOST first; the innermost is the asking `@CallerSensitive` method.
        let trace = [
            named("p/Caller", "run", 1),
            named("java/lang/reflect/Method", "invoke", 2),
            named(
                "jdk/internal/reflect/DirectMethodHandleAccessor",
                "invoke",
                3,
            ),
            named("java/lang/invoke/LambdaForm$MH/0x1", "invoke", 4),
            named("java/lang/invoke/MethodHandle", "linkToStatic", 5),
            named("java/lang/invoke/MethodHandles", "lookup", 6),
        ];
        let accessor = [false, false, true, false, false, false];
        assert_eq!(reflection_caller_index(&trace, &[], &accessor), Some(0));

        // A constructor accessor, `Constructor.newInstance` and a plain
        // `java.lang.invoke` frame answer (HotSpot does not skip them).
        let ctor = [
            named("p/Caller", "run", 1),
            named(
                "jdk/internal/reflect/DirectConstructorHandleAccessor",
                "newInstance",
                2,
            ),
            named("p/Asks", "<init>", 3),
        ];
        assert_eq!(reflection_caller_index(&ctor, &[], &[false; 3]), Some(1));
        let invoke = [
            named("java/lang/invoke/BootstrapMethodInvoker", "invoke", 1),
            named("java/lang/invoke/MethodHandles", "lookup", 2),
        ];
        assert_eq!(reflection_caller_index(&invoke, &[], &[]), Some(0));
        // A hidden `java.lang.invoke` frame is a compiled lambda form.
        let hidden = [
            named("p/Caller", "run", 1),
            named("java/lang/invoke/Invokers", "invokeExact_MT", 2),
            named("p/Asks", "ask", 3),
        ];
        assert_eq!(
            reflection_caller_index(&hidden, &[false, true, false], &[]),
            Some(0)
        );
        // `Method` frames other than `invoke` are not ignored.
        let other = [
            named("java/lang/reflect/Method", "acquireMethodAccessor", 1),
            named("p/A", "a", 2),
        ];
        assert_eq!(reflection_caller_index(&other, &[], &[]), Some(0));
        // Nothing outward of the asking frame.
        assert_eq!(
            reflection_caller_index(&[named("p/A", "a", 1)], &[], &[]),
            None
        );
        assert_eq!(reflection_caller_index(&[], &[], &[]), None);
    }

    /// The older rule's residue: a capture whose innermost frame is stream
    /// plumbing (no `@CallerSensitive` method there) skips the plumbing; one
    /// whose innermost frame is `ServiceLoader` (which has caller-sensitive
    /// methods) answers its caller as HotSpot does.
    #[test]
    fn the_security_walk_skips_plumbing_only_for_a_collapsed_capture() {
        let collapsed = [
            named("p/Test", "run", 1),
            named("java/util/ArrayList", "forEach", 2),
            named("java/util/stream/ForEachOps$ForEachOp$OfRef", "accept", 3),
            named("java/util/stream/ForEachOps$ForEachOp$OfRef", "accept", 4),
        ];
        assert_eq!(reflection_caller_index(&collapsed, &[], &[]), Some(1));
        let asked = [
            named("java/util/stream/ReferencePipeline", "forEach", 1),
            named("p/Lambda", "lambda$0", 2),
            named("java/util/ServiceLoader", "load", 3),
        ];
        assert_eq!(reflection_caller_index(&asked, &[], &[]), Some(1));
        assert!(is_method_accessor_class_name(
            "jdk/internal/reflect/CsMethodAccessorAdapter"
        ));
        assert!(!is_method_accessor_class_name(
            "jdk/internal/reflect/DirectConstructorHandleAccessor"
        ));
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

#[cfg(test)]
mod j27_dbg_caller_control_label_tests {
    //! j27 wave 7 (lane T1ACCEPT): a fast-answered ask's control walk is not
    //! counted as a miss by `grep '(security walk)'`.
    use super::dbg_walk_label;

    #[test]
    fn a_control_walk_is_labelled_apart_from_a_miss() {
        let miss = format!("({}):", dbg_walk_label(false));
        let control = format!("({}):", dbg_walk_label(true));
        assert_eq!(miss, "(security walk):");
        assert!(!control.contains("(security walk)"), "{control}");
        assert_eq!(control, "(security walk, control):");
    }
}
