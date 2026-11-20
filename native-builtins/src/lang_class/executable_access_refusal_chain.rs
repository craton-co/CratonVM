#[allow(unused_imports)]
use super::*;

/// The JDK frames HotSpot lists for an ACCESS refusal of `Method.invoke` or
/// `Constructor.newInstance`, OUTERMOST first: the served method at its call
/// of `AccessibleObject.checkAccess(Class, Class, Class, int)` (through
/// `Constructor.newInstanceWithCaller` for a constructor), that at its call
/// of `Reflection.newIllegalAccessException(..)`, which builds the throwable
/// (JDK 25 source: `Method.invoke` and `newInstanceWithCaller` call
/// `checkAccess` before the accessor exists). Every refusal of the member,
/// module and class halves comes out of that one `checkAccess`. Interpreter
/// round i3 wave 74, lane L3 -- the `Method` / `Constructor` twin of wave
/// 73's [`field_access_refusal_chain`].
pub fn executable_access_refusal_chain(constructor: bool) -> cratonvm_native_api::ServedRaiseChain {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const AO_CHECK: (&str, &str) = (
        "checkAccess",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I)V",
    );
    const NEW_IAE: (&str, &str) = (
        "newIllegalAccessException",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;I)Ljava/lang/IllegalAccessException;",
    );
    const CTOR: &str = "java/lang/reflect/Constructor";
    const WITH_CALLER: (&str, &str) = (
        "newInstanceWithCaller",
        "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;",
    );
    let hop = |class: &'static str, (name, descriptor): (&'static str, &'static str), site| {
        ServedRaiseHop {
            class,
            name,
            descriptor,
            site,
        }
    };
    let last = |(name, descriptor): (&'static str, &'static str)| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    let mut hops = if constructor {
        vec![
            hop(
                CTOR,
                ("newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;"),
                last(WITH_CALLER),
            ),
            hop(CTOR, WITH_CALLER, last(AO_CHECK)),
        ]
    } else {
        vec![hop(
            "java/lang/reflect/Method",
            (
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            last(AO_CHECK),
        )]
    };
    hops.push(hop(
        "java/lang/reflect/AccessibleObject",
        AO_CHECK,
        last(NEW_IAE),
    ));
    hops.push(hop(
        "jdk/internal/reflect/Reflection",
        NEW_IAE,
        ServedRaiseHopSite::Raise,
    ));
    ServedRaiseChain {
        throwable_class: "java/lang/IllegalAccessException",
        hops,
    }
}

/// Note [`executable_access_refusal_chain`] for `error` when it is an
/// `IllegalAccessException` the served door built itself (its only ones are
/// the access phase's refusals: a callee's throwable comes back wrapped in an
/// `InvocationTargetException`), so nothing else can carry the chain.
#[cold]
#[inline(never)]
pub fn note_executable_access_refusal_chain(
    ctx: &mut dyn NativeContext,
    constructor: bool,
    error: &cratonvm_types::error::MethodCallFailed,
) {
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
    if matches!(
        error,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalAccessException { .. }
        ))
    ) {
        ctx.note_served_raise_chain(executable_access_refusal_chain(constructor));
    }
}

/// Interpreter round i3 wave 75, lane L3 (`CRATONVM_REFLECT_INVOKE_SETTLED`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// a planned static `Method.invoke` target's dispatch through the settling
/// static door (`NativeInvokeAccess::invoke_static_settling`) on its
/// declaring class `owner`, with the settled owner its plan remembers. A
/// settled call skips the door's initialization question and member
/// resolution (and, with the native-callback memo, the by-name tail
/// altogether); the first calls are the by-name static tail exactly. The
/// declaring class was initialized by [`reflective_door_initialize`] above
/// the call, whose failure is raised unwrapped; a failure here is the
/// callee's, wrapped by the caller as before. Only for a target whose door
/// notes nothing (a class outside `java/` and `jdk/`): the settling door's
/// note would stand in for `note_reflective_target`'s otherwise.
pub fn invoke_static_settled_on_plan(
    ctx: &mut dyn NativeContext,
    plan: &MethodPlan,
    owner: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    use std::sync::atomic::Ordering;
    let raw = plan.settled.load(Ordering::Relaxed);
    let settled = raw.checked_sub(1).map(ClassId::new);
    let (result, now_settled) = ctx.invoke_static_settling(
        Some(owner),
        settled,
        class_name,
        method_name,
        descriptor,
        args,
    );
    if let Some(now) = now_settled.filter(|now| settled != Some(*now)) {
        plan.settled
            .store(now.as_u32().wrapping_add(1), Ordering::Relaxed);
    }
    result
}

pub fn native_method_invoke(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_method_invoke_timed(ctx, args, &mut PhaseTimer::off())
}

/// [`native_method_invoke`] with the door's phase marks
/// (`CRATONVM_REFLECT_DOOR_PHASE_STATS`, interpreter round i3 wave 77, lane
/// L3): the boxing wrapper's by-descriptor path.
pub fn native_method_invoke_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ph: &mut PhaseTimer,
) -> MethodCallResult {
    // Interpreter round i3 wave 74, lane L3: an access refusal lists
    // `AccessibleObject.checkAccess` -> `Reflection.newIllegalAccessException`
    // under `Method.invoke`, as HotSpot does (both modes).
    let result = native_method_invoke_body(ctx, args, true, None, ph, &mut false);
    if let Err(e) = &result {
        note_executable_access_refusal_chain(ctx, false, e);
    }
    result
}

/// Interpreter round i3 wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the words of a `Method` a planned call reads, in one
/// [`NativeContext::get_fields_by_name`] batch -- `slot` (the plan id),
/// `clazz` and `override` -- instead of three single-name reads (each a
/// canonicalisation, a header read and a field-name ring probe).
pub static METHOD_DOOR_WORDS: [&str; 3] = ["slot", "clazz", "override"];

/// What [`METHOD_DOOR_WORDS`] answered for a `Method` whose `slot` names a
/// plan of this VM.
pub struct MethodDoorWords {
    pub plan: Arc<MethodPlan>,
    pub clazz: Value,
    pub override_word: Value,
}

/// [`METHOD_DOOR_WORDS`] of `method`, when its `slot` names a plan.
pub fn method_door_words(ctx: &dyn NativeContext, method: ObjectRef) -> Option<MethodDoorWords> {
    let mut out = [Value::Object(None); 3];
    ctx.get_fields_by_name(method, &METHOD_DOOR_WORDS, &mut out);
    let Value::Int(id) = out[0] else {
        return None;
    };
    if id <= 0 {
        return None;
    }
    let plan = method_plans(ctx)?.get(id)?;
    Some(MethodDoorWords {
        plan,
        clazz: out[1],
        override_word: out[2],
    })
}

/// [`read_method_accessible`] from a batched `override` word: the same
/// answer, with the extra-slot base remembered on the plan. A word that is
/// not an `int` (a layout with no `override` field) takes the full read.
pub fn read_method_accessible_batched(
    ctx: &dyn NativeContext,
    method_obj: ObjectRef,
    override_word: Value,
    plan: &MethodPlan,
) -> bool {
    let Value::Int(v) = override_word else {
        return read_method_accessible(ctx, method_obj);
    };
    if v != 0 {
        return true;
    }
    let class_id = ctx.class_id_of_object(method_obj);
    let base = plan.method_extra_base_cached(ctx, class_id);
    if ctx.object_num_fields(method_obj) <= base + METHOD_EXTRA_OFFSET_ACCESSIBLE {
        return false;
    }
    matches!(
        ctx.get_field(method_obj, base + METHOD_EXTRA_OFFSET_ACCESSIBLE),
        Value::Int(v) if v != 0
    )
}

/// `Method.invoke` for the boxing wrapper
/// (`lang_reflect::native_method_invoke_boxed`) under
/// `CRATONVM_REFLECT_DOOR_WORD_BATCH`: [`native_method_invoke`] on the
/// method's batched words, answered with its plan so the wrapper takes the
/// return descriptor from it instead of building and parsing a descriptor
/// `String` per call. `None` -- the switch or the plan switch off, a null
/// `Method`, or a method with no plan -- means the caller takes the old
/// path, which reads every word itself.
///
/// Wave 77, lane L3: `ph` takes the door's phase marks
/// (`CRATONVM_REFLECT_DOOR_PHASE_STATS`), and the answer's `bool` says the
/// body boxed a primitive the callee returned itself
/// (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS` only): the box is a wrapper, never
/// a `Class` mirror, so the wrapper's return-shape check has nothing to find.
pub fn native_method_invoke_planned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ph: &mut PhaseTimer,
) -> Option<(MethodCallResult, Arc<MethodPlan>, bool)> {
    if !ctx.reflect_door_word_batch() || !ctx.reflect_invoke_plan() {
        return None;
    }
    let Some(Value::Object(Some(this))) = args.first() else {
        return None;
    };
    let words = method_door_words(&*ctx, *this)?;
    ph.mark(crate::reflect_door_phases::PH_WORDS);
    let plan = Arc::clone(&words.plan);
    let mut fresh_box = false;
    let result = native_method_invoke_body(ctx, args, true, Some(words), ph, &mut fresh_box);
    if let Err(e) = &result {
        note_executable_access_refusal_chain(ctx, false, e);
    }
    Some((result, plan, fresh_box))}

/// `DirectMethodHandleAccessor$NativeAccessor.invoke0(Method, Object,
/// Object[])`: HotSpot's `JVM_InvokeMethod`. It performs NO access check --
/// the JDK's `Method.invoke` in front of the accessor did, with the true
/// caller -- and the accessor may hold an earlier copy of the method (the root
/// caches it) whose `override` flag says nothing about this call, so the body
/// runs with its member, class and module gates off, as
/// [`native_constructor_new_instance0`] does for constructors. Reached only
/// when `Method.invoke` runs its own bytecode (`CRATONVM_JDK_REFLECTIVE_INVOKE`,
/// interpreter round i2 wave 59, lane L3; the i45-L3 proposal): no reflective
/// call record is entered, because the JDK frames it would synthesize are
/// real on that path.
pub fn native_method_invoke0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_method_invoke_body(ctx, args, false, None, &mut PhaseTimer::off(), &mut false)
}

/// `JVM_InvokeMethod`'s receiver-check text (`Reflection::invoke`), for
/// `NativeAccessor.invoke0` (`CRATONVM_REFLECT_INVOKE0_VM_TEXTS`; measured on
/// HotSpot 25 by `R21ReflectInvoke` `fact sigpoly-wrong-receiver`).
pub const INVOKE0_RECEIVER_TEXT: &str = "object is not an instance of declaring class";
/// `JVM_InvokeMethod`'s argument-count text (`fact sigpoly-wrong-arity`).
pub const INVOKE0_ARITY_TEXT: &str = "wrong number of arguments";

/// `CRATONVM_REFLECT_INVOKE0_VM_TEXTS` -- correctness, default ON, `=0`
/// restores the Java accessor's texts on `invoke0` (JIT round 21 wave 1, lane
/// reflect). A `NativeFlags` field load.
pub fn invoke0_vm_texts_enabled() -> bool {
    crate::nbflags().reflect_invoke0_vm_texts
}

/// `CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_TEXTS` -- correctness, default ON,
/// `=0` keeps the `Method.invoke` door on the Java accessor's texts for every
/// method (JIT round 21 wave 1, lane reflect; review item 3 of
/// `docs/feature-designs/native-dispatch-architecture.md`).
pub fn door_native_accessor_texts_enabled() -> bool {
    crate::nbflags().reflect_door_native_accessor_texts
}

/// `CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS` -- correctness, default ON,
/// `=0` restores the Java accessor's argument texts on `invoke0` /
/// `newInstance0` and the `Constructor.newInstance` door's arity text (JIT
/// round 21 wave 8, lane reflect2; page
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// A `NativeFlags` field load.
pub fn native_accessor_arg_texts_enabled() -> bool {
    crate::nbflags().reflect_native_accessor_arg_texts
}

/// Does the `Method.invoke` / `Constructor.newInstance` door (`gate_access`)
/// stand in front of a `NativeAccessor` for EVERY member because
/// `-Djdk.reflect.useNativeAccessorOnly=true` is set? JDK 25's
/// `MethodHandleAccessorFactory.useNativeAccessor` asks
/// `ReflectionFactory.useNativeAccessorOnly()`, which is
/// `"true".equals(System.getProperty("jdk.reflect.useNativeAccessorOnly"))`
/// once the module system is up. The JDK's own accessor path
/// (`CRATONVM_JDK_REFLECTIVE_INVOKE`) reads it itself and never reaches here
/// with `gate_access`. Callers ask only on a refusal: the property read is
/// off the call's happy path. `CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_ONLY`
/// (correctness, default ON, `=0` ignores the property; JIT round 21 wave 8,
/// lane reflect2).
///
/// Read once per VM (r21 review rev-w68-native item 3, JIT round 21 wave 10,
/// lane walk): JDK 25 caches the answer in `ReflectionFactory.config()`, so a
/// later `System.setProperty` does not change it. The first refusal's answer
/// is kept in [`ReflectDoorCaches::native_accessor_only`]; a context with no
/// cache slot (mock contexts) reads the property each time.
///
/// JIT round 21 wave 13 (review rev-native item 4 of
/// `r21-review-w13-native-findings-20261019.md`): the cell is filled at the
/// JDK's own point, not at the first refusal. JDK 25's
/// `MethodHandleAccessorFactory.useNativeAccessor` returns `true` before it
/// asks `ReflectionFactory.useNativeAccessorOnly()` while
/// `!VM.isJavaLangInvokeInited()`, and `ReflectionFactory.config()` answers
/// the uncached `DEFAULT_CONFIG` (false) while `!VM.isModuleSystemInited()`
/// and otherwise loads and caches the property for good. So the answer is
/// fixed by the first accessor creation after both inits (or by whatever JDK
/// code called `config()` first, which [`jdk_reflection_config_native_accessor_only`]
/// reads from the JDK's own static). The door stands in for that accessor
/// creation, so every door call offers the fill
/// ([`door_notes_reflection_config_point`]), happy path included; once the
/// cell is set that is one atomic load. A refusal before the point answers
/// `false` and fills nothing (`DEFAULT_CONFIG` is not cached).
pub fn door_honours_native_accessor_only(ctx: &dyn NativeContext, gate_access: bool) -> bool {
    if !gate_access || !crate::nbflags().reflect_door_native_accessor_only {
        return false;
    }
    match reflect_door_caches(ctx) {
        Some(caches) => native_accessor_only_cached(&caches.native_accessor_only, || {
            jdk_reflection_config_native_accessor_only(ctx)
        }),
        None => native_accessor_only_property(
            ctx.get_system_property("jdk.reflect.useNativeAccessorOnly").as_deref(),
        ),
    }
}

/// The door's offer of the JDK's config point (see
/// [`door_honours_native_accessor_only`]): fills this VM's cell when it is
/// still empty and the JDK would have fixed the answer by now. Reads only (no
/// allocation, no Java), so it is no GC point; after the fill, one atomic load.
#[inline]
pub fn door_notes_reflection_config_point(ctx: &dyn NativeContext, gate_access: bool) {
    if !gate_access || !crate::nbflags().reflect_door_native_accessor_only {
        return;
    }
    if let Some(caches) = reflect_door_caches(ctx) {
        if caches.native_accessor_only.get().is_none() {
            let _ = native_accessor_only_cached(&caches.native_accessor_only, || {
                jdk_reflection_config_native_accessor_only(ctx)
            });
        }
    }
}

/// The per-VM `cell`'s answer: the cell's value once set; otherwise `fixed()`
/// -- `Some(answer)` when the JDK would have fixed it by now, which fills the
/// cell, or `None` before that point, which answers `false` (JDK 25's
/// uncached `DEFAULT_CONFIG`) and leaves the cell empty.
pub fn native_accessor_only_cached(
    cell: &std::sync::OnceLock<bool>,
    fixed: impl FnOnce() -> Option<bool>,
) -> bool {
    if let Some(v) = cell.get() {
        return *v;
    }
    match fixed() {
        Some(v) => *cell.get_or_init(|| v),
        None => false,
    }
}

/// What JDK 25's `ReflectionFactory.useNativeAccessorOnly()` answers for an
/// accessor created now, or `None` while it would still answer the uncached
/// default:
///
/// 1. `ReflectionFactory.config` already set (some JDK code called
///    `config()`): its `useNativeAccessorOnly` component, verbatim.
/// 2. Else, once `VM.isModuleSystemInited()` (`initLevel >= 2`) and
///    `VM.isJavaLangInvokeInited()`: the property now (`loadConfig`), which
///    the accessor factory's first `config()` would cache.
/// 3. Else `None`.
///
/// A `VM` whose statics cannot be read counts as initialized (the
/// pre-wave-13 answer: the property read now). Reads of loaded state only.
pub fn jdk_reflection_config_native_accessor_only(ctx: &dyn NativeContext) -> Option<bool> {
    let read_property = || {
        native_accessor_only_property(
            ctx.get_system_property("jdk.reflect.useNativeAccessorOnly").as_deref(),
        )
    };
    if let Some(rf) = ctx.class_id_by_name("jdk/internal/reflect/ReflectionFactory") {
        if let Some(index) = ctx.static_field_index_by_name(rf, "config") {
            if let Value::Object(Some(config)) = ctx.get_static_field(rf, index) {
                return Some(matches!(
                    ctx.get_field_by_name(config, "useNativeAccessorOnly"),
                    Value::Int(v) if v != 0
                ));
            }
        }
    }
    let Some(vm) = ctx.class_id_by_name("jdk/internal/misc/VM") else {
        return Some(read_property());
    };
    let int_static = |name: &str| match ctx.static_field_index_by_name(vm, name) {
        Some(index) => match ctx.get_static_field(vm, index) {
            Value::Int(v) => Some(v),
            _ => None,
        },
        None => None,
    };
    let (Some(init_level), Some(jli)) = (int_static("initLevel"), int_static("javaLangInvokeInited"))
    else {
        return Some(read_property());
    };
    jdk_config_point_reached(init_level, jli != 0).then(read_property)}

/// `VM.isModuleSystemInited()` (`initLevel >= MODULE_SYSTEM_INITED`, 2) and
/// `VM.isJavaLangInvokeInited()`: the first accessor creation after both
/// caches `ReflectionFactory.config()`.
pub fn jdk_config_point_reached(init_level: i32, java_lang_invoke_inited: bool) -> bool {
    const MODULE_SYSTEM_INITED: i32 = 2;
    init_level >= MODULE_SYSTEM_INITED && java_lang_invoke_inited
}

/// `ReflectionFactory.loadConfig`'s test: exactly `"true"` (no case folding,
/// no trimming).
pub fn native_accessor_only_property(value: Option<&str>) -> bool {
    value == Some("true")
}

/// `JVM_InvokeMethod` / `JVM_NewInstanceFromConstructor`'s argument refusal
/// (`Reflection::invoke`), for a failed argument coercion `e` of `arg_val`
/// into a parameter of descriptor `pdesc`: a null for a primitive parameter is
/// `unbox_for_primitive`'s `IllegalArgumentException` with NO message (and no
/// cause); every other mismatch is `widen`'s / the `is_a` test's plain
/// `IllegalArgumentException("argument type mismatch")`, with no cause either
/// (the Java accessors' `NullPointerException` / `ClassCastException` causes
/// come from `ValueConversions` and the handle's cast, which never run on the
/// native path). A failure that is not a plain `IllegalArgumentException` is
/// returned as it is. Raised from the native frame: no reflective check is
/// noted (the JDK frames it would list are the Java accessor's). JIT round 21
/// wave 8, lane reflect2 (`CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS`).
pub fn native_accessor_argument_refusal(
    ctx: &mut dyn NativeContext,
    arg_val: Value,
    pdesc: &str,
    e: MethodCallFailed,
) -> MethodCallFailed {
    let is_plain_iae = matches!(
        &e,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::IllegalArgumentException { .. }
        ))
    );
    if !is_plain_iae {
        return e;
    }
    if !native_accessor_null_to_primitive(arg_val, pdesc) {
        return illegal_arg_exc(INVOKE0_ARGUMENT_TEXT.to_string());
    }
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/IllegalArgumentException",
        "()V",
        &[],
        || e,
    )
}

/// `JVM_InvokeMethod`'s argument-mismatch text (`Reflection::widen`, the
/// reference parameter's `is_a` test).
pub const INVOKE0_ARGUMENT_TEXT: &str = "argument type mismatch";

/// Is this refusal `unbox_for_primitive`'s null arm (a null argument for a
/// primitive parameter), the one `IllegalArgumentException` HotSpot's
/// native accessor raises without a message?
pub fn native_accessor_null_to_primitive(arg_val: Value, pdesc: &str) -> bool {
    matches!(arg_val, Value::Object(None))
        && matches!(pdesc, "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C")
}

/// Would JDK 25's `MethodHandleAccessorFactory.useNativeAccessor` pick
/// `NativeAccessor` (HotSpot's `JVM_InvokeMethod`) for this method, once
/// `java.lang.invoke` is initialized and without
/// `-Djdk.reflect.useNativeAccessorOnly`? Its three member rules:
/// `isSignaturePolymorphicMethod` (native, varargs, one `Object[]` parameter,
/// declared by `MethodHandle` or `VarHandle`); a varargs method with no
/// parameter or whose last parameter is not an array; `slotCount >= 255`
/// (one per parameter, one more per `long` / `double`, one for the receiver).
/// `param_descs` are the parameter descriptors, `modifiers` the `Method`'s
/// (`ACC_VARARGS` 0x80, `ACC_NATIVE` 0x100 included, as HotSpot fills them).
pub fn method_takes_native_accessor(
    class_name: &str,
    modifiers: i32,
    is_static: bool,
    param_descs: &[String],
) -> bool {
    const ACC_VARARGS: i32 = 0x0080;
    const ACC_NATIVE: i32 = 0x0100;
    let varargs = modifiers & ACC_VARARGS != 0;
    let signature_polymorphic = varargs
        && modifiers & ACC_NATIVE != 0
        && matches!(
            class_name,
            "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
        )
        && param_descs.len() == 1
        && param_descs[0] == "[Ljava/lang/Object;";
    let varargs_without_array =
        varargs && param_descs.last().is_none_or(|last| !last.starts_with('['));
    let slots = param_descs.len()
        + param_descs.iter().filter(|p| matches!(p.as_str(), "J" | "D")).count()
        + usize::from(!is_static);
    signature_polymorphic || varargs_without_array || slots >= 255
}

