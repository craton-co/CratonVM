// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.1 — `java.lang.reflect` full coverage helpers and natives.
//!
//! This module sits next to `lang_class.rs` (which already hosts
//! Method/Field/Constructor natives in the historical layout) and adds the
//! missing JDK 25 reflection surface required by ByteBuddy / CGLIB / Jackson:
//!
//!   * `AccessibleObject.trySetAccessible()` — non-throwing variant of
//!     `setAccessible(true)`.
//!   * `AccessibleObject.canAccess(Object)` (typed for Method, Field,
//!     Constructor) — `null` for static members, otherwise an instance
//!     whose declaring class must be assignable from `obj.getClass()`.
//!   * `Class.getEnclosingClass()` — the immediate enclosing Class for a
//!     local/anonymous/member nested class, otherwise null. Reads
//!     `EnclosingMethod` then falls back to the InnerClasses table.
//!   * `Class.getNestHost()` (the public, no-`0` overload that the JDK
//!     uses for `Lookup.findVirtual` on private interfaces).
//!   * `Class.isHidden()` (no-`0` overload).
//!   * `Parameter.isImplicit()` / `isSynthetic()` — derived from the
//!     parameter access flags stored in slot 1.
//!   * `Executable.getParameters()` — populated from the cached parameter
//!     types and a synthesized `argN` name when MethodParameters attribute
//!     is absent.
//!   * `Method.getTypeAnnotationBytes0()` — return null (no type
//!     annotations tracked yet) — explicit no-op so callers don't NPE on
//!     the return-value handling.
//!
//! The new natives live here, not in `lang_class.rs`, so the WP2.1 surface
//! can be reviewed on its own and so the file-disjoint policy keeps Wave 2
//! owners from stomping on each other.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::lang_class::{
    annotation_element_to_java, box_value, create_constructor_object, create_field_object,
    create_method_object, descriptor_to_class_mirror, method_class_name_desc, method_clazz_value,
    method_descriptor_for_invoke, method_exception_types_value, method_modifiers_value,
    method_name_value, method_parameter_types_value, method_return_type_value, mirror_class_id,
    mirror_class_name, native_method_invoke, parse_descriptor_param_and_return,
    read_constructor_descriptor, read_field_meta, read_method_descriptor,
};
use crate::obj_arg;

use cratonvm_types::error::MethodCallFailed;
use std::sync::OnceLock;

/// Cached `CRATONVM_DBG_METHOD_INVOKE_BOX` lookup. `Method.invoke`'s
/// defensive-box wrap-up runs on every reflective call (and ByteBuddy /
/// CGLIB / Jackson hit this thousands of times during JDK boot). Reading
/// `env::var_os` per call goes through the platform environ lock —
/// avoid that by caching at first use. The env var is a debug switch;
/// setting it after the first reflective invoke has no effect (matches
/// the `vm::runtime::exceptions::iae_trace_enabled` convention).
static DBG_METHOD_INVOKE_BOX: OnceLock<bool> = OnceLock::new();

#[inline]
fn dbg_method_invoke_box_enabled() -> bool {
    *DBG_METHOD_INVOKE_BOX.get_or_init(|| crate::nbflags().dbg_method_invoke_box)
}

// ---------------------------------------------------------------------------
// JVM access flags used by Parameter.isImplicit / isSynthetic
// ---------------------------------------------------------------------------
//
// These bits live in `Parameter.modifiers` (we store the access flags in
// slot 1 of a synthetic Parameter). Per JVMS §4.7.24 (MethodParameters),
// `ACC_MANDATED` (0x8000) marks JLS-implicit parameters (e.g. the outer
// `this` synthesized for inner classes), and `ACC_SYNTHETIC` (0x1000) marks
// compiler-generated parameters not in the source.
const ACC_SYNTHETIC: i32 = 0x1000;
const ACC_MANDATED: i32 = 0x8000;

// ---------------------------------------------------------------------------
// AccessibleObject.trySetAccessible — JDK 9+ non-throwing setter.
// ---------------------------------------------------------------------------
//
// Per spec, returns true iff the override flag could be set (we always
// return true since CratonVM doesn't enforce module-level deep-reflection
// gates beyond what `check_reflection_module_access` already does), AND
// writes the flag through to the appropriate extra slot.

pub(crate) fn native_method_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `trySetAccessible` reports failure instead of throwing.
    if crate::lang_class::check_class_loader_define_class_is_encapsulated(ctx, this).is_err() {
        return Ok(Some(Value::Int(0)));
    }
    crate::lang_class::write_method_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_field_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `trySetAccessible` reports failure instead of throwing.
    if crate::lang_class::check_class_loader_define_class_is_encapsulated(ctx, this).is_err() {
        return Ok(Some(Value::Int(0)));
    }
    crate::lang_class::write_field_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_constructor_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `trySetAccessible` reports failure instead of throwing.
    if crate::lang_class::check_class_loader_define_class_is_encapsulated(ctx, this).is_err() {
        return Ok(Some(Value::Int(0)));
    }
    crate::lang_class::write_constructor_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

// AccessibleObject base — without typed receiver, fall back to setting
// whichever extra-slot offset is present.
pub(crate) fn native_accessible_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `trySetAccessible` reports failure instead of throwing.
    if crate::lang_class::check_class_loader_define_class_is_encapsulated(ctx, this).is_err() {
        return Ok(Some(Value::Int(0)));
    }
    // We don't know which subtype the receiver is — try all three writers.
    crate::lang_class::write_method_accessible_external(ctx, this, true);
    crate::lang_class::write_field_accessible_external(ctx, this, true);
    crate::lang_class::write_constructor_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_accessible_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let flag = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    if flag {
        if let Err(msg) =
            crate::lang_class::check_class_loader_define_class_is_encapsulated(ctx, this)
        {
            return Err(
                cratonvm_types::error::RuntimeError::InaccessibleObjectException { message: msg }
                    .into(),
            );
        }
    }
    crate::lang_class::write_method_accessible_external(ctx, this, flag);
    crate::lang_class::write_field_accessible_external(ctx, this, flag);
    crate::lang_class::write_constructor_accessible_external(ctx, this, flag);
    ctx.set_field_by_name(this, "override", Value::Int(if flag { 1 } else { 0 }));
    Ok(None)
}

// ---------------------------------------------------------------------------
// AccessibleObject.canAccess(Object) — JDK 9+ access check.
// ---------------------------------------------------------------------------
//
// Returns true iff the reflective member can be invoked / read with the
// given receiver.
//
// Two questions, and until 2026-08-06 only the first was asked. (1) Is the
// receiver the right SHAPE: null for a static member, an instance of the
// declaring class otherwise. (2) Is the member ACCESSIBLE from here at all:
// `setAccessible(true)` already granted, or the JLS 6.6.1 + JPMS check passes
// unaided. Skipping (2) made `canAccess` answer true for a private `java.base`
// field the caller could not read — `probes/SetAccessibleModuleProbe.java`'s
// `afterDenied isAccessible` line, where HotSpot 25 says false.
//
// Question (1) is not a `false`, though — it is an ARGUMENT error, and HotSpot
// raises it before question (2) is asked at all. Measured on Temurin 25.0.3
// (`probes/CanAccessReceiverProbe.java`, the full 28-row static x null x
// wrong-type x right-type matrix):
//
//   instance member, obj == null        -> IAE "null object for <member>"
//   instance member, not an instance    -> IAE "object is not an instance of <Class>"
//   static member,   obj != null        -> IAE "non-null object for <member>"
//   constructor,     obj != null        -> IAE "non-null object for <member>"
//
// and `Integer.value.canAccess("x")` THROWS rather than answering the `false`
// its access check would produce, which is what pins the ordering. All four
// answered a plain `false` here before this fix. `setAccessible(true)` does not
// suppress any of them: the argument is validated whether or not the override
// is set.

/// Does this member require a `null` receiver? Static members do, and so do
/// constructors — `Modifier.isStatic` is false for a constructor, but HotSpot
/// groups it with the static arm (measured: `ctor.canAccess(anInstance)` throws
/// `"non-null object for public Target()"`, it does not answer `false`).
fn receiver_must_be_null(is_static: bool, is_constructor: bool) -> bool {
    is_static || is_constructor
}