/// The `UnsupportedOperationException` message HotSpot's native stub of a
/// signature-polymorphic method raises when that method is invoked through
/// reflection (`methodHandles.cpp` `MH_invoke_UOE` / `MH_invokeExact_UOE` /
/// `VH_UOE`), or `None` for every other method. JDK 25's
/// `MethodHandleAccessorFactory.useNativeAccessor` sends exactly these
/// (`isSignaturePolymorphicMethod`: native, varargs, one `Object[]`
/// parameter, declared by `MethodHandle` or `VarHandle`) to
/// `NativeAccessor.invoke0`. JIT round 21 wave 1, lane reflect
/// (`CRATONVM_REFLECT_SIG_POLY_UOE`).
pub fn reflective_signature_polymorphic_refusal(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    modifiers: i32,
) -> Option<&'static str> {
    const ACC_NATIVE: i32 = 0x0100;
    if !descriptor.starts_with("([Ljava/lang/Object;)") {
        return None;
    }
    match class_name {
        "java/lang/invoke/MethodHandle"
            if descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;" =>
        {
            match method_name {
                "invokeExact" => Some("MethodHandle.invokeExact cannot be invoked reflectively"),
                "invoke" => Some("MethodHandle.invoke cannot be invoked reflectively"),
                _ => None,
            }
        }
        "java/lang/invoke/VarHandle" if modifiers & ACC_NATIVE != 0 => {
            Some("VarHandle access mode methods cannot be invoked reflectively")
        }
        _ => None,
    }
}

/// `CRATONVM_DBG_INVOKE_COERCE=1`: dump the method, formal descriptors,
/// actual argument runtime types and innermost Java caller frames on a
/// `Method.invoke` coercion mismatch at parameter `i`. Env-gated; shared by
/// the two argument loops of [`native_method_invoke_body`] (wave 76, lane L3).
#[cold]
#[inline(never)]
pub fn dbg_invoke_coerce_dump(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    method_name: &str,
    i: usize,
    pdesc: &str,
    param_descs: &[String],
    raw_args: &[Value],
) {
    let arg_types: Vec<String> = raw_args
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => {
                let cid = ctx.class_id_of_object(*o);
                ctx.class_name_of_id(cid).unwrap_or_default()
            }
            Value::Object(None) => "null".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    let trace = ctx.capture_stack_trace();
    let total = trace.len();
    let skip = total.saturating_sub(24);
    let mut rendered = String::new();
    for (j, entry) in trace.iter().enumerate().skip(skip) {
        use std::fmt::Write as _;
        let _ = write!(
            rendered,
            "\n  #{j}/{total} {cls}.{m} (bci={bci})",
            cls = entry.class_name,
            m = entry.method_name,
            bci = entry.byte_code_index,
        );
    }
    tracing::warn!(
        target: "cratonvm::invoke_coerce",
        "[DBG_INVOKE_COERCE] {class_name}.{method_name} param[{i}]={pdesc} \
         params={param_descs:?} argTypes={arg_types:?} caller chain:{rendered}"
    );
}