/// `member.toString()`, for the two messages that embed it.
///
/// Routed through the member's own `toString` rather than rebuilt from the
/// modifiers and descriptor: the JDK's message is literally `"null object for "
/// + member`, so reusing the same text keeps the two in step for free. Falls
/// back to the empty string if the call fails — a message that is missing its
/// tail is still the right exception, and inventing a DIFFERENT exception out
/// of a `toString` failure would be worse than the divergence being fixed.
fn member_display(ctx: &mut dyn NativeContext, member: ObjectRef) -> String {
    match ctx.invoke_virtual(member, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// HotSpot's receiver-argument validation, run BEFORE any access decision.
///
/// `Ok(())` means the receiver is the right shape and the access half may run.
fn check_can_access_receiver(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
    obj_arg: Value,
    must_be_null: bool,
) -> Result<(), MethodCallFailed> {
    if must_be_null {
        if matches!(obj_arg, Value::Object(None)) {
            return Ok(());
        }
        let shown = member_display(ctx, member);
        return Err(crate::lang_class::illegal_arg_exc(format!(
            "non-null object for {shown}"
        )));
    }

    let Value::Object(Some(receiver)) = obj_arg else {
        let shown = member_display(ctx, member);
        return Err(crate::lang_class::illegal_arg_exc(format!(
            "null object for {shown}"
        )));
    };

    let Value::Object(Some(declaring)) = method_clazz_value(ctx, member) else {
        // No declaring-class mirror to test against. Not knowable, so do not
        // manufacture an argument error out of it; the access half will fail
        // this member closed on its own.
        return Ok(());
    };
    let Some(declaring_id) = mirror_class_id(ctx, declaring) else {
        return Ok(());
    };
    let receiver_id = ctx.class_id_of_object(receiver);
    // `is_subclass` is full assignability — it walks interfaces as well as
    // superclasses, so a default method's declaring INTERFACE is matched by an
    // implementing receiver, and lambda proxies route through
    // `lambda_proxy_satisfies`. That is `Class.isInstance`, which is the test
    // HotSpot makes.
    if ctx.is_subclass(receiver_id, declaring_id) {
        return Ok(());
    }
    // A negative from `is_subclass` is only trustworthy when the receiver's
    // hierarchy is READABLE: `is_subclass_or_unreadable` returns false exactly
    // when the superclass chain terminated at a real `java/lang/Object` without
    // meeting the ancestor. Anything else is "cannot tell", and this throw is
    // new on a path that previously only ever returned `false` — a fabricated
    // stand-in with no modelled supertype chain must not be the thing that
    // invents an exception.
    if is_subclass_or_unreadable(ctx, receiver_id, declaring_id) {
        return Ok(());
    }
    let name = ctx
        .class_name_of_id(declaring_id)
        .map(|n| crate::lang_class::dotted_binary_name(&n))
        .unwrap_or_default();
    Err(crate::lang_class::illegal_arg_exc(format!(
        "object is not an instance of {name}"
    )))
}

fn can_access_member(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
    obj_arg: Value,
    is_static: bool,
    modifiers: i32,
) -> Result<bool, MethodCallFailed> {
    // Field and Method only — `Constructor.canAccess` has its own entry point,
    // because its rule is not derivable from `Modifier.isStatic`.
    check_can_access_receiver(
        ctx,
        member,
        obj_arg,
        receiver_must_be_null(is_static, false),
    )?;

    // Static member: receiver is null, validated just above.
    if is_static {
        let declaring_id = match method_clazz_value(ctx, member) {
            Value::Object(Some(m)) => mirror_class_id(ctx, m),
            _ => None,
        };
        let Some(declaring_id) = declaring_id else {
            return Ok(false);
        };
        // `None` receiver: a static member has no target type, exactly as
        // `Field.checkAccess` passes `null` for one. Measured on Temurin 25.0.3,
        // the protected STATIC `java.io.PipedInputStream.PIPE_SIZE` reads OK
        // through every receiver, so the refinement must not reach here.
        return Ok(member_is_accessible_here(
            ctx,
            member,
            declaring_id,
            modifiers,
            None,
        ));
    }

    // Instance member: the receiver is non-null and an instance of the
    // declaring class, both established by `check_can_access_receiver`.
    let receiver = match obj_arg {
        Value::Object(Some(r)) => r,
        _ => return Ok(false),
    };

    let declaring = match method_clazz_value(ctx, member) {
        Value::Object(Some(m)) => m,
        _ => return Ok(false),
    };
    let declaring_id = match mirror_class_id(ctx, declaring) {
        Some(id) => id,
        None => return Ok(false),
    };
    let receiver_id = ctx.class_id_of_object(receiver);
    // Carry the receiver's class on to the access half. `canAccess` must answer
    // the question `Field.get` will actually answer, and JLS §6.6.2.1 makes that
    // question receiver-dependent: measured on Temurin 25.0.3 from a classpath
    // subclass of `java.io.ByteArrayOutputStream`, `buf.canAccess` is `true` for
    // the caller's own instance and `false` for a bare superclass instance or a
    // sibling subclass — the same three answers `Field.get` gives. Passing the
    // ClassId rather than the ObjectRef keeps the whole subtree free of object
    // references a moving GC could invalidate.
    Ok(member_is_accessible_here(
        ctx,
        member,
        declaring_id,
        modifiers,
        Some(receiver_id),
    ))
}

/// The second half of `canAccess`: the override flag, else the unaided access
/// check. Split out so the static and instance arms cannot drift apart.
fn member_is_accessible_here(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
    declaring_id: cratonvm_types::ClassId,
    modifiers: i32,
    receiver: Option<ClassId>,
) -> bool {
    if crate::lang_class::accessible_override_is_set(ctx, member) {
        return true;
    }
    crate::lang_class::verify_member_access(ctx, declaring_id, modifiers, receiver)
}

pub(crate) fn native_method_can_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_static = (modifiers & 0x0008) != 0;
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    let ok = can_access_member(ctx, this, obj, is_static, modifiers)?;
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

pub(crate) fn native_field_can_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_static = (modifiers & 0x0008) != 0;
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    let ok = can_access_member(ctx, this, obj, is_static, modifiers)?;
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

pub(crate) fn native_constructor_can_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    // `Modifier.isStatic` is false for a constructor, but HotSpot still
    // requires a null receiver and raises `"non-null object for <ctor>"` for
    // anything else rather than answering `false` — measured, not assumed.
    check_can_access_receiver(ctx, this, obj, receiver_must_be_null(false, true))?;
    let declaring_id = match method_clazz_value(ctx, this) {
        Value::Object(Some(m)) => mirror_class_id(ctx, m),
        _ => None,
    };
    let ok = match declaring_id {
        // The receiver is required to be null (validated just above), so there
        // is no receiver whose type could narrow JLS 6.6.2.1's protected rule.
        // `None` is the only correct argument here, not a fallback.
        Some(id) => member_is_accessible_here(ctx, this, id, modifiers, None),
        None => false,
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Class.getEnclosingClass() — derived from EnclosingMethod or InnerClasses.
// ---------------------------------------------------------------------------
//
// Per JLS, `getEnclosingClass()` returns:
//   1. For local / anonymous classes: the class that contains the method or
//      constructor that defined this one (from `EnclosingMethod` attribute).
//   2. For member nested classes: the class whose body contains this one
//      (from the `InnerClasses` attribute, where `outer_class` is non-null).
//   3. Otherwise: null.
//
// Note: this is distinct from `getDeclaringClass` which returns null for
// anonymous and local classes, returning the enclosing class only for
// non-static member classes. `getEnclosingClass` returns the enclosing
// class for ALL nested classes.

/// Loader-scoped resolve of an enclosing/outer class NAME to a `ClassId`,
/// preferring the SAME defining loader as `class_id` before falling back to
/// the global (loader-blind) store.
///
/// hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md follow-up
/// (2026-07-06): `getEnclosingClass`'s two lookups (`EnclosingMethod` ->
/// enclosing class, `InnerClasses` -> outer class) previously went straight
/// to `ctx.class_id_by_name(name)` / `ctx.ensure_class_initialized(name)` --
/// the same loader-blind global lookup this doc's "Known remaining
/// limitation" section already names. That is silently wrong whenever 2+
/// DIFFERENT user-defined loaders each have their OWN class under the
/// referenced enclosing-class name -- the common case for Groovy, which
/// compiles every script under the identical top-level class name (e.g.
/// Spring's `GroovyBeanDefinitionReader` always evaluates its script as
/// `"beans"`, so `beans$_run_closure1`'s `EnclosingMethod` attribute always
/// names the enclosing class `"beans"`, and two sequential test methods
/// each running their own fresh `GroovyShell`/`GroovyClassLoader` produce
/// two DISTINCT `beans` classes sharing that name).
///
/// Once the stale-mirror `CRATONVM_LOADER_AWARE_RESOLUTION` gate fix
/// (2026-07-06) made the global lookup correctly refuse to guess between
/// 2+ same-named user-loader classes (returning `None`/ambiguous instead of
/// picking one), `getEnclosingClass()` on a Groovy closure started
/// returning `null` where it used to return SOME (possibly wrong) class --
/// surfacing downstream as `Closure.getThisType()`'s `GeneratedClosure.class
/// .isAssignableFrom(this.getClass().getEnclosingClass())` throwing a NullPointerException
/// (`Class.isAssignableFrom: argument is null`) once the loop's `aload_1`
/// went null. The global lookup finding "2+ different loaders, ambiguous" is
/// the CORRECT answer in the general case -- but `class_id` here already
/// carries the exact context needed to disambiguate faithfully: `this`
/// class's own defining loader is by construction the SAME loader that
/// compiled its enclosing/outer class, so probe that loader's exact
/// namespace FIRST (`class_id_defined_by_loader_exact`, no delegation
/// fallback -- same mechanism `findLoadedClass` already uses faithfully)
/// before falling through to the pre-existing global-store attempt. Built-in
/// loaders (`loader_id_of_class` 0/1/2) skip this probe entirely and keep
/// the exact prior behavior -- this only changes the answer for a
/// user-defined-loader class whose enclosing/outer class the SAME loader
/// has ALSO defined, which is the only case that was ever ambiguous.
fn loader_scoped_enclosing_lookup(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    let loader_id = ctx.loader_id_of_class(class_id);
    if loader_id > 2 {
        if let Some(id) = ctx.class_id_defined_by_loader_exact(name, loader_id as u32) {
            return Some(id);
        }
    }
    ctx.class_id_by_name(name)
        .or_else(|| ctx.ensure_class_initialized(name).ok())
}

pub(crate) fn native_class_get_enclosing_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // First check EnclosingMethod attribute (local + anonymous classes).
    // The enclosing class may not be loaded yet (e.g. when reflection touches
    // a nested class without the enclosing one being explicitly referenced),
    // so fall back to `ensure_class_initialized` if the cached lookup misses.
    if let Some((enc_class, _name, _desc)) = ctx.enclosing_method(class_id) {
        if let Some(enc_id) = loader_scoped_enclosing_lookup(ctx, class_id, &enc_class) {
            let mirror = ctx.get_class_mirror(enc_id);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // Then fall back to InnerClasses (member nested classes — including
    // static-nested classes per JLS §8.1.3 / §15.8.5).
    //
    // Walk the InnerClasses table directly so we accept any entry whose
    // `inner_class` matches this class and whose `outer_class` is non-empty
    // (i.e. the class is a member of another class). The simple-name slot
    // (`inner_name`) is empty only for anonymous classes — and anonymous
    // classes always carry an EnclosingMethod attribute that we already
    // handled above, so `outer_class != ""` is the right condition here.
    let this_name = ctx.class_name_of_id(class_id);
    if let Some(this_name) = this_name {
        let entries = ctx.inner_classes(class_id);
        for (inner, outer, _inner_name, _flags) in &entries {
            if inner == &this_name && !outer.is_empty() {
                if let Some(outer_id) = loader_scoped_enclosing_lookup(ctx, class_id, outer) {
                    let mirror = ctx.get_class_mirror(outer_id);
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
    }

    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Parameter.isImplicit / isSynthetic — derived from access flags.
// ---------------------------------------------------------------------------

pub(crate) fn native_parameter_is_implicit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let implicit = (modifiers & ACC_MANDATED) != 0;
    Ok(Some(Value::Int(if implicit { 1 } else { 0 })))
}

pub(crate) fn native_parameter_is_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let synthetic = (modifiers & ACC_SYNTHETIC) != 0;
    Ok(Some(Value::Int(if synthetic { 1 } else { 0 })))
}

/// `java.lang.reflect.Parameter.isNamePresent()`.
///
/// SB-02b-#4: `build_parameter_array` synthesizes `Parameter` objects directly
/// and never runs the real-JDK `Executable.privateGetParameters()` that sets
/// `hasRealParameterData`. The real `isNamePresent()` bytecode reads that flag
/// (`executable.hasRealParameterData() && name != null`) and so would always
/// return `false` — which makes Spring's `StandardReflectionParameterNameDiscoverer`
/// return `null` for the WHOLE executable the moment ANY parameter reports no
/// name (e.g. the enum constructor's `$enum$name` / `$enum$ordinal`, whose names
/// ARE present from the `MethodParameters` attribute). A name is "present" iff
/// it is not the synthetic `arg<index>` placeholder that `build_parameter_array`
/// falls back to when `MethodParameters` is absent. (`Parameter` is a concrete
/// class, so this same-class native wins over the JDK bytecode — unlike the
/// inherited-default-method case in #1.)
///
/// getNestMembers0-sibling regression: `build_parameter_array` writes the
/// name to the real `name` FIELD BY NAME whenever that field genuinely
/// exists on the loaded `Parameter` class (`by_name_landed`), and only
/// falls back to writing synthetic slot 0 when it doesn't. Reading slot 0
/// FIRST — as this used to — is backwards for the (common, real-JDK) case:
/// slot 0 was never written on that path, so it holds whatever the
/// allocator happened to leave there (zeroed on some runs, stale/reused
/// object-header bytes on others), not reliably the name. That produced a
/// real, intermittent misclassification (e.g. leftover bytes resembling an
/// `argN` placeholder) which silently poisoned Spring's whole-method name
/// discovery — see the Spring Boot WebSocket `subProtocolWebSocketHandler`
/// multi-candidate-autowire regression this was found from. Try the
/// by-name field first (matching the writer's own priority); fall back to
/// slot 0 only when the class has no such declared field (pure synthetic
/// layout).
pub(crate) fn native_parameter_is_name_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
    .unwrap_or_default();
    if name.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    // `arg<digits>` is the JDK's synthesized placeholder → name NOT present.
    let synthesized = name
        .strip_prefix("arg")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
    Ok(Some(Value::Int(if synthesized { 0 } else { 1 })))
}

// ---------------------------------------------------------------------------
// Executable.getParameters() — synthesize Parameter[] from descriptor.
// ---------------------------------------------------------------------------
//
// When the MethodParameters attribute is absent (the common case for code
// not compiled with `-parameters`), the JDK falls back to synthesizing
// `arg0`, `arg1`, … names with modifiers=0. We follow the same convention.
//
// Each Parameter object has the following synthetic 4-field layout:
//   slot 0: name        (String)
//   slot 1: modifiers   (Int)
//   slot 2: type        (Class) — the parameter's static type mirror
//   slot 3: declaringExecutable (Object) — the source Method/Constructor

const PARAMETER_NUM_FIELDS: usize = 4;

/// Resolve the (declaring class id, method name) of a Method/Constructor
/// reflection object so we can look up its `MethodParameters` attribute.
///
/// Returns `None` if the receiver is missing the standard `clazz` / `name`
/// fields (which would only happen for a partially-initialized synthetic
/// reflection object — see `create_method_object` for the layout).
fn executable_class_and_name(
    ctx: &dyn NativeContext,
    executable: ObjectRef,
) -> Option<(ClassId, String)> {
    let mirror = match ctx.get_field_by_name(executable, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let class_id = mirror_class_id(ctx, mirror)?;
    let name = match ctx.get_field_by_name(executable, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        // Constructor reflection objects use "<init>" implicitly — the JDK
        // doesn't store a name field on Constructor. Fall back to that.
        _ => "<init>".to_string(),
    };
    Some((class_id, name))
}

fn build_parameter_array(
    ctx: &mut dyn NativeContext,
    declaring_executable: ObjectRef,
    descriptor: &str,
) -> ObjectRef {
    let (param_descs, _) = parse_descriptor_param_and_return(descriptor);
    let arr = ctx.new_ref_array(ClassId::new(0), param_descs.len());

    let parameter_class_id = ctx
        .ensure_class_initialized("java/lang/reflect/Parameter")
        .unwrap_or(ClassId::new(0));

    // WP2.1 — try to read the per-parameter names + access flags from the
    // class-file `MethodParameters` attribute (JVMS 4.7.24). This is what
    // javac emits when invoked with `-parameters`. If absent (or if any
    // entry has `name_index == 0`), fall back to the synthetic `argN`
    // placeholder so the JDK's documented behavior is preserved.
    let parameter_meta: Vec<(String, u16)> =
        match executable_class_and_name(ctx, declaring_executable) {
            Some((class_id, method_name)) => {
                ctx.method_parameters(class_id, &method_name, descriptor)
            }
            None => Vec::new(),
        };

    // Size to the REAL `java.lang.reflect.Parameter` layout when loaded —
    // its bytecode getters write cache fields (`parameterClassCache`,
    // `parameterTypeCache`) that live beyond the 4 declared-value slots,
    // so a 4-slot alloc would be an undersized object.
    let alloc_fields = core::cmp::max(
        PARAMETER_NUM_FIELDS,
        ctx.class_num_total_fields(parameter_class_id),
    );

    // GC-safety: `arr` and `declaring_executable` are held across the whole
    // loop below, which repeatedly allocates/classloads once per parameter
    // (`alloc_object`, `create_string`, `descriptor_to_class_mirror` --
    // all GC-triggering). Pin both for the loop's duration; each iteration's
    // own `p`/`name` locals additionally need pinning across that same
    // iteration's hazards before their use further down.
    let arr_pin = ctx.pin_native_root(arr);
    let declaring_executable_pin = ctx.pin_native_root(declaring_executable);

    for (i, pdesc) in param_descs.iter().enumerate() {
        let p = ctx.alloc_object(parameter_class_id, alloc_fields);
        let p_pin = ctx.pin_native_root(p);

        let (name_str, modifiers) = match parameter_meta.get(i) {
            Some((n, flags)) if !n.is_empty() => (n.clone(), *flags as i32),
            Some((_, flags)) => (format!("arg{i}"), *flags as i32),
            None => (format!("arg{i}"), 0),
        };
        let name = ctx.create_string(&name_str);
        let name_pin = ctx.pin_native_root(name);
        let type_mirror = descriptor_to_class_mirror(ctx, pdesc);

        let p = ctx.read_native_pin(p_pin, p);
        let name = ctx.read_native_pin(name_pin, name);
        ctx.unpin_native_roots(p_pin);
        let declaring_executable =
            ctx.read_native_pin(declaring_executable_pin, declaring_executable);

        // Real-JDK Parameter layout (name, modifiers, executable, index) —
        // write by field name so the REAL Parameter bytecode works:
        // `Parameter.getType()` is `executable.getSharedParameterTypes()[index]`.
        // The old slot writes put the TYPE mirror where the real layout
        // keeps `executable`, so getType() dispatched
        // getSharedParameterTypes on a Class mirror → NoSuchMethodError
        // (every JUnit5 @ParameterizedTest argument resolution died there).
        ctx.set_field_by_name(p, "name", Value::Object(Some(name)));
        ctx.set_field_by_name(p, "modifiers", Value::Int(modifiers));
        ctx.set_field_by_name(p, "executable", Value::Object(Some(declaring_executable)));
        ctx.set_field_by_name(p, "index", Value::Int(i as i32));
        // Synthetic fallback — Parameter class has no named fields here
        // (set_field_by_name no-ops): keep the legacy slot layout
        // [0]=name, [1]=modifiers, [2]=type mirror, [3]=executable that the
        // synthetic Parameter natives read.
        let by_name_landed = matches!(
            ctx.get_field_by_name(p, "executable"),
            Value::Object(Some(e)) if e == declaring_executable
        );
        if !by_name_landed {
            ctx.set_field(p, 0, Value::Object(Some(name)));
            ctx.set_field(p, 1, Value::Int(modifiers));
            ctx.set_field(p, 2, Value::Object(Some(type_mirror)));
            ctx.set_field(p, 3, Value::Object(Some(declaring_executable)));
        }

        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(p)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    arr
}

pub(crate) fn native_method_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = read_method_descriptor(ctx, this).unwrap_or_default();
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_constructor_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = read_constructor_descriptor(ctx, this).unwrap_or_default();
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_executable_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Try Method-style descriptor first, then Constructor-style.
    let descriptor = read_method_descriptor(ctx, this)
        .or_else(|| read_constructor_descriptor(ctx, this))
        .unwrap_or_default();
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// Class.getEnclosingMethod() — Java-level wrapper that returns a Method.
// ---------------------------------------------------------------------------
//
// This is the public counterpart to `getEnclosingMethod0()` (which returns
// a 3-element Object[]). The Java code in `Class.getEnclosingMethod()`
// reconstructs a Method from the Object[] but it requires the enclosing
// class to expose a matching `getDeclaredMethod`, which doesn't always
// work in our reflection layer for synthetic classes. This native does
// the reconstruction directly.

pub(crate) fn native_class_get_enclosing_method_public(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let (enc_class_name, method_name, method_desc) = match ctx.enclosing_method(class_id) {
        Some(t) => t,
        None => return Ok(Some(Value::Object(None))),
    };
    if method_name.is_empty() {
        // Class is enclosed by an initializer (not a method).
        return Ok(Some(Value::Object(None)));
    }

    let enc_class_id = match ctx
        .class_id_by_name(&enc_class_name)
        .or_else(|| ctx.ensure_class_initialized(&enc_class_name).ok())
    {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // Find the matching MethodMetadata
    let methods = ctx.declared_methods(enc_class_id);
    for meta in &methods {
        if &*meta.name == method_name && &*meta.descriptor == method_desc {
            let m = create_method_object(ctx, meta)?;
            return Ok(Some(Value::Object(Some(m))));
        }
    }

    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Class.getEnclosingConstructor() — same pattern as enclosing method.
// ---------------------------------------------------------------------------

pub(crate) fn native_class_get_enclosing_constructor_public(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    let (enc_class_name, method_name, method_desc) = match ctx.enclosing_method(class_id) {
        Some(t) => t,
        None => return Ok(Some(Value::Object(None))),
    };
    if method_name != "<init>" {
        return Ok(Some(Value::Object(None)));
    }

    let enc_class_id = match ctx
        .class_id_by_name(&enc_class_name)
        .or_else(|| ctx.ensure_class_initialized(&enc_class_name).ok())
    {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };

    // Find the matching MethodMetadata
    let methods = ctx.declared_methods(enc_class_id);
    for meta in &methods {
        if &*meta.name == "<init>" && &*meta.descriptor == method_desc {
            let c = create_constructor_object(ctx, meta)?;
            return Ok(Some(Value::Object(Some(c))));
        }
    }

    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Method.getDefaultValue() — for annotation methods.
// ---------------------------------------------------------------------------
//
// Returns null for non-annotation methods or annotation methods without an
// explicit default. For annotation-element methods that DO declare a default
// (`String[] basePackages() default {}`, `int max() default 5`, ...), we
// parse the JVMS §4.7.22 `AnnotationDefault` attribute via
// `NativeContext::method_annotation_default` and convert it to a Java Value
// using the same `annotation_element_to_java` path that populates default
// element values inside annotation proxies.
//
// This is required for Spring's `AttributeMethods` constructor (which sets
// `hasDefaultValueMethod` based on `m.getDefaultValue() != null`) and for
// `AnnotationUtils.AliasDescriptor.validateDefaultValueConfiguration` (which
// reads each element's default to verify aliased attributes share the same
// default — null means "no default declared", which the validator rejects
// for `@AliasFor`-targeted attributes).

pub(crate) fn native_method_get_default_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (class_id, name, desc) = match method_class_name_desc(ctx, this) {
        Some(t) => t,
        None => return Ok(Some(Value::Object(None))),
    };
    let default = match ctx.method_annotation_default(class_id, &name, &desc) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    // S111r19 — pass the annotation method's return-type descriptor so
    // empty arrays carry the correct component class (avoids the
    // `Object[]`-as-`Annotation[]` aliasing in Spring's `adaptValue`).
    let ret_desc = desc.strip_prefix("()").map(|s| s.to_string());
    // Annotation defaults containing `Class` values are resolved by the JDK
    // in the declaring annotation interface's defining loader. In particular,
    // Spring compares an explicit `@Reflective(value = X.class)` value with
    // the `processors()` default through identity-sensitive Class equality;
    // resolving this default globally made the two otherwise identical
    // `SimpleReflectiveProcessor.class` mirrors come from different forks.
    let container_loader =
        crate::classloader::defining_loader_for(ctx.vm_identity(), class_id.as_u32());
    Ok(Some(crate::lang_class::annotation_element_to_java_typed(
        ctx,
        &default,
        ret_desc.as_deref(),
        Some(class_id),
        container_loader,
        Some(class_id),
    )?))
}

// ---------------------------------------------------------------------------
// Method.isVarArgs / isBridge / isDefault / isSynthetic
// ---------------------------------------------------------------------------

pub(crate) fn native_method_is_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_varargs = (modifiers & 0x0080) != 0; // ACC_VARARGS
    Ok(Some(Value::Int(if is_varargs { 1 } else { 0 })))
}

pub(crate) fn native_method_is_bridge(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_bridge = (modifiers & 0x0040) != 0; // ACC_BRIDGE
    Ok(Some(Value::Int(if is_bridge { 1 } else { 0 })))
}

pub(crate) fn native_method_is_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_synth = (modifiers & ACC_SYNTHETIC) != 0;
    Ok(Some(Value::Int(if is_synth { 1 } else { 0 })))
}

pub(crate) fn native_method_is_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A default method is a non-abstract, non-static method declared in an
    // interface. We need: declaring class is interface AND method is not
    // abstract AND not static.
    let this = obj_arg(args, 0)?;
    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_abstract = (modifiers & 0x0400) != 0;
    let is_static = (modifiers & 0x0008) != 0;
    if is_abstract || is_static {
        return Ok(Some(Value::Int(0)));
    }

    let declaring = match method_clazz_value(ctx, this) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Int(0))),
    };
    let declaring_id = match mirror_class_id(ctx, declaring) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let is_interface = ctx.is_interface_class(declaring_id);
    Ok(Some(Value::Int(if is_interface { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Method.getExceptionTypes / getGenericExceptionTypes / toString
// ---------------------------------------------------------------------------
//
// `getExceptionTypes` simply returns the `exceptionTypes` field that
// `create_method_object` populates from the JVMS §4.7.5 `Exceptions`
// attribute. We register a native (rather than relying on the JDK Java
// implementation `return exceptionTypes.clone();`) so frameworks that
// dispatch via the VM's stackless-invoke fast path get a deterministic
// answer that does NOT depend on the JDK's `Method` constructor having
// been driven through the proxy code path.
//
// `getGenericExceptionTypes` is best-effort: when the method has a
// generic Signature attribute with a `^` (throws) clause, we map each
// throws-type via `generics::type_sig_to_java`. Otherwise we fall back
// to the raw `exceptionTypes` array (same as
// `getExceptionTypes`) — which is what the JDK does when no Signature
// attribute is present (`AbstractExecutable.getGenericInfo()` early-
// returns and the Java code returns the raw types).

pub(crate) fn native_method_get_exception_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // The field is always non-null (set in create_method_object). Return
    // it directly — `getExceptionTypes()` semantically returns a fresh
    // clone, but reflection callers don't mutate the array, so handing
    // back the cached reference is safe and matches what we do for
    // `getParameterTypes` / `getReturnType`.
    Ok(Some(method_exception_types_value(ctx, this)))
}

/// Constructor.getExceptionTypes — mirrors the Method native. CGLib's
/// Enhancer.emitConstructors path (ReflectUtils.getExceptionTypes,
/// Constructor.getExceptionTypes:293) NPEs if the `exceptionTypes` field
/// is null. We populate the field in `create_constructor_object` and
/// hand the cached array back here. If the field is somehow null
/// (defensive — e.g. constructor objects synthesized via a path that
/// bypasses `create_constructor_object`), return an empty Class[] so
/// callers do not crash.
pub(crate) fn native_constructor_get_exception_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match ctx.get_field_by_name(this, "exceptionTypes") {
        v @ Value::Object(Some(_)) => Ok(Some(v)),
        _ => {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        }
    }
}

pub(crate) fn native_method_get_generic_exception_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mirror = match method_clazz_value(ctx, this) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(method_exception_types_value(ctx, this))),
    };
    let class_id = match mirror_class_id(ctx, mirror) {
        Some(id) => id,
        None => return Ok(Some(method_exception_types_value(ctx, this))),
    };
    let method_name = match method_name_value(ctx, this) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(Some(method_exception_types_value(ctx, this))),
    };
    let descriptor = match read_method_descriptor(ctx, this) {
        Some(d) => d,
        None => return Ok(Some(method_exception_types_value(ctx, this))),
    };

    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &descriptor) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            if !method_sig.throws.is_empty() {
                let decl = if method_sig.type_params.is_empty() {
                    Value::Object(Some(ctx.get_class_mirror(class_id)))
                } else {
                    Value::Object(Some(this))
                };
                let arr = ctx.new_ref_array(ClassId::new(0), method_sig.throws.len());
                // GC-safety: `type_sig_to_java` per iteration can trigger
                // classloading (GC); `arr` and the ObjectRef inside `decl`
                // are both reused across iterations, unpinned otherwise.
                let arr_pin = ctx.pin_native_root(arr);
                let decl_pin = match decl {
                    Value::Object(Some(d)) => Some(ctx.pin_native_root(d)),
                    _ => None,
                };
                for (i, t) in method_sig.throws.iter().enumerate() {
                    let decl = match (decl_pin, decl) {
                        (Some(pin), Value::Object(Some(d))) => {
                            Value::Object(Some(ctx.read_native_pin(pin, d)))
                        }
                        _ => decl,
                    };
                    let _gscope = crate::generics::GenericDeclScope::new(decl);
                    let v = crate::generics::type_sig_to_java(ctx, t);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, v?);
                }
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
    }
    // No Signature attribute, or no `^Type` throws section in it:
    // pin to the raw `exceptionTypes` array. WP2.8 will revisit deep
    // generic Type proxy support; today's surface is "raw Class for
    // every throws-type".
    Ok(Some(method_exception_types_value(ctx, this)))
}