/// `ph`: the door's phase marks (`CRATONVM_REFLECT_DOOR_PHASE_STATS`; an off
/// timer marks nothing). `fresh_box`: set when the return is a wrapper this
/// body boxed from a primitive the callee returned
/// (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`, planned calls only). Interpreter
/// round i3 wave 77, lane L3.
pub fn native_method_invoke_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    gate_access: bool,
    words: Option<MethodDoorWords>,
    ph: &mut PhaseTimer,
    fresh_box: &mut bool,
) -> MethodCallResult {
    use crate::reflect_door_phases::{
        PH_ARGS, PH_BOX, PH_CALL, PH_GATES, PH_INIT, PH_RECV, PH_SELECT, PH_WORDS,
    };
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Method.invoke: null Method".to_string()),
            }
            .into())
        }
    };
    // JIT round 21 wave 13 (review rev-native item 4): the accessor creation
    // this door stands for is where JDK 25 fixes `useNativeAccessorOnly`.
    door_notes_reflection_config_point(&*ctx, gate_access);

    // Interpreter round i3 wave 74, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN`):
    // the method's invocation plan, when its mint made one ([`MethodPlan`]).
    // Every fact below it answers is the one the by-name reads compute.
    // Wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`): the plan, `clazz`
    // and `override` already read in one batch ([`native_method_invoke_planned`]).
    let (plan_arc, batch_words) = match words {
        Some(w) => (Some(w.plan), Some((w.clazz, w.override_word))),
        None => (
            if ctx.reflect_invoke_plan() {
                method_plan_of(&*ctx, this)
            } else {
                None
            },
            None,
        ),
    };
    let plan: Option<&MethodPlan> = plan_arc.as_deref();
    ph.mark(PH_WORDS);
    // Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`):
    // a planned call takes the plan's caller-independent class facts.
    let plan_facts = plan.is_some() && ctx.reflect_invoke_plan_facts();

    // Read method metadata from the Method object via JDK field names
    // (C6: real-JDK layout differs from our synthetic slot indices).
    let modifiers = match plan {
        Some(p) => p.modifiers,
        None => match ctx.get_field_by_name(this, "modifiers") {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    let is_static = (modifiers & ACC_STATIC) != 0;

    // Get declaring class name. A batched non-null `clazz` is the named read
    // `method_clazz_value` answers first; anything else takes that read.
    let clazz_value = match batch_words {
        Some((Value::Object(Some(m)), _)) => Value::Object(Some(m)),
        _ => method_clazz_value(ctx, this),
    };
    // `mut`: re-read from its pin when the declaring class's `<clinit>` runs
    // below (wave 79).
    let mut declaring_mirror = match clazz_value {
        Value::Object(Some(m)) => m,
        other => {
            // CRATONVM_DBG_MINVOKE=1 вЂ” dump everything knowable about the
            // degraded Method object so the producer can be identified
            // (observed: Gradle DefaultServiceRegistry configure-method
            // dispatch receiving a Method whose clazz slot reads null).
            if crate::nbflags().dbg_minvoke {
                let name = match method_name_value(ctx, this) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => "<no name>".to_string(),
                };
                let nslots = ctx.object_num_fields(this);
                let cid = ctx.class_id_of_object(this);
                let cls = ctx.class_name_of_id(cid).unwrap_or_default();
                eprintln!(
                    "[MINVOKE-DBG] clazz={other:?} name={name} obj_class={cls} nslots={nslots} modifiers={modifiers}"
                );
            }
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Method.invoke: no declaring class".to_string()),
            }
            .into());
        }
    };
    let class_name: std::borrow::Cow<'_, str> = match plan {
        Some(p) => std::borrow::Cow::Borrowed(&*p.class_name),
        None => {
            std::borrow::Cow::Owned(mirror_class_name(ctx, declaring_mirror).unwrap_or_default())
        }
    };

    // Get method name
    let method_name: std::borrow::Cow<'_, str> = match plan {
        Some(p) => std::borrow::Cow::Borrowed(&*p.name),
        None => std::borrow::Cow::Owned(match method_name_value(ctx, this) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }),
    };

    // Serializable-lambda support: `ObjectOutputStream` reflectively invokes
    // the synthetic `writeReplace()` we expose on lambda proxies (see
    // `declared_methods_with_synthetic`) to obtain a `SerializedLambda`
    // stand-in. Handle it before the access check вЂ” the synthetic method is
    // private and `ObjectStreamClass` may invoke it without first toggling the
    // reflective `accessible` flag, which would otherwise raise
    // IllegalAccessException. Build the record from the proxy's call-site
    // metadata + captured fields so real OOS writes a portable form (which
    // deserializes via the JVM-generated `$deserializeLambda$` + invokedynamic
    // path) instead of the un-loadable `$$Lambda` class name.
    if method_name == "writeReplace" {
        let wr_desc = method_descriptor_for_invoke(ctx, this);
        if wr_desc == "()Ljava/lang/Object;" {
            if let Some(Value::Object(Some(recv))) = args.get(1) {
                if let Some(result) = lambda_proxy_write_replace(ctx, *recv) {
                    return result;
                }
            }
        }
    }
    // Interpreter round i4 wave 86, lane L4: the serialization-hostile pair of
    // an accidentally serializable lambda (see `declared_methods_with_synthetic`)
    // has no bytecode here; its JDK body is `throw new
    // NotSerializableException("Non-serializable lambda")`, which
    // `Method.invoke` wraps in `InvocationTargetException` (and
    // `ObjectOutputStream` unwraps as the `IOException` it is).
    if method_name == "writeObject" || method_name == "readObject" {
        if let Some(Value::Object(Some(recv))) = args.get(1) {
            if let Some(failure) = lambda_proxy_hostile_serialization(ctx, this, *recv, &method_name)
            {
                return Err(wrap_as_invocation_target_exception(ctx, failure));
            }
        }
    }

    // Capture the declaring class's EXACT id from the mirror once, before the
    // access checks. Both the caller-entitlement step and the JPMS step want
    // it, and resolving it by binary name instead is ambiguous when two child
    // loaders define the same name (see
    // `check_reflection_module_access_with_target_id`'s doc comment and
    // `regression-suite/src/RFieldSiteCache.java` `twoLoadersOneName`).
    let declaring_cid = match plan {
        Some(p) => Some(p.class_id),
        None => mirror_class_id(ctx, declaring_mirror),
    };

    // Access control: accessible flag lives in a CratonVM extra slot.
    let accessible = !gate_access
        || match (batch_words, plan) {
            (Some((_, override_word)), Some(p)) => {
                read_method_accessible_batched(ctx, this, override_word, p)
            }
            _ => read_method_accessible(ctx, this),
        };
    let is_public = (modifiers & ACC_PUBLIC) != 0;
    // Interpreter round i2 wave 59, lane L3: `Method.invoke` builds
    // `checkAccess(caller, clazz, isStatic ? null : obj.getClass(), modifiers)`'s
    // argument list first, so a null receiver of an INSTANCE method is the
    // `NullPointerException` raised below, ahead of every access refusal --
    // member, class or module (`L3W59ReflectClassAccess` rows
    // `private-null-receiver` / `module-null-receiver`). The gates were asked
    // first and a denied method answered `IllegalAccessException`.
    let gates_skipped =
        accessible || (!is_static && !matches!(args.get(1), Some(Value::Object(Some(_)))));
    // A private method's nest hosts are loaded before the access check judges
    // them, as `Method.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `getNestHost` does (JIT round 20 wave 16, lane nest;
    // `settle_reflective_nest_hosts`). A GC point only when a host is loaded:
    // `this`, the declaring mirror and every argument (the receiver, the
    // `Object[]`) are pinned there, and `args` is read from the refreshed
    // copy from here on.
    let mut this = this;
    let settled_args = match declaring_cid {
        Some(declaring) if !gates_skipped && !is_public && modifiers & SA_ACC_PRIVATE != 0 => {
            match resolve_caller_class_id(ctx) {
                Some(caller) => settle_reflective_nest_hosts(
                    ctx,
                    caller,
                    declaring,
                    modifiers,
                    &mut [&mut this, &mut declaring_mirror],
                    &mut [],
                    args,
                ),
                None => None,            }        }
        _ => None,
    };
    let args: &[Value] = settled_args.as_deref().unwrap_or(args);
    // Most framework reflection invokes public methods. Do not format an
    // exception-only diagnostic string on that successful hot path.
    if !gates_skipped && !is_public {
        // JLS 6.6.1 / JEP 181: a non-public method is still reachable without
        // `setAccessible(true)` when the CALLER is entitled to it — the
        // declaring class itself, a confirmed nestmate (private), or a
        // same-runtime-package / subclass caller (package-private, protected).
        // `check_access` takes no `ctx` and so cannot ask that question; it
        // rejected `RJdkReflect$Subject.secret` invoked from its own nest host
        // (regression-suite/src/RJdkReflect.java:160), which HotSpot 25 allows.
        // `check_field_access` is the field-shaped half of the same rule.
        // Pure widening: the fallback below is unchanged for every input the
        // caller step does not accept.
        // JLS §6.6.2.1's receiver refinement applies here exactly as it does to
        // the field path — `Method.checkAccess` computes the same third argument
        // to `Reflection.verifyMemberAccess`:
        //
        //     checkAccess(caller, clazz,
        //                 Modifier.isStatic(modifiers) ? null : obj.getClass(),
        //                 modifiers);
        //
        // Measured on Temurin 25.0.3 with `Object.clone` — protected, in
        // java.lang, cross-module, and needing no fabricated fixtures — invoked
        // from a classpath class:
        //
        //   receiver = the caller's own instance   InvocationTargetException
        //                                          (CloneNotSupportedException,
        //                                          i.e. access GRANTED)
        //   receiver = new Object()                IllegalAccessException
        //   receiver = "str"                       IllegalAccessException
        //   receiver = new int[] {1}               IllegalAccessException
        //   receiver = another class's instance    IllegalAccessException
        //   receiver = null                        NullPointerException
        //
        // Resolved to a `ClassId` BEFORE the call, never passed as an
        // `ObjectRef`: `caller_may_access_member`'s whole subtree is kept free
        // of object references so a moving GC cannot invalidate one under it,
        // and `check_field_access` pre-resolves its receiver for the same
        // reason.
        //
        // The `null` row above is NOT implemented here. It is rank 1 of the
        // field precedence lattice — the `NullPointerException` that falls out
        // of dereferencing `obj.getClass()` while building the argument list —
        // and `Method.invoke` has no equivalent of
        // `null_receiver_on_instance_field` yet. `reflective_target_class_id`
        // maps a null receiver to `None`, the fail-open value, so that row keeps
        // whatever answer it has today rather than acquiring a new one.
        let invoke_receiver = match args.get(1) {
            Some(Value::Object(obj_opt)) => *obj_opt,
            _ => None,
        };
        let receiver_cid = reflective_target_class_id(ctx, modifiers, invoke_receiver);
        let caller_cid = resolve_caller_class_id(ctx);
        let caller_entitled = match (caller_cid, declaring_cid) {
            (Some(caller), Some(declaring)) => crate::lang_reflect::caller_may_access_member(
                ctx,
                caller,
                declaring,
                modifiers,
                receiver_cid,
            ),
            _ => false,
        };
        if !caller_entitled {
            // NOTE: dev landed a NARROWER fix for the same defect while this was
            // in flight — a same-class-only carve-out, motivated by HikariConfig's
            // private-final AtomicReference and measured on Temurin 25 as legal for
            // both private (0x0002) and package-private (0x0008). This rule SUBSUMES
            // it: `caller_may_access_member`'s first arm is the declaring class
            // itself. Kept the wider rule; dev's evidence recorded here.
            check_access(
                ctx,
                modifiers,
                false,
                declaring_cid,
                &format!("Method.invoke: {}.{}", class_name, method_name),
            )?;
        }
    }
    // NEW-19: module-level JPMS check. When `accessible == true` the override
    // flag short-circuits it (JEP 403).
    //
    // JEP 403/261 distinction: a PUBLIC method of an EXPORTED package needs
    // only `exports`, not `opens` — `ArrayList.size()` must invoke without any
    // --add-opens, because java.base exports java.util WITHOUT opening it.
    // Only a non-public method requires the opens/deep check.
    //
    // But "needs only exports" is not "needs nothing", and the public arm used
    // to have NO check at all. `Method.invoke` is the exact shape of
    // `Constructor.newInstance`: both call `AccessibleObject.checkAccess` ->
    // `Reflection.verifyMemberAccess` -> `verifyModuleAccess` ->
    // `memberModule.isExported(pkg, callerModule)`, and `verifyMemberAccess`
    // runs that module test BEFORE the `Modifier.isPublic(modifiers)` shortcut,
    // so public members do not skip it. Measured on Temurin 25.0.3:
    // `jdk.internal.misc.VM.isBooted()` is public+static on a public class, and
    // `Method.invoke` throws IllegalAccessException ("module java.base does not
    // export jdk.internal.misc to unnamed module"), while
    // `ArrayList.size()` invokes normally. Without this arm CratonVM
    // fabricated a success where the spec mandates a failure.
    //
    // 2026-08-07: the non-public arm used to ask
    // `check_reflection_module_access` — the OPENS/deep-reflection question —
    // and that is the wrong question for `invoke`. `opens` is
    // `setAccessible(true)`'s gate, not a reflective CALL's. HotSpot's
    // `AccessibleObject.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `verifyModuleAccess` tests `memberModule.isExported(pkg, callerModule)`
    // and never consults `opens`; a package that is exported-but-not-open is
    // invocable, it is merely not deep-reflectable.
    //
    // The visible cost was an OVER-REFUSAL of legal JLS §6.6.2 access, and it
    // was invisible to the language-level check sitting right above this, which
    // had already answered correctly. Measured on `Object.finalize` (protected,
    // `java.lang`, exported but not open) invoked with NO `setAccessible`, from
    // a classpath class, against HotSpot 25.0.3:
    //
    //   receiver = the caller's own class     HotSpot OK    was IllegalAccessException
    //   receiver = a subclass of the caller   HotSpot OK    was IllegalAccessException
    //   receiver = the declaring class        both IllegalAccessException
    //   receiver = an unrelated class         both IllegalAccessException
    //
    // Both DENY rows already agreed, which is why this survived: a suite that
    // asserts only the refusals is satisfied by an implementation that refuses
    // everything. `regression-suite/src/RJdkFieldModule.java` asserts the
    // positive half too, and is what caught it.
    //
    // `Object.finalize` is the probe, not the point — the rule governs every
    // reflective call to an inherited `protected` member, `Object.clone`
    // included.
    //
    // So both arms ask the same question now, and the `is_public` split is only
    // about which helper carries the exact-identity target id.
    //
    // Interpreter round i3 wave 74, lane L3: with a plan, the question is not
    // asked when it cannot refuse -- the override skips it, and a target every
    // accessor is exported to (an unnamed-module class outside any layer
    // module, or class-path-only mode) needs no caller walk. Asked per call.
    let export_settled = plan.is_some()
        && (gates_skipped
            || declaring_cid.is_some_and(|cid| ctx.reflective_export_to_every_accessor(cid)));
    if export_settled {
        // Nothing to refuse.
    } else if !is_public {
        if let Err(msg) = check_reflection_export_access_with_target_id(
            ctx,
            &class_name,
            declaring_cid,
            gates_skipped,
        ) {
            // JO6-1 / J9: `Reflection.newIllegalAccessException`'s module form.
            let legacy = format!("Method.invoke: {class_name}.{method_name}: {msg}");
            return Err(reflective_access_refusal(
                ctx,
                declaring_cid,
                modifiers,
                legacy,
            ));
        }
    } else if let Err(msg) = check_reflection_export_access_with_target_id(
        ctx,
        &class_name,
        declaring_cid,
        gates_skipped,
    ) {
        let legacy = format!("Method.invoke: {class_name}.{method_name}: {msg}");
        return Err(reflective_access_refusal(
            ctx,
            declaring_cid,
            modifiers,
            legacy,
        ));
    }
    // `verifyMemberAccess`'s class half (interpreter round i2 wave 59, lane
    // L3; `CRATONVM_REFLECT_CLASS_ACCESS`): a member -- public or not -- of a
    // NON-public class is reachable only from that class's runtime package.
    // Every refusal of `checkAccess` carries the same text, so its place
    // relative to the member half above does not show. Skipped with the other
    // gates for an instance method's null receiver (`gates_skipped`).
    if !gates_skipped && ctx.reflect_class_access() {
        if let Some(declaring) = declaring_cid {
            // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): the
            // plan's kept class flags, the same word the read answers.
            let kept_flags = if plan_facts {
                plan.and_then(|p| p.class_flags)
            } else {
                None
            };
            let reachable = match kept_flags {
                Some(flags) => member_class_is_reachable_by_runtime_package_with_flags(
                    ctx, declaring, flags, None,
                ),
                None => member_class_is_reachable_by_runtime_package(ctx, declaring, None),
            };
            if !reachable {
                let legacy = format!(
                    "Method.invoke: {class_name}.{method_name}: class is not public and the \
                     caller is not in its runtime package"
                );
                return Err(reflective_access_refusal(
                    ctx,
                    declaring_cid,
                    modifiers,
                    legacy,
                ));
            }
        }
    }
    ph.mark(PH_GATES);

    // Get descriptor вЂ” stored in CratonVM extra slot (not a real JDK field).
    let descriptor: std::borrow::Cow<'_, str> = match plan {
        Some(p) => std::borrow::Cow::Borrowed(&*p.descriptor),
        None => std::borrow::Cow::Owned(method_descriptor_for_invoke(ctx, this)),
    };

    // Parse parameter types from descriptor
    let (param_descs, ret_desc): (std::borrow::Cow<'_, [String]>, std::borrow::Cow<'_, str>) =
        match plan {
            Some(p) => (
                std::borrow::Cow::Borrowed(&p.param_descs[..]),
                std::borrow::Cow::Borrowed(&*p.ret_desc),
            ),
            None => {
                let (params, ret) = parse_descriptor_param_and_return(&descriptor);
                (
                    std::borrow::Cow::Owned(params),
                    std::borrow::Cow::Owned(ret),
                )
            }
        };

    // Extract receiver (args[1]) and arguments array (args[2])
    let mut receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };
    let mut args_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    // Build invocation arguments. Pre-size for params + optional receiver
    // so the per-arg push loop never reallocates on hot reflective dispatch
    // paths (ByteBuddy / Jackson hit this thousands of times during boot).
    let receiver_slots = if is_static { 0 } else { 1 };
    let mut invoke_args: Vec<Value> = Vec::with_capacity(param_descs.len() + receiver_slots);

    // Interpreter round i2 wave 58, lane L3: JDK 25's `Method.invoke` builds
    // its accessor (`acquireMethodAccessor`) after the access check -- whose
    // `obj.getClass()` raises a null receiver's NPE first -- and the accessor
    // factory initializes the declaring class before the accessor's own
    // checks, OUTSIDE the method handle: a failed `<clinit>`'s
    // `ExceptionInInitializerError` is thrown as is, never wrapped in an
    // `InvocationTargetException` (it was: the class initialized inside the
    // call below).
    //
    // Wave 79, lane L3
    // (`i74-L3-method-invoke-reads-raw-references-after-the-declaring-class-initializer-20261107.md`):
    // the `<clinit>` is arbitrary Java and a GC point, and the declaring
    // mirror, the receiver and the argument array are read after it, so a
    // class that is not yet initialized runs it with the three pinned and
    // re-read (as `Constructor.newInstance` pins its argument array). An
    // initialized class -- every call but the first -- takes the old test.
    if is_static || receiver.is_some() || accessible {
        match declaring_cid {
            Some(cid) if !ctx.is_class_initialized(cid) => {
                (declaring_mirror, receiver, args_array) = reflective_door_initialize_pinned(
                    ctx,
                    cid,
                    declaring_mirror,
                    receiver,
                    args_array,
                )?;
            }
            _ => {}
        }
    }
    ph.mark(PH_INIT);

    // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): the receiver is
    // a plain object of the plan's class itself (the receiver test's `exact`
    // below, computed before any Java runs).
    let mut exact_receiver = false;
    // JIT round 21 wave 1, lane reflect (`CRATONVM_REFLECT_INVOKE0_VM_TEXTS`,
    // correctness, default ON; page
    // `docs/feature-designs/native-dispatch-architecture.md`):
    // `NativeAccessor.invoke0` (`gate_access == false`) is HotSpot's
    // `JVM_InvokeMethod`, whose own checks raise from the native frame with the
    // VM's texts -- a message-less NPE, "object is not an instance of declaring
    // class", "wrong number of arguments" -- not the Java accessor's
    // (`checkReceiver` / `checkArgumentCount`), which never run on that path.
    //
    // r21 review item 3 (`CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_TEXTS`,
    // correctness, default ON): the `Method.invoke` door (`gate_access`) stands
    // in front of whichever accessor JDK 25's `MethodHandleAccessorFactory`
    // would pick, and it picks `NativeAccessor` for the methods
    // [`method_takes_native_accessor`] names, so those get the same VM texts.
    // A null receiver reaches the accessor only past `checkAccess`'s
    // `obj.getClass()`, i.e. with `setAccessible(true)` (`accessible`).
    let vm_texts = invoke0_vm_texts_enabled()
        && (!gate_access
            || (door_native_accessor_texts_enabled()
                && method_takes_native_accessor(&class_name, modifiers, is_static, &param_descs)));
    // JIT round 21 wave 8, lane reflect2 (`CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_ONLY`):
    // under `-Djdk.reflect.useNativeAccessorOnly=true` the factory picks
    // `NativeAccessor` for EVERY member, so the door gives its texts for every
    // method. The property is read only on a refusal (a failed check), never
    // on the call's happy path.
    let vm_texts_now = move |ctx: &dyn NativeContext| {
        vm_texts
            || (invoke0_vm_texts_enabled() && door_honours_native_accessor_only(ctx, gate_access))
    };
    if !is_static {
        // Instance method: receiver is first arg
        let Some(recv) = receiver else {
            if accessible && vm_texts_now(&*ctx) {
                return Err(
                    cratonvm_types::error::RuntimeError::NullPointerException { message: None }
                        .into(),
                );
            }
            // Raised inside the call (wave 46, lane L3): HotSpot's trace starts
            // at `Method.invoke`'s access check (`obj.getClass()`), or, for a
            // `setAccessible(true)` method, at the accessor's `checkReceiver`.
            let check = if accessible {
                cratonvm_native_api::ReflectiveCheck::NullReceiverAtReceiverCheck
            } else {
                cratonvm_native_api::ReflectiveCheck::NullReceiverAtAccessCheck
            };
            return Err(raise_reflective_check(ctx, check, |_| {
                cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"Object.getClass()\" because \"obj\" is null".to_string(),
                    ),
                }
                .into()
            }));
        };
        // JDK contract: `Method.invoke` throws `IllegalArgumentException`
        // ("object is not an instance of declaring class") when the receiver
        // is not assignable to the Method's declaring class вЂ” and it does so
        // BEFORE any dispatch. CratonVM dispatches instance invokes virtually
        // by name on the receiver's runtime class (see `use_virtual_dispatch`
        // below), so a wrong-type receiver previously fell through to a
        // name-resolution miss and surfaced as `NoSuchMethodError`. That is
        // the wrong throwable AND the wrong *kind*: `NoSuchMethodError` is an
        // `Error`, whereas `IllegalArgumentException` is a `RuntimeException`.
        // Callers that wrap reflective getters in `catch (RuntimeException ...)`
        // and rely on the JDK IAE being swallowed (e.g. Hibernate 8's
        // `UniqueSlotExtractor.collectModelPartColumnValues`, which decomposes
        // every attribute during flush and tolerates a mismatched bidirectional
        // getter) instead saw the `Error` escape вЂ” breaking flush/commit and
        // cascading into unrelated failures. Restore the JDK ordering and
        // throwable here.
        //
        // The assignability test must include INTERFACE implementation, not
        // just the superclass chain. A bare `is_subclass` walks the class
        // hierarchy and (for synthetic lambda proxies) the lambda table, but
        // NOT dynamic `java.lang.reflect.Proxy` instances, annotation proxies,
        // or synthetic collection display classes вЂ” so this reuses the same
        // robust admission rule as `Class.isInstance` (`native_class_is_instance`)
        // to avoid false IAEs on those legitimate interface receivers.
        //
        // Previously this check was SKIPPED entirely when the declaring class
        // was an interface, on the mistaken belief that we couldn't model
        // interface implementation. That left a wrong-type receiver on an
        // interface-declared `Method` to fall through to name-based virtual
        // dispatch and surface an *uncatchable* `NoSuchMethodError` (a
        // VM-internal `Error`, not a Java throwable) вЂ” which aborts the whole
        // VM. ByteBuddy's `JavaDispatcher` reflectively invokes JDK reflect
        // *default* methods (e.g. `AnnotatedType.getAnnotatedOwnerType()`) on
        // sentinel receivers that do NOT implement the declaring interface
        // (its `TypeDescription$Generic$AnnotationReader$NoOp` implements
        // `AnnotatedElement`, not `AnnotatedType`); HotSpot throws
        // `IllegalArgumentException` there, which ByteBuddy tolerates, whereas
        // our `NoSuchMethodError` escaped Java entirely and ABENDed every
        // Spring test class that mocks an interface (Mockito inline mock maker).
        // Only `java.lang.Object` (the universal supertype, always assignable,
        // including array receivers) is exempt.
        let declaring_for_receiver = match plan {
            Some(p) => Some(p.class_id),
            None => mirror_class_id(ctx, declaring_mirror),
        };
        if let Some(declaring_id) = declaring_for_receiver {
            let declaring_is_object = match plan {
                Some(p) => p.declaring_is_object,
                None => {
                    ctx.class_name_arc_of_id(declaring_id).as_deref() == Some("java/lang/Object")
                }
            };
            if !declaring_is_object {
                // With a plan: a plain object of the declaring class itself is
                // an instance, no mirror walk needed (an array's header names
                // its component, so arrays take the full test).
                let exact = plan.is_some()
                    && ctx.heap_kind_of(recv) == cratonvm_types::ObjectKind::Object
                    && ctx.class_id_of_object(recv) == declaring_id;
                exact_receiver = exact;
                let is_instance = exact
                    || matches!(
                        native_class_is_instance(
                            ctx,
                            &[
                                Value::Object(Some(declaring_mirror)),
                                Value::Object(Some(recv)),
                            ],
                        ),
                        Ok(Some(Value::Int(1)))
                    );
                if !is_instance && vm_texts_now(&*ctx) {
                    return Err(illegal_arg_exc(INVOKE0_RECEIVER_TEXT.to_string()));
                }
                if !is_instance {
                    // `getClass().getName()`, which for an array is the array
                    // (`[LH;`), not the component its header id names.
                    let recv_name = class_get_name_of(&*ctx, recv);
                    let decl_name = reflect_text_class_name(
                        &*ctx,
                        &ctx.class_name_of_id(declaring_id).unwrap_or_default(),
                    );
                    return Err(raise_reflective_check(
                        ctx,
                        cratonvm_native_api::ReflectiveCheck::Receiver,
                        |_| {
                            illegal_arg_exc(format!(
                                "object of type {recv_name} is not an instance of {decl_name}"
                            ))
                        },
                    ));
                }
            }
        }
        invoke_args.push(Value::Object(Some(recv)));
    }
    ph.mark(PH_RECV);

    // Validate argument-array length matches parameter count (HotSpot throws
    // IllegalArgumentException when they differ вЂ” except when the target is
    // varargs and the last parameter receives the tail). This matches
    // `java.lang.reflect.Method.invoke` semantics.
    let actual_arg_count = match args_array {
        Some(arr) => ctx.array_length(arr),
        None => 0,
    };
    if actual_arg_count != param_descs.len() {
        if vm_texts_now(&*ctx) {
            return Err(illegal_arg_exc(INVOKE0_ARITY_TEXT.to_string()));
        }
        let expected = param_descs.len();
        return Err(raise_reflective_check(
            ctx,
            cratonvm_native_api::ReflectiveCheck::ArgumentCount,
            |_| {
                illegal_arg_exc(format!(
                    "wrong number of arguments: {actual_arg_count} expected: {expected}"
                ))
            },
        ));
    }

    // Interpreter round i2 wave 61, lane L3 (`CRATONVM_REFLECT_CS_ADAPTER`):
    // HotSpot's accessor for a static `@CallerSensitive` method with a
    // `@CallerSensitiveAdapter` overload runs that overload with the caller
    // appended (`MethodHandleAccessorFactory.findCSMethodAdapter`), so its
    // frames are the adapter's (`Class.forName(String)` reached reflectively
    // lists ONE `forName` frame, `forName(String, Class)`'s). The caller's
    // mirror is taken before the arguments are read: it may allocate. A JDK
    // class's static method only (the name screen keeps every other call off
    // the class-manager lock).
    let cs_adapter =
        if is_static && (class_name.starts_with("java/") || class_name.starts_with("jdk/")) {
            // Wave 74, lane L3: a plan asks the class manager once per method.
            let adapter: Option<std::borrow::Cow<'_, str>> = match plan {
                Some(p) => p
                    .cs_adapter
                    .get_or_init(|| {
                        declaring_cid
                            .and_then(|cid| {
                                ctx.caller_sensitive_adapter(cid, &method_name, &descriptor)
                            })
                            .map(|adapter| Arc::<str>::from(adapter))
                    })
                    .as_deref()
                    .map(std::borrow::Cow::Borrowed),
                None => declaring_cid
                    .and_then(|cid| ctx.caller_sensitive_adapter(cid, &method_name, &descriptor))
                    .map(std::borrow::Cow::Owned),
            };
            adapter.and_then(|adapter| {
                let caller = resolve_caller_class_id(ctx)?;
                Some((adapter, ctx.get_class_mirror(caller)))
            })
        } else {
            None
        };

    // Round-9 perf: fast-path when supplied arg types match the descriptor
    // exactly. Spring DI / Jackson hot paths hit `Method.invoke` thousands
    // of times per request with call-sites whose arg types already match
    // the formal parameter types 1:1; in that case `coerce_arg_strict`'s
    // wrapper-class / widening machinery is pure overhead.
    //
    // Pre-materialise the raw args (cheap вЂ” single array load each) so the
    // fast-path predicate can inspect them without a second pass.
    //
    // Interpreter round i3 wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`):
    // a batched call reads the raw args straight into `invoke_args` and
    // coerces them in place -- one vector instead of two, the same predicate
    // over the same values, every argument coerced when one is not exact, and
    // the first failure raised at the same index -- with the mirror's class id
    // read once rather than per argument.
    if batch_words.is_some() {
        let first = invoke_args.len();
        for i in 0..param_descs.len() {
            invoke_args.push(match args_array {
                Some(arr) => ctx.get_array_element(arr, i),
                None => Value::Object(None),
            });
        }
        if !args_match_descriptor_exactly(ctx, &invoke_args[first..], &param_descs) {
            let near = ctx.class_id_from_mirror(declaring_mirror);
            for (i, pdesc) in param_descs.iter().enumerate() {
                let arg_val = invoke_args[first + i];
                match coerce_arg_strict(ctx, arg_val, pdesc, "Method.invoke argument", near) {
                    Ok(coerced) => invoke_args[first + i] = coerced,
                    Err(e) => {
                        if crate::nbflags().dbg_invoke_coerce {
                            // The arguments before `i` are already coerced.
                            dbg_invoke_coerce_dump(
                                ctx,
                                &class_name,
                                &method_name,
                                i,
                                pdesc,
                                &param_descs,
                                &invoke_args[first..],
                            );
                        }
                        // `JVM_InvokeMethod`'s argument refusals (JIT round 21 wave 8,
                        // lane reflect2): see [`native_accessor_argument_refusal`].
                        if native_accessor_arg_texts_enabled() && vm_texts_now(&*ctx) {
                            return Err(native_accessor_argument_refusal(
                                ctx,
                                arg_val,
                                pdesc.as_str(),
                                e,
                            ));
                        }
                        let e = raise_reflective_coercion_failure(
                            ctx,
                            arg_val,
                            pdesc.as_str(),
                            e,
                            false,
                        );
                        return Err(e);
                    }
                }
            }
        }
    } else {
        let raw_args: Vec<Value> = (0..param_descs.len())
            .map(|i| match args_array {
                Some(arr) => ctx.get_array_element(arr, i),
                None => Value::Object(None),
            })
            .collect();
        if args_match_descriptor_exactly(ctx, &raw_args, &param_descs) {
            // No coercion needed — push each arg straight through.
            for v in raw_args {
                invoke_args.push(v);
            }
        } else {
            // Coerce each argument strictly, raising IllegalArgumentException on
            // type mismatch (per java.lang.reflect.Method.invoke javadoc).
            for (i, pdesc) in param_descs.iter().enumerate() {
                let arg_val = raw_args[i];
                match coerce_arg_strict(
                    ctx,
                    arg_val,
                    pdesc,
                    "Method.invoke argument",
                    ctx.class_id_from_mirror(declaring_mirror),
                ) {
                    Ok(coerced) => invoke_args.push(coerced),
                    Err(e) => {
                        // JDK-faithful cause: HotSpot's reflective unboxing calls
                        // `Number.xxxValue()` on a null argument passed for a
                        // primitive formal parameter, so the resulting
                        // `IllegalArgumentException` carries a `NullPointerException`
                        // cause. Spring's `InvocableHandlerMethod.doInvoke` (and
                        // other callers) branch on `ex.getCause() instanceof
                        // NullPointerException` to pick a friendlier message, so a
                        // bare message-only IAE here diverges from HotSpot even
                        // though the exception TYPE matches. See
                        // `illegal_arg_exc_null_to_primitive`.
                        // CRATONVM_DBG_INVOKE_COERCE=1 — dump the method, formal
                        // descriptors, actual arg runtime types, and innermost Java
                        // caller frames on a coercion mismatch. Env-gated.
                        if crate::nbflags().dbg_invoke_coerce {
                            dbg_invoke_coerce_dump(
                                ctx,
                                &class_name,
                                &method_name,
                                i,
                                pdesc,
                                &param_descs,
                                &raw_args,
                            );
                        }
                        // Raised inside the call (wave 46, lane L3), after the dump
                        // above has read `raw_args`: building the throwable may
                        // collect and move them.
                        // `JVM_InvokeMethod`'s argument refusals (JIT round 21 wave 8,
                        // lane reflect2): see [`native_accessor_argument_refusal`].
                        if native_accessor_arg_texts_enabled() && vm_texts_now(&*ctx) {
                            return Err(native_accessor_argument_refusal(
                                ctx,
                                arg_val,
                                pdesc.as_str(),
                                e,
                            ));
                        }
                        let e = raise_reflective_coercion_failure(
                            ctx,
                            arg_val,
                            pdesc.as_str(),
                            e,
                            false,
                        );
                        return Err(e);
                    }
                }
            }
        }
    }
    ph.mark(PH_ARGS);

    // Invoke the method. Any Java exception thrown by the callee must be
    // wrapped in InvocationTargetException per the Method.invoke contract.
    //
    // Per JLS В§15.12.4.4 / `Method.invoke` Javadoc: "If the underlying method
    // is an instance method, it is invoked using dynamic method lookup ...
    // overriding based on the runtime type of the target object will occur."
    //
    // So for non-static, non-private, non-`<init>` instance methods we MUST
    // perform virtual dispatch on the receiver's runtime class вЂ” even when
    // the cached Method points at a concrete declaring class (e.g.
    // `Super.class.getDeclaredMethod("hello")` on a `SubB` receiver should
    // call `SubB.hello`, not `Super.hello`).
    //
    // Static methods, `<init>`, and private methods bypass virtual dispatch
    // and call the resolved class directly.
    const ACC_PRIVATE: i32 = 0x0002;
    const ACC_PROTECTED: i32 = 0x0004;
    let is_private = (modifiers & ACC_PRIVATE) != 0;
    let is_protected = (modifiers & ACC_PROTECTED) != 0;
    let is_init = method_name == "<init>";

    // JLS В§8.4.8.1: a package-private (default-access) method is only
    // overridden by a same-named/same-descriptor subclass method in the SAME
    // runtime package as the declaring class. Virtual dispatch by name on
    // the receiver's runtime class can therefore resolve to a completely
    // unrelated, independently-declared same-named package-private method
    // there instead of the exact method this `Method` mirror represents вЂ”
    // exactly the javax.inject/jakarta.inject TCK's package-private-across-
    // packages cases (SpringAtInjectTckTests). When that's the situation,
    // `Method.invoke` must invoke the EXACT declaring-class method (no
    // retarget), same as a private method.
    let is_package_private = !is_public && !is_protected && !is_private;
    let crosses_package = is_package_private
        && !is_static
        && receiver
            .map(|recv| {
                // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): a
                // receiver of the declaring class itself is in its package;
                // any other compares slices of the interned names (the same
                // runtime-package slices as the other arm, no `String`s).
                if plan_facts {
                    if exact_receiver {
                        return false;
                    }
                    // Wave 79, lane L3
                    // (`i77-L3-method-invoke-crosses-package-reads-a-hidden-class-suffix-as-a-package-20261110.md`):
                    // both arms take the RUNTIME package, which strips a
                    // hidden class's `/0x<hex>` tail first -- a hidden
                    // class is in its lookup class's package (JEP 371), so
                    // its override of a package-private method is selected.
                    let recv_cid = ctx.class_id_of_object(recv);
                    let recv_class_name = ctx.class_name_arc_of_id(recv_cid);
                    let recv_name: &str = recv_class_name.as_deref().unwrap_or("");
                    return runtime_package_name(&class_name) != runtime_package_name(recv_name)
                        && !redeclared_between(
                            &*ctx,
                            recv,
                            declaring_cid,
                            &method_name,
                            &descriptor,
                        );
                }
                let recv_cid = ctx.class_id_of_object(recv);
                let recv_class_name = ctx.class_name_of_id(recv_cid).unwrap_or_default();
                runtime_package_name(&class_name) != runtime_package_name(&recv_class_name)
                    && !redeclared_between(&*ctx, recv, declaring_cid, &method_name, &descriptor)
            })
            .unwrap_or(false);
    let use_virtual_dispatch = !is_static && !is_private && !is_init && !crosses_package;
    // The classes whose code the call may run: the declaring class, and the
    // receiver's, which virtual selection may pick. Read before the call (no
    // reference survives it; a header read); only a failure asks whether one
    // is hidden ([`wrap_reflective_callee_failure`]).
    let callee_classes = [
        declaring_cid,
        match invoke_args.first() {
            Some(Value::Object(Some(r))) if !is_static => Some(ctx.class_id_of_object(*r)),
            _ => None,
        },
    ];

    // JIT round 21 wave 1, lane reflect (`CRATONVM_REFLECT_SIG_POLY_UOE`,
    // correctness, default ON): HotSpot's `MethodHandle.invoke` /
    // `invokeExact` and `VarHandle` access-mode natives, when reached through
    // reflection (JDK 25 serves them through `NativeAccessor.invoke0`), throw
    // `UnsupportedOperationException` -- after `JVM_InvokeMethod`'s receiver,
    // arity and argument checks above -- which reflection wraps. The call
    // used to dispatch the handle with the `Object[]` as its one argument.
    if crate::nbflags().reflect_sig_poly_uoe {
        if let Some(message) =
            reflective_signature_polymorphic_refusal(&class_name, &method_name, &descriptor, modifiers)
        {
            let failure: MethodCallFailed =
                cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                    message: message.to_string(),
                }
                .into();
            return Err(wrap_as_invocation_target_exception(ctx, failure));
        }
    }

    let iae_trace = crate::nbflags().iae_trace;
    if iae_trace {
        eprintln!("[Method.invoke] about to invoke: class={} method={} desc={} is_static={} use_virtual={}",
                  class_name, method_name, descriptor, is_static, use_virtual_dispatch);
    }
    ph.mark(PH_SELECT);
    let result = if use_virtual_dispatch {
        // invoke_virtual takes the receiver separately and prepends it.
        // We already pushed receiver as invoke_args[0]; strip it for the
        // virtual call.
        let recv = match invoke_args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Method.invoke: null receiver for virtual dispatch".to_string()),
                }
                .into());
            }
        };
        let virtual_args: &[Value] = &invoke_args[1..];
        if crate::nbflags().bd_debug {
            eprintln!(
                "[Method.invoke] virtual class={} method={} desc={} recv={:p}",
                class_name,
                method_name,
                descriptor,
                recv.as_ptr()
            );
        }
        // JVMS 5.4.6: selection for this receiver may END IN AN ERROR the
        // receiver-class dispatch below cannot see (it never learns the
        // resolved method's owner); HotSpot's accessor raises it from its
        // `invokeinterface` / `invokevirtual`, wrapped like any callee throw.
        //
        // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): a receiver
        // of the plan's class itself is the owner's own class, which
        // selection never judges (`reflective_selection_failure`'s first
        // compare), and the declared door's selection answers "no move" for
        // it (`invoke_virtual_exact`) -- both questions are skipped.
        let exact_door = plan_facts && exact_receiver;
        if !exact_door {
            let owner = match plan {
                Some(_) => declaring_cid,
                None => mirror_class_id(ctx, declaring_mirror),
            };
            if let Some(owner) = owner {
                if let Some(failure) =
                    ctx.virtual_selection_failure(recv, owner, &method_name, &descriptor)
                {
                    return Err(wrap_reflective_callee_failure(
                        ctx,
                        failure,
                        ReflectiveAccessor::Method,
                        callee_classes,
                    ));
                }
            }
        }
        ph.mark(PH_SELECT);
        // Round 13 wave 1 (lane mhffm patch): through the declaring class, so
        // the VM selects against the resolved method (JVMS 5.4.5/5.4.6) rather
        // than running the receiver's first same-signature method.
        let dispatched = if exact_door {
            ctx.invoke_virtual_exact(&class_name, recv, &method_name, &descriptor, virtual_args)
        } else {
            ctx.invoke_virtual_declared(&class_name, recv, &method_name, &descriptor, virtual_args)
        };
        match dispatched {
            Ok(v) => v,
            Err(failure) => {
                return Err(wrap_reflective_callee_failure(
                    ctx,
                    failure,
                    ReflectiveAccessor::Method,
                    callee_classes,
                ));
            }
        }
    } else if (is_private || crosses_package) && !is_static && !is_init {
        // Private instance methods bypass virtual dispatch AND must never be
        // retargeted to a subclass's same-name method. `ctx.invoke`
        // (invoke_on_class_shared) retargets a call whose declaring class is
        // abstract/interface onto the receiver's concrete class вЂ” correct for
        // an abstract/interface method with no Code, but WRONG for a private
        // concrete method that merely lives in an abstract class. Reflectively
        // invoking `AbstractSharedSessionContract.writeObject` (private, in an
        // abstract class) on a `SessionImpl` receiver therefore ran
        // `SessionImpl.writeObject`, so `ObjectStreamClass.invokeWriteObject`
        // skipped the superclass slot's hook: Hibernate never wrote the
        // SessionFactory UUID and deserialization reconnected a null factory
        // в†’ NPE (HIB-DEV-05). `invoke_special` resolves to the declaring class
        // with no retarget вЂ” exactly the invokespecial semantics a private
        // method requires, and exactly what a cross-package package-private
        // method also needs (see `crosses_package` above).
        //
        // Use the `Method` mirror's OWN already-resolved declaring `ClassId`
        // rather than re-resolving `class_name`, for exactly the reason the
        // static branch below documents: name-based resolution goes through the
        // loader-blind global lookup, which collapses to ONE copy per binary
        // name. When an isolating loader has defined its own copy of the
        // declaring class, the name lookup returns the APPLICATION copy and we
        // run that copy's bytecode against a receiver from the isolated loader
        // — so every constant-pool reference inside the invoked method resolves
        // in the wrong loader namespace from then on.
        //
        // Reproduced by Spring Boot's `@ForkedClassPath` tests whose test
        // method is package-private and inherited from a base class in a
        // DIFFERENT package (`crosses_package` → true), e.g.
        // `TomcatServletWebServerServletContextListenerTests` extending
        // `…web.server.servlet.AbstractServletWebServerServletContextListenerTests`:
        // `Mockito.mock()` ran in the fork's namespace (called from the fork's
        // own nested `@Configuration` class) while the test body's
        // `Mockito.verify()` ran application-loader bytecode against the
        // application copy of `MockUtil`, whose `mockMakers` map never saw the
        // mock → `NotAMockException`. See servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md.
        let special_owner = match plan {
            Some(_) => declaring_cid,
            None => mirror_class_id(ctx, declaring_mirror),
        };
        let special_result = match special_owner {
            Some(cid) => ctx.invoke_special_by_class_id(
                cid,
                &class_name,
                &method_name,
                &descriptor,
                &invoke_args,
            ),
            None => ctx.invoke_special(&class_name, &method_name, &descriptor, &invoke_args),
        };
        match special_result {
            Ok(v) => v,
            Err(failure) => {
                return Err(wrap_reflective_callee_failure(
                    ctx,
                    failure,
                    ReflectiveAccessor::Method,
                    callee_classes,
                ));
            }
        }
    } else {
        // Use the Method object's OWN already-resolved declaring ClassId
        // when available, instead of re-resolving `class_name` by name.
        // Name-based resolution goes through the loader-blind global lookup,
        // which is unsound (reports "not found") once 2+ different
        // user-defined loaders each register their own distinct class under
        // this identical simple name — an ordinary occurrence for harnesses
        // that mint a fresh ClassLoader + identically-named generated class
        // per invocation (Spring's TestCompiler/@CompileWithForkedClassLoader
        // — see `invoke_by_class_id_shared`'s doc comment). This matters most
        // for STATIC methods (never virtually dispatched, always reaching
        // this branch), which is exactly the reflective-invoke shape
        // Method.invoke() takes right after successfully loading the
        // generated class via a specific ClassLoader.
        // Interpreter round i2 wave 62, lane L3: a static JDK target's door is
        // noted on the reflective call while it runs, so a throwable a callee
        // builds inside a native serving it lists the method's JDK frames
        // (`NativeExceptionAccess::note_reflective_target`). The same screen
        // as the adapter's above; withdrawn before any wrapping below. Wave
        // 64: the VM keeps the note on the thread's door record (the carrier
        // of every static JDK door), so each `Some` is paired with a `None`.
        let jdk_static =
            is_static && (class_name.starts_with("java/") || class_name.starts_with("jdk/"));
        let static_owner = match plan {
            Some(_) => declaring_cid,
            None => mirror_class_id(ctx, declaring_mirror),
        };
        let dispatch_result = match (static_owner, cs_adapter) {
            (Some(cid), Some((adapter, caller_mirror))) => {
                invoke_args.push(Value::Object(Some(caller_mirror)));
                ctx.note_reflective_target(Some((cid, &*method_name, &*adapter)));
                let result =
                    ctx.invoke_by_class_id(cid, &class_name, &method_name, &adapter, &invoke_args);
                ctx.note_reflective_target(None);
                result
            }
            (Some(cid), None) if jdk_static => {
                ctx.note_reflective_target(Some((cid, &*method_name, &*descriptor)));
                let result = ctx.invoke_by_class_id(
                    cid,
                    &class_name,
                    &method_name,
                    &descriptor,
                    &invoke_args,
                );
                ctx.note_reflective_target(None);
                result
            }
            // Interpreter round i3 wave 75, lane L3 (`CRATONVM_REFLECT_INVOKE_SETTLED`):
            // a planned static target outside java/ and jdk/ (no door note, no
            // adapter) takes the settling static door with the owner its plan
            // remembers.
            (Some(cid), None) if is_static && plan.is_some() && ctx.reflect_invoke_settled() => {
                match plan {
                    Some(p) => invoke_static_settled_on_plan(
                        ctx,
                        p,
                        cid,
                        &class_name,
                        &method_name,
                        &descriptor,
                        &invoke_args,
                    ),
                    None => ctx.invoke_by_class_id(
                        cid,
                        &class_name,
                        &method_name,
                        &descriptor,
                        &invoke_args,
                    ),
                }
            }
            (Some(cid), None) => {
                ctx.invoke_by_class_id(cid, &class_name, &method_name, &descriptor, &invoke_args)
            }
            (None, _) => ctx.invoke(&class_name, &method_name, &descriptor, &invoke_args),
        };
        match dispatch_result {
            Ok(v) => v,
            Err(failure) => {
                return Err(wrap_reflective_callee_failure(
                    ctx,
                    failure,
                    ReflectiveAccessor::Method,
                    callee_classes,
                ));
            }
        }
    };
    ph.mark(PH_CALL);

    if crate::nbflags().diag_method_invoke_null {
        let void_ret = ret_desc == "V" || ret_desc.is_empty();
        if !void_ret {
            match &result {
                None => {
                    eprintln!(
                        "[Method.invoke] non-void return mapped to None: {}.{}{}",
                        class_name, method_name, descriptor
                    );
                }
                Some(Value::Object(None)) => {
                    eprintln!(
                        "[Method.invoke] non-void return null reference: {}.{}{}",
                        class_name, method_name, descriptor
                    );
                }
                _ => {}
            }
        }
    }

    // Box the return value — CANONICAL. Measured on HotSpot 25.0.3+9:
    // `m.invoke(null) == Integer.valueOf(7)` for a `()I` returning 7, and the
    // same for C/B/S/Z/J in bound; `()F`/`()D` and out-of-bound values come
    // back fresh, which is what `box_value_canonical` delegates for. Since
    // JDK 18 `Method.invoke` runs through a `MethodHandle` accessor whose
    // return adaptation is a direct handle to `X.valueOf`.
    let out = match result {
        Some(val) => {
            // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): a
            // primitive the callee returned for a primitive return type is
            // boxed here into a wrapper (every arm of `box_value_canonical` /
            // `box_value` for such a pair allocates or caches one), never a
            // `Class` mirror -- the wrapper's shape check has nothing to find.
            if plan_facts
                && !matches!(val, Value::Object(_))
                && matches!(&*ret_desc, "I" | "J" | "Z" | "B" | "S" | "C" | "F" | "D")
            {
                *fresh_box = true;
            }
            let boxed = box_value_canonical(ctx, val, &ret_desc);
            Ok(Some(boxed))
        }
        None => Ok(Some(Value::Object(None))), // void method returns null
    };
    ph.mark(PH_BOX);
    out
}