/// Method.toString() — builds the canonical JDK string:
///   `<modifiers> <returnType> <declaringClass>.<name>(<paramTypes>) [throws ...]`
///
/// This is registered as a native to bypass the JDK's reliance on
/// `Modifier.toString` and `Type.getTypeName` chains that may not be
/// fully reachable in synthetic-jdk mode. The format matches OpenJDK's
/// `Method.sharedToString` so callers that grep for "void com.foo.Bar.baz()"
/// patterns (test-runner output, log lines) match.
pub(crate) fn native_method_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    let modifiers = match method_modifiers_value(ctx, this) {
        Value::Int(v) => v,
        _ => 0,
    };

    let mut s = String::new();
    // Build modifier prefix in JLS order. Method.toString uses the
    // method-level mask (no ACC_VARARGS / ACC_BRIDGE / ACC_SYNTHETIC).
    if (modifiers & 0x0001) != 0 {
        s.push_str("public ");
    }
    if (modifiers & 0x0002) != 0 {
        s.push_str("private ");
    }
    if (modifiers & 0x0004) != 0 {
        s.push_str("protected ");
    }
    if (modifiers & 0x0008) != 0 {
        s.push_str("static ");
    }
    if (modifiers & 0x0010) != 0 {
        s.push_str("final ");
    }
    if (modifiers & 0x0020) != 0 {
        s.push_str("synchronized ");
    }
    if (modifiers & 0x0100) != 0 {
        s.push_str("native ");
    }
    if (modifiers & 0x0400) != 0 {
        s.push_str("abstract ");
    }
    if (modifiers & 0x0800) != 0 {
        s.push_str("strictfp ");
    }

    // Return type — `getTypeName()` style: dotted class name, primitive
    // bare names ("int"), array suffix `[]`.
    let ret_mirror = match method_return_type_value(ctx, this) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let ret_name = ret_mirror
        .and_then(|m| mirror_class_name(ctx, m))
        .unwrap_or_else(|| "void".to_string());
    s.push_str(&class_name_to_type_name(&ret_name));
    s.push(' ');

    // Declaring class
    let decl_mirror = match method_clazz_value(ctx, this) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let decl_name = decl_mirror
        .and_then(|m| mirror_class_name(ctx, m))
        .unwrap_or_default();
    s.push_str(&decl_name.replace('/', "."));
    s.push('.');

    // Method name
    let name = match method_name_value(ctx, this) {
        Value::Object(Some(sref)) => ctx.read_string(sref).unwrap_or_default(),
        _ => String::new(),
    };
    s.push_str(&name);

    // Parameter types
    s.push('(');
    if let Value::Object(Some(arr)) = method_parameter_types_value(ctx, this) {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if i > 0 {
                s.push(',');
            }
            if let Value::Object(Some(pm)) = ctx.get_array_element(arr, i) {
                let pn = mirror_class_name(ctx, pm).unwrap_or_default();
                s.push_str(&class_name_to_type_name(&pn));
            }
        }
    }
    s.push(')');

    // Exception types (throws ...)
    if let Value::Object(Some(arr)) = method_exception_types_value(ctx, this) {
        let len = ctx.array_length(arr);
        if len > 0 {
            s.push_str(" throws ");
            for i in 0..len {
                if i > 0 {
                    s.push(',');
                }
                if let Value::Object(Some(em)) = ctx.get_array_element(arr, i) {
                    let en = mirror_class_name(ctx, em).unwrap_or_default();
                    s.push_str(&en.replace('/', "."));
                }
            }
        }
    }

    let out = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(out))))
}

/// Convert an internal class name (slash-delimited binary form) into the
/// `java.lang.Class.getTypeName()` representation that `Method.toString`
/// uses: dotted package + simple name, with array dimensions rendered
/// as `[]` suffixes, primitives kept bare ("int", "boolean", ...).
fn class_name_to_type_name(name: &str) -> String {
    if name.is_empty() {
        return name.to_string();
    }
    // Array form: leading '[' chars + element descriptor.
    if let Some(stripped) = name.strip_prefix('[') {
        let mut dims = 1;
        let mut rest = stripped;
        while let Some(s) = rest.strip_prefix('[') {
            dims += 1;
            rest = s;
        }
        let elem = match rest.chars().next() {
            Some('B') => "byte".to_string(),
            Some('C') => "char".to_string(),
            Some('D') => "double".to_string(),
            Some('F') => "float".to_string(),
            Some('I') => "int".to_string(),
            Some('J') => "long".to_string(),
            Some('S') => "short".to_string(),
            Some('Z') => "boolean".to_string(),
            Some('L') => {
                // Lcom/foo/Bar;
                let inner = &rest[1..rest.len().saturating_sub(1)];
                inner.replace('/', ".")
            }
            _ => rest.to_string(),
        };
        let mut out = elem;
        for _ in 0..dims {
            out.push_str("[]");
        }
        return out;
    }
    // Primitives (rare here — mirror_class_name returns dotted form
    // for primitives, but tolerate either).
    match name {
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void" => {
            return name.to_string();
        }
        _ => {}
    }
    name.replace('/', ".")
}