// ---------------------------------------------------------------------------
// Class.getDeclaredMethods / getDeclaredMethod
// ---------------------------------------------------------------------------

/// F2 вЂ” synthetic-JDK reflection augmentation table.
///
/// When a JDK class is loaded as a synthetic stub (no real `.class` file
/// found on the classpath, see `class_manager::create_synthetic_stub`)
/// its `class.methods` is empty, so `declared_methods` returns an empty
/// list. Reflective lookups via `Class.getDeclaredMethod(...)` therefore
/// throw `NoSuchMethodException` even though the JDK contractually
/// declares those methods (and we usually back them with native
/// implementations registered in `lib.rs`).
///
/// This table provides the missing public/protected method signatures
/// for the well-known JDK classes that real-world frameworks reflect on.
/// CGLIB's `ReflectUtils.<clinit>` is the canonical caller вЂ” it does:
///
///   ClassLoader.class.getDeclaredMethod(
///       "defineClass",
///       String.class, byte[].class, int.class, int.class,
///       ProtectionDomain.class);
///
/// and previously NSME'd because our synthetic `java/lang/ClassLoader`
/// stub had no declared methods.
///
/// The augmentation is applied **only** when a method is missing from
/// `ctx.declared_methods(...)` вЂ” real-class loading (when the JDK
/// bytecode is present) takes precedence, exactly as the JDK contract
/// demands.
///
/// Each entry is `(name, descriptor, access_flags)` вЂ” `access_flags`
/// uses the JVM bit values (ACC_PUBLIC=0x1, ACC_PROTECTED=0x4,
/// ACC_FINAL=0x10, ACC_NATIVE=0x100, ACC_STATIC=0x8). The flags
/// reflect the OpenJDK 25 declarations.
pub fn synthetic_jdk_method_decls(class_name: &str) -> &'static [(&'static str, &'static str, u16)] {
    match class_name {
        // java.lang.ClassLoader вЂ” surface the public/protected `defineClass`
        // overloads + companion methods that frameworks reflect on. Access
        // flags match OpenJDK 25 `java/lang/ClassLoader.java`.
        //
        // ACC_PROTECTED|ACC_FINAL = 0x14
        // ACC_PUBLIC              = 0x01
        // ACC_PUBLIC|ACC_STATIC   = 0x09
        "java/lang/ClassLoader" => &[
            // protected final Class<?> defineClass(byte[] b, int off, int len)
            //   вЂ” DEPRECATED legacy overload (no name).
            ("defineClass", "([BII)Ljava/lang/Class;", 0x14),
            // protected final Class<?> defineClass(String name, byte[] b, int off, int len)
            ("defineClass", "(Ljava/lang/String;[BII)Ljava/lang/Class;", 0x14),
            // protected final Class<?> defineClass(String name, byte[] b, int off, int len,
            //                                      ProtectionDomain protectionDomain)
            //   вЂ” the overload CGLIB's ReflectUtils.<clinit> reflects on.
            (
                "defineClass",
                "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;",
                0x14,
            ),
            // protected final Class<?> defineClass(String name, ByteBuffer b,
            //                                      ProtectionDomain protectionDomain)
            (
                "defineClass",
                "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
                0x14,
            ),
            // protected final void resolveClass(Class<?> c)
            ("resolveClass", "(Ljava/lang/Class;)V", 0x14),
            // protected Class<?> findClass(String name)
            ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x04),
            // protected final Class<?> findLoadedClass(String name)
            ("findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x14),
            // protected final Class<?> findSystemClass(String name)
            ("findSystemClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x14),
            // public Class<?> loadClass(String name)
            ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", 0x01),
            // public final ClassLoader getParent()
            ("getParent", "()Ljava/lang/ClassLoader;", 0x11),
            // public String getName()
            ("getName", "()Ljava/lang/String;", 0x01),
            // public static ClassLoader getSystemClassLoader()
            (
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;",
                0x09,
            ),
            // public static ClassLoader getPlatformClassLoader()
            (
                "getPlatformClassLoader",
                "()Ljava/lang/ClassLoader;",
                0x09,
            ),
            // protected static boolean registerAsParallelCapable()
            ("registerAsParallelCapable", "()Z", 0x0c),
            // public URL getResource(String name)
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;", 0x01),
            // public InputStream getResourceAsStream(String name)
            (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                0x01,
            ),
        ],
        // G2: java/lang/reflect/Method вЂ” surface canonical declared
        // methods so that frameworks (ByteBuddy, Mockito) which call
        // `Method.class.getDeclaredMethods()` get a non-empty array.
        // ByteBuddy's `JavaDispatcher.<clinit>` iterates these and
        // would NPE if reflection on Method itself returned an empty
        // (or null) array.
        //
        // ACC_PUBLIC = 0x01; ACC_PUBLIC|ACC_NATIVE = 0x101.
        // All Method methods are public (final on a few вЂ” 0x11).
        "java/lang/reflect/Method" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getReturnType", "()Ljava/lang/Class;", 0x01),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x01),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x01),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x01),
            ("getGenericReturnType", "()Ljava/lang/reflect/Type;", 0x01),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getDefaultValue", "()Ljava/lang/Object;", 0x01),
            ("isVarArgs", "()Z", 0x01),
            ("isBridge", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("isDefault", "()Z", 0x01),
            ("invoke", "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;", 0x81),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
            ("toShortSignature", "()Ljava/lang/String;", 0x00),
        ],
        // G2: java/lang/reflect/Field вЂ” same rationale.
        "java/lang/reflect/Field" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getType", "()Ljava/lang/Class;", 0x01),
            ("getGenericType", "()Ljava/lang/reflect/Type;", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("get", "(Ljava/lang/Object;)Ljava/lang/Object;", 0x01),
            ("set", "(Ljava/lang/Object;Ljava/lang/Object;)V", 0x01),
            ("getInt", "(Ljava/lang/Object;)I", 0x01),
            ("setInt", "(Ljava/lang/Object;I)V", 0x01),
            ("getLong", "(Ljava/lang/Object;)J", 0x01),
            ("setLong", "(Ljava/lang/Object;J)V", 0x01),
            ("getBoolean", "(Ljava/lang/Object;)Z", 0x01),
            ("setBoolean", "(Ljava/lang/Object;Z)V", 0x01),
            ("getByte", "(Ljava/lang/Object;)B", 0x01),
            ("setByte", "(Ljava/lang/Object;B)V", 0x01),
            ("getChar", "(Ljava/lang/Object;)C", 0x01),
            ("setChar", "(Ljava/lang/Object;C)V", 0x01),
            ("getShort", "(Ljava/lang/Object;)S", 0x01),
            ("setShort", "(Ljava/lang/Object;S)V", 0x01),
            ("getFloat", "(Ljava/lang/Object;)F", 0x01),
            ("setFloat", "(Ljava/lang/Object;F)V", 0x01),
            ("getDouble", "(Ljava/lang/Object;)D", 0x01),
            ("setDouble", "(Ljava/lang/Object;D)V", 0x01),
            ("isEnumConstant", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
        ],
        // G2: java/lang/reflect/Constructor вЂ” same rationale.
        "java/lang/reflect/Constructor" => &[
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getModifiers", "()I", 0x01),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x01),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x01),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x01),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;", 0x81),
            ("isVarArgs", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("equals", "(Ljava/lang/Object;)Z", 0x01),
            ("hashCode", "()I", 0x01),
        ],
        // G2: java/lang/reflect/Executable (parent of Method + Constructor).
        // Frameworks sometimes reflect on the abstract parent class.
        "java/lang/reflect/Executable" => &[
            ("getName", "()Ljava/lang/String;", 0x401), // ACC_PUBLIC|ACC_ABSTRACT
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x401),
            ("getModifiers", "()I", 0x401),
            ("getParameterTypes", "()[Ljava/lang/Class;", 0x401),
            ("getExceptionTypes", "()[Ljava/lang/Class;", 0x401),
            ("getParameterCount", "()I", 0x01),
            ("getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", 0x01),
            ("getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x401),
            ("getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", 0x401),
            ("getGenericParameterTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getGenericExceptionTypes", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getParameters", "()[Ljava/lang/reflect/Parameter;", 0x01),
            ("isVarArgs", "()Z", 0x01),
            ("isSynthetic", "()Z", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x401),
        ],
        // WP2.1-narrow вЂ” JDBC SPI interface methods. The native registry
        // already pins the canonical natives (see WP7.2 anchor tests
        // `each_jdbc_core_type_has_registered_natives` /
        // `each_jdbc_core_type_has_multiple_anchor_natives` in
        // `vm/tests/wp7_2_jdbc_core_types_reachable.rs`), but the synthetic
        // stub for these interfaces ships with `methods: vec![]`, so
        // `Class.getDeclaredMethods()` returned an empty array even though
        // the dispatch path itself worked. This synthetic-method table is
        // the documented hook (see [`synthetic_jdk_method_decls`] header)
        // for surfacing those declarations to the reflection layer when
        // real JDK bytecode is unavailable. All flags are
        // ACC_PUBLIC|ACC_ABSTRACT (0x401) вЂ” these are interface methods.
        //
        // The lists below are subsets of the JDBC SPI surface, chosen to
        // mirror the WP7.2 width-of-surface anchors so frameworks holding
        // real JDBC bytecode (HikariCP wrapping, ByteBuddy stubbing,
        // Spring/Hibernate proxies) see a non-empty reflective surface.
        "java/sql/Connection" => &[
            ("createStatement", "()Ljava/sql/Statement;", 0x401),
            (
                "prepareStatement",
                "(Ljava/lang/String;)Ljava/sql/PreparedStatement;",
                0x401,
            ),
            (
                "prepareCall",
                "(Ljava/lang/String;)Ljava/sql/CallableStatement;",
                0x401,
            ),
            ("getMetaData", "()Ljava/sql/DatabaseMetaData;", 0x401),
            ("close", "()V", 0x401),
            ("isClosed", "()Z", 0x401),
            ("setAutoCommit", "(Z)V", 0x401),
            ("getAutoCommit", "()Z", 0x401),
            ("commit", "()V", 0x401),
            ("rollback", "()V", 0x401),
            ("setReadOnly", "(Z)V", 0x401),
            ("isReadOnly", "()Z", 0x401),
            ("setCatalog", "(Ljava/lang/String;)V", 0x401),
            ("getCatalog", "()Ljava/lang/String;", 0x401),
            ("setSchema", "(Ljava/lang/String;)V", 0x401),
            ("getSchema", "()Ljava/lang/String;", 0x401),
            ("isValid", "(I)Z", 0x401),
        ],
        "java/sql/Statement" => &[
            ("execute", "(Ljava/lang/String;)Z", 0x401),
            (
                "executeQuery",
                "(Ljava/lang/String;)Ljava/sql/ResultSet;",
                0x401,
            ),
            ("executeUpdate", "(Ljava/lang/String;)I", 0x401),
            ("close", "()V", 0x401),
            ("isClosed", "()Z", 0x401),
            ("getConnection", "()Ljava/sql/Connection;", 0x401),
            ("getResultSet", "()Ljava/sql/ResultSet;", 0x401),
            ("getUpdateCount", "()I", 0x401),
            ("setQueryTimeout", "(I)V", 0x401),
            ("getQueryTimeout", "()I", 0x401),
            ("cancel", "()V", 0x401),
        ],
        "java/sql/PreparedStatement" => &[
            ("execute", "()Z", 0x401),
            ("executeQuery", "()Ljava/sql/ResultSet;", 0x401),
            ("executeUpdate", "()I", 0x401),
            ("setInt", "(II)V", 0x401),
            ("setLong", "(IJ)V", 0x401),
            ("setString", "(ILjava/lang/String;)V", 0x401),
            ("setBoolean", "(IZ)V", 0x401),
            ("setNull", "(II)V", 0x401),
            ("setObject", "(ILjava/lang/Object;)V", 0x401),
            ("clearParameters", "()V", 0x401),
            ("close", "()V", 0x401),
        ],
        "java/sql/ResultSet" => &[
            ("next", "()Z", 0x401),
            ("close", "()V", 0x401),
            ("wasNull", "()Z", 0x401),
            ("getString", "(I)Ljava/lang/String;", 0x401),
            ("getString", "(Ljava/lang/String;)Ljava/lang/String;", 0x401),
            ("getInt", "(I)I", 0x401),
            ("getInt", "(Ljava/lang/String;)I", 0x401),
            ("getLong", "(I)J", 0x401),
            ("getLong", "(Ljava/lang/String;)J", 0x401),
            ("getBoolean", "(I)Z", 0x401),
            ("getBoolean", "(Ljava/lang/String;)Z", 0x401),
            ("getObject", "(I)Ljava/lang/Object;", 0x401),
            ("getObject", "(Ljava/lang/String;)Ljava/lang/Object;", 0x401),
            ("getMetaData", "()Ljava/sql/ResultSetMetaData;", 0x401),
            ("isClosed", "()Z", 0x401),
        ],
        "java/sql/Driver" => &[
            (
                "connect",
                "(Ljava/lang/String;Ljava/util/Properties;)Ljava/sql/Connection;",
                0x401,
            ),
            ("acceptsURL", "(Ljava/lang/String;)Z", 0x401),
            ("getMajorVersion", "()I", 0x401),
            ("getMinorVersion", "()I", 0x401),
            ("jdbcCompliant", "()Z", 0x401),
        ],
        "java/sql/DatabaseMetaData" => &[
            (
                "getDatabaseProductName",
                "()Ljava/lang/String;",
                0x401,
            ),
            (
                "getDatabaseProductVersion",
                "()Ljava/lang/String;",
                0x401,
            ),
            ("getDriverName", "()Ljava/lang/String;", 0x401),
            ("getDriverVersion", "()Ljava/lang/String;", 0x401),
            ("getURL", "()Ljava/lang/String;", 0x401),
            ("getUserName", "()Ljava/lang/String;", 0x401),
            ("getDatabaseMajorVersion", "()I", 0x401),
            ("getDatabaseMinorVersion", "()I", 0x401),
            ("getJDBCMajorVersion", "()I", 0x401),
            ("getJDBCMinorVersion", "()I", 0x401),
            ("getConnection", "()Ljava/sql/Connection;", 0x401),
        ],
        // WP2.1-class-modern вЂ” surface the modern `java.lang.Class` API
        // methods (Java 11вЂ“25 sealed-class / record-class / nest-mate
        // accessors plus the canonical reflection-info methods) so
        // ByteBuddy's `TypeDescription.forLoadedType(Class.class)` and
        // Hibernate's record/sealed scanners see a non-empty
        // declared-method list when the synthetic-JDK `Class` stub is in
        // use. The natives backing each entry are already registered in
        // `lib.rs` / `phases_early.rs` / `lang_reflect.rs` вЂ” this table
        // surfaces them to the reflection layer.
        //
        // Flags: 0x01 ACC_PUBLIC, 0x11 ACC_PUBLIC|ACC_FINAL,
        //        0x101 ACC_PUBLIC|ACC_NATIVE.
        // `Class` itself is final, so all instance methods are effectively
        // final вЂ” but the JDK source marks only a few that way; we follow
        // the OpenJDK 25 declarations to stay byte-compatible.
        "java/lang/Class" => &[
            // Identity / naming
            ("getName", "()Ljava/lang/String;", 0x01),
            ("getSimpleName", "()Ljava/lang/String;", 0x01),
            ("getCanonicalName", "()Ljava/lang/String;", 0x01),
            ("getTypeName", "()Ljava/lang/String;", 0x01),
            ("toString", "()Ljava/lang/String;", 0x01),
            ("toGenericString", "()Ljava/lang/String;", 0x01),
            ("descriptorString", "()Ljava/lang/String;", 0x01),
            // Modifiers / shape predicates
            ("getModifiers", "()I", 0x01),
            ("isInterface", "()Z", 0x101),
            ("isArray", "()Z", 0x101),
            ("isPrimitive", "()Z", 0x101),
            ("isAnnotation", "()Z", 0x101),
            ("isSynthetic", "()Z", 0x01),
            ("isEnum", "()Z", 0x01),
            ("isRecord", "()Z", 0x01),
            ("isSealed", "()Z", 0x01),
            ("isHidden", "()Z", 0x101),
            ("isAnonymousClass", "()Z", 0x01),
            ("isLocalClass", "()Z", 0x01),
            ("isMemberClass", "()Z", 0x01),
            ("isInstance", "(Ljava/lang/Object;)Z", 0x101),
            ("isAssignableFrom", "(Ljava/lang/Class;)Z", 0x101),
            // Hierarchy
            ("getSuperclass", "()Ljava/lang/Class;", 0x101),
            ("getInterfaces", "()[Ljava/lang/Class;", 0x01),
            ("getGenericSuperclass", "()Ljava/lang/reflect/Type;", 0x01),
            ("getGenericInterfaces", "()[Ljava/lang/reflect/Type;", 0x01),
            ("getComponentType", "()Ljava/lang/Class;", 0x01),
            ("getEnclosingClass", "()Ljava/lang/Class;", 0x01),
            ("getEnclosingMethod", "()Ljava/lang/reflect/Method;", 0x01),
            ("getEnclosingConstructor", "()Ljava/lang/reflect/Constructor;", 0x01),
            ("getDeclaringClass", "()Ljava/lang/Class;", 0x01),
            ("getNestHost", "()Ljava/lang/Class;", 0x01),
            ("getNestMembers", "()[Ljava/lang/Class;", 0x01),
            ("isNestmateOf", "(Ljava/lang/Class;)Z", 0x01),
            // Sealed-class API (Java 17+)
            ("getPermittedSubclasses", "()[Ljava/lang/Class;", 0x01),
            // Record API (Java 16+)
            ("getRecordComponents", "()[Ljava/lang/reflect/RecordComponent;", 0x01),
            // Reflection вЂ” declared / inherited members
            ("getDeclaredFields", "()[Ljava/lang/reflect/Field;", 0x01),
            ("getDeclaredMethods", "()[Ljava/lang/reflect/Method;", 0x01),
            ("getDeclaredConstructors", "()[Ljava/lang/reflect/Constructor;", 0x01),
            ("getDeclaredClasses", "()[Ljava/lang/Class;", 0x01),
            (
                "getDeclaredField",
                "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
                0x01,
            ),
            (
                "getDeclaredMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                0x81,
            ),
            (
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                0x81,
            ),
            ("getFields", "()[Ljava/lang/reflect/Field;", 0x01),
            ("getMethods", "()[Ljava/lang/reflect/Method;", 0x01),
            ("getConstructors", "()[Ljava/lang/reflect/Constructor;", 0x01),
            ("getClasses", "()[Ljava/lang/Class;", 0x01),
            (
                "getField",
                "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
                0x01,
            ),
            (
                "getMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                0x81,
            ),
            (
                "getConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                0x81,
            ),
            // Annotations (AnnotatedElement surface + the annotated-type API)
            (
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            ("getAnnotations", "()[Ljava/lang/annotation/Annotation;", 0x01),
            (
                "getDeclaredAnnotations",
                "()[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getDeclaredAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "getDeclaredAnnotationsByType",
                "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
                0x01,
            ),
            (
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z",
                0x01,
            ),
            (
                "getAnnotatedSuperclass",
                "()Ljava/lang/reflect/AnnotatedType;",
                0x01,
            ),
            (
                "getAnnotatedInterfaces",
                "()[Ljava/lang/reflect/AnnotatedType;",
                0x01,
            ),
            // Class loader / module / signers / protection domain
            ("getClassLoader", "()Ljava/lang/ClassLoader;", 0x01),
            ("getModule", "()Ljava/lang/Module;", 0x01),
            ("getPackage", "()Ljava/lang/Package;", 0x01),
            ("getPackageName", "()Ljava/lang/String;", 0x01),
            (
                "getProtectionDomain",
                "()Ljava/security/ProtectionDomain;",
                0x01,
            ),
            ("getSigners", "()[Ljava/lang/Object;", 0x01),
            ("getEnumConstants", "()[Ljava/lang/Object;", 0x01),
            (
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;",
                0x01,
            ),
            (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                0x01,
            ),
            // Generics
            ("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;", 0x01),
            // Casts / forName
            (
                "cast",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                0x01,
            ),
            (
                "asSubclass",
                "(Ljava/lang/Class;)Ljava/lang/Class;",
                0x01,
            ),
            (
                "forName",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                0x09,
            ),
            (
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                0x09,
            ),
            (
                "newInstance",
                "()Ljava/lang/Object;",
                0x01,
            ),
            (
                "desiredAssertionStatus",
                "()Z",
                0x01,
            ),
            ("arrayType", "()Ljava/lang/Class;", 0x01),
            ("componentType", "()Ljava/lang/Class;", 0x01),
        ],
        _ => &[],
    }
}

/// F2 вЂ” build a `MethodMetadata` for a synthetic JDK declaration.
pub fn synthetic_method_meta(
    decl: &(&'static str, &'static str, u16),
    declaring_class_id: ClassId,
) -> MethodMetadata {
    MethodMetadata {
        name: decl.0.to_string(),
        descriptor: decl.1.to_string(),
        access_flags: decl.2,
        declaring_class_id,
        exceptions: Vec::new(),
        signature: None,
    }
}

/// F2 вЂ” return the merged `declared_methods` for `class_id`, augmenting
/// with the synthetic JDK declarations from
/// [`synthetic_jdk_method_decls`] whenever the class file shipped no
/// matching method (typical of synthetic-stub bootstrap loading).
///
/// A method is considered "already present" if a (name, descriptor)
/// pair exists in `ctx.declared_methods`; the synthetic entry is then
/// skipped. This preserves real-class precedence вЂ” when JDK bytecode
/// is loaded, its declarations win.
pub fn declared_methods_with_synthetic(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Vec<MethodMetadata> {
    let mut methods = ctx.declared_methods(class_id);

    // Serializable-lambda support: a real JVM-spun lambda class carries a
    // private `Object writeReplace()` that `ObjectOutputStream` (via
    // `ObjectStreamClass.getInheritableMethod`) invokes to swap the lambda for
    // a `SerializedLambda`. Our synthetic lambda proxies have no bytecode, so
    // without exposing one here real OOS serializes the proxy by its
    // un-loadable `$$Lambda` class name and deserialization throws
    // ClassNotFoundException. Report the synthetic writeReplace so reflection
    // finds it; `native_method_invoke` builds the SerializedLambda when it is
    // actually called. (Private + declared on the proxy class itself, matching
    // the real lambda, so `getInheritableMethod` accepts it.)
    if is_lambda_proxy_id(class_id) {
        // Interpreter round i6 wave 118, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`,
        // default ON since the i6 wave-126 flip; probe `L3W118LambdaProxyReflection.java`):
        // the spun class's methods from the VM's one record of the proxy,
        // which the JVMTI and JNI readers answer from (the SAM method, its
        // bridges, then `writeReplace` or the hostile pair, in the spun class
        // file's order); its constructor is `getDeclaredConstructors`'.
        if crate::nbflags().reflect_lambda_proxy_facts {
            if let Some(record) = ctx.lambda_proxy_record(class_id) {
                for m in record.methods.into_iter().filter(|m| m.name != "<init>") {
                    if !methods
                        .iter()
                        .any(|p| p.name == m.name && p.descriptor == m.descriptor)
                    {
                        methods.push(m);
                    }
                }
                return methods;
            }
        }
        // The SAM method itself (e.g. `Supplier.get()`) has no real bytecode
        // on the synthetic lambda proxy class, so it never shows up in
        // `ctx.declared_methods`. Reflective callers that enumerate methods
        // to find advisable targets вЂ” notably AspectJ pointcut matching
        // during Spring autoproxying (`execution(* ...Supplier+.get())`) вЂ”
        // see an empty candidate set and conclude no advisor applies, so the
        // lambda is never wrapped in a proxy at all (`AopUtils.isAopProxy`
        // false). Synthesize the entry the same way `getInterfaces()` already
        // synthesizes `[SAM]` for lambda proxies (see above).
        if let Some(meta) = ctx.lambda_proxy_serial_metadata(class_id) {
            let already_present = methods
                .iter()
                .any(|m| m.name == meta.sam_method_name && m.descriptor == meta.sam_descriptor);
            if !already_present {
                methods.push(MethodMetadata {
                    name: meta.sam_method_name.clone(),
                    descriptor: meta.sam_descriptor,
                    access_flags: 0x0001, // ACC_PUBLIC
                    declaring_class_id: class_id,
                    exceptions: Vec::new(),
                    signature: None,
                });
            }
            // `altMetafactory` `FLAG_BRIDGES`: HotSpot's spun class declares
            // one public method per bridge descriptor, forwarding to the SAM
            // (`(ObjM & StrM) () -> "s"` lists `String m()` AND `Object m()`).
            // Invoking one reaches the body through the ordinary virtual path,
            // which admits the bridge (`lambda_accepts_descriptor`).
            for bridge in meta.bridges {
                let already_present = methods
                    .iter()
                    .any(|m| m.name == meta.sam_method_name && m.descriptor == bridge);
                if !already_present {
                    methods.push(MethodMetadata {
                        name: meta.sam_method_name.clone(),
                        descriptor: bridge,
                        // ACC_PUBLIC | ACC_BRIDGE, as `InnerClassLambdaMetafactory`
                        // writes a bridge (interpreter round i4 wave 86, lane L4:
                        // it was ACC_PUBLIC alone, so `Method.isBridge()` was false
                        // where HotSpot answers true).
                        access_flags: 0x0041,
                        declaring_class_id: class_id,
                        exceptions: Vec::new(),
                        signature: None,
                    });
                }
            }
        }
        // ...but ONLY for a lambda the JDK would actually have spun a
        // `writeReplace()` onto. Real HotSpot generates one solely for
        // serializable lambdas; an ordinary `Supplier<String> s = () -> "x"`
        // declares just its SAM method, and `getDeclaredMethod("writeReplace")`
        // throws NoSuchMethodException. Reporting it unconditionally made every
        // CratonVM lambda look serializable to a reflective method scan.
        let serializability = ctx.lambda_proxy_serializability(class_id);
        // Interpreter round i4 wave 86, lane L4: an ACCIDENTALLY serializable
        // lambda (a `Serializable` interface, no `FLAG_SERIALIZABLE`) gets
        // JDK 25's serialization-hostile pair instead of `writeReplace`
        // (`InnerClassLambdaMetafactory.generateSerializationHostileMethods`:
        // `private final`, `throws NotSerializableException`, written after
        // the SAM method and its bridges, `writeObject` first). Calling either
        // throws `NotSerializableException("Non-serializable lambda")`
        // (`native_method_invoke`). Probe `L4W86LambdaSerializationShapes`.
        if serializability == cratonvm_native_api::LambdaSerializability::AccidentallySerializable {
            for (name, descriptor) in [
                ("writeObject", "(Ljava/io/ObjectOutputStream;)V"),
                ("readObject", "(Ljava/io/ObjectInputStream;)V"),
            ] {
                if !methods.iter().any(|m| m.name == name && m.descriptor == descriptor) {
                    methods.push(MethodMetadata {
                        name: name.to_string(),
                        descriptor: descriptor.to_string(),
                        // ACC_PRIVATE | ACC_FINAL.
                        access_flags: 0x0012,
                        declaring_class_id: class_id,
                        exceptions: vec!["java/io/NotSerializableException".to_string()],
                        signature: None,
                    });
                }
            }
        }
        if matches!(
            serializability,
            cratonvm_native_api::LambdaSerializability::ByInheritance
                | cratonvm_native_api::LambdaSerializability::ByFlag
        ) && ctx.lambda_proxy_host(class_id).is_some()
            && !methods.iter().any(|m| m.name == "writeReplace")
        {
            methods.push(MethodMetadata {
                name: "writeReplace".to_string(),
                descriptor: "()Ljava/lang/Object;".to_string(),
                // ACC_PRIVATE | ACC_FINAL, matching the real spun method.
                access_flags: 0x0012,
                declaring_class_id: class_id,
                exceptions: Vec::new(),
                signature: None,
            });
        }
        return methods;
    }

    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return methods,
    };

    let synth = synthetic_jdk_method_decls(&class_name);
    if synth.is_empty() {
        return methods;
    }

    for decl in synth {
        let already_present = methods
            .iter()
            .any(|m| m.name == decl.0 && m.descriptor == decl.1);
        if !already_present {
            methods.push(synthetic_method_meta(decl, class_id));
        }
    }

    methods
}

// bytebuddy_probe stack-overflow guard (agent15).
//
// `Class.getDeclaredMethods` / `Class.getMethods` are heavy ByteBuddy hot
// paths. The work each call does (`create_method_object` в†’ allocate a
// reflect.Method mirror per declared method в†’ `descriptor_to_class_mirror`
// per parameter в†’ potentially `load_class` + `ensure_class_initialized` on
// each parameter type в†’ run that class's `<clinit>` via the interpreter)
// can re-enter these natives when ByteBuddy's `JavaDispatcher`,
// SpringFactoriesLoader, or Mockito's `MockMethodInterceptor` walk the
// declared-method table of every class they touch during bootstrap.
//
// The existing depth guards on `native_method_invoke_boxed` (lang_reflect)
// and `native_class_is_assignable_from` (this file) do not cover the
// recursion that flows: native_class_get_declared_methods в†’
// create_method_object в†’ descriptor_to_class_mirror в†’ load_class в†’
// <clinit> в†’ ByteBuddy.run() в†’ native_class_get_declared_methods вЂ¦ which
// is purely Rust-stack recursion that bypasses both invoke guards.
//
// Mirror the same pattern: a thread-local depth counter, bail with a
// recoverable empty-array (`Class[0]`) rather than throwing, since
// throwing during bootstrap class init turns into ExceptionInInitializerError
// in ByteBuddy and kills the framework.
/// zr z11/nd: restores a thread-local recursion depth on every exit of the
/// native that raised it, the allocators' `NativeAllocOom` unwind included
/// (the explicit restore at a native's end does not run on an unwind).
pub struct DepthRestore(pub &'static std::thread::LocalKey<std::cell::Cell<u32>>, pub u32);

impl Drop for DepthRestore {
    fn drop(&mut self) {
        let prev = self.1;
        self.0.with(|d| d.set(prev));
    }
}


thread_local! {
    pub static GET_DECLARED_METHODS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    pub static GET_METHODS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Item 3 of
/// `docs/feature-designs/native-dispatch-architecture.md`
/// (interpreter round i2 wave 54, lane L3): HotSpot's `Reflection::new_method`
/// / `new_constructor` / `new_field` resolve each listed member's signature
/// types -- parameters then return type, then the `throws` types; a field's
/// type -- through the HOLDER's loader (`SystemDictionary::resolve_or_fail`),
/// which asks that loader for every type it has not initiated yet and fails
/// the listing with `NoClassDefFoundError` (its cause the loader's
/// `ClassNotFoundException`) for one it cannot find. Measured, JDK 25.0.3,
/// `L3W54ReflectionLoadsSignatureTypes.java`.
///
/// `members` are `(descriptor, throws)` pairs -- a method's or a field's
/// descriptor. Each type goes through the `CONSTANT_Class` resolution door
/// (`class_id_by_name_via_referencing_class`), so a missing type fails as a
/// bytecode reference to it from the holder would. Primitives are skipped.
/// Only under `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` (the callers ask).
/// Positive control: `CRATONVM_DBG_REFLECT_SIGNATURE_TYPES=1` prints one
/// `[reflect-sig] <holder>: <n> type(s) resolved` line per listing.
pub fn resolve_reflected_signature_types<'m>(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    members: impl IntoIterator<Item = (&'m str, &'m [String])>,
) -> Result<(), MethodCallFailed> {
    match signature_type_absent_in(ctx, holder, members)? {
        Some(name) => Err(reflected_signature_type_absent(ctx, &name)?),
        None => Ok(()),
    }
}

/// [`resolve_reflected_signature_types`]'s resolution: the first type no
/// loader finds, as its internal name (or array descriptor), or `None`; a
/// throwable the resolution raised for any other reason is `Err`. The served
/// listing doors raise the absence themselves, marked by their JDK chain
/// ([`served_listing_prepass`]; 2026-10-07).
pub fn signature_type_absent_in<'m>(
    ctx: &mut dyn NativeContext,
    holder: ClassId,
    members: impl IntoIterator<Item = (&'m str, &'m [String])>,
) -> Result<Option<String>, MethodCallFailed> {
    // A lambda proxy has no class file or loader of its own to resolve
    // through; its member types are its host's, resolved already.
    if is_lambda_proxy_id(holder) {
        return Ok(None);
    }
    let mut names: Vec<String> = Vec::new();
    for (descriptor, throws) in members {
        if descriptor.starts_with('(') {
            let (params, ret) = parse_descriptor_param_and_return(descriptor);
            names.extend(params);
            names.push(ret);
        } else {
            names.push(descriptor.to_string());
        }
        names.extend(throws.iter().map(|t| format!("L{t};")));
    }
    let mut resolved = 0usize;
    for desc in &names {
        // `[...` resolves as an array class (its element through the
        // loader); `L...;` as its internal name; a primitive not at all.
        let name = if desc.starts_with('[') {
            let element = desc.trim_start_matches('[');
            // An element that is neither a reference nor one of the eight
            // primitives (`[V`, which a user loader defines as a field type
            // under `-Xverify:none`) names no class: HotSpot's resolution
            // fails the listing with `NoClassDefFoundError: [V` (t1 wave 13,
            // lane I24W, probe `T1W13I24WStructuralFormat` row S30).
            if !matches!(
                element.as_bytes(),
                [b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z'] | [b'L', ..]
            ) {
                return Ok(Some(desc.clone()));
            }
            if !element.starts_with('L') {
                continue;
            }
            desc.as_str()
        } else if let Some(inner) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            inner
        } else {
            continue;
        };
        match ctx.class_id_by_name_via_referencing_class(holder, name) {
            Ok(_) => {}
            // The resolver reports plain absence as a VM error, not a Java
            // throwable (`classloader_real::absorb_class_absent`'s residual),
            // which escaped the listing as an internal error no `catch`
            // could see -- `getMethod("main", ...)` of a class on the
            // application class path killed the thread (interpreter round i2
            // wave 58, lane L5, measured with the launcher probe
            // `L5W57LauncherSignatureType`). HotSpot's
            // `SystemDictionary::resolve_or_fail` raises `NoClassDefFoundError`
            // (the internal name) caused by the loader's
            // `ClassNotFoundException` (the binary name).
            Err(MethodCallFailed::InternalError(_)) => return Ok(Some(name.to_string())),
            Err(thrown) => return Err(thrown),        }
        resolved += 1;    }
    if false {
        eprintln!(
            "[reflect-sig] {}: {resolved} type(s) resolved",
            ctx.class_name_of_id(holder).unwrap_or_default()
        );
    }
    Ok(None)
}

/// One class of a PUBLIC listing door (`getFields` / `getField` /
/// `getMethods` / `getMethod`, which this crate serves without
/// `getDeclaredFields0` / `getDeclaredMethods0`): the JDK's
/// `privateGetDeclaredFields(true)` / `privateGetDeclaredMethods(true)` of
/// that class, which HotSpot's `Reflection::new_field` / `new_method` make
/// record its PUBLIC declared members' signature types as initiated
/// (interpreter round i2 wave 59, lane L5;
/// `interpreter-L5-the-signature-type-switch-misses-public-fields-and-compatible-listings-FIXED`,
/// probe `L5W58AppLoaderSignatureType`). Their resolution under
/// `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` moved to the door's entry
/// on 2026-10-07 ([`served_listing_prepass`]), which lists the classes the
/// JDK's walk lists, in its order, and marks the raise with the door's JDK
/// frames; this walk's own order is not the JDK's. One predictable branch
/// with the record unarmed.
#[inline]
pub fn resolve_public_listing_rung(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    fields: bool,
) -> Result<(), MethodCallFailed> {
    if !ctx.jdk_names_initiated_armed() {
        return Ok(());
    }
    resolve_public_listing_rung_slow(ctx, class_id, fields)
}

#[cold]
#[inline(never)]
pub fn resolve_public_listing_rung_slow(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    fields: bool,
) -> Result<(), MethodCallFailed> {
    ctx.note_reflected_members(class_id, if fields { 2 } else { 0 }, true);
    Ok(())
}

// ---------------------------------------------------------------------------
// The served listing doors' signature-type pass, in the JDK's order
// ---------------------------------------------------------------------------
//
// 2026-10-07 (item 3 of
// `docs/feature-designs/native-dispatch-architecture.md`).
// A `java.lang.Class` listing door this crate SERVES (`getFields` in both
// modes; under `--compatible` every public and declared listing and lookup)
// runs no JDK frame, so the `NoClassDefFoundError` of a missing signature
// type was captured with the door's caller on top, where HotSpot lists the
// door's JDK frames down to the native that resolves the types (measured,
// JDK 25.0.3: `tools/probes/interp/L5/I47ListingSignatureTypeFrames.java`),
// and five served doors resolved no type at all. Under
// `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` each served door now first
// runs the listings its JDK method makes, in the JDK's order and with its
// early stops ([`served_listing_prepass`]): `privateGetPublicFields` (its
// own fields, then each interface, then the superclass), `getField0` (the
// same, stopping at the field), `privateGetPublicMethods` (its own, the
// superclass, each interface), `getMethodsRecursive` (its own, stopping at a
// match, else the superclass and every interface), and one class's declared
// or public members for the rest. The first missing type raises HotSpot's
// error with the raise marked by the door's JDK chain
// (`NativeExceptionAccess::mark_served_raise`): the door, each recursion hop
// at its call of the next (an interface's at the FIRST recursive call, a
// superclass's at the LAST, or the reverse where the JDK walks the
// superclass first), down to the `getDeclared*0` native's own frame.

/// A `java.lang.Class` listing door a native serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingDoor {
    DeclaredMethods,
    DeclaredMethod,
    DeclaredFields,
    DeclaredField,
    DeclaredConstructors,
    DeclaredConstructor,
    Methods,
    Method,
    Fields,
    Field,
    Constructors,
    Constructor,
}

/// Which members a listing resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingMembers {
    Methods,
    Fields,
    Constructors,
}

/// One recursion hop of a public listing: into an interface, or into the
/// superclass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingStep {
    Interface,
    Superclass,
}

/// What a lookup door looks for, so its walk stops where the JDK's does.
pub enum ListingLookup<'a> {
    /// A listing door: no early stop.
    All,
    /// `getField(name)`.
    Field(&'a str),
    /// `getMethod(name, types)`: the parameter part of the descriptor,
    /// `"(...)"`, built from the parameter types' names.
    Method(&'a str, &'a str),
}

pub const CLASS: &str = "java/lang/Class";

/// The members of `class_id` a `getDeclared*0(publicOnly)` call resolves:
/// `(descriptor, throws)` per member, in declaration order.
pub fn listing_signatures(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    members: ListingMembers,
    public_only: bool,
) -> Vec<(String, Vec<String>)> {
    let public = |flags: u16| !public_only || (flags & (ACC_PUBLIC as u16)) != 0;
    match members {
        ListingMembers::Fields => declared_fields_with_aliases(ctx, class_id)
            .iter()
            .filter(|f| public(f.access_flags))
            .map(|f| (f.descriptor.clone(), Vec::new()))
            .collect(),
        ListingMembers::Methods => declared_methods_with_synthetic(ctx, class_id)
            .iter()
            .filter(|m| m.name != "<init>" && m.name != "<clinit>" && public(m.access_flags))
            .map(|m| (m.descriptor.clone(), m.exceptions.clone()))
            .collect(),
        // The JDK lists no constructor of an interface
        // (`privateGetDeclaredConstructors`).
        ListingMembers::Constructors if ctx.is_interface_class(class_id) => Vec::new(),
        ListingMembers::Constructors => declared_methods_with_synthetic(ctx, class_id)
            .iter()
            .filter(|m| m.name == "<init>" && public(m.access_flags))
            .map(|m| (m.descriptor.clone(), m.exceptions.clone()))
            .collect(),
    }
}

/// How a listed signature type was found missing.
pub enum TypeAbsence {
    /// No loader found it, and nothing was raised yet: the internal name.
    Unbuilt(String),
    /// The resolution door already raised the `NoClassDefFoundError` (a user
    /// loader's `ClassNotFoundException` is its cause).
    Raised(cratonvm_types::ObjectRef),
}

/// The first signature type of `class_id`'s listed `members` that no loader
/// finds, as [`resolve_reflected_signature_types`] resolves them, or `None`.
pub fn listing_absent_type(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    members: ListingMembers,
    public_only: bool,
) -> Result<Option<TypeAbsence>, MethodCallFailed> {
    let signatures = listing_signatures(ctx, class_id, members, public_only);
    match signature_type_absent_in(
        ctx,
        class_id,
        signatures.iter().map(|(d, t)| (d.as_str(), t.as_slice())),
    ) {
        Ok(absent) => Ok(absent.map(TypeAbsence::Unbuilt)),
        Err(MethodCallFailed::ExceptionThrown(exc))
            if ctx
                .class_name_of_id(ctx.class_id_of_object(exc))
                .is_some_and(|name| name == "java/lang/NoClassDefFoundError") =>
        {
            Ok(Some(TypeAbsence::Raised(exc)))
        }
        Err(other) => Err(other),
    }
}

/// The outcome of one rung of a JDK-order walk.
pub enum ListingWalk {
    /// Every listed class resolved; `true` when a lookup found its member.
    Done(bool),
    /// The first missing type, and the recursion hops down to its class.
    Absent(TypeAbsence, Vec<ListingStep>),
}

/// Does `class_id` declare a public field `name` (`searchFields` over
/// `privateGetDeclaredFields(true)`)?
pub fn declares_public_field(ctx: &mut dyn NativeContext, class_id: ClassId, name: &str) -> bool {
    declared_fields_with_aliases(ctx, class_id)
        .iter()
        .any(|f| f.name == name && (f.access_flags & (ACC_PUBLIC as u16)) != 0)
}

/// Does `class_id` declare a public method `name` whose parameter part is
/// `params` (`PublicMethods.MethodList.filter`), static only with
/// `include_static`?
pub fn declares_public_method(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
    params: &str,
    include_static: bool,
) -> bool {
    declared_methods_with_synthetic(ctx, class_id)
        .iter()
        .any(|m| {
            m.name == name
                && (m.access_flags & (ACC_PUBLIC as u16)) != 0
                && (include_static || (m.access_flags & 0x0008) == 0)
                && m.descriptor.starts_with(params)
        })
}

/// `privateGetPublicFields` (`lookup` `All`) or `getField0` (`Field`) of
/// `class_id`: its own public fields, then each interface in declaration
/// order, then (for a class) the superclass; `getField0` stops at the field.
/// A class whose public fields already resolved in this walk is not walked
/// again (the JDK caches its `publicFields`).
pub fn walk_public_fields(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    lookup: Option<&str>,
    steps: &mut Vec<ListingStep>,
    done: &mut Vec<ClassId>,
) -> Result<ListingWalk, MethodCallFailed> {
    if lookup.is_none() && done.contains(&class_id) {
        return Ok(ListingWalk::Done(false));
    }
    if let Some(absence) = listing_absent_type(ctx, class_id, ListingMembers::Fields, true)? {
        return Ok(ListingWalk::Absent(absence, steps.clone()));
    }
    if lookup.is_some_and(|name| declares_public_field(ctx, class_id, name)) {
        return Ok(ListingWalk::Done(true));
    }
    for iface in ctx.class_interfaces(class_id) {
        steps.push(ListingStep::Interface);
        let walk = walk_public_fields(ctx, iface, lookup, steps, done)?;
        steps.pop();
        if !matches!(walk, ListingWalk::Done(false)) {
            return Ok(walk);
        }
    }
    // `getSuperclass()` of an interface is null (and `getField0` asks
    // `isInterface()` first).
    if !ctx.is_interface_class(class_id) {
        if let Some(parent) = ctx.superclass_of(class_id) {
            steps.push(ListingStep::Superclass);
            let walk = walk_public_fields(ctx, parent, lookup, steps, done)?;
            steps.pop();
            if !matches!(walk, ListingWalk::Done(false)) {
                return Ok(walk);
            }
        }
    }
    done.push(class_id);
    Ok(ListingWalk::Done(false))
}

/// `privateGetPublicMethods` (`lookup` `None`): its own public methods, the
/// superclass, then each interface; or `getMethodsRecursive` (`Some`): its
/// own, stopping at a match, else the superclass (passing `include_static`
/// down) and EVERY interface (without static methods), whatever the
/// superclass found.
pub fn walk_public_methods(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    lookup: Option<(&str, &str)>,
    include_static: bool,
    steps: &mut Vec<ListingStep>,
    done: &mut Vec<ClassId>,
) -> Result<ListingWalk, MethodCallFailed> {
    if lookup.is_none() && done.contains(&class_id) {
        return Ok(ListingWalk::Done(false));
    }
    if let Some(absence) = listing_absent_type(ctx, class_id, ListingMembers::Methods, true)? {
        return Ok(ListingWalk::Absent(absence, steps.clone()));
    }
    if lookup.is_some_and(|(name, params)| {
        declares_public_method(ctx, class_id, name, params, include_static)
    }) {
        return Ok(ListingWalk::Done(true));
    }
    let mut found = false;
    // `getSuperclass()` of an interface is null.
    let parent = if ctx.is_interface_class(class_id) {
        None
    } else {
        ctx.superclass_of(class_id)
    };
    if let Some(parent) = parent {
        steps.push(ListingStep::Superclass);
        let walk = walk_public_methods(ctx, parent, lookup, include_static, steps, done)?;
        steps.pop();
        match walk {
            ListingWalk::Done(f) => found |= f,
            absent => return Ok(absent),
        }
    }
    for iface in ctx.class_interfaces(class_id) {
        steps.push(ListingStep::Interface);
        let walk = walk_public_methods(ctx, iface, lookup, false, steps, done)?;
        steps.pop();
        match walk {
            ListingWalk::Done(f) => found |= f,
            absent => return Ok(absent),
        }
    }
    done.push(class_id);
    Ok(ListingWalk::Done(found))
}

/// One `java/lang/Class` hop of a listing chain, at its FIRST or LAST call of
/// `next` (`next_descriptor`).
pub fn listing_hop(
    name: &'static str,
    descriptor: &'static str,
    next: &'static str,
    next_descriptor: &'static str,
    last: bool,
) -> cratonvm_native_api::ServedRaiseHop {
    use cratonvm_native_api::{ServedRaiseHop, ServedRaiseHopSite};
    let site = if last {
        ServedRaiseHopSite::LastCall {
            name: next,
            descriptor: Some(next_descriptor),
        }
    } else {
        ServedRaiseHopSite::FirstCall {
            name: next,
            descriptor: Some(next_descriptor),
        }
    };
    ServedRaiseHop {
        class: CLASS,
        name,
        descriptor,
        site,
    }
}

/// HotSpot's JDK frames for `door` raising a missing signature type after
/// `steps` recursion hops, OUTERMOST first: the door, its hops, the
/// `privateGetDeclared*` call and the `getDeclared*0` native.
pub fn listing_raise_chain(
    door: ListingDoor,
    steps: &[ListingStep],
) -> cratonvm_native_api::ServedRaiseChain {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const M_ARR: &str = "()[Ljava/lang/reflect/Method;";
    const F_ARR: &str = "()[Ljava/lang/reflect/Field;";
    const C_ARR: &str = "()[Ljava/lang/reflect/Constructor;";
    const PGDM: (&str, &str) = (
        "privateGetDeclaredMethods",
        "(Z)[Ljava/lang/reflect/Method;",
    );
    const PGDF: (&str, &str) = ("privateGetDeclaredFields", "(Z)[Ljava/lang/reflect/Field;");
    const PGDC: (&str, &str) = (
        "privateGetDeclaredConstructors",
        "(Z)[Ljava/lang/reflect/Constructor;",
    );
    const GDM0: (&str, &str) = ("getDeclaredMethods0", "(Z)[Ljava/lang/reflect/Method;");
    const GDF0: (&str, &str) = ("getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;");
    const GDC0: (&str, &str) = (
        "getDeclaredConstructors0",
        "(Z)[Ljava/lang/reflect/Constructor;",
    );
    const PGPF: (&str, &str) = ("privateGetPublicFields", F_ARR);
    const PGPM: (&str, &str) = ("privateGetPublicMethods", M_ARR);
    const GF0: (&str, &str) = ("getField0", "(Ljava/lang/String;)Ljava/lang/reflect/Field;");
    const GM0: (&str, &str) = (
        "getMethod0",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    );
    const GMR: (&str, &str) = (
        "getMethodsRecursive",
        "(Ljava/lang/String;[Ljava/lang/Class;ZZ)Ljava/lang/PublicMethods$MethodList;",
    );
    const GC0: (&str, &str) = (
        "getConstructor0",
        "([Ljava/lang/Class;I)Ljava/lang/reflect/Constructor;",
    );
    let hop = |(n, d): (&'static str, &'static str),
               (nn, nd): (&'static str, &'static str),
               last: bool| { listing_hop(n, d, nn, nd, last) };
    // The recursive method a public listing walks, and whether its
    // interface hop is the LAST call (the superclass the first).
    let (door_hop, recursive, interface_last, private, native): (
        Option<((&str, &str), (&str, &str))>,
        Option<(&str, &str)>,
        bool,
        (&str, &str),
        (&str, &str),
    ) = match door {
        ListingDoor::DeclaredMethods => (
            Some((("getDeclaredMethods", M_ARR), PGDM)),
            None,
            false,
            PGDM,
            GDM0,
        ),
        ListingDoor::DeclaredMethod => (
            Some((
                (
                    "getDeclaredMethod",
                    "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                ),
                PGDM,
            )),
            None,
            false,
            PGDM,
            GDM0,
        ),
        ListingDoor::DeclaredFields => (
            Some((("getDeclaredFields", F_ARR), PGDF)),
            None,
            false,
            PGDF,
            GDF0,
        ),
        ListingDoor::DeclaredField => (
            Some((
                (
                    "getDeclaredField",
                    "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
                ),
                PGDF,
            )),
            None,
            false,
            PGDF,
            GDF0,
        ),
        ListingDoor::DeclaredConstructors => (
            Some((("getDeclaredConstructors", C_ARR), PGDC)),
            None,
            false,
            PGDC,
            GDC0,
        ),
        ListingDoor::Constructors => (
            Some((("getConstructors", C_ARR), PGDC)),
            None,
            false,
            PGDC,
            GDC0,
        ),
        ListingDoor::DeclaredConstructor | ListingDoor::Constructor => {
            (None, None, false, PGDC, GDC0)
        }
        ListingDoor::Fields => (
            Some((("getFields", F_ARR), PGPF)),
            Some(PGPF),
            false,
            PGDF,
            GDF0,
        ),
        ListingDoor::Field => (
            Some((
                ("getField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;"),
                GF0,
            )),
            Some(GF0),
            false,
            PGDF,
            GDF0,
        ),
        ListingDoor::Methods => (
            Some((("getMethods", M_ARR), PGPM)),
            Some(PGPM),
            true,
            PGDM,
            GDM0,
        ),
        ListingDoor::Method => (None, Some(GMR), true, PGDM, GDM0),
    };
    let mut hops: Vec<ServedRaiseHop> = Vec::new();
    match door {
        ListingDoor::DeclaredConstructor => {
            hops.push(hop(
                (
                    "getDeclaredConstructor",
                    "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                ),
                GC0,
                false,
            ));
            hops.push(hop(GC0, PGDC, false));
        }
        ListingDoor::Constructor => {
            hops.push(hop(
                (
                    "getConstructor",
                    "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                ),
                GC0,
                false,
            ));
            hops.push(hop(GC0, PGDC, false));
        }
        ListingDoor::Method => {
            hops.push(hop(
                (
                    "getMethod",
                    "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                ),
                GM0,
                false,
            ));
            hops.push(hop(GM0, GMR, false));
        }
        _ => {
            if let Some((door_m, next)) = door_hop {
                hops.push(hop(door_m, next, false));
            }
        }
    }
    if let Some(recursive) = recursive {
        for step in steps {
            let last = match step {
                ListingStep::Interface => interface_last,
                ListingStep::Superclass => !interface_last,
            };
            hops.push(hop(recursive, recursive, last));
        }
        hops.push(hop(recursive, private, false));
    }
    hops.push(hop(private, native, false));
    hops.push(ServedRaiseHop {
        class: CLASS,
        name: native.0,
        descriptor: native.1,
        site: ServedRaiseHopSite::Raise,
    });
    ServedRaiseChain {
        throwable_class: "java/lang/NoClassDefFoundError",
        hops,
    }
}

/// Raise HotSpot's error for a missing type, the raise marked by `door`'s
/// chain after `steps` (see the section comment): built here for an
/// [`TypeAbsence::Unbuilt`] name, REBUILT for a [`TypeAbsence::Raised`] one
/// (same message, same cause), whose capture ran before the mark.
#[cold]
#[inline(never)]
pub fn raise_listing_absent(
    ctx: &mut dyn NativeContext,
    door: ListingDoor,
    steps: &[ListingStep],
    absence: TypeAbsence,
) -> MethodCallFailed {
    // Rooted across the mark (which loads classes) and the rebuild.
    let raised_pin = match absence {
        TypeAbsence::Raised(exc) => Some((exc, ctx.pin_native_root(exc))),
        TypeAbsence::Unbuilt(_) => None,
    };
    let marked = ctx.mark_served_raise(listing_raise_chain(door, steps));
    // Positive control of the served doors' pass (with the listings' own
    // `[reflect-sig] <holder>: <n> type(s) resolved` lines).
    if false {
        eprintln!(
            "[reflect-sig] {door:?} raises a missing type after {steps:?} (marked={marked}, {})",
            if raised_pin.is_some() {
                "rebuilt"
            } else {
                "built"
            }
        );
    }
    let raised = match (absence, raised_pin) {
        (TypeAbsence::Unbuilt(name), _) => match reflected_signature_type_absent(ctx, &name) {
            Ok(raised) | Err(raised) => raised,
        },
        (TypeAbsence::Raised(_), Some((exc, pin))) => {
            let exc = ctx.read_native_pin(pin, exc);
            let rebuilt = rebuild_no_class_def_found(ctx, exc);
            let exc = ctx.read_native_pin(pin, exc);
            ctx.unpin_native_roots(pin);
            rebuilt.unwrap_or(MethodCallFailed::ExceptionThrown(exc))
        }
        (TypeAbsence::Raised(exc), None) => MethodCallFailed::ExceptionThrown(exc),
    };
    if marked {
        ctx.withdraw_served_raise();
    }
    raised
}

/// A new `NoClassDefFoundError` with `exc`'s message and cause (`None` when
/// the rebuild cannot run; the caller then raises `exc` as it is).
pub fn rebuild_no_class_def_found(
    ctx: &mut dyn NativeContext,
    exc: cratonvm_types::ObjectRef,
) -> Option<MethodCallFailed> {
    let pin = ctx.pin_native_root(exc);
    let message = match ctx.invoke_virtual(exc, "getMessage", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(m)))) => ctx.read_string(m),
        _ => None,
    };
    let exc = ctx.read_native_pin(pin, exc);
    let cause = match ctx.invoke_virtual(exc, "getCause", "()Ljava/lang/Throwable;", &[]) {
        Ok(Some(Value::Object(cause))) => cause,
        _ => None,
    };
    ctx.unpin_native_roots(pin);
    let message = message?;
    let cause_pin = cause.map(|c| (c, ctx.pin_native_root(c)));
    let error = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/NoClassDefFoundError",
        1,
        &message,
    );
    let error = match error {
        Ok(error) => error,
        Err(e) => {
            if let Some((_, p)) = cause_pin {
                ctx.unpin_native_roots(p);
            }
            return Some(e);
        }
    };
    if let Some((cause, p)) = cause_pin {
        let cause = ctx.read_native_pin(p, cause);
        let error_pin = ctx.pin_native_root(error);
        let _ = ctx.invoke_virtual(
            error,
            "initCause",
            "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
            &[Value::Object(Some(cause))],
        );
        let error = ctx.read_native_pin(error_pin, error);
        ctx.unpin_native_roots(error_pin);
        ctx.unpin_native_roots(p);
        return Some(MethodCallFailed::ExceptionThrown(error));
    }
    Some(MethodCallFailed::ExceptionThrown(error))
}

/// `"(...)"`: the parameter part of the descriptor of a method whose
/// parameter types are the mirrors of `types` (`null`: none), or `None` when
/// a mirror cannot be named.
pub fn parameter_types_descriptor(
    ctx: &mut dyn NativeContext,
    types: Option<cratonvm_types::ObjectRef>,
) -> Option<String> {
    let mut out = String::from("(");
    if let Some(arr) = types {
        for i in 0..ctx.array_length(arr) {
            let Value::Object(Some(mirror)) = ctx.get_array_element(arr, i) else {
                return None;
            };
            let name = mirror_class_name(ctx, mirror)?;
            match name.as_str() {
                "int" => out.push('I'),
                "long" => out.push('J'),
                "boolean" => out.push('Z'),
                "byte" => out.push('B'),
                "char" => out.push('C'),
                "short" => out.push('S'),
                "float" => out.push('F'),
                "double" => out.push('D'),
                "void" => out.push('V'),
                array if array.starts_with('[') => out.push_str(array),
                object => {
                    out.push('L');
                    out.push_str(object);
                    out.push(';');
                }
            }
        }
    }
    out.push(')');
    Some(out)
}

/// The listings `door`'s JDK method makes for `receiver`, in its order and
/// with its early stops, each resolving its members' signature types (see
/// the section comment). Only under `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`.
pub fn served_listing_prepass(
    ctx: &mut dyn NativeContext,
    receiver: ClassId,
    door: ListingDoor,
    lookup: ListingLookup<'_>,
) -> Result<(), MethodCallFailed> {
    if !ctx.reflection_loads_signature_types() || is_lambda_proxy_id(receiver) {
        return Ok(());
    }
    let mut steps: Vec<ListingStep> = Vec::new();
    let mut done: Vec<ClassId> = Vec::new();
    let (members, public_only) = match door {
        ListingDoor::DeclaredMethods | ListingDoor::DeclaredMethod => {
            (ListingMembers::Methods, false)
        }
        ListingDoor::DeclaredFields | ListingDoor::DeclaredField => (ListingMembers::Fields, false),
        ListingDoor::DeclaredConstructors | ListingDoor::DeclaredConstructor => {
            (ListingMembers::Constructors, false)
        }
        ListingDoor::Constructors | ListingDoor::Constructor => {
            (ListingMembers::Constructors, true)
        }
        ListingDoor::Fields | ListingDoor::Field => {
            let name = match lookup {
                ListingLookup::Field(name) => Some(name),
                _ => None,
            };
            return match walk_public_fields(ctx, receiver, name, &mut steps, &mut done)? {
                ListingWalk::Absent(absence, steps) => {
                    Err(raise_listing_absent(ctx, door, &steps, absence))
                }
                ListingWalk::Done(_) => Ok(()),
            };
        }
        ListingDoor::Methods | ListingDoor::Method => {
            let wanted = match lookup {
                ListingLookup::Method(name, params) => Some((name, params)),
                _ => None,
            };
            return match walk_public_methods(ctx, receiver, wanted, true, &mut steps, &mut done)? {
                ListingWalk::Absent(absence, steps) => {
                    Err(raise_listing_absent(ctx, door, &steps, absence))
                }
                ListingWalk::Done(_) => Ok(()),
            };
        }
    };
    match listing_absent_type(ctx, receiver, members, public_only)? {
        Some(absence) => Err(raise_listing_absent(ctx, door, &[], absence)),
        None => Ok(()),
    }
}

/// HotSpot's failure for a reflected member's signature type that no loader
/// finds: `NoClassDefFoundError: <internal name>` caused by
/// `ClassNotFoundException: <binary name>` (measured, JDK 25.0.3:
/// `L3W54ReflectionLoadsSignatureTypes`, `L5W57LauncherSignatureType`). An
/// array name reports its element, as the resolution that failed did.
#[cold]
#[inline(never)]
pub fn reflected_signature_type_absent(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    let element = name
        .trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(name);
    let cause = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/ClassNotFoundException",
        1,
        &element.replace('/', "."),
    )?;
    let cause_pin = ctx.pin_native_root(cause);
    let error = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/NoClassDefFoundError",
        1,
        element,
    );
    let cause = ctx.read_native_pin(cause_pin, cause);
    let error = match error {
        Ok(error) => error,
        Err(e) => {
            ctx.unpin_native_roots(cause_pin);
            return Err(e);
        }
    };
    let error_pin = ctx.pin_native_root(error);
    let _ = ctx.invoke_virtual(
        error,
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(cause))],
    );
    let error = ctx.read_native_pin(error_pin, error);
    ctx.unpin_native_roots(cause_pin);
    Ok(MethodCallFailed::ExceptionThrown(error))
}