// ---------------------------------------------------------------------------
// Field.isSynthetic / isEnumConstant
// ---------------------------------------------------------------------------

pub(crate) fn native_field_is_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_synth = (modifiers & ACC_SYNTHETIC) != 0;
    Ok(Some(Value::Int(if is_synth { 1 } else { 0 })))
}

pub(crate) fn native_field_is_enum_constant(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    // ACC_ENUM = 0x4000
    let is_enum = (modifiers & 0x4000) != 0;
    Ok(Some(Value::Int(if is_enum { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Constructor.isSynthetic / isVarArgs
// ---------------------------------------------------------------------------

pub(crate) fn native_constructor_is_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_synth = (modifiers & ACC_SYNTHETIC) != 0;
    Ok(Some(Value::Int(if is_synth { 1 } else { 0 })))
}

pub(crate) fn native_constructor_is_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_varargs = (modifiers & 0x0080) != 0; // ACC_VARARGS
    Ok(Some(Value::Int(if is_varargs { 1 } else { 0 })))
}

pub(crate) fn native_constructor_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Constructor.getName() returns the declaring class's binary name.
    let this = obj_arg(args, 0)?;
    let declaring = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, declaring).unwrap_or_default();
    let dotted = name.replace('/', ".");
    let s = ctx.create_string(&dotted);
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// JLS §6.6.1 caller-sensitive access for reflective member use.
// ---------------------------------------------------------------------------
//
// `Method.invoke` / `Field.get|set` / `Constructor.newInstance` do NOT require
// `setAccessible(true)` merely because the member is non-public. The real JDK
// routes each of them through `Reflection.verifyMemberAccess(caller,
// declaringClass, obj, modifiers)` — the ordinary JLS §6.6.1 rules evaluated
// against the *caller* class:
//
//   * `private`       → the declaring class itself, or a **nestmate**
//                       (JEP 181, confirmed per JVMS §5.4.4).
//   * package-private → a caller in the same runtime package (same package
//                       name AND same defining loader).
//   * `protected`     → same runtime package, or a subclass of the declaring
//                       class (JLS §6.6.2).
//
// CratonVM's method path had no caller step at all: `lang_class::check_access`
// answers "public, or `setAccessible(true)`, else deny". That rejects the very
// common nestmate case — an enclosing class reflectively invoking a private
// method of its own nested class — which HotSpot 25 accepts.
// `regression-suite/src/RJdkReflect.java:160` is exactly that call, and it
// failed identically in `--real-jdk` and `--jdk-only`.
//
// `lang_class::check_field_access` already grew the same-class half of this for
// fields (HikariConfig's private-final `AtomicReference`); the helper below is
// the complete, member-shaped twin. It is deliberately **pure widening**: it is
// only ever consulted after the old rule has already said "not public and not
// overridden", and every answer it gives is an `allow`. Nothing the old blanket
// rule accepted is now rejected.

/// Member access-flag bits (JVMS §4.6 `method_info.access_flags`). Named apart
/// from the `ACC_SYNTHETIC` / `ACC_MANDATED` parameter-flag constants above so
/// the two flag namespaces cannot be confused at a call site.
const ACC_PUBLIC_MEMBER: i32 = 0x0001;
const ACC_PRIVATE_MEMBER: i32 = 0x0002;
const ACC_PROTECTED_MEMBER: i32 = 0x0004;

/// Upper bound on the superclass walk in [`caller_is_subclass_of`]. A well
/// formed hierarchy is far shallower; the bound exists so a corrupted or
/// cyclic `superclass_of` chain degrades to "deny" instead of hanging the
/// reflective call.
const MAX_SUPERCLASS_WALK: usize = 128;

/// The runtime package of `class_id`: `(package name, defining loader id)`.
/// JLS §6.6.1 package access is *runtime* package access — two classes named
/// `com.foo.Bar` defined by different loaders are NOT in the same package.
fn runtime_package_of(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<(String, i32)> {
    let name = ctx.class_name_of_id(class_id)?;
    let pkg = match name.rfind('/') {
        Some(i) => name[..i].to_string(),
        // Default package (the regression-suite classes live here).
        None => String::new(),
    };
    Some((pkg, ctx.loader_id_of_class(class_id)))
}

/// Resolve the *confirmed* nest host name of `class_id`, mirroring
/// `classloading::access_control::confirmed_nest_host`.
///
/// A `NestHost` attribute is only a *claim*. JVMS §5.4.4 requires the claimed
/// host to list the claimant back in its `NestMembers` before the claim grants
/// anything — otherwise any class could name a victim as its host and read the
/// victim's privates. When the claim cannot be confirmed the class is treated
/// as its own nest host, so the spoof simply fails to match.
///
/// The host is resolved with `class_id_by_name_near(.., class_id)` rather than
/// the ambient `class_id_by_name` so a duplicate binary name defined by another
/// loader cannot be substituted for the real host.
///
/// # The hidden-class arm
///
/// It mirrors `classloading::access_control::confirmed_nest_host`'s arm, and
/// for that function's stated reason: a JEP 371 hidden class's `nest_host` is
/// **not** read from its class file at all — `define_class_with_options`
/// overwrites whatever the bytes claimed with the defining `Lookup`'s own
/// class, and no `NestMembers` round-trip is possible because the host cannot
/// name a class whose name no class file can spell. The claim is authoritative
/// because only the defining call could have made it.
///
/// L15 recorded this arm as missing and unfixable, on the grounds that
/// `NativeContext` had no hidden-class question to ask. **That was a grep for
/// the wrong name.** There is no `is_hidden_class`, but there is
/// `NativeContext::is_class_hidden` (`native-api/src/registry.rs`), backed by a
/// real `ClassManager` read in `vm/src/vm/vm_exec.rs` and by the mock's
/// `hidden_classes` set — not a defaulted `false`, so this arm is exercised
/// rather than inert. Without it a hidden class always resolved to itself as
/// host and never matched a nestmate, so `defineHiddenClass(.., NESTMATE)` and
/// lambda-proxy classes were denied reflective access their bytecode already
/// has. That failed CLOSED, which is why it was a divergence and not a hole;
/// closing it only ever admits, and it cannot admit anything
/// `access_control.rs` does not already admit at the bytecode level.
///
/// The W3-2 complement holds here unchanged: a hidden class defined WITHOUT
/// `ClassOption::NESTMATE` has its class-file `NestHost` discarded at
/// definition time, so it reaches the self-host arm above and this one never
/// sees it.
fn confirmed_nest_host_name(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<String> {
    let own = ctx.class_name_of_id(class_id)?;
    let claimed = match ctx.nest_host_name(class_id) {
        // No `NestHost` attribute (or a self-referential one): the class is
        // its own nest host. This is the case for the enclosing class of a
        // nest, which carries `NestMembers` but no `NestHost`.
        Some(h) if h != own => h,
        _ => return Some(own),
    };
    // Arm order matches `confirmed_nest_host`: self-host, then hidden, then the
    // confirmation round-trip.
    if ctx.is_class_hidden(class_id) {
        return Some(claimed);
    }
    let host_id = match ctx.class_id_by_name_near(&claimed, class_id) {
        Some(id) => id,
        // Host not loadable/loaded → claim unconfirmed → own host.
        None => return Some(own),
    };
    if ctx.nest_member_names(host_id).iter().any(|m| *m == own) {
        Some(claimed)
    } else {
        Some(own)
    }
}

/// JEP 181 nestmate test: two classes are nestmates iff they resolve to the
/// same *confirmed* nest host.
fn classes_are_nestmates(ctx: &mut dyn NativeContext, a: ClassId, b: ClassId) -> bool {
    if a == b {
        return true;
    }
    match (
        confirmed_nest_host_name(ctx, a),
        confirmed_nest_host_name(ctx, b),
    ) {
        (Some(ha), Some(hb)) => ha == hb,
        _ => false,
    }
}

/// Is `caller` a subclass of `declaring` (JLS §6.6.2, the `protected` arm)?
///
/// FAILS CLOSED, unlike [`is_subclass_or_unreadable`]: this arm only ever
/// *widens* — its `false` leaves `caller_may_access_member` at the refusal it
/// would have reached anyway — so an unreadable hierarchy costs nothing here.
/// Every caller that turns a `false` into a NEW refusal must use the tri-state
/// walk instead.
fn caller_is_subclass_of(ctx: &mut dyn NativeContext, caller: ClassId, declaring: ClassId) -> bool {
    let mut cursor = caller;
    for _ in 0..MAX_SUPERCLASS_WALK {
        if cursor == declaring {
            return true;
        }
        cursor = match ctx.superclass_of(cursor) {
            Some(s) => s,
            None => return false,
        };
    }
    false
}

/// Is `subject` `ancestor` or a subclass of it — answering "yes" whenever the
/// hierarchy cannot be read?
///
/// This is the walk every rule needs when a `false` becomes a NEW refusal.
/// `superclass_of` answering `None` is ambiguous: it is the truth for
/// `java.lang.Object`, and it is equally what a fabricated synthetic-JDK
/// stand-in with no modelled supertype answers. Only the first is evidence that
/// the walk really did visit a whole hierarchy without meeting `ancestor`; the
/// second is an unreadable input, and an unreadable input must not invent a
/// denial. Same reasoning for exhausting [`MAX_SUPERCLASS_WALK`]: a chain that
/// long is a cycle or a corrupt model, not a measured answer.
///
/// The synthetic-JDK mode is the concrete reason this matters rather than being
/// theoretical: `--synthetic-jdk` fabricates stand-ins for real JDK classes and
/// gives them supertypes only where a stub table declares them, so a class there
/// routinely has a truncated chain that a real class file would never have. A
/// rule about real class hierarchies must not catch it, and the synthetic-jdk vm
/// gate is blocking at zero failures.
///
/// Note this is deliberately NOT `NativeContext::is_subclass`: that predicate is
/// two-valued and reports a fabricated stand-in as "not a subclass", which is
/// exactly the spurious refusal above.
///
/// Interfaces are not walked, and do not need to be: every rule that uses this
/// asks about an INSTANCE relationship (a receiver's class, a caller's
/// superclass chain), and instance fields and the `protected` arm of JLS §6.6.2
/// are both class-only questions.
pub(crate) fn is_subclass_or_unreadable(
    ctx: &mut dyn NativeContext,
    subject: ClassId,
    ancestor: ClassId,
) -> bool {
    let mut cursor = subject;
    for _ in 0..MAX_SUPERCLASS_WALK {
        if cursor == ancestor {
            return true;
        }
        match ctx.superclass_of(cursor) {
            Some(s) => cursor = s,
            None => {
                // A chain that ended at a real `java/lang/Object` is a COMPLETE
                // hierarchy that never met `ancestor` — deny. A chain that ended
                // anywhere else ended somewhere unreadable — allow.
                return !matches!(
                    ctx.class_name_arc_of_id(cursor).as_deref(),
                    Some("java/lang/Object")
                );
            }
        }
    }
    true
}

/// JLS §6.6.2.1, the receiver refinement of the `protected` rule: once a
/// foreign-package subclass has been admitted by `caller_is_subclass_of`, it
/// may still only reach the member through a receiver whose class is `caller`
/// itself or a subclass of `caller`.
///
/// `receiver == None` means the question does not arise — a `static` member has
/// no receiver (HotSpot passes `null` for `targetClass` and
/// `verifyMemberAccess` skips the refinement outright), and a call site that
/// simply does not have the receiver in hand must not manufacture a denial from
/// its absence.
///
/// FAILS OPEN on anything it cannot read, matching
/// `lang_class::public_member_class_is_reachable` — the walk and its fail-open
/// rule both live in [`is_subclass_or_unreadable`] so this rule, the
/// `setAccessible` carve-out and the receiver-type check cannot drift on what
/// "cannot tell" means.
fn protected_receiver_is_permitted(
    ctx: &mut dyn NativeContext,
    caller: ClassId,
    receiver: Option<ClassId>,
) -> bool {
    let Some(receiver) = receiver else {
        return true;
    };
    is_subclass_or_unreadable(ctx, receiver, caller)
}

/// Decide whether `caller` is entitled — by the ordinary JLS §6.6.1 rules, with
/// no `setAccessible(true)` override — to reflectively use a member of
/// `declaring` whose access flags are `modifiers`.
///
/// This is the caller-class question of the three that gate deep reflection.
/// It does **not** answer the other two: the `setAccessible` override flag is
/// checked before this is reached, and the JPMS `opens`/`exports` edge is
/// checked after it by `lang_class::check_reflection_module_access`. Answering
/// `true` here does not bypass the module check — see
/// `docs/known-issues/jdk-only/L1-reflect-setaccessible-invoke.md`.
///
/// `receiver` is the JLS §6.6.2.1 *target type* — HotSpot's
/// `Reflection.verifyMemberAccess(currentClass, memberClass, targetClass,
/// modifiers)` third argument, which `Field.checkAccess` fills in as
/// `Modifier.isStatic(modifiers) ? null : obj.getClass()`. It steers ONLY the
/// `protected` arm; every other arm ignores it, and passing `None` reproduces
/// this function's pre-receiver behaviour exactly.
///
/// Measured on Temurin 25.0.3 from a classpath subclass of
/// `java.io.ByteArrayOutputStream`, reading the `protected` `buf`/`count` with
/// no `setAccessible` and no flags — the four rows the refinement exists for:
///
/// | receiver                      | outcome                    |
/// |-------------------------------|----------------------------|
/// | the caller's own class        | OK                         |
/// | a subclass of the caller      | OK                         |
/// | the superclass (`BAOS` bare)  | `IllegalAccessException`   |
/// | a SIBLING subclass of `BAOS`  | `IllegalAccessException`   |
///
/// The sibling row is the one that reads as surprising and is the point of the
/// rule: being a subclass of the *declaring* class is not enough, the receiver
/// must be under the *caller*. Neither `--add-exports java.base/java.io` nor
/// `--add-opens java.base/java.io` moves any of the four — the refinement is
/// pure JLS and orthogonal to JPMS. `protected static` is exempt: the same
/// matrix on the `protected static` `java.io.PipedInputStream.PIPE_SIZE`
/// answers OK for every receiver, including a sibling, a bare `Object` and
/// `null`. Both halves are asserted in `regression-suite/src/
/// RJdkFieldModule.java` sections 7 and 7b.
pub(crate) fn caller_may_access_member(
    ctx: &mut dyn NativeContext,
    caller: ClassId,
    declaring: ClassId,
    modifiers: i32,
    receiver: Option<ClassId>,
) -> bool {
    // A public member of an accessible class needs no caller analysis. (Call
    // sites short-circuit this already; keep it so the helper is safe alone.)
    if (modifiers & ACC_PUBLIC_MEMBER) != 0 {
        return true;
    }
    // The declaring class may always reach its own members.
    if caller == declaring {
        return true;
    }
    if (modifiers & ACC_PRIVATE_MEMBER) != 0 {
        // JEP 181: private is nest-scoped, nothing wider.
        return classes_are_nestmates(ctx, caller, declaring);
    }
    // `protected` and package-private both admit a same-runtime-package caller.
    //
    // This arm must stay AHEAD of the protected arm below, because the receiver
    // refinement is skipped entirely when the caller and the declaring class
    // share a runtime package (`verifyMemberAccess` guards it with
    // `if (!isSameClassPackage)`). Witness on Temurin 25.0.3: a subclass in the
    // SAME package as the declaring class reads the protected field through a
    // bare superclass receiver with no complaint.
    let same_package = match (
        runtime_package_of(ctx, caller),
        runtime_package_of(ctx, declaring),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if same_package {
        return true;
    }
    if (modifiers & ACC_PROTECTED_MEMBER) != 0 {
        // JLS §6.6.2: a subclass reaches inherited protected members...
        if !caller_is_subclass_of(ctx, caller, declaring) {
            return false;
        }
        // ...but §6.6.2.1 then narrows WHICH objects it may reach them on.
        return protected_receiver_is_permitted(ctx, caller, receiver);
    }
    // Package-private with a foreign package: denied.
    false
}

// ---------------------------------------------------------------------------
// Method.invoke return-value boxing wrapper.
// ---------------------------------------------------------------------------
//
// ByteBuddy's `JavaDispatcher` proxy validates the return of every reflective
// call by inspecting `result.getClass()` and comparing against the dispatched
// method's declared return type. JDK 25's `Method.invoke` Javadoc requires
// that primitive returns be boxed into the matching wrapper (`int` -> Integer,
// `long` -> Long, etc.). When the dispatch path keeps the value as a raw
// `Value::Int` / `Value::Long` (e.g. some MethodHandle bypass paths), ByteBuddy
// sees an unboxed primitive against an `Object` slot and raises:
//   "Cannot assign int to public abstract int [method]"
//
// `lang_class::native_method_invoke` already boxes via `box_value`, but this
// wrapper is defensive: it re-checks the result and applies the canonical
// JVM-spec boxing if a primitive Value somehow survives. This is harmless for
// already-boxed values (the `_ => value` arm in `box_value` is a no-op for
// `Value::Object`).
// bytebuddy_probe stack-overflow guard (agent11).
//
// ByteBuddy's `JavaDispatcher` proxy can produce reflective call chains where
// `Method.invoke` recursively dispatches back through `Method.invoke` (e.g.
// when the invoked method itself uses reflection). Without a bound, this
// recurses through `native_method_invoke_boxed` → `native_method_invoke` →
// interpreter → `native_method_invoke_boxed` … and exhausts the Rust thread
// stack, aborting the process with no JVM-level stack trace.
//
// A thread-local depth counter converts the hard Rust-stack overflow into a
// recoverable Java `StackOverflowError` that ByteBuddy (and any Java caller)
// can catch and surface. The limit is intentionally conservative: real
// reflective dispatch chains observed in Spring/ByteBuddy bootstrap rarely
// exceed depth ~20. A 100-frame ceiling keeps a comfortable margin while
// catching pathological recursion well before the Rust guard-page fires.
thread_local! {
    static INVOKE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn native_method_invoke_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Reentrancy / recursion guard — see comment on INVOKE_DEPTH above.
    let prev_depth = INVOKE_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if prev_depth > 100 {
        // Roll back the increment so the next top-level call starts fresh.
        INVOKE_DEPTH.with(|d| d.set(prev_depth));
        return Err(cratonvm_types::error::RuntimeError::StackOverflowError.into());
    }

    // RAII-style depth restore: any early return (including `?`) must still
    // decrement so we don't leak depth across calls. Run the original body
    // inside a closure and capture its Result so we can decrement before
    // propagating.
    let result = (|| -> MethodCallResult {
        // GC-safety: capture the Method's declared descriptor BEFORE running the
        // target method. `native_method_invoke` below executes arbitrary Java
        // that can trigger a GC and MOVE the `Method` mirror. `args` is a
        // pre-call snapshot of operand-stack `Value`s held on the native's Rust
        // stack — NOT a GC root — so after the inner invoke `args.first()` is a
        // stale pointer, and reading fields off it (`method_descriptor_for_invoke`
        // → `get_field_by_name` → `class_id_of`) dereferences freed/moved memory.
        // That is the intermittent, load-dependent SIGSEGV seen running JUnit
        // suites (e.g. TestServerInfo) under CPU contention, where a GC is far
        // more likely to land inside the inner invoke. The descriptor is
        // invariant (the method's signature), so capture it now while the mirror
        // is still valid and reuse it afterwards.
        let pre_descriptor = match args.first() {
            Some(Value::Object(Some(o))) => Some(method_descriptor_for_invoke(ctx, *o)),
            _ => None,
        };

        // Delegate to the canonical implementation.
        let raw = native_method_invoke(ctx, args)?;

        let raw_val = match raw {
            Some(v) => v,
            None => return Ok(None),
        };

        // Recover the declared return descriptor (captured pre-invoke above) so we
        // can sanity-check the return shape against what the JDK contract
        // requires (primitive returns must come back as wrapper objects, never
        // as raw `Value::Int`/`Value::Long`/etc. and never as a primitive
        // `Class<int>` mirror).
        let descriptor = match pre_descriptor {
            Some(d) => d,
            None => return Ok(Some(raw_val)),
        };
        let (_params, ret_desc) = parse_descriptor_param_and_return(&descriptor);
        let primitive_ret = matches!(
            ret_desc.as_str(),
            "I" | "J" | "Z" | "B" | "S" | "C" | "F" | "D"
        );

        // Defensive fallback (G3 / bytebuddy_probe): if the inner returned a
        // `Class<primitive>` mirror instead of a wrapper instance and the
        // declared return descriptor is itself a primitive, the inner
        // dispatch produced a primitive-Class mirror by mistake (e.g. a
        // mis-wired native handler that conflated the *return type* with the
        // *return value*). ByteBuddy's `JavaDispatcher` proxy formats that as
        //   "Cannot assign int to public abstract int ..."
        // because `value.toString()` for `int.class` is "int". Box the
        // descriptor-default (0 / false) so the call surfaces a defined value
        // rather than a Class mirror, and log loud enough that we can spot it
        // in repro logs.
        if primitive_ret {
            if let Value::Object(Some(obj)) = raw_val {
                let class_id = ctx.class_id_of_object(obj);
                let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
                if class_name == "java/lang/Class" {
                    let placeholder = match ret_desc.as_str() {
                        "J" => Value::Long(0),
                        "F" => Value::Float(0.0),
                        "D" => Value::Double(0.0),
                        _ => Value::Int(0),
                    };
                    let recovered = box_value(ctx, placeholder, &ret_desc);
                    tracing::warn!(
                        "[Method.invoke] inner returned Class mirror for primitive return \
                     `{}` — recovering with default-boxed value (descriptor={})",
                        ret_desc,
                        descriptor,
                    );
                    if dbg_method_invoke_box_enabled() {
                        eprintln!(
                            "[Method.invoke] recovered Class<primitive> -> default-boxed; \
                         ret_desc=`{}`",
                            ret_desc,
                        );
                    }
                    return Ok(Some(recovered));
                }
                // Already-boxed Object: pass through.
                return Ok(Some(raw_val));
            }
        }

        // If the inner already returned a boxed Object (or null) and the return
        // is a reference type, pass through unchanged.
        if matches!(raw_val, Value::Object(_)) {
            return Ok(Some(raw_val));
        }

        // Otherwise we have a raw primitive `Value`. Box it according to the
        // Method's declared return type. For non-primitive return descriptors
        // (somehow paired with a primitive Value) this is a no-op via the
        // `_ => value` arm of `box_value`.
        let needs_box = primitive_ret || ret_desc == "V";
        if !needs_box {
            return Ok(Some(raw_val));
        }

        let boxed = box_value(ctx, raw_val, &ret_desc);
        let boxed_kind: &'static str = match boxed {
            Value::Object(Some(_)) => "wrapper",
            Value::Object(None) => "null",
            _ => "primitive(unchanged)",
        };
        tracing::debug!(
            "[Method.invoke] boxed primitive Value into {} for return descriptor `{}`",
            boxed_kind,
            ret_desc
        );
        if dbg_method_invoke_box_enabled() {
            eprintln!(
                "[Method.invoke] defensive box: ret_desc=`{}` boxed_kind={}",
                ret_desc, boxed_kind,
            );
        }
        Ok(Some(boxed))
    })();

    // Restore depth on every exit path (success or error). Use `set(prev)`
    // rather than `set(get-1)` so that even if some inner panic-unwind
    // somehow skipped a decrement, we still settle back to the depth we
    // observed on entry.
    INVOKE_DEPTH.with(|d| d.set(prev_depth));
    result
}

// ---------------------------------------------------------------------------
// register_wp2_1_natives — registry-side entry point.
// ---------------------------------------------------------------------------

/// Register the WP2.1 net-new natives. Called from `register_essential_natives`
/// AFTER the existing reflection registrations so these supplement (don't
/// override) the historical layer.
/// One `Type`-typed slot of a reflection object, read from EITHER
/// representation: the synthetic stub keeps its members in positional slots,
/// the real `sun.reflect.generics.reflectiveObjects.*Impl` in named fields.
/// `None` means the receiver is not that kind of type at all - the callers
/// read it as "not equal", the same fail-closed rule the `TypeVariable`
/// natives above already use.
fn reflect_type_slot(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    stub_class: &str,
    stub_slot: usize,
    real_field: &str,
) -> Option<Value> {
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if cls == stub_class {
        return Some(ctx.get_field(obj, stub_slot));
    }
    match ctx.get_field_by_name(obj, real_field) {
        v @ Value::Object(_) => Some(v),
        _ => None,
    }
}

/// `Objects.equals(a, b)` over two `Type` references, dispatching to the
/// receiver's own `equals` so a nested `TypeVariable` contributes its
/// (declaration, name) identity rather than a rendered name.
fn type_value_equals(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(true),
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return Ok(true);
            }
            let r = ctx.invoke_virtual(
                x,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(y))],
            )?;
            Ok(matches!(r, Some(Value::Int(v)) if v != 0))
        }
        _ => Ok(false),
    }
}