pub fn link_isolated_method_signatures(
    ctx: &mut dyn NativeContext,
    declaring_class_id: ClassId,
    methods: &[&MethodMetadata],
) -> Result<(), MethodCallFailed> {
    let Some(loader) =
        crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), declaring_class_id.as_u32())
    else {
        return Ok(());
    };
    if !crate::classloader::url_classloader_isolated_from_app(ctx, loader) {
        return Ok(());
    }
    // gen r4w3/rooting: every iteration's `loadClass` / `initialize_class` is a
    // GC point, so the loader must stay pinned across the WHOLE loop (a
    // per-iteration pin of the outer, never-refreshed local re-pinned a stale
    // address from the second iteration on). Pin once, re-read per iteration.
    let loader_pin = ctx.pin_native_root(loader);
    let result = link_isolated_method_signatures_loop(ctx, loader_pin, loader, methods);
    ctx.unpin_native_roots(loader_pin);
    result
}

/// Loop body of [`link_isolated_method_signatures`]; `loader_pin` holds the
/// isolated loader for the duration (gen r4w3/rooting).
pub fn link_isolated_method_signatures_loop(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader: ObjectRef,
    methods: &[&MethodMetadata],
) -> Result<(), MethodCallFailed> {
    for method in methods {
        let (_, descriptor) = parse_descriptor_param_and_return(&method.descriptor);
        let Some(name) = descriptor
            .trim_start_matches('[')
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
        else {
            continue;
        };
        if is_global_resolution_namespace(name) {
            continue;
        }
        let name_obj = ctx.create_string(&name.replace('/', "."));
        let name_pin = ctx.pin_native_root(name_obj);
        let loader = ctx.read_native_pin(loader_pin, loader);
        let name_obj = ctx.read_native_pin(name_pin, name_obj);
        let loaded = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_obj);
        ctx.unpin_native_roots(name_pin);
        let mirror = match loaded {
            Ok(Some(Value::Object(Some(mirror)))) => mirror,
            Ok(_) => return Err(isolated_loader_class_not_found(ctx, name)?),
            Err(failed) => {
                // W7-26 R2 -- a failure is not a miss. `isolated_loader_class_not_found`
                // synthesises a `NoClassDefFoundError` naming this type, which is the
                // right answer only when the isolated loader could not FIND the class.
                // JDK 25 `URLClassLoader.findClass` raises `ClassNotFoundException`
                // for that case alone; a found-but-unusable class leaves `defineClass`
                // as its own `LinkageError` -- `ClassFormatError`, `VerifyError`,
                // `UnsupportedClassVersionError`, `IncompatibleClassChangeError` --
                // and each of those is a different `catch` in application code.
                // `absorb_class_absent` returns `Ok(())` only for the two
                // class-absent shapes (tested by `ClassId` hierarchy, never by
                // name); everything else it hands straight back through `?`.
                //
                // NOTE for the record: W7-26's patch text for this site says the
                // re-mint is a `ClassNotFoundException`. It is a
                // `NoClassDefFoundError` -- see `isolated_loader_class_not_found`
                // just below. The argument is unaffected, the class name in the
                // prose is not.
                crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                return Err(isolated_loader_class_not_found(ctx, name)?);
            }
        };
        let mirror_pin = ctx.pin_native_root(mirror);
        // Netty CompositeByteBuf clinit bug (20260731): forcing full
        // initialization here is only safe when this thread is not already
        // mid-<clinit>. `loadClass` above already proved the type is present
        // and loadable in this isolated loader's view -- that's all JVMS
        // §5.5 requires for exposing it as a reflective method's
        // return/parameter type. Actually running its <clinit> as well is an
        // extra (originally intended to turn a missing transitive type into
        // an eager NoClassDefFoundError for OnBeanCondition), but doing so
        // while this thread is already inside another class's still-running
        // <clinit> lets the forced class observe THAT class's statics at
        // their pre-assignment default instead of failing to resolve --
        // see docs/feature-designs/native-dispatch-architecture.md.
        //
        // NEITHER link nor initialize: HotSpot's `JVM_GetClassDeclaredMethods`
        // resolves (loads) a method's signature types and never verifies or runs
        // their `<clinit>`. A return type whose initializer needs a class the loader
        // hides (`JacksonJsonMessageConverter` under
        // `@ClassPathExclusions("jackson-*-3*")`, whose static state names
        // `tools.jackson`) made `getDeclaredMethods` throw
        // `NoClassDefFoundError` where HotSpot answers the methods
        // (`WebSocketMessagingAutoConfigurationTests`).
        // Loading was proven by `loadClass` above; HotSpot does not link the
        // type either (verifying `JacksonJsonMessageConverter` reaches the
        // hidden `tools.jackson` classes and fails the same way).
        let link_ok = true;
        ctx.unpin_native_roots(mirror_pin);
        if !link_ok {
            return Err(isolated_loader_class_not_found(ctx, name)?);
        }
    }
    Ok(())
}

pub fn isolated_loader_class_not_found(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    let exception = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/NoClassDefFoundError",
        1,
        &name.replace('/', "."),
    );
    Ok(MethodCallFailed::ExceptionThrown(exception?))
}

pub fn native_class_get_declared_methods(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Reentrancy / recursion guard вЂ” see comment on GET_DECLARED_METHODS_DEPTH.
    let prev_depth = GET_DECLARED_METHODS_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if prev_depth > 50 {
        GET_DECLARED_METHODS_DEPTH.with(|d| d.set(prev_depth));
        // Bail safe: return an empty Method[] so ByteBuddy / Mockito can
        // recover rather than die on ExceptionInInitializerError.
        let arr = empty_listing_array(ctx, "java/lang/reflect/Method");
        return Ok(Some(Value::Object(Some(arr))));
    }
    // zr z11/nd: restored on the UNWIND too. An allocation refusal inside
    // this native unwinds (`NativeAllocOom`) past the restore at the end, so
    // every `OutOfMemoryError` raised in here leaked one level; after 51 of
    // them on a thread this answered an empty array for every class.
    let _depth_restore = DepthRestore(&GET_DECLARED_METHODS_DEPTH, prev_depth);
    let result = (|| -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Class.getDeclaredMethods on null".to_string()),
                }
                .into())
            }
        };
        let class_id = match mirror_class_id(ctx, this) {
            Some(id) => id,
            None => {
                // Primitive or array type
                let arr = empty_listing_array(ctx, "java/lang/reflect/Method");
                return Ok(Some(Value::Object(Some(arr))));
            }
        };
        // HotSpot `JVM_GetClassDeclaredMethods` links the class first (the
        // VM's policy decides; interpreter round i1 wave 17).
        ctx.link_class_for_reflection(class_id)?;
        // 2026-10-07 (item 3 of the i47-L5 page): called with no argument, this
        // serves the JDK's `getDeclaredMethods()` (`--compatible`), whose
        // `privateGetDeclaredMethods` frames the missing-type raise is marked by
        // (`served_listing_prepass`); `getDeclaredMethods0(publicOnly)` takes two.
        if args.len() == 1 {
            served_listing_prepass(
                ctx,
                class_id,
                ListingDoor::DeclaredMethods,
                ListingLookup::All,
            )?;
        }

        gdmp::reset();
        let __gdm_all = std::time::Instant::now();
        let mut __glap = gdmp::Lap::new();
        let methods = declared_methods_with_synthetic(ctx, class_id);
        __glap.mark(11);
        if crate::vmflags().loader.dbg_obsreg {
            let __cname = ctx.class_name_of_id(class_id).unwrap_or_default();
            if __cname.contains("SecurityFilterAutoConfigurationEarlyInitializationTests")
                || __cname.contains("PathRequestTests")
                || __cname.contains("ManagementWebSecurityAutoConfigurationTests")
            {
                eprintln!(
                    "[OBSREG-DBG] getDeclaredMethods class={__cname} class_id={class_id:?} count={} names={:?}",
                    methods.len(),
                    methods.iter().map(|m| format!("{}{}", m.name, m.descriptor)).collect::<Vec<_>>()
                );
            }
        }
        // `getDeclaredMethods0(boolean publicOnly)` вЂ” the real JDK calls this
        // with publicOnly=true on the `Class.getMethods()` / `getMethod()` path
        // (`privateGetPublicMethods`) and TRUSTS it to return only PUBLIC
        // methods (mirroring `getDeclaredFields0`, which already honours the
        // flag). Previously we ignored args[1], so `getMethods()` surfaced
        // package-private/protected methods and `getMethod("get", int)` resolved
        // a non-public method instead of throwing NoSuchMethodException
        // (ReflectiveIndexAccessorTests.nonPublicReadMethod). The synthetic
        // one-arg `getDeclaredMethods()` wrapper passes no flag в†’ publicOnly=false.
        let public_only = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
        let filtered_names = crate::reflect_filter::filtered_method_names(ctx, class_id);
        // Filter out <init> and <clinit>, plus non-public when publicOnly,
        // plus the core reflection member filter (reflect_filter.rs) — a
        // filtered method (e.g. sun.misc.Unsafe.getUnsafe) does not exist
        // as far as getDeclaredMethods is concerned.
        let mut visible: Vec<&MethodMetadata> = methods
            .iter()
            .filter(|m| m.name != "<init>" && m.name != "<clinit>")
            .filter(|m| !public_only || (m.access_flags & 0x0001) != 0)
            .filter(|m| !crate::reflect_filter::is_filtered(&filtered_names, &m.name))
            .collect();

        // Interpreter round i5 wave 101, lane L3 (proposal
        // `i100-L3-proposal-hotspot-method-array-order-for-every-class`;
        // `CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER`, default off): HotSpot's
        // `JVM_GetClassDeclaredMethods` walks `InstanceKlass::methods()`, which
        // `Method::sort_methods` quicksorted by name-`Symbol` address. Rank the
        // WHOLE method list (`<init>` / `<clinit>` move the partitions) from
        // the JDK's own CDS archive, then keep the visible members. Exact for a
        // class whose names are all archived (every archived JDK class); an
        // approximation for run-time names. Only with a CDS index; the bridge
        // and annotation passes below then stand down (a bridge shares its
        // overload's `Symbol`, so where it lands is the quicksort's answer).
        let mut hotspot_ordered = false;
        let hotspot_order_on = reflect_hotspot_method_order_on(&*ctx);
        if annotation_order_dbg() {
            // w101 hand-back: which step declines (`CRATONVM_DBG_ANNORDER=1`).
            let index = cds_symbol_index(ctx);
            let hits = methods
                .iter()
                .filter(|m| index.offset_of(&m.name).is_some())
                .count();
            eprintln!(
                "[gdmorder] class={} switch={} caches={} visible={} index_names={} archived_hits={}/{}",
                ctx.class_name_of_id(class_id).unwrap_or_default(),
                hotspot_order_on,
                reflect_door_caches(&*ctx).is_some(),
                visible.len(),
                index.len(),
                hits,
                methods.len()
            );
        }
        if visible.len() > 1 && hotspot_order_on {
            let index = cds_symbol_index(ctx);
            if index.len() > 0 {
                let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
                let pool_rank = method_name_pool_ranks(&*ctx, class_id, &index, &names);
                let order = hotspot_method_array_order_ranked(&index, &names, &pool_rank);
                let shown: rustc_hash::FxHashSet<*const MethodMetadata> =
                    visible.iter().map(|m| *m as *const MethodMetadata).collect();
                let reordered: Vec<&MethodMetadata> = order
                    .into_iter()
                    .map(|i| &methods[i])
                    .filter(|m| shown.contains(&(*m as *const MethodMetadata)))
                    .collect();
                if reordered.len() == visible.len() {
                    visible = reordered;
                    hotspot_ordered = true;
                }
            }
        }
        if annotation_order_dbg() {
            // Wave 102, lane L3: the order this call answers, so a wrong
            // probe row tells "not applied here" from "applied, then lost"
            // (a `ReflectionData` filled by an earlier call, another door).
            eprintln!(
                "[gdmorder] class={} public_only={} applied={} order={}",
                ctx.class_name_of_id(class_id).unwrap_or_default(),
                public_only,
                hotspot_ordered,
                visible
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        // Bridge-method adjacency ordering. A compiler-generated bridge method
        // (ACC_BRIDGE, 0x40) is emitted AFTER its bridged counterpart in the
        // class file, but typically as the LAST method of the class вЂ” so a
        // bridged method and its bridge are not adjacent. HotSpot's
        // `getDeclaredMethods` (symbol-table order) keeps the bridge right after
        // its same-name bridged method, and two kinds of Spring code rely on
        // that relative order:
        //   * `ReflectionUtils.findMethod` / generic-type resolution take the
        //     FIRST same-name method, which MUST stay the real (non-bridge) one,
        //     else `getGenericReturnType()` reads the bridge's erased `Object`
        //     return (GenericTypeResolverTests.resolveTypeFromNestedParameterizedType);
        //   * the bridge-method tests collect all same-named methods and pick
        //     the Object-returning bridge as `methods.get(0)`/`get(1)`, asserting
        //     `isBridge()` (MergedAnnotationsTests.getWithTypeHierarchyFromBridgeMethod,
        //     MergedAnnotationsComposedOnSingleAnnotatedElementTests
        //     .typeHierarchyStrategyMultipleComposedAnnotationsOnBridgeMethod).
        // Both are satisfied by moving each bridge to IMMEDIATELY AFTER the first
        // same-name non-bridge method (never making a bridge the first sibling).
        // Minimal, stable: everything else keeps its class-file position, and
        // only classes that declare a bridge with a same-name sibling change.
        if !hotspot_ordered && visible.iter().any(|m| m.access_flags & 0x0040 != 0) {
            // Match a bridge to its bridged target by (name, parameter types) --
            // NOT name alone. A single overloaded name (e.g. `append`) can have
            // MANY non-bridge siblings plus SEVERAL, DIFFERENT bridges (one per
            // interface it separately implements, e.g. `Appendable.append(char)`
            // / `append(CharSequence)` / `append(CharSequence,int,int)` on
            // `AbstractStringBuilder`). The previous name-only match attached
            // every same-named bridge to the FIRST non-bridge overload it
            // scanned (`append(Object)`), yanking all of them out of their
            // natural position and ahead of the OTHER `append` overloads. Real
            // JDK reflection keeps bridges immediately after the specific
            // overload they bridge (verified: HotSpot's own
            // `getDeclaredMethods()` places the three `Appendable` bridges
            // after ALL real `append` overloads, each adjacent to its exact
            // parameter-type match; CratonVM previously grouped them right
            // after the first `append`). This is a real, independently-
            // verified ordering bug fix on its own merits -- found while
            // investigating why `MockMethodAdvice.isOverridden()` (ByteBuddy's
            // `MethodGraph.Compiler`, used by Mockito's inline mock maker)
            // answers differently on CratonVM for
            // `AbstractStringBuilder.substring(int)` on a mocked
            // `StringBuilder` (see
            // CRATONVM-SPRING-GENUINE-BUGLIST's MockitoBeanByTypeLookup
            // entry). Landing this alone does NOT flip that specific
            // `isOverridden` answer -- confirmed by direct A/B: calling
            // Mockito's own `compiler.compile(...)` externally with this fix
            // in place already resolves `substring(int)`'s graph node to
            // `AbstractStringBuilder` (matching HotSpot) even BEFORE this fix
            // landed, yet the real `isOverridden()` method, executed as
            // interpreted bytecode, still returns the wrong boolean -- so the
            // substring-mocking bug's root cause is a separate, still-open
            // interpreter defect in executing that specific method body, not
            // (only) this ordering gap. Kept as a standalone fix regardless.
            fn param_types(descriptor: &str) -> &str {
                let start = descriptor.find('(').map(|i| i + 1).unwrap_or(0);
                let end = descriptor.find(')').unwrap_or(descriptor.len());
                if start <= end {
                    &descriptor[start..end]
                } else {
                    ""
                }
            }
            let mut reordered: Vec<&MethodMetadata> = Vec::with_capacity(visible.len());
            let mut placed = vec![false; visible.len()];
            for i in 0..visible.len() {
                if placed[i] {
                    continue;
                }
                reordered.push(visible[i]);
                placed[i] = true;
                // After a non-bridge method, pull in any later same-name,
                // same-parameter-types bridge(s) so each bridge follows the
                // exact overload it bridges.
                if visible[i].access_flags & 0x0040 == 0 {
                    let params_i = param_types(&visible[i].descriptor);
                    for j in (i + 1)..visible.len() {
                        if !placed[j]
                            && visible[j].access_flags & 0x0040 != 0
                            && visible[j].name == visible[i].name
                            && param_types(&visible[j].descriptor) == params_i
                        {
                            reordered.push(visible[j]);
                            placed[j] = true;
                        }
                    }
                }
            }
            visible = reordered;
        }

        // Round 16 wave 13 (lane annorder): an annotation interface lists its
        // methods in HotSpot's `getDeclaredMethods()` order (method-name
        // `Symbol` addresses: CDS-archived names first, in archive order). The
        // real `AnnotationType` fills `memberDefaults` in this order, and
        // `Annotation.toString()` prints two defaulted members of one bucket
        // in it. ACC_ANNOTATION = 0x2000; an annotation has no bridges.
        if !hotspot_ordered
            && visible.len() > 1
            && ctx.class_access_flags(class_id) & 0x2000 != 0
            && annotation_cds_member_order()
        {
            let index = cds_symbol_index(ctx);
            let names: Vec<&str> = visible.iter().map(|m| m.name.as_str()).collect();
            let order = hotspot_declared_method_order(&index, &names);
            let reordered: Vec<&MethodMetadata> = order.into_iter().map(|i| visible[i]).collect();
            visible = reordered;
        } else if visible.len() > 1
            && annotation_cds_member_order()
            && plain_class_method_order_is_exact(ctx, class_id, &methods)
        {
            // JIT round 21 (lane npecompat, AO-1): a plain class, where
            // HotSpot's order can be stated exactly -- see
            // `plain_class_method_order_is_exact`. `JVM_GetClassDeclaredMethods`
            // lists `InstanceKlass::methods()` in array order, the array
            // `Method::sort_methods` sorted once at parse time
            // (`hotspot_method_array_order`, overloads and bridges in the
            // quicksort's order, as `getDeclaredConstructors()` already
            // takes them). Replaces the bridge-adjacency order above.
            let index = cds_symbol_index(ctx);
            let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
            let order = hotspot_method_array_order(&index, &names);
            let reordered: Vec<&MethodMetadata> = order
                .into_iter()
                .map(|i| &methods[i])
                .filter(|m| visible.iter().any(|v| std::ptr::eq(*v, *m)))
                .collect();
            if reordered.len() == visible.len() {
                visible = reordered;
            }
        }

        __glap.mark(12);
        // HotSpot's `Reflection::new_method` resolves each listed method's
        // signature types through the class's loader, which initiates them
        // (`Instrumentation.getInitiatedClasses`; one relaxed load while no
        // record is armed; interpreter round i1 wave 47, lane L5).
        if ctx.jdk_names_initiated_armed() {
            ctx.note_reflected_members(class_id, 0, public_only);
        }
        // Interpreter round i2 wave 54, lane L3: and LOADS them, as HotSpot
        // does (a missing one fails the listing), under
        // `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`.
        if ctx.reflection_loads_signature_types() {
            resolve_reflected_signature_types(
                ctx,
                class_id,
                visible
                    .iter()
                    .map(|m| (m.descriptor.as_str(), m.exceptions.as_slice())),
            )?;
        }
        link_isolated_method_signatures(ctx, class_id, &visible)?;
        __glap.mark(13);

        // GC-safe: `create_method_object` allocates (see `build_mirror_array`).
        // zs s3/nd3: a loading lookup (`listing_component_id`); `this` is not
        // read after `mirror_class_id`.
        let method_component = listing_component_id(ctx, "java/lang/reflect/Method");
        let arr = build_mirror_array_comp(ctx, method_component, visible.len(), |ctx, i| {
            Ok(create_method_object(ctx, visible[i])?)
        })?;
        if gdmp::on() {
            let __name = ctx.class_name_of_id(class_id).unwrap_or_default();
            gdmp::report(
                &__name,
                visible.len(),
                __gdm_all.elapsed().as_nanos() as u64,
            );
        }
        Ok(Some(Value::Object(Some(arr))))
    })();
    // Restore depth on every exit path (success or error).
    GET_DECLARED_METHODS_DEPTH.with(|d| d.set(prev_depth));
    result
}