/// `Objects.hashCode(t)` - 0 for null, the receiver's virtual `hashCode`
/// otherwise.
fn type_value_hash(
    ctx: &mut dyn NativeContext,
    v: Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    match v {
        Value::Object(Some(o)) => Ok(match ctx.invoke_virtual(o, "hashCode", "()I", &[])? {
            Some(Value::Int(h)) => h,
            _ => 0,
        }),
        _ => Ok(0),
    }
}

/// `Arrays.equals(Type[], Type[])`.
fn type_array_equals(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    let (Value::Object(oa), Value::Object(ob)) = (a, b) else {
        return Ok(false);
    };
    let (Some(aa), Some(bb)) = (oa, ob) else {
        return Ok(oa.is_none() && ob.is_none());
    };
    let n = ctx.array_length(aa);
    if n != ctx.array_length(bb) {
        return Ok(false);
    }
    for i in 0..n {
        let x = ctx.get_array_element(aa, i);
        let y = ctx.get_array_element(bb, i);
        if !type_value_equals(ctx, x, y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `Arrays.hashCode(Type[])` - 0 for a null array, else the JDK fold.
fn type_array_hash(
    ctx: &mut dyn NativeContext,
    v: Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let Value::Object(Some(arr)) = v else {
        return Ok(0);
    };
    let n = ctx.array_length(arr);
    let mut h: i32 = 1;
    for i in 0..n {
        let e = ctx.get_array_element(arr, i);
        h = h.wrapping_mul(31).wrapping_add(type_value_hash(ctx, e)?);
    }
    Ok(h)
}

/// The generic component type of a `GenericArrayType`, from either
/// representation: the synthetic stub keeps it in slot 0, the real JDK
/// `GenericArrayTypeImpl` in a named field. `None` means the receiver is not a
/// generic array type at all (or carries no component), which the callers read
/// as "not equal" / "hash 0" rather than guessing.
fn generic_array_component(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    let slot = if cls == "java/lang/reflect/GenericArrayType" {
        ctx.get_field(obj, 0)
    } else {
        ctx.get_field_by_name(obj, "genericComponentType")
    };
    match slot {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub(crate) fn register_wp2_1_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // W7-77: publish the legacy `java/lang/reflect/Method` mirror map so the
    // read-side sweep names the row instead of leaving it as a comment. This
    // registrar is reached in BOTH modes (via `register_annotation_overrides`
    // <- `register_essential_natives_with_shims`). Unconditional and
    // idempotent-by-pointer, for the reason `declare_slot_map`'s own doc gives.
    cratonvm_native_api::read_alias::declare_slot_map(&crate::lang_class::METHOD_LEGACY_SLOT_MAP);
    // --- Method.invoke return-boxing safety net (overrides the historical
    // registration in lib.rs::register_essential_natives because
    // register_wp2_1_natives is called AFTER it). See the comment on
    // `native_method_invoke_boxed` for the ByteBuddy JavaDispatcher case.
    registry.register(
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        native_method_invoke_boxed,
    );

    // --- AccessibleObject.trySetAccessible ---
    registry.register(
        "java/lang/reflect/AccessibleObject",
        "trySetAccessible",
        "()Z",
        native_accessible_try_set_accessible,
    );
    registry.register(
        "java/lang/reflect/Method",
        "trySetAccessible",
        "()Z",
        native_method_try_set_accessible,
    );
    registry.register(
        "java/lang/reflect/Field",
        "trySetAccessible",
        "()Z",
        native_field_try_set_accessible,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "trySetAccessible",
        "()Z",
        native_constructor_try_set_accessible,
    );

    // --- AccessibleObject.canAccess(Object) — typed per subclass ---
    registry.register(
        "java/lang/reflect/Method",
        "canAccess",
        "(Ljava/lang/Object;)Z",
        native_method_can_access,
    );
    registry.register(
        "java/lang/reflect/Field",
        "canAccess",
        "(Ljava/lang/Object;)Z",
        native_field_can_access,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "canAccess",
        "(Ljava/lang/Object;)Z",
        native_constructor_can_access,
    );

    // --- Class.getEnclosingClass — overrides the previous null-stub. ---
    registry.register(
        "java/lang/Class",
        "getEnclosingClass",
        "()Ljava/lang/Class;",
        native_class_get_enclosing_class,
    );

    // --- Class.getEnclosingMethod / getEnclosingConstructor (typed wrappers) ---
    registry.register(
        "java/lang/Class",
        "getEnclosingMethod",
        "()Ljava/lang/reflect/Method;",
        native_class_get_enclosing_method_public,
    );
    registry.register(
        "java/lang/Class",
        "getEnclosingConstructor",
        "()Ljava/lang/reflect/Constructor;",
        native_class_get_enclosing_constructor_public,
    );

    // --- Parameter.isImplicit / isSynthetic ---
    registry.register(
        "java/lang/reflect/Parameter",
        "isImplicit",
        "()Z",
        native_parameter_is_implicit,
    );
    registry.register(
        "java/lang/reflect/Parameter",
        "isSynthetic",
        "()Z",
        native_parameter_is_synthetic,
    );
    registry.register(
        "java/lang/reflect/Parameter",
        "isNamePresent",
        "()Z",
        native_parameter_is_name_present,
    );

    // --- Executable.getParameters / per-subclass ---
    registry.register(
        "java/lang/reflect/Method",
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
        native_method_get_parameters,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
        native_constructor_get_parameters,
    );
    registry.register(
        "java/lang/reflect/Executable",
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
        native_executable_get_parameters,
    );

    // --- Method.getDefaultValue ---
    registry.register(
        "java/lang/reflect/Method",
        "getDefaultValue",
        "()Ljava/lang/Object;",
        native_method_get_default_value,
    );

    // --- Method.getExceptionTypes / getGenericExceptionTypes / toString ---
    // WP2.1: Wire `getExceptionTypes` directly to the cached
    // `exceptionTypes` field (now populated from the class-file
    // Exceptions attribute by `create_method_object`). Also provide
    // a generic-aware variant + a deterministic toString native.
    registry.register(
        "java/lang/reflect/Method",
        "getExceptionTypes",
        "()[Ljava/lang/Class;",
        native_method_get_exception_types,
    );
    // CGLib Enhancer.emitConstructors -> ReflectUtils.getExceptionTypes ->
    // Constructor.getExceptionTypes:293 — JDK Java code does
    // `return exceptionTypes.clone();` and NPEs if the field is null.
    // We populate the field in `create_constructor_object` (lang_class.rs)
    // and bind this native so the read path is deterministic.
    registry.register(
        "java/lang/reflect/Constructor",
        "getExceptionTypes",
        "()[Ljava/lang/Class;",
        native_constructor_get_exception_types,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getGenericExceptionTypes",
        "()[Ljava/lang/reflect/Type;",
        native_method_get_generic_exception_types,
    );
    registry.register(
        "java/lang/reflect/Method",
        "toString",
        "()Ljava/lang/String;",
        native_method_to_string,
    );

    // --- Method.isVarArgs / isBridge / isSynthetic / isDefault ---
    registry.register(
        "java/lang/reflect/Method",
        "isVarArgs",
        "()Z",
        native_method_is_varargs,
    );
    registry.register(
        "java/lang/reflect/Method",
        "isBridge",
        "()Z",
        native_method_is_bridge,
    );
    registry.register(
        "java/lang/reflect/Method",
        "isSynthetic",
        "()Z",
        native_method_is_synthetic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "isDefault",
        "()Z",
        native_method_is_default,
    );

    // --- Field.isSynthetic / isEnumConstant ---
    registry.register(
        "java/lang/reflect/Field",
        "isSynthetic",
        "()Z",
        native_field_is_synthetic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "isEnumConstant",
        "()Z",
        native_field_is_enum_constant,
    );

    // --- Constructor.isSynthetic / isVarArgs / getName ---
    registry.register(
        "java/lang/reflect/Constructor",
        "isSynthetic",
        "()Z",
        native_constructor_is_synthetic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "isVarArgs",
        "()Z",
        native_constructor_is_varargs,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getName",
        "()Ljava/lang/String;",
        native_constructor_get_name,
    );

    // --- Class.getEnclosingMethod / getEnclosingConstructor are now real ---

    // --- Field.getGenericType — WP2.1 (FAIL-10) ---
    // Real-JDK Field.getGenericType is a Java method that delegates to
    // sun.reflect.generics.repository.FieldRepository, which we don't
    // implement. This native parses the JVMS §4.7.9 Signature attribute
    // on the field directly via cratonvm_reader::signature and builds a
    // ParameterizedType / TypeVariable / GenericArrayType / WildcardType
    // runtime object via crate::generics::type_sig_to_java. Promoted from
    // synthetic-only registration so real-JDK mode also returns
    // ParameterizedType for `List<String> list` instead of raw List.class.
    registry.register(
        "java/lang/reflect/Field",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
        crate::lang_class::native_field_get_generic_type,
    );

    // Real-JDK Method generic accessors delegate into sun.reflect.generics,
    // which CratonVM does not fully support. Prefer Signature-attribute natives
    // so Spring generic method resolution sees declaration-scoped variables.
    registry.register(
        "java/lang/reflect/Method",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
        crate::lang_class::native_method_get_generic_param_types,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getGenericReturnType",
        "()Ljava/lang/reflect/Type;",
        crate::lang_class::native_method_get_generic_return_type,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        crate::lang_class::native_method_get_type_parameters,
    );

    // --- Constructor.getGenericParameterTypes / RecordComponent.getGenericType ---
    // Same rationale as Field.getGenericType above: real-JDK
    // Constructor/RecordComponent generic accessors delegate to a
    // sun.reflect.generics repository CratonVM doesn't implement, so they
    // returned the RAW type (`List` not `List<TestSlice>`). These were only
    // registered in `lib.rs::register_synthetic_overrides` (compiled out of
    // the real-JDK CLI build), so real mode lost record/ctor generics —
    // Jackson then deserialized a record's `List<TestSlice>` component into
    // `List<LinkedHashMap>`. Promote to the essential path.
    // `native_method_get_generic_param_types` already handles the `<init>`
    // descriptor (see `method_class_name_desc`).
    registry.register(
        "java/lang/reflect/Constructor",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
        crate::lang_class::native_method_get_generic_param_types,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        crate::lang_class::native_method_get_type_parameters,
    );
    registry.register(
        "java/lang/reflect/RecordComponent",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
        crate::lang_class::native_record_component_get_generic_type,
    );

    // --- ParameterizedType / TypeVariable / WildcardType / GenericArrayType ---
    // accessor natives — required so the Type objects produced by
    // `getGenericType` actually expose `getRawType()`, `getActualTypeArguments()`,
    // `getName()`, etc. Real-JDK has these as concrete impls in
    // `sun.reflect.generics.reflectiveObjects.*`; we allocate the interface
    // class directly and intercept the abstract interface methods.
    // These were previously only registered in `phases_late::register_p67_misc`,
    // which lives behind the synthetic-jdk feature gate.
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "getRawType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "getActualTypeArguments",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "getOwnerType",
        "()Ljava/lang/reflect/Type;",
        // field 2 = ownerType (set by generics::type_sig_to_java for nested
        // `Outer<...>.Inner<...>` signatures; null otherwise). Returning the
        // field rather than a hard-coded null lets Spring's variable resolvers
        // walk to an enclosing generic class. Synthetic PTs are allocated with
        // 3 fields, so the slot-2 read is in bounds.
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );

    // S111r13 — Real-JDK ParameterizedTypeImpl native overrides.
    //
    // SportMe (Spring Boot) reaches `Method.invoke(pti, "getActualTypeArguments", ...)`
    // via Spring's `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke:236`.
    // The receiver is a real `sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl`
    // built by JDK reifier code. The PTI's instance fields (in declaration order)
    // are: actualTypeArguments[0], rawType[1], ownerType[2] — and `get_field_by_name`
    // confirms they're populated correctly. However, dispatching the PTI bytecode
    // for `getActualTypeArguments` (which does `aload_0; getfield actualTypeArguments;
    // invokevirtual [Type;.clone()`) returns null, causing the calling Spring code
    // to NPE on `arraylength` at TPIH:236. The bytecode's `getfield #7` constant-
    // pool resolution must mis-map to a wrong slot for these JDK reifier classes.
    //
    // Bypass the broken bytecode path by registering native overrides that read
    // the fields by name. This is consistent with how we already handle the
    // `java/lang/reflect/ParameterizedType` interface natives above (which fire
    // for our synthetically-built PTIs).
    let pti_real = "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl";
    registry.register(
        pti_real,
        "getRawType",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Reference-typed return: `get_field_by_name` is not
            // descriptor-aware and answers `Int(0)` for an unwritten `rawType`
            // slot, which then fails a `checkcast` to Class/Type. Read by
            // resolved index. See `docs/feature-designs/by-name-field-reads.md`.
            Ok(Some(crate::field_read::ref_field(ctx, this, "rawType")))
        },
    );
    registry.register(
        pti_real,
        "getRawType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Reference-typed return: `get_field_by_name` is not
            // descriptor-aware and answers `Int(0)` for an unwritten `rawType`
            // slot, which then fails a `checkcast` to Class/Type. Read by
            // resolved index. See `docs/feature-designs/by-name-field-reads.md`.
            Ok(Some(crate::field_read::ref_field(ctx, this, "rawType")))
        },
    );
    registry.register(
        pti_real,
        "getActualTypeArguments",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Clone the array so callers that mutate it (e.g. Arrays.asList wrappers)
            // don't observe shared state — matches the real PTI bytecode contract
            // (`return actualTypeArguments.clone();`).
            let ata = ctx.get_field_by_name(this, "actualTypeArguments");
            if let Value::Object(Some(arr)) = ata {
                let len = ctx.array_length(arr);
                // Preserve the source array's component type. The stored
                // `actualTypeArguments` is a `java/lang/reflect/Type[]` (built via
                // generics::new_type_array); cloning into a `ClassId(0)` array made
                // an `Object[]`, so `getActualTypeArguments() instanceof Type[]`
                // was false and Spring's `SerializableTypeWrapper$
                // MethodInvokeTypeProvider.getType` `(Type) result` cast threw
                // `Object cannot be cast to java/lang/reflect/Type` — breaking
                // every bean whose generic collection property is resolved via
                // ResolvableType (e.g. CollectionsWithDefaultTypesTests).
                //
                // A ref array's header carries its COMPONENT class id, so reusing
                // `class_id_of_object(arr)` clones with the same component without
                // any class-loading side effect. (An earlier attempt resolved the
                // component via `ensure_class_initialized("java/lang/reflect/Type")`
                // on every call — that re-entrant class load broke XML/JAXP init,
                // which calls this native while the module graph is mid-setup.)
                let comp_cid = ctx.class_id_of_object(arr);
                let clone = ctx.new_ref_array(comp_cid, len);
                for i in 0..len {
                    let el = ctx.get_array_element(arr, i);
                    ctx.set_array_element(clone, i, el);
                }
                Ok(Some(Value::Object(Some(clone))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    registry.register(
        pti_real,
        "getOwnerType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `()Ljava/lang/reflect/Type;` — see `getRawType` above.
            Ok(Some(crate::field_read::ref_field(ctx, this, "ownerType")))
        },
    );
    registry.register(pti_real, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    });
    registry.register(
        pti_real,
        "getTypeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        },
    );

    // Spring's ResolvableType creates its own ParameterizedType wrapper. Its
    // bytecode getTypeName() path currently loses the first type argument while
    // joining names, so render the same field shape directly.
    let spring_spt = "org/springframework/core/ResolvableType$SyntheticParameterizedType";
    registry.register(
        spring_spt,
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        },
    );
    registry.register(
        spring_spt,
        "getTypeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        },
    );

    // Same field-resolution issue affects TypeVariableImpl / WildcardTypeImpl /
    // GenericArrayTypeImpl. Provide field-by-name natives so they keep working
    // when reached via real-JDK reifier code paths.
    //
    // REIFY-AWARE bounds (WildcardTypeImpl): the `upperBounds`/`lowerBounds`
    // fields hold UNREIFIED `sun.reflect.generics.tree.*` nodes until the
    // real bytecode's lazy `reifyBounds` runs — which it never does, because
    // these natives shadow it. Returning the field verbatim leaked tree
    // nodes typed as `Type[]`; Gradle's
    // `JavaPropertyReflectionUtil.hasTypeVariable` then CCE'd
    // ("SimpleClassTypeSignature cannot be cast to Type") and the decorated
    // class generator failed for every ProjectBuilder service. Resolve tree
    // nodes to Class mirrors here (wildcard bounds are class/interface types
    // in practice); pass already-reified entries through untouched.
    fn wti_tree_node_to_mirror(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        node: cratonvm_types::ObjectRef,
        node_class: &str,
    ) -> Option<Value> {
        // SimpleClassTypeSignature: `name` holds the dotted binary name.
        // ClassTypeSignature: `path` is a List<SimpleClassTypeSignature>;
        // join segment names with '$' after the first (inner classes).
        let dotted = if node_class.ends_with("SimpleClassTypeSignature") {
            match ctx.get_field_by_name(node, "name") {
                Value::Object(Some(s)) => ctx.read_string(s)?,
                _ => return None,
            }
        } else if node_class.ends_with("ClassTypeSignature") {
            let list = match ctx.get_field_by_name(node, "path") {
                Value::Object(Some(l)) => l,
                _ => return None,
            };
            let arr = match ctx
                .invoke_virtual(list, "toArray", "()[Ljava/lang/Object;", &[])
                .ok()
                .flatten()
            {
                Some(Value::Object(Some(a))) => a,
                _ => return None,
            };
            let mut name = String::new();
            for i in 0..ctx.array_length(arr) {
                if let Value::Object(Some(seg)) = ctx.get_array_element(arr, i) {
                    if let Value::Object(Some(s)) = ctx.get_field_by_name(seg, "name") {
                        let part = ctx.read_string(s)?;
                        if name.is_empty() {
                            name = part;
                        } else {
                            name.push('$');
                            name.push_str(&part);
                        }
                    }
                }
            }
            if name.is_empty() {
                return None;
            }
            name
        } else {
            return None;
        };
        let slashed = dotted.replace('.', "/");
        let cid = match ctx.class_id_by_name(&slashed) {
            Some(c) => Some(c),
            None => match ctx.load_class(&slashed) {
                Ok(Some(Value::Object(Some(_)))) => ctx.class_id_by_name(&slashed),
                _ => None,
            },
        }?;
        Some(Value::Object(Some(ctx.get_class_mirror(cid))))
    }
    /// Recover the declaring `GenericDeclaration` (Method/Constructor/Class) that
    /// owns a lazily-reified `sun.reflect.generics.reflectiveObjects.*` object, by
    /// walking its `LazyReflectiveObjectGenerator.factory` →
    /// `CoreReflectionFactory.decl`. The JDK reifier threads this declaration
    /// through every nested Type it builds so a type-variable USE (`? super T`)
    /// resolves to the declaration's REAL type parameter; CratonVM reuses it as
    /// the `GenericDeclScope` for `type_sig_to_java`.
    fn reifier_decl_from_factory(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        this: cratonvm_types::ObjectRef,
    ) -> Value {
        let factory = match ctx.get_field_by_name(this, "factory") {
            Value::Object(Some(f)) => f,
            _ => return Value::Object(None),
        };
        match ctx.get_field_by_name(factory, "decl") {
            v @ Value::Object(Some(_)) => v,
            _ => Value::Object(None),
        }
    }
    fn wti_bounds_reified(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        this: cratonvm_types::ObjectRef,
        field: &str,
    ) -> Result<Value, MethodCallFailed> {
        let raw = ctx.get_field_by_name(this, field);
        let Value::Object(Some(arr)) = raw else {
            return Ok(raw);
        };
        if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
            return Ok(raw);
        }
        // A wildcard bound may itself be a type-variable USE (`? super T`, the
        // shape Kotlin emits for a suspending function's `Continuation`
        // parameter). Resolve such uses against the declaring method/class so
        // `T` becomes the declaration's REAL `sun.reflect…TypeVariableImpl`
        // (with its proper bounds + genericDeclaration) instead of a bound-less
        // synthetic. Without this scope, kotlin-reflect's
        // `extractContinuationArgument` recovers a decl-less `TypeVariable@…`
        // and `MethodParameterKotlinTests."Suspending function return type"`
        // sees `bounds[0] == Object` instead of `Producer<? extends Number>`.
        let _gscope = crate::generics::GenericDeclScope::new(reifier_decl_from_factory(ctx, this));
        let len = ctx.array_length(arr);
        let mut out: Vec<Value> = Vec::with_capacity(len);
        let mut any_tree = false;
        for i in 0..len {
            let elem = ctx.get_array_element(arr, i);
            if let Value::Object(Some(node)) = elem {
                let cid = ctx.class_id_of_object(node);
                let cls = ctx.class_name_of_id(cid).unwrap_or_default();
                if cls.starts_with("sun/reflect/generics/tree/") {
                    any_tree = true;
                    // SB-02b: a wildcard bound can itself be parameterized
                    // (`? super Producer<? extends Number>`, the shape Kotlin
                    // emits for the synthetic `Continuation` parameter of a
                    // suspending function). The old `wti_tree_node_to_mirror`
                    // collapsed any class bound to its RAW Class mirror,
                    // dropping the `<...>` — which made kotlin-reflect's
                    // `extractContinuationArgument` recover a raw return type.
                    // Reify the tree node through the full `TypeSig` →
                    // `type_sig_to_java` pipeline (handles nested type
                    // arguments and wildcards). Fall back to the raw mirror,
                    // then to Object, if the shape isn't modellable.
                    let reified = jdk_tree_to_typesig(ctx, node)
                        .map(|ts| crate::generics::typesig_to_real_type(ctx, &ts))
                        .filter(|v| !matches!(v, Ok(Value::Object(None))))
                        .or_else(|| wti_tree_node_to_mirror(ctx, node, &cls).map(Ok))
                        .or_else(|| {
                            // Unresolvable exotic bound: degrade to Object
                            // (the JDK's implicit upper bound) rather than
                            // leaking a non-Type.
                            ctx.class_id_by_name("java/lang/Object")
                                .map(|c| Ok(Value::Object(Some(ctx.get_class_mirror(c)))))
                        })
                        .unwrap_or(Ok(Value::Object(None)));
                    out.push(reified?);
                    continue;
                }
            }
            out.push(elem);
        }
        if !any_tree {
            // An EMPTY raw bound array (e.g. an unbounded wildcard `?`, whose
            // `lowerBounds` field is an empty `sun.reflect…FieldTypeSignature[0]`)
            // has no tree node to reify, but it must still be handed back as a
            // `java/lang/reflect/Type[]` — not the raw `FieldTypeSignature[]`.
            // Returning the raw array looked fine for direct callers
            // (`Arrays.toString` prints `[]`), but Spring's SerializableTypeWrapper
            // reflectively casts the result `(Type[]) getLowerBounds()` and threw
            // `ClassCastException: …FieldTypeSignature cannot be cast to
            // [Ljava/lang/reflect/Type;` (ResolvableTypeTests gh32327/gh33535/
            // hasResolvableGenericsWithSingleWildcard/isAssignableFromForWildcards).
            // A non-empty `!any_tree` array is one we already reified+wrote back as
            // a Type[] on a prior call, so returning it as-is stays correct.
            if len == 0 {
                let type_cid = ctx
                    .class_id_by_name("java/lang/reflect/Type")
                    .unwrap_or(cratonvm_types::ClassId::new(0));
                let result = ctx.new_ref_array(type_cid, 0);
                ctx.set_field_by_name(this, field, Value::Object(Some(result)));
                return Ok(Value::Object(Some(result)));
            }
            return Ok(Value::Object(Some(arr)));
        }
        let type_cid = ctx
            .class_id_by_name("java/lang/reflect/Type")
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let result = ctx.new_ref_array(type_cid, out.len());
        for (i, v) in out.iter().enumerate() {
            ctx.set_array_element(result, i, *v);
        }
        // Write back so subsequent reads (incl. real bytecode getfield) see
        // reified values — mirrors the lazy write-back in the real impl.
        ctx.set_field_by_name(this, field, Value::Object(Some(result)));
        Ok(Value::Object(Some(result)))
    }
    // SB-02b — convert a real-JDK `sun.reflect.generics.tree.*` node into
    // CratonVM's `TypeSig` AST so `crate::generics::type_sig_to_java` can
    // reify it WITH its nested type arguments / wildcards (the raw-mirror
    // shortcut above lost them). Returns `None` for shapes we don't model.
    fn jdk_tree_to_typesig(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        node: cratonvm_types::ObjectRef,
    ) -> Option<crate::generics::TypeSig> {
        use crate::generics::TypeSig;
        let cid = ctx.class_id_of_object(node);
        let cls = ctx.class_name_of_id(cid).unwrap_or_default();
        if cls.ends_with("TypeVariableSignature") {
            let id = match ctx.get_field_by_name(node, "identifier") {
                Value::Object(Some(s)) => ctx.read_string(s)?,
                _ => return None,
            };
            return Some(TypeSig::TypeVar(id));
        }
        if cls.ends_with("ArrayTypeSignature") {
            let comp = match ctx.get_field_by_name(node, "componentType") {
                Value::Object(Some(c)) => c,
                _ => return None,
            };
            return Some(TypeSig::Array(Box::new(jdk_tree_to_typesig(ctx, comp)?)));
        }
        // SimpleClassTypeSignature / ClassTypeSignature -> Class{name, args}.
        if cls.ends_with("SimpleClassTypeSignature") {
            let name = match ctx.get_field_by_name(node, "name") {
                Value::Object(Some(s)) => ctx.read_string(s)?,
                _ => return None,
            };
            return Some(TypeSig::Class {
                name: name.replace('.', "/"),
                type_args: jdk_collect_type_args(ctx, node),
                owner: None,
            });
        }
        if cls.ends_with("ClassTypeSignature") {
            let list = match ctx.get_field_by_name(node, "path") {
                Value::Object(Some(l)) => l,
                _ => return None,
            };
            let arr = match ctx
                .invoke_virtual(list, "toArray", "()[Ljava/lang/Object;", &[])
                .ok()
                .flatten()
            {
                Some(Value::Object(Some(a))) => a,
                _ => return None,
            };
            let mut name = String::new();
            let mut type_args: Vec<crate::generics::TypeArg> = Vec::new();
            for i in 0..ctx.array_length(arr) {
                if let Value::Object(Some(seg)) = ctx.get_array_element(arr, i) {
                    if let Value::Object(Some(s)) = ctx.get_field_by_name(seg, "name") {
                        if let Some(part) = ctx.read_string(s) {
                            if name.is_empty() {
                                name = part;
                            } else {
                                name.push('$');
                                name.push_str(&part);
                            }
                        }
                    }
                    // Type args belong to the innermost (last non-empty)
                    // simple-class segment; later segments override earlier.
                    let seg_args = jdk_collect_type_args(ctx, seg);
                    if !seg_args.is_empty() {
                        type_args = seg_args;
                    }
                }
            }
            if name.is_empty() {
                return None;
            }
            return Some(TypeSig::Class {
                name: name.replace('.', "/"),
                type_args,
                owner: None,
            });
        }
        None
    }
    // Read a SimpleClassTypeSignature's `typeArgs` (`TypeArgument[]`) and map
    // each to a `TypeArg`; empty when the segment is not parameterized.
    fn jdk_collect_type_args(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        sts: cratonvm_types::ObjectRef,
    ) -> Vec<crate::generics::TypeArg> {
        let mut out = Vec::new();
        if let Value::Object(Some(ta_arr)) = ctx.get_field_by_name(sts, "typeArgs") {
            if ctx.heap_kind_of(ta_arr) == cratonvm_types::ObjectKind::Array {
                for j in 0..ctx.array_length(ta_arr) {
                    if let Value::Object(Some(ta)) = ctx.get_array_element(ta_arr, j) {
                        if let Some(arg) = jdk_typearg_to_typearg(ctx, ta) {
                            out.push(arg);
                        }
                    }
                }
            }
        }
        out
    }
    // Map a `sun.reflect.generics.tree.TypeArgument` (a `Wildcard`, or an
    // exact `FieldTypeSignature`) to CratonVM's `TypeArg`.
    fn jdk_typearg_to_typearg(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        ta: cratonvm_types::ObjectRef,
    ) -> Option<crate::generics::TypeArg> {
        use crate::generics::{TypeArg, TypeSig};
        let cid = ctx.class_id_of_object(ta);
        let cls = ctx.class_name_of_id(cid).unwrap_or_default();
        if cls.ends_with("Wildcard") {
            // `? super X`  -> lowerBounds=[X] (X not BottomSignature)
            // `? extends X`-> lowerBounds=[Bottom], upperBounds=[X != Object]
            // `?`          -> upperBounds=[Object], lowerBounds=[Bottom]
            if let Some(lo) = jdk_first_non_bottom_bound(ctx, ta, "lowerBounds") {
                return Some(TypeArg::Super(jdk_tree_to_typesig(ctx, lo)?));
            }
            if let Some(up) = jdk_first_non_bottom_bound(ctx, ta, "upperBounds") {
                let ts = jdk_tree_to_typesig(ctx, up)?;
                if let TypeSig::Class {
                    name, type_args, ..
                } = &ts
                {
                    if name == "java/lang/Object" && type_args.is_empty() {
                        return Some(TypeArg::Unbounded);
                    }
                }
                return Some(TypeArg::Extends(ts));
            }
            return Some(TypeArg::Unbounded);
        }
        Some(TypeArg::Exact(jdk_tree_to_typesig(ctx, ta)?))
    }
    // First bound node in `field` that is not a `BottomSignature` placeholder.
    fn jdk_first_non_bottom_bound(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        wildcard: cratonvm_types::ObjectRef,
        field: &str,
    ) -> Option<cratonvm_types::ObjectRef> {
        let arr = match ctx.get_field_by_name(wildcard, field) {
            Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
            _ => return None,
        };
        for i in 0..ctx.array_length(arr) {
            if let Value::Object(Some(b)) = ctx.get_array_element(arr, i) {
                let bcls = ctx
                    .class_name_of_id(ctx.class_id_of_object(b))
                    .unwrap_or_default();
                if bcls.ends_with("BottomSignature") {
                    continue;
                }
                return Some(b);
            }
        }
        None
    }
    fn empty_annotation_array(ctx: &mut dyn cratonvm_native_api::registry::NativeContext) -> Value {
        let cid = ctx
            .class_id_by_name("java/lang/annotation/Annotation")
            .or_else(|| {
                ctx.ensure_class_initialized("java/lang/annotation/Annotation")
                    .ok()
            })
            .unwrap_or(cratonvm_types::ClassId::new(0));
        Value::Object(Some(ctx.new_ref_array(cid, 0)))
    }
    fn empty_annotation_array_for_type(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        args: &[Value],
    ) -> Value {
        let cid = match args.get(1) {
            Some(Value::Object(Some(mirror))) => ctx
                .class_id_from_mirror(*mirror)
                .unwrap_or_else(|| cratonvm_types::ClassId::new(0)),
            _ => cratonvm_types::ClassId::new(0),
        };
        Value::Object(Some(ctx.new_ref_array(cid, 0)))
    }
    fn register_type_variable_annotation_natives(
        registry: &mut NativeMethodRegistry,
        class_name: &'static str,
    ) {
        // KEEP the null RESULT (constant, justified): `getAnnotation` /
        // `getDeclaredAnnotation` return null when the requested annotation is
        // ABSENT — that is the spec'd answer, not a stub. CratonVM's synthetic
        // TypeVariable / AnnotatedType carriers hold no annotation storage at
        // all (`getAnnotatedBounds` in `lang_class.rs` builds AnnotatedTypes
        // with a fixed EMPTY `allOnSameTargetTypeAnnotations`), so "absent" is
        // true for every query, and the sibling `getAnnotations` /
        // `getDeclaredAnnotations` below return a matching EMPTY array (never
        // null) — the pair is self-consistent for a caller that checks both.
        // Revisit only if those carriers ever gain real annotation data; then
        // these two must scan it instead of answering null.
        //
        // Wave 3: what was NOT spec-correct is the argument handling.
        // `AnnotatedElement.getAnnotation`/`getDeclaredAnnotation` are
        // documented to throw NullPointerException for a null annotation class
        // (real `TypeVariableImpl` opens with `Objects.requireNonNull`), and
        // swallowing that turned a caller bug into an indistinguishable
        // "annotation absent". Reject null; keep null for a genuine miss.
        fn annotation_query_null_checked(
            _ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
            match args.get(1) {
                Some(Value::Object(Some(_))) => Ok(Some(Value::Object(None))),
                _ => Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("annotationClass is null".into()),
                }
                .into()),
            }
        }
        registry.register(
            class_name,
            "getAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            annotation_query_null_checked,
        );
        registry.register(
            class_name,
            "getDeclaredAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            annotation_query_null_checked,
        );
        registry.register(
            class_name,
            "getAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
            |ctx, _args| Ok(Some(empty_annotation_array(ctx))),
        );
        registry.register(
            class_name,
            "getDeclaredAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
            |ctx, _args| Ok(Some(empty_annotation_array(ctx))),
        );
        registry.register(
            class_name,
            "getAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            |ctx, args| Ok(Some(empty_annotation_array_for_type(ctx, args))),
        );
        registry.register(
            class_name,
            "getDeclaredAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            |ctx, args| Ok(Some(empty_annotation_array_for_type(ctx, args))),
        );
    }
    let tvi_real = "sun/reflect/generics/reflectiveObjects/TypeVariableImpl";
    register_type_variable_annotation_natives(registry, tvi_real);
    registry.register(tvi_real, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `()Ljava/lang/String;` — see `getRawType` above.
        Ok(Some(crate::field_read::ref_field(ctx, this, "name")))
    });
    // getBounds() reifies the `bounds` field (a volatile Object[] holding the
    // unreified sun.reflect…FieldTypeSignature nodes). The JDK bytecode does the
    // same lazily, but running it reflectively (Spring's SerializableTypeWrapper
    // proxy) returned the raw FieldTypeSignature[] → ClassCastException to Type[]
    // (ResolvableTypeTests.identifyTypeVariable + the bounded-type-variable cases).
    // Reify ourselves via the same helper the WildcardTypeImpl bounds use; it now
    // always returns a java/lang/reflect/Type[] (incl. the empty case).
    registry.register(
        tvi_real,
        "getBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(wti_bounds_reified(ctx, this, "bounds")?))
        },
    );
    registry.register(
        tvi_real,
        "getAnnotatedBounds",
        "()[Ljava/lang/reflect/AnnotatedType;",
        crate::lang_class::native_type_variable_get_annotated_bounds,
    );
    registry.register(tvi_real, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(s)) => Value::Object(Some(s)),
            _ => Value::Object(Some(ctx.create_string("?"))),
        }))
    });
    registry.register(
        tvi_real,
        "getTypeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(ctx.create_string("?"))),
            }))
        },
    );
    let wti_real = "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl";
    registry.register(
        wti_real,
        "getUpperBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(wti_bounds_reified(ctx, this, "upperBounds")?))
        },
    );
    registry.register(
        wti_real,
        "getLowerBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(wti_bounds_reified(ctx, this, "lowerBounds")?))
        },
    );
    // …and the two rendering accessors, which `pti_real`/`tvi_real` above both
    // have and this one did not. In a synthetic-library build there is no
    // `WildcardTypeImpl` bytecode to fall back on, so `wildcard.toString()`
    // raised `NoSuchMethodError: …WildcardTypeImpl.getTypeName()` and printed
    // `…WildcardTypeImpl@6`, where HotSpot prints `? extends java.lang.Number`.
    // `render_type_name` knows the shape (it grew a matching arm for this
    // class); route both names to it, exactly as `pti_real` does.
    registry.register(wti_real, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    });
    registry.register(
        wti_real,
        "getTypeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        },
    );
    let gat_real = "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl";
    registry.register(
        gat_real,
        "getGenericComponentType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `()Ljava/lang/reflect/Type;` — see `getRawType` above.
            Ok(Some(crate::field_read::ref_field(
                ctx,
                this,
                "genericComponentType",
            )))
        },
    );
    register_type_variable_annotation_natives(registry, "java/lang/reflect/TypeVariable");
    registry.register(
        "java/lang/reflect/TypeVariable",
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "getBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "getAnnotatedBounds",
        "()[Ljava/lang/reflect/AnnotatedType;",
        crate::lang_class::native_type_variable_get_annotated_bounds,
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "getGenericDeclaration",
        "()Ljava/lang/reflect/GenericDeclaration;",
        |ctx, args| {
            // Field 2 = the declaring Class/Executable (set in
            // generics::type_param_to_java). ByteBuddy's mock generation
            // requires this to be non-null.
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) > 2 {
                Ok(Some(ctx.get_field(this, 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(ctx.create_string("?"))),
            }))
        },
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "getTypeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(ctx.create_string("?"))),
            }))
        },
    );
    // JDK `TypeVariableImpl.equals` compares by (genericDeclaration, name);
    // `hashCode` is `genericDeclaration.hashCode() ^ name.hashCode()`. Our
    // synthetic `TypeVariable` objects are distinct instances per signature
    // conversion (the `T` from `getTypeParameters()` and the `T` reused in a
    // `getGenericInterfaces()` parameterization are NOT the same object as they
    // are on HotSpot), so without these natives they fall back to `Object`
    // identity equality and never compare equal. That breaks any library that
    // resolves a type variable across a class hierarchy — e.g. Hibernate
    // Validator matching `ConstraintValidator<A,T>`'s `T` against the declaring
    // class's type parameter to discover the validated type
    // (`HV000030: No validator could be found ...`).
    registry.register(
        "java/lang/reflect/TypeVariable",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if this == other {
                return Ok(Some(Value::Int(1)));
            }
            // `this` is a synthetic TypeVariable (name=field0, decl=field2).
            let this_name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            };
            let this_decl = ctx.get_field(this, 2);
            // `other` may be EITHER another synthetic TypeVariable OR the *real*
            // `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` — JDK 25's
            // `Class.getTypeParameters()` returns the real impl while our
            // `getGenericInterfaces()` parameterization returns the synthetic one,
            // so a generics resolver (Hibernate Validator) compares the two. Read
            // the other's name/declaration from whichever representation it is.
            let other_cls = ctx
                .class_name_of_id(ctx.class_id_of_object(other))
                .unwrap_or_default();
            let (other_name, other_decl) = if other_cls == "java/lang/reflect/TypeVariable" {
                (
                    match ctx.get_field(other, 0) {
                        Value::Object(Some(s)) => ctx.read_string(s),
                        _ => None,
                    },
                    ctx.get_field(other, 2),
                )
            } else {
                // Real TypeVariableImpl (or anything exposing the JDK field names).
                let n = match ctx.get_field_by_name(other, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                if n.is_none() {
                    // Not a type variable we can compare against.
                    return Ok(Some(Value::Int(0)));
                }
                (n, ctx.get_field_by_name(other, "genericDeclaration"))
            };
            // genericDeclaration: Class/Executable mirrors are canonical, so
            // identity comparison matches JDK `TypeVariableImpl.equals`.
            let decl_eq = match (this_decl, other_decl) {
                (Value::Object(a), Value::Object(b)) => a == b && a.is_some(),
                _ => false,
            };
            let name_eq = this_name.is_some() && this_name == other_name;
            Ok(Some(Value::Int(if decl_eq && name_eq { 1 } else { 0 })))
        },
    );
    registry.register(
        "java/lang/reflect/TypeVariable",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let decl_hash = if ctx.object_num_fields(this) > 2 {
                match ctx.get_field(this, 2) {
                    Value::Object(Some(d)) => ctx.identity_hash_code(d),
                    _ => 0,
                }
            } else {
                0
            };
            let name_hash = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .map(|n| {
                        // java.lang.String.hashCode over UTF-16 code units.
                        let mut h: i32 = 0;
                        for u in n.encode_utf16() {
                            h = h.wrapping_mul(31).wrapping_add(u as i32);
                        }
                        h
                    })
                    .unwrap_or(0),
                _ => 0,
            };
            Ok(Some(Value::Int(decl_hash ^ name_hash)))
        },
    );
    registry.register(
        "java/lang/reflect/WildcardType",
        "getUpperBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    registry.register(
        "java/lang/reflect/WildcardType",
        "getLowerBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    registry.register(
        "java/lang/reflect/GenericArrayType",
        "getGenericComponentType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    // JDK `GenericArrayTypeImpl` compares by the generic COMPONENT type and
    // nothing else - `equals` is `Objects.equals(component, other.component)`,
    // `hashCode` is `Objects.hashCode(component)`. Without these two the
    // synthetic `GenericArrayType` falls through to the `Object.equals` /
    // `Object.hashCode` natives, which compare reflection stubs by their
    // RENDERED TYPE NAME. A rendered name cannot tell `S[]` declared on one
    // class from `S[]` declared on another: both render `S[]`, so they
    // compared EQUAL and hashed alike, where HotSpot answers not-equal
    // because the components are `TypeVariable`s carrying different generic
    // declarations. The sibling `TypeVariable` natives above already do it
    // the JDK way, which is why only the array wrapper was wrong.
    //
    // The blast radius is a cache. Spring's `SerializableTypeWrapper` keys
    // every wrapped `Type` in a static map by the `Type` itself, so the
    // collision served the FIRST `S[]`'s proxy for the SECOND's. The second
    // resolver then held a type variable belonging to a class it knows
    // nothing about, `S` stayed unresolved, and an `@Autowired S[]` field
    // widened to `Object[]` - every bean in the factory got injected.
    // `AutowiredAnnotationBeanPostProcessorTests
    // .genericsBasedFieldInjectionWithSubstitutedVariables` is the witness: 4
    // beans where 1 is expected, and ONLY when the method-injection test ran
    // first in the same JVM, which is why it passes when run alone.
    registry.register(
        "java/lang/reflect/GenericArrayType",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if this == other {
                return Ok(Some(Value::Int(1)));
            }
            // `other` is either another synthetic stub (component in slot 0) or
            // the real `sun.reflect.generics.reflectiveObjects
            // .GenericArrayTypeImpl` (named field). Anything without a readable
            // component is not a generic array type: not equal, and no
            // speculative virtual call that could leave an exception pending.
            let (Some(a), Some(b)) = (
                generic_array_component(ctx, this),
                generic_array_component(ctx, other),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            if a == b {
                return Ok(Some(Value::Int(1)));
            }
            let eq = ctx.invoke_virtual(
                a,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(b))],
            )?;
            Ok(Some(Value::Int(i32::from(
                matches!(eq, Some(Value::Int(v)) if v != 0),
            ))))
        },
    );
    registry.register(
        "java/lang/reflect/GenericArrayType",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `Objects.hashCode(component)` — 0 for a missing component, and
            // a VIRTUAL call so a `TypeVariable` component contributes its
            // (declaration, name) hash rather than an identity hash. Anything
            // else would break the equals/hashCode contract this pair now
            // establishes.
            let Some(c) = generic_array_component(ctx, this) else {
                return Ok(Some(Value::Int(0)));
            };
            let h = match ctx.invoke_virtual(c, "hashCode", "()I", &[])? {
                Some(Value::Int(v)) => v,
                _ => 0,
            };
            Ok(Some(Value::Int(h)))
        },
    );

    // The same JDK contract for the other two synthetic type stubs. They shared
    // the `GenericArrayType` defect for the same reason - no bytecode
    // `equals`/`hashCode`, so `Object.equals` compared them by RENDERED NAME -
    // and they are reached THROUGH the array wrapper: `Repository<S>[]` is a
    // `GenericArrayType` whose component is a `ParameterizedType`, so fixing
    // only the wrapper still let `Repository<S>` declared on one class compare
    // equal to `Repository<S>` declared on another.
    //
    // `ParameterizedTypeImpl`: equal iff ownerType, rawType and the
    // actualTypeArguments all match; hash is
    // `Arrays.hashCode(args) ^ hash(owner) ^ hash(raw)`.
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if this == other {
                return Ok(Some(Value::Int(1)));
            }
            const STUB: &str = "java/lang/reflect/ParameterizedType";
            let (Some(ra), Some(rb)) = (
                reflect_type_slot(ctx, this, STUB, 0, "rawType"),
                reflect_type_slot(ctx, other, STUB, 0, "rawType"),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            if !type_value_equals(ctx, ra, rb)? {
                return Ok(Some(Value::Int(0)));
            }
            let (Some(oa), Some(ob)) = (
                reflect_type_slot(ctx, this, STUB, 2, "ownerType"),
                reflect_type_slot(ctx, other, STUB, 2, "ownerType"),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            if !type_value_equals(ctx, oa, ob)? {
                return Ok(Some(Value::Int(0)));
            }
            let (Some(aa), Some(ab)) = (
                reflect_type_slot(ctx, this, STUB, 1, "actualTypeArguments"),
                reflect_type_slot(ctx, other, STUB, 1, "actualTypeArguments"),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            Ok(Some(Value::Int(i32::from(type_array_equals(ctx, aa, ab)?))))
        },
    );
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            const STUB: &str = "java/lang/reflect/ParameterizedType";
            let a = reflect_type_slot(ctx, this, STUB, 1, "actualTypeArguments")
                .unwrap_or(Value::Object(None));
            let o =
                reflect_type_slot(ctx, this, STUB, 2, "ownerType").unwrap_or(Value::Object(None));
            let r = reflect_type_slot(ctx, this, STUB, 0, "rawType").unwrap_or(Value::Object(None));
            let h = type_array_hash(ctx, a)? ^ type_value_hash(ctx, o)? ^ type_value_hash(ctx, r)?;
            Ok(Some(Value::Int(h)))
        },
    );
    // `WildcardTypeImpl`: equal iff both bound arrays match; hash is
    // `Arrays.hashCode(lower) ^ Arrays.hashCode(upper)`.
    registry.register(
        "java/lang/reflect/WildcardType",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if this == other {
                return Ok(Some(Value::Int(1)));
            }
            const STUB: &str = "java/lang/reflect/WildcardType";
            let (Some(ua), Some(ub)) = (
                reflect_type_slot(ctx, this, STUB, 0, "upperBounds"),
                reflect_type_slot(ctx, other, STUB, 0, "upperBounds"),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            if !type_array_equals(ctx, ua, ub)? {
                return Ok(Some(Value::Int(0)));
            }
            let (Some(la), Some(lb)) = (
                reflect_type_slot(ctx, this, STUB, 1, "lowerBounds"),
                reflect_type_slot(ctx, other, STUB, 1, "lowerBounds"),
            ) else {
                return Ok(Some(Value::Int(0)));
            };
            Ok(Some(Value::Int(i32::from(type_array_equals(ctx, la, lb)?))))
        },
    );
    registry.register(
        "java/lang/reflect/WildcardType",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            const STUB: &str = "java/lang/reflect/WildcardType";
            let u =
                reflect_type_slot(ctx, this, STUB, 0, "upperBounds").unwrap_or(Value::Object(None));
            let l =
                reflect_type_slot(ctx, this, STUB, 1, "lowerBounds").unwrap_or(Value::Object(None));
            let h = type_array_hash(ctx, l)? ^ type_array_hash(ctx, u)?;
            Ok(Some(Value::Int(h)))
        },
    );

    let _ = create_field_object; // silence unused-import lint (used in tests)
    let _ = read_field_meta;
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests — see also native-builtins/tests/wp2_1_reflect.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Smoke: registry exposes the WP2.1 surface without panicking and
    /// without duplicate-method warnings.
    #[test]
    fn register_wp2_1_natives_smoke() {
        let mut registry = NativeMethodRegistry::new();
        register_wp2_1_natives(&mut registry);
        // Registry should have at least all the new methods we added (~20).
        assert!(
            registry.len() >= 20,
            "expected at least 20 WP2.1 natives, got {}",
            registry.len(),
        );
    }
}