/// `NoSuchMethodException`'s message, transcribed rather than composed.
///
/// MEASURED on both VMs; the oracle names the class and the parameter types
/// and this VM named neither:
///
/// ```text
///   H.class.getMethod("nope")           HotSpot  P8$H.nope()        was  nope
///   H.class.getMethod("m", int.class)   HotSpot  P8$H.m(int)        was  m
///   Runnable.class.getDeclaredConstructor()
///                                       HotSpot  java.lang.Runnable.<init>()   was  <init>
/// ```
///
/// This is the message a framework prints when reflection misses, and `nope`
/// on its own does not say which class was searched or with what signature —
/// which is the entire diagnostic value of the exception.
///
/// Rendering follows `Class.getName()`, NOT `getTypeName()`, and the separator
/// is a bare comma. Both were measured after being guessed wrong:
///
/// ```text
///   getMethod("zz", int.class, String.class)  P9$H.zz(int,java.lang.String)
///   getMethod("zz", int[].class)              P9$H.zz([I)
///   getMethod("zz", String[][].class)         P9$H.zz([[Ljava.lang.String;)
///   getMethod("zz", (Class<?>) null)          P9$H.zz(null)
///   getMethod("zz")                           P9$H.zz()
/// ```
///
/// So an array parameter appears as `[I`, never `int[]` — reaching for
/// [`array_descriptor_to_type_name`] here is exactly wrong — and there is no
/// space after the comma. This is the JDK's `Class.methodToString`, which
/// joins `getName()` with `","`.
pub fn no_such_method_message(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    member: &str,
    param_types_arr: Option<cratonvm_types::ObjectRef>,
) -> String {
    let owner = ctx
        .class_name_of_id(class_id)
        .map(|n| reflect_text_class_name(ctx, &n))
        .unwrap_or_default();
    no_such_method_message_of(ctx, &owner, member, param_types_arr)
}

/// [`no_such_method_message`] for an owner given by its `Class.getName()`
/// (a primitive mirror has no class id: `int.class.getMethod("x")` is
/// HotSpot's `int.x()`; interpreter round i1 wave 52, lane L3).
pub fn no_such_method_message_of(
    ctx: &dyn NativeContext,
    owner: &str,
    member: &str,
    param_types_arr: Option<cratonvm_types::ObjectRef>,
) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(arr) = param_types_arr {
        let n = ctx.array_length(arr);
        for i in 0..n {
            let rendered = match ctx.get_array_element(arr, i) {
                Value::Object(Some(m)) => mirror_type_name(ctx, m),
                _ => "null".to_string(),
            };
            params.push(rendered);
        }
    }
    format!("{owner}.{member}({})", params.join(","))
}

/// A `Class` mirror rendered as `Class.getName()` renders it — which for an
/// array is its descriptor with dots (`[[Ljava.lang.String;`), not `String[][]`.
/// Measured; see [`no_such_method_message`].
pub fn mirror_type_name(ctx: &dyn NativeContext, mirror: cratonvm_types::ObjectRef) -> String {
    reflect_text_class_name(ctx, &mirror_class_name(ctx, mirror).unwrap_or_default())
}

pub fn native_class_get_declared_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredMethod on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // JDK 25's `Objects.requireNonNull(name)`: no message (wave 53,
            // lane L3; see `native_class_get_declared_field`).
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();

    // Parameter types array (args[2]) вЂ” may be null or empty
    let param_types_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // WF6 вЂ” even if the class id can't be resolved, try to short-circuit
            // for known WildFly/Keycloak entry-class `main(String[])` lookups so
            // that jboss-modules' bootstrap progresses past the NoSuchMethod
            // wall. Fall through to the existing NSME otherwise.
            if let Ok(Some(method_obj)) =
                wf_shim_synth_main_method(ctx, this, &target_name, param_types_arr)
            {
                return Ok(Some(Value::Object(Some(method_obj))));
            }
            return Err(cratonvm_types::error::RuntimeError::NoSuchMethodException {
                message: target_name,
            }
            .into());
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(
        ctx,
        class_id,
        ListingDoor::DeclaredMethod,
        ListingLookup::All,
    )?;

    // gen r4w3/rooting: `filtered_method_names` runs Java on every call and
    // `param_type_matches` may load through a user loader, so `this` and the
    // parameter-types array are pinned here and re-read after each GC point.
    let this_pin = ctx.pin_native_root(this);
    let pt_pin = param_types_arr.map(|arr| ctx.pin_native_root(arr));

    // Core reflection member filter (see reflect_filter.rs): checked before
    // any resolution so the single-member form agrees with the array form
    // in `native_class_get_declared_methods` on every row.
    if crate::reflect_filter::is_filtered(
        &crate::reflect_filter::filtered_method_names(ctx, class_id),
        &target_name,
    ) {
        ctx.unpin_native_roots(this_pin);
        return Err(cratonvm_types::error::RuntimeError::NoSuchMethodException {
            message: target_name,
        }
        .into());
    }
    // gen r4w3/rooting: re-read after the filter's Java upcall.
    let param_types_arr = match (param_types_arr, pt_pin) {
        (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
        _ => None,
    };

    // F2: include synthetic JDK declarations for classes loaded as
    // synthetic stubs (e.g. java/lang/ClassLoader has no real .class
    // file in synthetic-jdk mode, so its `methods` list is empty).
    let methods = declared_methods_with_synthetic(ctx, class_id);

    // Round 9 audit fix (HIGH #7): build a cache key string from the
    // requested parameter-type mirrors. The key is a synthetic
    // `;`-delimited list of dotted class names вЂ” not a JVM descriptor,
    // but unique within `(class_id, name)` so identical Spring /
    // Hibernate / ByteBuddy probes hit the cache instead of re-walking.
    // A `None` `param_types_arr` is semantically identical to an empty
    // array (both mean "match a no-arg method" вЂ” see the cold-walk loop
    // below), so both map to the same empty cache key, which is correct.
    // The cached entry stays valid as long as the class hasn't been
    // redefined (the round-8 invalidation hook drops every entry keyed on
    // `class_id` on redefine).
    //
    // A `None` element in the parameter array makes the key
    // unrepresentable; we set `cache_key_ok = false` so we skip both
    // the probe and the insert (the cold-walk loop still preserves
    // the original `matched = false; continue` behaviour per meta).
    let mut cache_key_ok = true;
    let cache_key_desc: String = match param_types_arr {
        None => String::new(),
        Some(pt_arr) => {
            let n = ctx.array_length(pt_arr);
            let mut s = String::with_capacity(n * 16);
            for i in 0..n {
                let mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        cache_key_ok = false;
                        break;
                    }
                };
                let name = mirror_class_name(ctx, mirror).unwrap_or_default();
                s.push_str(&name);
                // Strict mode: two same-named classes are two parameter types, so the
                // cached answer for one must not serve the other.
                if ctx.is_jdk_only() {
                    if let Some(id) = mirror_class_id(ctx, mirror) {
                        s.push('#');
                        s.push_str(&id.as_u32().to_string());
                    }
                }
                s.push(';');
            }
            s
        }
    };

    // Probe the LinkResolver cache. On hit, re-fetch the metadata vec
    // and pick the entry at the cached index вЂ” much faster than
    // re-running the parameter-type comparison loop.
    if cache_key_ok {
        if let Some((decl, idx)) =
            ctx.link_resolver_get_method(class_id, &target_name, &cache_key_desc)
        {
            if decl == class_id {
                if let Some(meta) = methods.get(idx as usize) {
                    if meta.name == target_name {
                        // gen r4w3/rooting: `this`/array unused past here.
                        ctx.unpin_native_roots(this_pin);
                        let method_obj = create_method_object(ctx, meta)?;
                        return Ok(Some(Value::Object(Some(method_obj))));
                    }
                }
            }
            // Cached entry no longer applicable (synthetic-method table
            // mutated, redefine raced past our invalidation window, etc.) вЂ”
            // fall through to the cold walk.
        }
    }

    // If param_types is provided, match by name + parameter count/types
    for (meta_idx, meta) in methods.iter().enumerate() {
        if meta.name != target_name || meta.name == "<init>" || meta.name == "<clinit>" {
            continue;
        }
        // gen r4w3/rooting: a previous candidate's `param_type_matches` may
        // have collected; take the array's current address from the pin.
        let param_types_arr = match (param_types_arr, pt_pin) {
            (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
            _ => None,
        };

        // JDK semantics: a `null` parameterTypes argument is treated as a
        // ZERO-length array вЂ” `Class.getDeclaredMethod(name, (Class[])null)`
        // matches ONLY a no-arg method. `Class.searchMethods` compares the
        // query types against each candidate via `arrayContentsEq(null, p)`,
        // which is true iff `p` is empty. Previously a `None` array skipped
        // the arity/type check entirely and returned the FIRST same-named
        // method regardless of parameter count, so a no-arg
        // `getDeclaredMethod("writeReplace")` probe вЂ” exactly what
        // `java.io.ObjectStreamClass.getInheritableMethod` issues with
        // `argTypes == null` while resolving the serialization replacement
        // hook вЂ” wrongly matched a 1-arg `writeReplace(Object)` overload.
        // `ObjectStreamClass.invokeWriteReplace` then called it with 0 args
        // в†’ `Method.invoke` IllegalArgumentException (HIB-CV-36). Treat a
        // missing array as "expected zero parameters".
        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
        let expected_count = match param_types_arr {
            Some(pt_arr) => ctx.array_length(pt_arr),
            None => 0,
        };
        if param_descs.len() != expected_count {
            continue;
        }
        if let (Some(pt_arr), Some(pt_pin)) = (param_types_arr, pt_pin) {
            // Check each parameter type matches
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                // gen r4w3/rooting: re-read — the previous element's
                // `param_type_matches` may have collected.
                let pt_arr = ctx.read_native_pin(pt_pin, pt_arr);
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                if !param_type_matches(ctx, expected_mirror, pdesc, meta.declaring_class_id) {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }

        // Round 9 audit fix (HIGH #7): cache the cold-miss result so
        // subsequent identical probes short-circuit. Skip if the cache
        // key could not be built (a None mirror in the parameter array).
        if cache_key_ok {
            ctx.link_resolver_insert_method(
                class_id,
                &target_name,
                &cache_key_desc,
                class_id,
                meta_idx as u32,
            );
        }
        // gen r4w3/rooting: `this`/array unused past here.
        ctx.unpin_native_roots(this_pin);
        let method_obj = create_method_object(ctx, meta)?;
        return Ok(Some(Value::Object(Some(method_obj))));
    }
    // gen r4w3/rooting: the cold walk's `param_type_matches` calls may have
    // collected; take current addresses for the fallbacks below.
    let this = ctx.read_native_pin(this_pin, this);
    let param_types_arr = match (param_types_arr, pt_pin) {
        (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
        _ => None,
    };

    // WF6 вЂ” last-resort synthesis for jboss-modules / WildFly / Keycloak.
    //
    // The jboss-modules launcher resolves the module's main entry point as:
    //
    //     Class<?> mainClass = Class.forName("org.jboss.as.server.Main");
    //     Method m = mainClass.getDeclaredMethod("main", String[].class);
    //     m.invoke(null, (Object) args);
    //
    // For module jars we never actually load (`as-server`, Keycloak quarkus
    // run launchers, etc.) the `methods` table is empty and we would throw
    // `NoSuchMethodException`, which jboss-modules wraps and rethrows as a
    // fatal startup error. Earlier waves stuffed a synthetic class definition
    // in the loader; this didn't help because `getDeclaredMethod` walks the
    // class file's own method table вЂ” which is still empty for the synthetic
    // stub. Synthesise a no-op `main(String[])` Method *here* so the launcher
    // can invoke it (the invoke is intercepted natively elsewhere).
    if let Ok(Some(method_obj)) =
        wf_shim_synth_main_method(ctx, this, &target_name, param_types_arr)
    {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(Some(method_obj))));
    }

    let message = no_such_method_message(&*ctx, class_id, &target_name, param_types_arr);
    ctx.unpin_native_roots(this_pin);
    Err(cratonvm_types::error::RuntimeError::NoSuchMethodException { message }.into())
}

/// WF6 вЂ” short-circuit `Class.getDeclaredMethod` / `Class.getMethod` for the
/// jboss-modules / WildFly / Keycloak launcher pattern
/// `getDeclaredMethod("main", String[].class)`.
///
/// Only fires when ALL of the following hold (so the universal natives stay
/// universal for every other call site):
///   * the method name is exactly `"main"`,
///   * the parameter-types array is a single-element array, and
///   * the array's only element is `String[].class` (mirror class name
///     matches `[Ljava/lang/String;` / `java.lang.String[]`), and
///   * the class mirror's name contains one of the well-known jboss /
///     keycloak / wildfly entry-class fragments.
///
/// On a match we build a `MethodMetadata` for a public-static no-op
/// `main([Ljava/lang/String;)V` and run it through the existing
/// `create_method_object` so the Method mirror is layout-compatible with the
/// rest of the reflection machinery (parameterTypes, returnType,
/// exceptionTypes, annotation byte arrays, CratonVM extra slots).
pub fn wf_shim_synth_main_method(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    param_types_arr: Option<ObjectRef>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Real-bytecode audit: this WF6 reflective short-circuit is disabled
    // by default. It synthesized a fake `main(String[])` Method mirror
    // for any class whose name contains a WildFly / Keycloak fragment,
    // intercepting reflective lookup even when the real bytecode had
    // already been loaded. Re-enable via `CRATONVM_USE_WILDFLY_REFLECT_SHIM=1`
    // for boot-test (exit-rc-only) diagnostics.
    if !crate::nbflags().use_wildfly_reflect_shim {
        let _ = (this, name, param_types_arr);
        return Ok(None);
    }
    if name != "main" {
        return Ok(None);
    }
    let Some(pt_arr) = param_types_arr else {
        return Ok(None);
    };
    if ctx.array_length(pt_arr) != 1 {
        return Ok(None);
    }
    // Parameter element must be String[].class.
    let elem_mirror = match ctx.get_array_element(pt_arr, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(None),
    };
    let elem_name = mirror_class_name(ctx, elem_mirror).unwrap_or_default();
    // Accept JVM-internal form ("[Ljava/lang/String;") and dotted form
    // ("java.lang.String[]" / "[Ljava.lang.String;") that some mirror
    // helpers return.
    let is_string_array = elem_name == "[Ljava/lang/String;"
        || elem_name == "[Ljava.lang.String;"
        || elem_name == "java.lang.String[]"
        || elem_name == "java/lang/String[]";
    if !is_string_array {
        return Ok(None);
    }

    let class_name = mirror_class_name(ctx, this).unwrap_or_default();
    // Normalise вЂ” `mirror_class_name` may return either dotted or
    // slash-separated form depending on how the mirror was created. We
    // match on a fragment of either.
    let cn = class_name.replace('.', "/");
    let is_known_entry = cn.contains("jboss/as/server/Main")
        || cn.contains("jboss/as/Main")
        || cn.contains("jboss/modules/Main")
        || cn.contains("keycloak")
        || cn.contains("wildfly");
    if !is_known_entry {
        return Ok(None);
    }

    // Synthesise a public-static no-op `main([Ljava/lang/String;)V`. We
    // need a `declaring_class_id` for `create_method_object`: prefer the
    // mirror's real class id when available, otherwise fall back to id 0
    // (the `unwrap_or` is just defensive вЂ” by the time we get here the
    // class mirror has at least been allocated).
    let declaring_class_id = mirror_class_id(ctx, this).unwrap_or(ClassId::new(0));
    let meta = MethodMetadata {
        name: "main".to_string(),
        descriptor: "([Ljava/lang/String;)V".to_string(),
        access_flags: (ACC_PUBLIC | ACC_STATIC) as u16,
        declaring_class_id,
        exceptions: Vec::new(),
        signature: None,
    };
    tracing::warn!(
        target: "wf-shim",
        "synthesising no-op main(String[]) Method for {} (getDeclaredMethod / getMethod)",
        class_name
    );
    Ok(Some(create_method_object(ctx, &meta)?))
}

// ---------------------------------------------------------------------------
// java.lang.reflect.Constructor вЂ” object layout and natives
// ---------------------------------------------------------------------------

/// Stable metadata for `java.lang.reflect.Constructor` mirrors created by
/// `create_constructor_object`.
///
/// CratonVM stores the raw descriptor / parameter count / accessible flag in
/// heap slots *after* `class_num_total_fields(java/lang/reflect/Constructor)`.
/// When `ClassManager::upgrade_synthetic_class` replaces the stub with the real
/// JDK class, `num_total_fields` grows (Wave 3-B: `max(old, new)`), so the
/// absolute indices used at **allocation time** no longer match the indices
/// computed at **read time** вЂ” `read_constructor_descriptor` returns `None`,
/// `Constructor.newInstance` falls back to `()V`, and only `Object.<init>` runs
/// (Surefire: `JUnitPlatformProvider.launcher` stays null в†’
/// `TestPlanScannerFilter` NPE on `discover`).
///
/// Keyed by the mirror's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`)
/// since gc-common w29-e
/// (`common-w28b-remaining-identity-hash-keyed-side-tables`, rank 6). Before
/// that the key was `(vm_identity, identity hash)` (gc-common w21-d), and
/// before THAT the mirror's ADDRESS, which a moving collection handed to the
/// next mirror allocated there. The identity hash fixed the move but not a
/// live same-hash pair: every heap mints identity hashes from one 32-bit
/// counter that wraps, so two live `Constructor` mirrors can share one, and
/// then `newInstance` on B ran A's `<init>` (A's descriptor and parameter
/// count), or skipped B's access check on A's ACCESSIBLE flag. A weak lock key
/// names one live object of one VM, follows it across moves, and is freed --
/// never minted again -- by the first collection that finds the mirror dead:
/// [`forget_lang_class_keys`] then drops the row (`lib.rs::sweep_lock_keys`,
/// every collector), which replaces the w21-d address reconcile that rode
/// `classloader::gc_reconcile_defining_loaders`.
///
/// Registration MINTS the key; the read and write paths only look an existing
/// one up (`crate::existing_weak_lock_key`), so a mirror never registered
/// leaves no slot behind. Cost: one lock-key registry lookup per
/// `peek_constructor_mirror_side` (`Constructor.newInstance`,
/// `getParameterCount`, the accessibility check).
#[derive(Clone)]
pub struct ConstructorMirrorSideMeta {
    /// Raw `<init>` descriptor.  `Arc<str>` so cloning out of the table
    /// per `peek_constructor_mirror_side` does not allocate a fresh
    /// `String` heap buffer on every reflective Constructor read
    /// (hot path during JDK boot / Surefire fork).
    pub descriptor: Arc<str>,
    pub param_count: i32,
    pub accessible: bool,
    /// The VM that registered the row: VM teardown
    /// ([`forget_vm_lang_class_tables`]) drops it by VM as well as through
    /// its freed key.
    pub vm: usize,
}

/// The weak lock key of a `Constructor` mirror; see
/// [`ConstructorMirrorSideMeta`]. Unique per live object across every VM.
pub type ConstructorSideKey = usize;

// FxHashMap instead of std::HashMap: the key is one integer and needs
// no cryptographic strength. FxHash is a couple of multiply/xor ops.
pub fn constructor_mirror_side_table(
) -> &'static Mutex<FxHashMap<ConstructorSideKey, ConstructorMirrorSideMeta>> {
    static TABLE: OnceLock<Mutex<FxHashMap<ConstructorSideKey, ConstructorMirrorSideMeta>>> =
        OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// `obj` must be CURRENT (the key is minted at its address). The key is
/// computed before the table guard is taken. Answers whether the row was
/// filed (`false` only when no GC-stable key could be minted); wave 90's
/// `CRATONVM_REFLECT_CTOR_LAZY_DESC_STRING` builds the tail's descriptor
/// `String` only on `false`.
pub fn register_constructor_mirror_side(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    descriptor: &str,
    param_count: i32,
    accessible: bool,
) -> bool {
    let Ok(key) = crate::gc_stable_weak_lock_key(ctx, obj) else {
        return false;
    };
    let vm = ctx.vm_identity();
    let mut g = constructor_mirror_side_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    g.insert(
        key,
        ConstructorMirrorSideMeta {
            descriptor: Arc::from(descriptor),
            param_count,
            accessible,
            vm,
        },
    );
    true
}

