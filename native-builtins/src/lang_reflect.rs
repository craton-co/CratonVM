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

use crate::lang_class::ClassTextName;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::lang_class::{
    annotation_element_to_java, box_value, create_constructor_object, create_field_object,
    create_method_object, descriptor_to_class_mirror, method_class_name_desc, method_clazz_value,
    method_descriptor_for_invoke, method_exception_types_value, method_modifiers_value,
    method_name_value, method_parameter_types_value, method_return_type_value, mirror_class_id,
    mirror_class_name, parse_descriptor_param_and_return, read_constructor_descriptor,
    read_field_meta, read_method_descriptor,
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
// Per spec, returns true iff the override flag could be set -- the verdict
// of the same gate `setAccessible(true)` throws from
// (`lang_class::set_accessible_allowed`) -- AND writes the flag through to
// the appropriate extra slot.

pub(crate) fn native_method_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `trySetAccessible` reports failure instead of throwing: the full
    // `setAccessible(true)` gate's verdict (interpreter round i4 wave 86,
    // lane L5).
    if !crate::lang_class::set_accessible_allowed(ctx, this)? {
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
    // `trySetAccessible` reports failure instead of throwing: the full
    // `setAccessible(true)` gate's verdict (interpreter round i4 wave 86,
    // lane L5).
    if !crate::lang_class::set_accessible_allowed(ctx, this)? {
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
    // `trySetAccessible` reports failure instead of throwing: the full
    // `setAccessible(true)` gate's verdict (interpreter round i4 wave 86,
    // lane L5).
    if !crate::lang_class::set_accessible_allowed(ctx, this)? {
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
    // `trySetAccessible` reports failure instead of throwing: the full
    // `setAccessible(true)` gate's verdict (interpreter round i4 wave 86,
    // lane L5).
    if !crate::lang_class::set_accessible_allowed(ctx, this)? {
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
    // An array's header carries its COMPONENT's class id, so the id tests
    // below read an `H[]` as an `H` and admitted it for a member declared in
    // `H`. An array inherits instance members from `Object` alone (its two
    // interfaces declare none), so any other declaring class refuses it
    // (r11w14 lane vh, the reflective twin of
    // r11w13-orch-varhandle-set-on-an-array-receiver-throws-aioobe-not-cce).
    let array_receiver = ctx.heap_kind_of(receiver) == cratonvm_types::ObjectKind::Array;
    if array_receiver {
        if ctx.class_name_of_id(declaring_id).as_deref() == Some("java/lang/Object") {
            return Ok(());
        }
    } else {
        // `is_subclass` is full assignability — it walks interfaces as well as
        // superclasses, so a default method's declaring INTERFACE is matched by
        // an implementing receiver, and lambda proxies route through
        // `lambda_proxy_satisfies`. That is `Class.isInstance`, which is the
        // test HotSpot makes.
        if ctx.is_subclass(receiver_id, declaring_id) {
            return Ok(());
        }
        // A negative from `is_subclass` is only trustworthy when the
        // receiver's hierarchy is READABLE: `is_subclass_or_unreadable` returns
        // false exactly when the superclass chain terminated at a real
        // `java/lang/Object` without meeting the ancestor. Anything else is
        // "cannot tell", and this throw is new on a path that previously only
        // ever returned `false` — a fabricated stand-in with no modelled
        // supertype chain must not be the thing that invents an exception.
        if is_subclass_or_unreadable(ctx, receiver_id, declaring_id) {
            return Ok(());
        }
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
pub(crate) fn loader_scoped_enclosing_lookup(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    loader_scoped_enclosing_lookup_checked(ctx, class_id, name, false)
        .ok()
        .flatten()
}

/// [`loader_scoped_enclosing_lookup`] that, with `propagate` and
/// `lang_class::outer_resolution_errors_apply`
/// (`CRATONVM_CLASS_OUTER_RESOLUTION_ERRORS`, default ON), answers the class's
/// own constant-pool resolution error instead of the flat fallback: HotSpot's
/// `JVM_GetEnclosingMethodInfo` resolves the enclosing class with `klass_at`
/// and `CHECK_NULL` (JIT round 18 wave 11, lane mhresid; MHLOADER18-4). Every
/// success answers what [`loader_scoped_enclosing_lookup`] answers.
pub(crate) fn loader_scoped_enclosing_lookup_checked(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
    propagate: bool,
) -> Result<Option<ClassId>, MethodCallFailed> {
    let loader_id = ctx.loader_id_of_class(class_id);
    if loader_id > 2 {
        // JIT round 21 wave 26, lane nesting
        // (`CRATONVM_ENCLOSING_LOOKUP_STICKY_FIRST`): a resolution error
        // already recorded for this entry is thrown again BEFORE the loader's
        // own definitions are consulted. JVMS 5.4.3: once the class's
        // constant-pool entry failed, `klass_at` rethrows that error even
        // after the same loader defined the class (`R21Fz9ReviewEmUser1`'s
        // "$L after" lines on HotSpot 25; CratonVM's exact-namespace probe
        // below answered the late definition, "L").
        if propagate
            && crate::lang_class::enclosing_lookup_sticky_first_enabled()
            && crate::lang_class::class_outer_via_own_loader_enabled()
            && crate::lang_class::outer_resolution_errors_apply(ctx, class_id)
        {
            if let Some(recorded) =
                crate::lang_class::recorded_outer_resolution_failure_of(ctx, class_id, name)
            {
                return Err(recorded);
            }
        }
        if let Some(id) = ctx.class_id_defined_by_loader_exact(name, loader_id as u32) {
            return Ok(Some(id));
        }
    }
    // JIT round 18 wave 9, lane mhloader (`CRATONVM_CLASS_OUTER_VIA_OWN_LOADER`,
    // default on): HotSpot's `JVM_GetEnclosingMethodInfo` resolves the name
    // through the class's own constant pool (`klass_at`): the class's defining
    // loader decides, and the enclosing class is loaded, NOT initialized. The
    // flat lookup below answered the first-loaded copy for a user-loader class
    // whose loader had not asked for the name yet, and its by-name fallback ran
    // the enclosing class's `<clinit>` (`R18MhloaderOuter enclosing-no-clinit`).
    if crate::lang_class::class_outer_via_own_loader_enabled() {
        return match crate::lang_class::class_seen_by_referencing_class_checked(ctx, class_id, name)
        {
            Ok(id) => Ok(Some(id)),
            Err(failed)
                if propagate && crate::lang_class::outer_resolution_errors_apply(ctx, class_id) =>
            {
                Err(failed)
            }
            Err(_) => Ok(ctx.class_id_by_name(name)),
        };
    }
    Ok(ctx
        .class_id_by_name(name)
        .or_else(|| ctx.ensure_class_initialized(name).ok()))
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
    //
    // JIT round 18 wave 11 (lane mhresid; MHLOADER18-4): the JDK body reads
    // `getEnclosingMethod0`, whose resolution error HotSpot throws.
    if let Some((enc_class, _name, _desc)) = ctx.enclosing_method(class_id) {
        if let Some(enc_id) =
            loader_scoped_enclosing_lookup_checked(ctx, class_id, &enc_class, true)?
        {
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
    //
    // JIT round 17 wave 8, lane mh7: the JDK body reaches `getDeclaringClass0`
    // here, and HotSpot's InnerClasses consistency check with it
    // (`CRATONVM_CLASS_INNER_CLASSES_CHECK`); `--compatible` dispatches this bridge.
    //
    // JIT round 18 wave 11 (lane mhresid; MHLOADER18-4): and that
    // `getDeclaringClass0`'s outer-class resolution error, first.
    if crate::lang_class::outer_resolution_errors_apply(ctx, class_id)
        && ctx.array_component_class_id(class_id).is_none()
    {
        crate::lang_class::declaring_class_id_checked(ctx, class_id, true)?;
    }
    if let Some(icce) = crate::lang_class::declaring_class_icce(ctx, class_id) {
        return Err(icce);
    }
    let this_name = ctx.class_name_of_id(class_id);
    if let Some(this_name) = this_name {
        // JIT round 21 wave 19, lane innerclasses (page
        // `r21-review-w18-findings-FIXED-20261020.md` item 2): the FIRST self row
        // decides, as `getDeclaringClass0`'s (`lang_class::member_row_outer_of`,
        // `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`).
        let entries = ctx.inner_classes(class_id);
        let member_outer = crate::lang_class::member_row_outer_of(&*ctx, &entries, &this_name);
        if let Some(outer) = member_outer.as_ref() {
            {
                if let Some(outer_id) = loader_scoped_enclosing_lookup(ctx, class_id, outer) {
                    // JDK 25's `getEnclosingClass` is `getDeclaringClass0()`
                    // for a member class, which asks the outer class's
                    // `InnerClasses` to agree (interpreter round i2 wave 59,
                    // lane L3; `CRATONVM_INNER_CLASSES_AGREEMENT`).
                    if let Some(refusal) = crate::lang_class::inner_classes_disagreement_refusal(
                        ctx, class_id, outer_id,
                    ) {
                        return Err(refusal);
                    }
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

/// `Parameter.getName()` as the JDK body answers it: the stored name, or
/// `"arg" + index` when it is null or empty (a `MethodParameters` entry with
/// no name keeps a null `name`; interpreter round i4 wave 85, lane L3). The
/// real-layout `name` / `index` fields first, the legacy slot 0 otherwise.
pub(crate) fn native_parameter_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let real_layout = crate::field_read::declares_field(&*ctx, this, "index");
    let name = if real_layout {
        crate::field_read::ref_field_obj(&*ctx, this, "name")
    } else {
        match ctx.get_field(this, 0) {
            Value::Object(o) => o,
            _ => None,
        }
    };
    if let Some(s) = name {
        if ctx.read_string(s).is_some_and(|t| !t.is_empty()) {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    let index = if real_layout {
        crate::field_read::int_field_strict(&*ctx, this, "index").unwrap_or(0)
    } else {
        0
    };
    let s = ctx.create_string_uninterned(&format!("arg{index}"));
    Ok(Some(Value::Object(Some(s))))
}

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
    // Interpreter round i4 wave 85, lane L3: the synthesizing natives
    // (`Executable/Method/Constructor.getParameters`, which `--compatible`
    // dispatches) answer what the JDK's `privateGetParameters` would: the
    // `getParameters0` array when the attribute exists -- a nameless entry
    // with a NULL name, so `isNamePresent()` (`hasRealParameterData() && name
    // != null`) is false as on HotSpot -- and `argN` only when it does not.
    // Naming such an entry `argN` made `isNamePresent()` true once
    // `getParameters0` started answering the attribute (wave 84;
    // `L3W84ParameterFlags --compatible`).
    build_parameter_array_as(ctx, declaring_executable, descriptor, true)
}

/// [`build_parameter_array`]; with `unnamed_is_null` a `MethodParameters`
/// entry whose `name_index` is 0 gets a NULL `name`, as HotSpot's
/// `Reflection::new_parameter` leaves it (`Executable.getParameters0`'s
/// answer: `Parameter.getName()` then says `argN` and `isNamePresent()`
/// false, while the entry's flags -- javac 21+ emits the attribute without
/// names for MANDATED / SYNTHETIC parameters -- stay real). Without it such
/// an entry is named `argN` (the synthesized-array natives' answer).
fn build_parameter_array_as(
    ctx: &mut dyn NativeContext,
    declaring_executable: ObjectRef,
    descriptor: &str,
    unnamed_is_null: bool,
) -> ObjectRef {
    let (param_descs, _) = parse_descriptor_param_and_return(descriptor);

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
    let rows: Vec<(Option<String>, i32)> = (0..param_descs.len())
        .map(|i| match parameter_meta.get(i) {
            Some((n, flags)) if !n.is_empty() => (Some(n.clone()), *flags as i32),
            Some((_, flags)) if unnamed_is_null => (None, *flags as i32),
            Some((_, flags)) => (Some(format!("arg{i}")), *flags as i32),
            None => (Some(format!("arg{i}")), 0),
        })
        .collect();
    build_parameter_array_rows(ctx, declaring_executable, descriptor, &rows)
}

/// One `Parameter` per row of `rows` (`name`, `modifiers`), in a `Parameter[]`
/// of exactly `rows.len()` elements -- which for `getParameters0` is the
/// `MethodParameters` attribute's count, NOT the descriptor's (interpreter
/// round i5 wave 101, lane L3: HotSpot's `JVM_GetMethodParameters` sizes the
/// array by the attribute, and the JDK's `verifyParameters` then throws
/// `MalformedParametersException("Wrong number of parameters in
/// MethodParameters attribute")` on a mismatch). A row past the descriptor's
/// parameters gets no type mirror in the synthetic slot layout.
fn build_parameter_array_rows(
    ctx: &mut dyn NativeContext,
    declaring_executable: ObjectRef,
    descriptor: &str,
    rows: &[(Option<String>, i32)],
) -> ObjectRef {
    let (param_descs, _) = parse_descriptor_param_and_return(descriptor);

    // gen r4w3/rooting: `ensure_class_initialized` runs Parameter.<clinit>
    // (a GC point). Pin the caller's executable across it and allocate
    // `arr` only afterwards, so neither names a vacated from-space address.
    let declaring_executable_pin = ctx.pin_native_root(declaring_executable);
    let parameter_class_id = ctx
        .ensure_class_initialized("java/lang/reflect/Parameter")
        .unwrap_or(ClassId::new(0));
    let declaring_executable = ctx.read_native_pin(declaring_executable_pin, declaring_executable);

    // `Parameter[]`, the declared type (i6-L2): `Executable.getParameters`
    // hands out a clone, and a caller's `(Parameter[])` cast must hold.
    let arr = ctx.new_ref_array(parameter_class_id, rows.len());

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
    // (`declaring_executable` is already pinned above, below `arr_pin`.)
    let arr_pin = ctx.pin_native_root(arr);

    for (i, (name_str, modifiers)) in rows.iter().enumerate() {
        let modifiers = *modifiers;
        let p = ctx.alloc_object(parameter_class_id, alloc_fields);
        let p_pin = ctx.pin_native_root(p);

        let name = name_str.as_ref().map(|s| {
            let name = ctx.create_string(s);
            (name, ctx.pin_native_root(name))
        });
        let type_mirror = param_descs
            .get(i)
            .map(|pdesc| descriptor_to_class_mirror(ctx, pdesc));

        let p = ctx.read_native_pin(p_pin, p);
        let name = name.map(|(name, name_pin)| ctx.read_native_pin(name_pin, name));
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
        ctx.set_field_by_name(p, "name", Value::Object(name));
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
            ctx.set_field(p, 0, Value::Object(name));
            ctx.set_field(p, 1, Value::Int(modifiers));
            ctx.set_field(p, 2, Value::Object(type_mirror));
            ctx.set_field(p, 3, Value::Object(Some(declaring_executable)));
        }

        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(p)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    // gen r4w3/rooting: the executable pin is the lowest handle; releasing
    // it also releases `arr_pin`.
    ctx.unpin_native_roots(declaring_executable_pin);
    arr
}

/// What `Executable.privateGetParameters` throws for a malformed
/// `MethodParameters` attribute before it hands out an array, for the
/// synthesizing natives (`--compatible`; interpreter round i5 wave 101, lane
/// L3, `L3W101MalformedMethodParameters`): first `JVM_GetMethodParameters`'
/// `IllegalArgumentException` for a bad `name_index`, then the JDK's
/// `verifyParameters` (`MalformedParametersException`: the entry count against
/// the descriptor's, each name's legality, each entry's flags). `Ok` when the
/// member has no attribute.
fn verify_method_parameters(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    descriptor: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::MethodParameterName as N;
    use cratonvm_types::error::MethodCallFailed;
    fn malformed(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
        match crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/reflect/MalformedParametersException",
            1,
            message,
        ) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        }
    }
    let Some((class_id, method_name)) = executable_class_and_name(ctx, this) else {
        return Ok(());
    };
    let Some(entries) = ctx.method_parameters_attribute(class_id, &method_name, descriptor) else {
        return Ok(());
    };
    for (name, _) in &entries {
        let message = match name {
            N::OutOfBounds => "Constant pool index out of bounds",
            N::NotUtf8 => "Wrong type at constant pool index",
            N::Unnamed | N::Name(_) => continue,
        };
        return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: message.to_string(),
        }
        .into());
    }
    let (param_descs, _) = parse_descriptor_param_and_return(descriptor);
    if param_descs.len() != entries.len() {
        return Err(malformed(
            ctx,
            "Wrong number of parameters in MethodParameters attribute",
        ));
    }
    // `Modifier.FINAL | Modifier.SYNTHETIC | Modifier.MANDATED`.
    const MASK: u16 = 0x0010 | 0x1000 | 0x8000;
    for (name, flags) in &entries {
        if let N::Name(n) = name {
            if n.is_empty() || n.contains(|c: char| matches!(c, '.' | ';' | '[' | '/')) {
                return Err(malformed(ctx, &format!("Invalid parameter name \"{n}\"")));
            }
        }
        if flags & !MASK != 0 {
            return Err(malformed(ctx, "Invalid parameter modifiers"));
        }
    }
    Ok(())
}

pub(crate) fn native_method_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = read_method_descriptor(ctx, this).unwrap_or_default();
    verify_method_parameters(ctx, this, &descriptor)?;
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_constructor_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = read_constructor_descriptor(ctx, this).unwrap_or_default();
    verify_method_parameters(ctx, this, &descriptor)?;
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

/// The descriptor of a `Method` or `Constructor` mirror. A `Constructor` is
/// asked for its OWN descriptor first: `read_method_descriptor` knows nothing
/// about a constructor's tail layout and on a real-layout `Constructor` it can
/// answer a descriptor composed from fields the class does not have, which made
/// `Executable.getParameters0` see no `MethodParameters` entry for constructors
/// (SBR2 SPRINGAPP: `Constructor.getParameters()` over the real bytecode lost
/// every `-parameters` name).
pub(crate) fn executable_descriptor(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    let cid = ctx.class_id_of_object(this);
    let is_ctor = ctx.class_name_of_id(cid).as_deref() == Some("java/lang/reflect/Constructor");
    if is_ctor {
        read_constructor_descriptor(ctx, this)
            .or_else(|| read_method_descriptor(ctx, this))
            .unwrap_or_default()
    } else {
        read_method_descriptor(ctx, this)
            .or_else(|| read_constructor_descriptor(ctx, this))
            .unwrap_or_default()
    }
}

pub(crate) fn native_executable_get_parameters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let descriptor = executable_descriptor(ctx, this);
    verify_method_parameters(ctx, this, &descriptor)?;
    let arr = build_parameter_array(ctx, this, &descriptor);
    Ok(Some(Value::Object(Some(arr))))
}

/// `Executable.getParameters0()`'s array once the member HAS a
/// `MethodParameters` attribute: HotSpot's `JVM_GetMethodParameters`, an entry
/// with no name answered with a null `name` and its real flags (interpreter
/// round i4 wave 84, lane L3), built from the attribute's own entries
/// (interpreter round i5 wave 101, lane L3): one `Parameter` per ENTRY, a
/// `None` name (`name_index` 0) left null and an empty name kept empty, as
/// `JVM_GetMethodParameters` hands them to `verifyParameters`.
pub(crate) fn executable_parameters0_array_from_entries(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    descriptor: &str,
    entries: &[(Option<String>, u16)],
) -> ObjectRef {
    let rows: Vec<(Option<String>, i32)> = entries
        .iter()
        .map(|(name, flags)| (name.clone(), *flags as i32))
        .collect();
    build_parameter_array_rows(ctx, this, descriptor, &rows)
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
    // JIT round 18 wave 9, lane mhloader: the enclosing class as the class's
    // own constant pool resolves it (`CRATONVM_CLASS_OUTER_VIA_OWN_LOADER`).
    // Round 18 wave 11 (lane mhresid; MHLOADER18-4): resolved BEFORE the
    // initializer test, its error thrown, as the JDK body's
    // `getEnclosingMethod0` (`klass_at`) does.
    let enc_class_id =
        loader_scoped_enclosing_lookup_checked(ctx, class_id, &enc_class_name, true)?;
    if method_name.is_empty() {
        // Class is enclosed by an initializer (not a method).
        return Ok(Some(Value::Object(None)));
    }
    let enc_class_id = match enc_class_id {
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
    // JIT round 18 wave 9, lane mhloader: the enclosing class as the class's
    // own constant pool resolves it (`CRATONVM_CLASS_OUTER_VIA_OWN_LOADER`).
    // Round 18 wave 11 (lane mhresid; MHLOADER18-4): resolved before the
    // constructor test, its error thrown, as `getEnclosingMethod0` does.
    let enc_class_id =
        loader_scoped_enclosing_lookup_checked(ctx, class_id, &enc_class_name, true)?;
    if method_name != "<init>" {
        return Ok(Some(Value::Object(None)));
    }
    let enc_class_id = match enc_class_id {
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
        crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32());
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
            // `Class[]`, as the doc above says (i7-L2); `Class` is loaded.
            let component = crate::lang_class::reflection_component_id(ctx, "java/lang/Class");
            let arr = ctx.new_ref_array(component, 0);
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
                // `Type[]`, as HotSpot's reifier allocates (i6-L2).
                let type_component =
                    crate::lang_class::reflection_component_id(ctx, "java/lang/reflect/Type");
                let arr = ctx.new_ref_array(type_component, method_sig.throws.len());
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
                    // gen r4w3/rooting: pinned scope -- the thread-local decl
                    // is read back through its pin after the conversion's GC
                    // points; released (topmost pin) once the element is built.
                    let gscope = crate::generics::GenericDeclScope::new_pinned(ctx, decl);
                    let v = crate::generics::type_sig_to_java(ctx, t);
                    gscope.release(ctx);
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
    // JDK 25's `Method.sharedToString` (`printModifiersIfNonzero`): a
    // default method -- `PUBLIC` without `ABSTRACT` / `STATIC`, declared by
    // an interface -- prints `default` after its access modifier
    // (interpreter round i1 wave 52, lane L3; `L3W52ClassLookupAnswers`,
    // row `default-via-impl`).
    if (modifiers & (0x0400 | 0x0001 | 0x0008)) == 0x0001 {
        let declared_by_interface = match method_clazz_value(ctx, this) {
            Value::Object(Some(m)) => {
                mirror_class_id(ctx, m).is_some_and(|id| ctx.is_interface_class(id))
            }
            _ => false,
        };
        if declared_by_interface {
            s.push_str("default ");
        }
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
    s.push_str(&decl_name.class_text(&*ctx));
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
                    s.push_str(&en.class_text(&*ctx));
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
                inner.hidden_dotted()
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
    // Interpreter round i6 wave 116, lane L3 fixed the same `/0x<hex>` tail
    // here with a local helper (probe
    // `tools/probes/interp/L3/L3W116HiddenClassMethodToString.java`); dev's
    // JIT round 21 `hidden_dotted` / `class_text` renderings replace it.
    name.hidden_dotted()
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
    let dotted = name.class_text(&*ctx);
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
///
/// Also `classloader::lk_class_relation`'s package answer for `Lookup.in`
/// (`VerifyAccess.isSamePackage`: same loader and same `getPackageName`; JIT
/// round 21 wave 10, lane reflect). Reads only, no GC point.
pub(crate) fn runtime_package_of(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Option<(String, i32)> {
    let name = ctx.class_name_of_id(class_id)?;
    // JIT round 21 wave 8, lane indy (`CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE`,
    // page `r21-hidden-class-own-private-ctor-access-refused-20261019`): a JEP
    // 371 hidden class is stored under `<class-file name>/0x<hex>`, and that
    // `/` is part of the NAME, not a package separator -- the hidden class
    // lives in its lookup class's runtime package, as
    // `classloading::access_control::runtime_package_name` already says for
    // the bytecode check. Taking the last `/` put
    // `java/lang/String$$StringConcat/0x0` in package
    // `java/lang/String$$StringConcat`, so `MHN_resolve` refused `String`'s
    // package-private `findConstructor` of the concat class it had just spun.
    //
    // JIT round 21 wave 13 (review rev-native item 5): the name's shape first
    // (a string scan, no lock), then the VM's hidden bit, and only then the
    // switch, read once per VM (`lang_class::reflect_hidden_runtime_package_per_vm`).
    //
    // Interpreter round i6 wave 121, lane L4 fixed the same refusal
    // (`i121-L4-jdk-string-concat-factory-hidden-class-constructor-is-refused`)
    // under `CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE` (default ON, parity);
    // merged onto this rule, that switch stays a second kill switch (`=0`
    // either restores the old split). Both are folded into the one per-VM
    // bool since interpreter round i7 wave 136, lane L3.
    let file_name = hidden_class_file_name(&name);
    let base = if file_name.len() != name.len()
        && ctx.is_class_hidden(class_id)
        && hidden_runtime_package_enabled(ctx)
    {
        file_name
    } else {
        &name
    };
    let pkg = match base.rfind('/') {
        Some(i) => base[..i].to_string(),
        // Default package (the regression-suite classes live here).
        None => String::new(),
    };
    Some((pkg, ctx.loader_id_of_class(class_id)))
}

/// `CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE` (default ON; JIT round 21 wave 8,
/// lane indy): the reflective / `MHN_resolve` access check places a hidden
/// class in the runtime package of its class-file name (JEP 371), not in a
/// package named by its `/0x<hex>` tail. `0` restores the old split, and with
/// it the `StringConcatFactory` "Generator failed" refusal. Read only on a
/// non-public member whose caller is not its declaring class.
///
/// Read once per VM since JIT round 21 wave 13 (review rev-native item 5;
/// `lang_class::reflect_hidden_runtime_package_per_vm`), and only for a class
/// the VM says is hidden.
fn hidden_runtime_package_enabled(ctx: &dyn NativeContext) -> bool {
    crate::lang_class::reflect_hidden_runtime_package_per_vm(ctx)
}

/// A hidden class's stored name without its `/0x<hex>` suffix (the class-file
/// name); any other name unchanged (`lang_class::hidden_name_suffix_split`,
/// the shape `classloading::access_control::runtime_package_name` tests).
fn hidden_class_file_name(name: &str) -> &str {
    crate::lang_class::hidden_name_suffix_split(name).map_or(name, |(base, _)| base)
}

/// `Class.getName()`'s spelling of a class for a message: dotted, with a
/// hidden class's `/0x<hex>` tail kept verbatim (HotSpot's `external_name()`;
/// `classloading::access_control::external_class_name`).
pub(crate) fn external_name_of_maybe_hidden(name: &str, hidden: bool) -> String {
    if hidden {
        crate::lang_class::dotted_binary_name(name)
    } else {
        name.replace('/', ".")
    }
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
    // JIT round 18 wave 9, lane mhloader (`CRATONVM_CLASS_NEST_HOST_VALIDATE`):
    // a host of another defining loader is another run-time package, never
    // this class's host (HotSpot `InstanceKlass::nest_host`; the bytecode
    // check `access_control::confirmed_nest_host` asks the same).
    if crate::lang_class::class_nest_host_validate_enabled()
        && !crate::lang_class::same_defining_loader(ctx, class_id, host_id)
    {
        return Some(own);
    }
    if ctx.nest_member_names(host_id).iter().any(|m| *m == own) {
        Some(claimed)
    } else {
        Some(own)
    }
}

/// JEP 181 nestmate test: two classes are nestmates iff they resolve to the
/// same *confirmed* nest host.
///
/// Looks the claimed hosts up without loading (no GC point, so no caller has
/// to pin anything). HotSpot's `getNestHost` LOADS them: every reflective
/// door that reaches [`caller_may_access_member`] with a private member calls
/// `lang_class::settle_reflective_nest_hosts` first, which loads a claimed host
/// its namespace does not know yet with the door's references pinned, so the
/// lookup here finds it (or, after a failed load, the class is its own host).
/// JIT round 20 wave 16, lane nest
/// (`jr20-reflective-private-access-check-does-not-load-the-nest-host`).
fn classes_are_nestmates(ctx: &mut dyn NativeContext, a: ClassId, b: ClassId) -> bool {
    if a == b {
        return true;
    }
    // Two defining loaders are two run-time packages, so never one nest
    // (round 18 wave 9, lane mhloader; `access_control::are_nestmates`).
    if crate::lang_class::class_nest_host_validate_enabled()
        && !crate::lang_class::same_defining_loader(ctx, a, b)
    {
        return false;
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
// can catch and surface.
//
// Interpreter round i3 wave 79, lane L3
// (`i77-L3-method-invoke-depth-guard-throws-stackoverflowerror-at-101-nested-calls-20261110.md`):
// the guard was a fixed count of 100, which refused legal programs (HotSpot 25
// runs thousands of nested reflective levels on its default stack). It now
// measures what it protects: the OS thread's native stack left below the
// current frame ([`reflective_native_stack_exhausted`]). The count stays only as
// a last resort ([`REFLECTIVE_INVOKE_DEPTH_CEILING`], the main thread's
// `execute` re-entry ceiling) for a thread whose stack bounds the OS does not
// report; every nested level also re-enters `execute`, whose own per-thread
// ceiling (derived from the carrier's size) still applies.
thread_local! {
    static INVOKE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// This OS thread's `(low, high)` native stack bounds, `(usize::MAX, 0)`
    /// before the first ask, `(0, 0)` when the OS does not say.
    static REFLECTIVE_STACK_BOUNDS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((usize::MAX, 0)) };
}

/// The last-resort nesting ceiling of served `Method.invoke` calls on one
/// thread (see the guard's comment above): the `execute` re-entry ceiling the
/// VM derives for its 128 MiB main thread (64 MiB / 8 KiB per level).
const REFLECTIVE_INVOKE_DEPTH_CEILING: u32 = 8192;

/// Native stack a nested `Method.invoke` keeps free below its entry: the
/// target's re-entry, the `StackOverflowError` construction and the unwind
/// back through the outer levels all run on it. The JIT's compiled-recursion
/// floor keeps the same 1 MiB; a stack under 4 MiB keeps a quarter of itself.
const REFLECTIVE_STACK_RESERVE_BYTES: usize = 1 << 20;

/// The stack address at or below which a nested `Method.invoke` refuses with
/// `StackOverflowError`, for a thread whose native stack spans `[low, high)`.
fn reflective_stack_floor(low: usize, high: usize) -> usize {
    let size = high.saturating_sub(low);
    low.saturating_add(REFLECTIVE_STACK_RESERVE_BYTES.min(size / 4))
}

/// Is `sp` at or below the reflective floor of the stack `[low, high)`? An
/// `sp` outside the band (a frame on a stack other than the one whose bounds
/// were cached, e.g. a continuation's own stack) is never "exhausted": the
/// count and `execute`'s ceiling still bound it.
fn reflective_stack_exhausted_at(sp: usize, low: usize, high: usize) -> bool {
    low != 0 && high > low && sp >= low && sp < high && sp <= reflective_stack_floor(low, high)
}

/// This OS thread's native stack bounds, or `None` where the OS does not say.
#[cfg(windows)]
fn reflective_os_stack_bounds() -> Option<(usize, usize)> {
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: two out-pointers to locals, the current thread.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    (high > low && low != 0).then_some((low, high))
}

#[cfg(target_os = "linux")]
fn reflective_os_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: the pthread attribute is initialised by `pthread_getattr_np`
    // on success and destroyed after the read.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (rc == 0 && !addr.is_null() && size != 0).then(|| (addr as usize, addr as usize + size))
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn reflective_os_stack_bounds() -> Option<(usize, usize)> {
    None
}

/// Is this thread's native stack, at the caller's frame, within
/// [`REFLECTIVE_STACK_RESERVE_BYTES`] of its end? One TLS read and a compare
/// after the first call on a thread (which asks the OS once).
#[inline(never)]
pub(crate) fn reflective_native_stack_exhausted() -> bool {
    let marker = 0u8;
    let sp = std::hint::black_box(&marker) as *const u8 as usize;
    let (low, high) = reflective_stack_bounds_cached();
    reflective_stack_exhausted_at(sp, low, high)
}

/// This OS thread's `(low, high)` native stack bounds, `(0, 0)` when the OS
/// does not say; asked once per thread, then one TLS read.
fn reflective_stack_bounds_cached() -> (usize, usize) {
    let (mut low, mut high) = REFLECTIVE_STACK_BOUNDS.with(std::cell::Cell::get);
    if low == usize::MAX {
        (low, high) = reflective_os_stack_bounds().unwrap_or((0, 0));
        REFLECTIVE_STACK_BOUNDS.with(|c| c.set((low, high)));
    }
    (low, high)
}

/// The bounds a reflective door hands to
/// `NativeExceptionAccess::rebase_compiled_stack_budget` (wave 79, lane L3),
/// so the JIT's floor is moved without asking the OS: `None` when unknown.
pub(crate) fn reflective_stack_bounds() -> Option<(usize, usize)> {
    let (low, high) = reflective_stack_bounds_cached();
    (low != 0 && high > low).then_some((low, high))
}

/// A native that stands, for its WHOLE call, for the JDK frames of one
/// [`cratonvm_native_api::ReflectiveCallKind`]: its registration names the
/// kind (this row), and [`reflective_entry`] enters and leaves the record
/// around the row's body, so no body calls `enter_reflective_call` itself.
/// Interpreter round i6 wave 120, lane L3 (stage 2 of
/// `i46-L3-proposal-one-table-of-the-jdk-frames-a-native-stands-for`): the
/// six whole-native sites (`Method.invoke`, `Constructor.newInstance`,
/// `newInstance0`, the three `AccessController.doPrivileged` bridges). The
/// five sites whose record brackets an inner callback, conditionally (the
/// four `StackWalker` serving sites, `invokeWithArguments`), keep their own
/// calls.
pub(crate) trait ReflectiveEntryRow {
    /// The JDK frames this native stands for.
    const KIND: cratonvm_native_api::ReflectiveCallKind;
    /// The native's body, inside the record.
    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult;
    /// What the native does after the record is left (default: nothing).
    fn after_leave(_ctx: &mut dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
        result
    }
}

/// The registered entry of a [`ReflectiveEntryRow`]: the record entered,
/// the row's body, the record left on every exit path (the body returns
/// rather than unwinds; a panic is the funnel's), then its `after_leave`.
/// Entering, for these kinds, runs no Java (it may load the accessor class
/// once per thread, before the body reads `args`, as each body did).
pub(crate) fn reflective_entry<R: ReflectiveEntryRow>(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mark = ctx.enter_reflective_call(R::KIND);
    let result = R::serve(ctx, args);
    ctx.leave_reflective_call(mark);
    R::after_leave(ctx, result)
}

/// `Method.invoke`'s row: [`native_method_invoke_boxed_body`] inside the
/// `MethodInvoke` record. HotSpot's stack captures made inside the target list
/// the JDK frames this native stands for (`Method.invoke` and
/// `DirectMethodHandleAccessor.invoke`), so the VM's captures list them too
/// (interpreter round i1 wave 43, lane L3).
pub(crate) struct MethodInvokeRow;

impl ReflectiveEntryRow for MethodInvokeRow {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        use crate::reflect_door_phases::{PhaseTimer, KIND_INVOKE, PH_LEAVE};
        // Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_DOOR_PHASE_STATS`):
        // the door's phase marks (an off timer marks nothing). Since wave 120
        // they start inside the record and end before it is left.
        let mut ph = PhaseTimer::new(ctx.reflect_door_phase_stats());
        // Wave 79, lane L3: this door's native frames are not compiled
        // recursion; the JIT's stack budget is re-anchored below them.
        let budget = ctx.rebase_compiled_stack_budget(reflective_stack_bounds());
        let result = native_method_invoke_boxed_body(ctx, args, &mut ph);
        ctx.restore_compiled_stack_budget(budget);
        if ph.is_on() {
            ph.mark(PH_LEAVE);
            if result.is_ok() {
                crate::lang_class::record_door_phases(&*ctx, KIND_INVOKE, &ph);
            }
        }
        result
    }
}

/// `Method.invoke`: [`MethodInvokeRow`] through [`reflective_entry`].
fn native_method_invoke_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reflective_entry::<MethodInvokeRow>(ctx, args)
}

fn native_method_invoke_boxed_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ph: &mut crate::reflect_door_phases::PhaseTimer,
) -> MethodCallResult {
    use crate::reflect_door_phases::{PH_ENTER, PH_RET};
    // Reentrancy / recursion guard — see comment on INVOKE_DEPTH above.
    let prev_depth = INVOKE_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if prev_depth >= REFLECTIVE_INVOKE_DEPTH_CEILING || reflective_native_stack_exhausted() {
        // Roll back the increment so the next top-level call starts fresh.
        INVOKE_DEPTH.with(|d| d.set(prev_depth));
        return Err(cratonvm_types::error::RuntimeError::StackOverflowError.into());
    }
    ph.mark(PH_ENTER);

    // RAII-style depth restore: any early return (including `?`) must still
    // decrement so we don't leak depth across calls. Run the original body
    // inside a closure and capture its Result so we can decrement before
    // propagating.
    let result = (|| -> MethodCallResult {
        // Interpreter round i3 wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`):
        // a planned method's return descriptor is its plan's, a function of the
        // member alone held in Rust memory -- no per-call descriptor `String`
        // read off the mirror and parsed, and nothing read off a mirror after
        // the call.
        //
        // Wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): a return the
        // body boxed itself from a primitive (`fresh_box`) is a wrapper, which
        // the shape check passes through unchanged -- it is not asked.
        if let Some((raw, plan, fresh_box)) =
            crate::lang_class::native_method_invoke_planned(ctx, args, ph)
        {
            let out = match raw? {
                Some(raw_val) if fresh_box => Ok(Some(raw_val)),
                Some(raw_val) => {
                    finish_method_invoke_return(ctx, raw_val, plan.descriptor(), plan.ret_desc())
                }
                None => Ok(None),
            };
            ph.mark(PH_RET);
            return out;
        }

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
        let raw = crate::lang_class::native_method_invoke_timed(ctx, args, ph)?;

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
        let out = finish_method_invoke_return(ctx, raw_val, &descriptor, &ret_desc);
        ph.mark(PH_RET);
        out
    })();

    // Restore depth on every exit path (success or error). Use `set(prev)`
    // rather than `set(get-1)` so that even if some inner panic-unwind
    // somehow skipped a decrement, we still settle back to the depth we
    // observed on entry.
    INVOKE_DEPTH.with(|d| d.set(prev_depth));
    result
}

/// The return half of [`native_method_invoke_boxed_body`]: the shape checks
/// and the defensive boxing of `raw_val` against the method's return
/// descriptor `ret_desc` (`descriptor` is the full one, for the log only).
/// Shared by the by-descriptor path and the planned one
/// (`CRATONVM_REFLECT_DOOR_WORD_BATCH`, interpreter round i3 wave 76, lane L3).
fn finish_method_invoke_return(
    ctx: &mut dyn NativeContext,
    raw_val: Value,
    descriptor: &str,
    ret_desc: &str,
) -> MethodCallResult {
    let primitive_ret = matches!(ret_desc, "I" | "J" | "Z" | "B" | "S" | "C" | "F" | "D");

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
            // The interned name, not a fresh `String` (wave 76, lane L3).
            let class_name = ctx.class_name_arc_of_id(class_id);
            if class_name.as_deref() == Some("java/lang/Class") {
                let placeholder = match ret_desc {
                    "J" => Value::Long(0),
                    "F" => Value::Float(0.0),
                    "D" => Value::Double(0.0),
                    _ => Value::Int(0),
                };
                let recovered = box_value(ctx, placeholder, ret_desc);
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

    let boxed = box_value(ctx, raw_val, ret_desc);
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
    // gen r4w3/rooting: each element compare runs Java `equals` (a GC
    // point); pin both arrays and re-read them every iteration.
    let aa_pin = ctx.pin_native_root(aa);
    let bb_pin = ctx.pin_native_root(bb);
    let mut equal = true;
    for i in 0..n {
        let aa = ctx.read_native_pin(aa_pin, aa);
        let bb = ctx.read_native_pin(bb_pin, bb);
        let x = ctx.get_array_element(aa, i);
        let y = ctx.get_array_element(bb, i);
        if !type_value_equals(ctx, x, y)? {
            equal = false;
            break;
        }
    }
    ctx.unpin_native_roots(aa_pin);
    Ok(equal)
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
    // gen r4w3/rooting: each element's Java `hashCode` is a GC point; pin
    // `arr` and re-read it every iteration.
    let arr_pin = ctx.pin_native_root(arr);
    for i in 0..n {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let e = ctx.get_array_element(arr, i);
        h = h.wrapping_mul(31).wrapping_add(type_value_hash(ctx, e)?);
    }
    ctx.unpin_native_roots(arr_pin);
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

    // --- DirectConstructorHandleAccessor$NativeAccessor.newInstance0 ---
    // SBR2-CLASSREFL: a GENUINE native (HotSpot `JVM_NewInstanceFromConstructor`):
    // the real `Class.newInstance` body reaches it through
    // `ReflectionFactory.newInstance` -> `Constructor.newInstanceWithCaller` ->
    // the native accessor (the JDK's choice while `java.lang.invoke` is not
    // initialised). `(Constructor, Object[])` has the same argument layout as
    // `Constructor.newInstance(Object[])`, whose native body allocates, runs
    // `<init>` and wraps a throwing constructor in InvocationTargetException.
    // Registered `Bridge` explicitly: there is no bytecode to lose to, and it
    // must survive `--jdk-only`.
    registry.register_with_kind(
        "jdk/internal/reflect/DirectConstructorHandleAccessor$NativeAccessor",
        "newInstance0",
        "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // Interpreter round i2 wave 63, lane L1: JVMTI `VMObjectAlloc`.
            let made = crate::lang_class::native_constructor_new_instance0(ctx, args);
            crate::vm_object_alloc_event(ctx, made)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 17's name for the same native (`NativeConstructorAccessorImpl`).
    registry.register_with_kind(
        "jdk/internal/reflect/NativeConstructorAccessorImpl",
        "newInstance0",
        "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // Interpreter round i2 wave 63, lane L1: JVMTI `VMObjectAlloc`.
            let made = crate::lang_class::native_constructor_new_instance0(ctx, args);
            crate::vm_object_alloc_event(ctx, made)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // --- DirectMethodHandleAccessor$NativeAccessor.invoke0 ---
    // Interpreter round i2 wave 59, lane L3 (the i45-L3 proposal): a GENUINE
    // native (HotSpot `JVM_InvokeMethod`), the method twin of `newInstance0`
    // above. The JDK's `Method.invoke` body reaches it through
    // `MethodHandleAccessorFactory.newMethodAccessor` whenever it picks the
    // native accessor -- before `java.lang.invoke` is initialised, for a
    // `native` target, or under `jdk.reflect.useNativeAccessorOnly` -- which
    // happens only when `Method.invoke` itself is not served
    // (`CRATONVM_JDK_REFLECTIVE_INVOKE`). Registered `Bridge` explicitly:
    // there is no bytecode to lose to.
    registry.register_with_kind(
        "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
        "invoke0",
        "(Ljava/lang/reflect/Method;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        crate::lang_class::native_method_invoke0,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 17's name for the same native (`NativeMethodAccessorImpl`).
    registry.register_with_kind(
        "jdk/internal/reflect/NativeMethodAccessorImpl",
        "invoke0",
        "(Ljava/lang/reflect/Method;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        crate::lang_class::native_method_invoke0,
        cratonvm_native_api::NativeKind::Bridge,
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
            // A copy, as the JDK's (interpreter round i2 wave 56, lane L3).
            let arguments = ctx.get_field(this, 1);
            Ok(Some(crate::generics::copied_bounds(ctx, arguments)))
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
    //
    // 2026-10-02 (JIT round 16 wave 10, lane shadow): that `getfield` defect no
    // longer reproduces, and under `--jdk-only` every row below is RETIRED to the
    // real bytecode (`retired_shadow.rs` `RETIRED_SHADOW_SBR_REFLECT_TRIPLES`,
    // which SBR round 1 landed first, plus round 15's covariant
    // `getRawType()Type`); `R16ShadowPti` matches HotSpot there. The natives
    // still serve `--compatible`, unchanged.
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
                // gc-common w17-f: the source array is rooted across the
                // clone's allocation and re-read after it; the copy loop used
                // the address from before `new_ref_array`.
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let arr_h = scope.root(arr);
                let clone = scope.new_ref_array(comp_cid, len);
                let arr = scope.get(&arr_h);
                let ctx = &mut *scope;
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
        // gen r4w3/rooting: the per-bound reification below runs Java
        // (`toArray`, class loading, `getTypeParameters()`), so `this`, `arr`,
        // each tree `node` and every accumulated `out` object are pinned and
        // re-read. Pin order (LIFO): this < arr < scope decl < out[0..] ; each
        // `node` pin is taken and released above the out pins pushed so far.
        let this_pin = ctx.pin_native_root(this);
        let arr_pin = ctx.pin_native_root(arr);
        let scope_decl = reifier_decl_from_factory(ctx, this);
        let _gscope = crate::generics::GenericDeclScope::new_pinned(ctx, scope_decl);
        let len = ctx.array_length(arr);
        let mut out: Vec<Value> = Vec::with_capacity(len);
        // Pin handle of each `out` entry that holds an object.
        let mut out_pins: Vec<Option<usize>> = Vec::with_capacity(len);
        let mut any_tree = false;
        for i in 0..len {
            let arr = ctx.read_native_pin(arr_pin, arr);
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
                    // gen r4w3/rooting: `node` is re-read off its pin before the
                    // mirror fallback (the typesig pipeline is a GC point).
                    let node_pin = ctx.pin_native_root(node);
                    let reified = jdk_tree_to_typesig(ctx, node)
                        .map(|ts| crate::generics::typesig_to_real_type(ctx, &ts))
                        .filter(|v| !matches!(v, Ok(Value::Object(None))))
                        .or_else(|| {
                            let node = ctx.read_native_pin(node_pin, node);
                            wti_tree_node_to_mirror(ctx, node, &cls).map(Ok)
                        })
                        .or_else(|| {
                            // Unresolvable exotic bound: degrade to Object
                            // (the JDK's implicit upper bound) rather than
                            // leaking a non-Type.
                            ctx.class_id_by_name("java/lang/Object")
                                .map(|c| Ok(Value::Object(Some(ctx.get_class_mirror(c)))))
                        })
                        .unwrap_or(Ok(Value::Object(None)));
                    ctx.unpin_native_roots(node_pin);
                    let reified = reified?;
                    out_pins.push(match reified {
                        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                        _ => None,
                    });
                    out.push(reified);
                    continue;
                }
            }
            out_pins.push(match elem {
                Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                _ => None,
            });
            out.push(elem);
        }
        // gc-common w17-f: every pin taken here (this_pin is the lowest) is
        // held until the result array has been allocated and filled. The old
        // code refreshed the receiver and the bounds, released the pins, and
        // only THEN allocated the `Type[]` -- so the element stores and the
        // write-back went through addresses from before that allocation.
        // `_gscope` has no reader past this function's pin release.
        if !any_tree && len != 0 {
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
            // (The empty case falls through: `out` is empty, so the `Type[]`
            // below is the `Type[0]` written back.)
            // A COPY, as the JDK's `(Type[]) value.clone()` hands out
            // (interpreter round i2 wave 56, lane L3): the reified array is
            // the object's own, and a caller storing into the answer wrote
            // the variable's bounds (`L3W56CompatibleTypeVariableClass`,
            // `bounds-copy`).
            let arr = ctx.read_native_pin(arr_pin, arr);
            let copy = crate::generics::copy_type_array(ctx, arr);
            ctx.unpin_native_roots(this_pin);
            return Ok(Value::Object(Some(copy)));
        }
        let type_cid = ctx
            .class_id_by_name("java/lang/reflect/Type")
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let result = ctx.new_ref_array(type_cid, out.len());
        for (i, (v, pin)) in out.iter().zip(out_pins.iter()).enumerate() {
            let v = match (*v, *pin) {
                (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
                (v, _) => v,
            };
            ctx.set_array_element(result, i, v);
        }
        // Write back so subsequent reads (incl. real bytecode getfield) see
        // reified values — mirrors the lazy write-back in the real impl.
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this, field, Value::Object(Some(result)));
        // The caller gets a copy of the written-back array (wave 56, lane L3;
        // the arm above). Every pin is still held across its allocation.
        let copy = if out.is_empty() {
            result
        } else {
            crate::generics::copy_type_array(ctx, result)
        };
        ctx.unpin_native_roots(this_pin);
        Ok(Value::Object(Some(copy)))
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
            // gen r4w3/rooting: `jdk_collect_type_args` below recurses into
            // `toArray` (a GC point); pin `arr` and re-read it per segment.
            let arr_pin = ctx.pin_native_root(arr);
            for i in 0..ctx.array_length(arr) {
                let arr = ctx.read_native_pin(arr_pin, arr);
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
            ctx.unpin_native_roots(arr_pin);
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
                // gen r4w3/rooting: `jdk_typearg_to_typearg` can reach
                // `toArray` (a GC point); pin `ta_arr`, re-read per element.
                let ta_pin = ctx.pin_native_root(ta_arr);
                for j in 0..ctx.array_length(ta_arr) {
                    let ta_arr = ctx.read_native_pin(ta_pin, ta_arr);
                    if let Value::Object(Some(ta)) = ctx.get_array_element(ta_arr, j) {
                        if let Some(arg) = jdk_typearg_to_typearg(ctx, ta) {
                            out.push(arg);
                        }
                    }
                }
                ctx.unpin_native_roots(ta_pin);
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
                // `Objects.requireNonNull(annotationClass)`: no message
                // (interpreter round i2 wave 53, lane L3, measured on JDK
                // 25.0.3; probe `L3W53ReflectionNullArgs`).
                _ => {
                    Err(
                        cratonvm_types::error::RuntimeError::NullPointerException { message: None }
                            .into(),
                    )
                }
            }
        }
        /// `getAnnotationsByType` / `getDeclaredAnnotationsByType`: the same
        /// `requireNonNull` first (they answered an empty array of class id 0
        /// for a null class), then the empty array of the requested type.
        fn annotations_by_type_null_checked(
            ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
            match args.get(1) {
                Some(Value::Object(Some(_))) => {
                    Ok(Some(empty_annotation_array_for_type(ctx, args)))
                }
                _ => {
                    Err(
                        cratonvm_types::error::RuntimeError::NullPointerException { message: None }
                            .into(),
                    )
                }
            }
        }
        // A JDK `TypeVariableImpl` (built under `--compatible` too since
        // interpreter round i2 wave 56, lane L3) answers from its own body,
        // which reads the declaration's type annotations
        // (`TypeAnnotationParser.parseTypeVariableAnnotations`): these natives
        // answered an empty set for `class X<@A T>` (`--compatible`; probe
        // `L3W56TypeParameterAnnotations`). A null argument still raises here,
        // where the served-raise mark lists the JDK's frames. The carrier keeps
        // the empty answers.
        registry.register(
            class_name,
            "getAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getAnnotation", DESC, true)
                {
                    return answer;
                }
                annotation_query_null_checked(ctx, args)
            },
        );
        registry.register(
            class_name,
            "getDeclaredAnnotation",
            "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getDeclaredAnnotation", DESC, true)
                {
                    return answer;
                }
                annotation_query_null_checked(ctx, args)
            },
        );
        registry.register(
            class_name,
            "getAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "()[Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getAnnotations", DESC, false)
                {
                    return answer;
                }
                Ok(Some(empty_annotation_array(ctx)))
            },
        );
        registry.register(
            class_name,
            "getDeclaredAnnotations",
            "()[Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "()[Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getDeclaredAnnotations", DESC, false)
                {
                    return answer;
                }
                Ok(Some(empty_annotation_array(ctx)))
            },
        );
        registry.register(
            class_name,
            "getAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getAnnotationsByType", DESC, true)
                {
                    return answer;
                }
                annotations_by_type_null_checked(ctx, args)
            },
        );
        registry.register(
            class_name,
            "getDeclaredAnnotationsByType",
            "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
            |ctx, args| {
                const DESC: &str = "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;";
                if let Some(answer) =
                    type_variable_impl_body(ctx, args, "getDeclaredAnnotationsByType", DESC, true)
                {
                    return answer;
                }
                annotations_by_type_null_checked(ctx, args)
            },
        );
    }
    /// The JDK's own `TypeVariableImpl.<name><desc>` body for a
    /// `TypeVariableImpl` receiver (interpreter round i2 wave 56, lane L3), or
    /// `None` -- another receiver (the VM's carrier), a VM that does not build
    /// the JDK's type classes (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS` off
    /// under `--compatible`), a class without the body (a synthetic JDK), or,
    /// when `checks_argument`, a null argument (raised by the native, as
    /// before).
    fn type_variable_impl_body(
        ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
        args: &[Value],
        name: &str,
        desc: &str,
        checks_argument: bool,
    ) -> Option<MethodCallResult> {
        const TVI: &str = "sun/reflect/generics/reflectiveObjects/TypeVariableImpl";
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return None;
        };
        if checks_argument && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
            return None;
        }
        if !(ctx.is_jdk_only() || ctx.compatible_reflect_type_impls()) {
            return None;
        }
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(this))
            .as_deref()
            != Some(TVI)
        {
            return None;
        }
        if !ctx.method_exists(TVI, name, desc) {
            return None;
        }
        if cratonvm_types::flags::runtime_var_os("CRATONVM_FRAME_TRACE").is_some() {
            eprintln!("[TYPE_VARIABLE_IMPL] the JDK's body: {name}{desc}");
        }
        Some(ctx.invoke_special_bytecode_only(TVI, name, desc, args))
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
        |ctx, args| {
            // The JDK's body (`TypeAnnotationParser.parseAnnotatedBounds`)
            // keeps the bounds' type annotations (wave 56, lane L3; see
            // `type_variable_impl_body`).
            const DESC: &str = "()[Ljava/lang/reflect/AnnotatedType;";
            if let Some(answer) =
                type_variable_impl_body(ctx, args, "getAnnotatedBounds", DESC, false)
            {
                return answer;
            }
            crate::lang_class::native_type_variable_get_annotated_bounds(ctx, args)
        },
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
            // A copy, as the JDK's (wave 56, lane L3).
            let bounds = ctx.get_field(this, 1);
            Ok(Some(crate::generics::copied_bounds(ctx, bounds)))
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
            // A copy, as the JDK's (wave 56, lane L3).
            let bounds = ctx.get_field(this, 0);
            Ok(Some(crate::generics::copied_bounds(ctx, bounds)))
        },
    );
    registry.register(
        "java/lang/reflect/WildcardType",
        "getLowerBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let bounds = ctx.get_field(this, 1);
            Ok(Some(crate::generics::copied_bounds(ctx, bounds)))
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
            // gen r4w3/rooting: each component compare runs Java `equals`
            // (a GC point), after which `this`/`other` (and `args`) are
            // stale; pin both and re-read before every later slot read.
            let this_pin = ctx.pin_native_root(this);
            let other_pin = ctx.pin_native_root(other);
            let equal = (|| -> Result<bool, cratonvm_types::error::MethodCallFailed> {
                let (Some(ra), Some(rb)) = (
                    reflect_type_slot(ctx, this, STUB, 0, "rawType"),
                    reflect_type_slot(ctx, other, STUB, 0, "rawType"),
                ) else {
                    return Ok(false);
                };
                if !type_value_equals(ctx, ra, rb)? {
                    return Ok(false);
                }
                let this = ctx.read_native_pin(this_pin, this);
                let other = ctx.read_native_pin(other_pin, other);
                let (Some(oa), Some(ob)) = (
                    reflect_type_slot(ctx, this, STUB, 2, "ownerType"),
                    reflect_type_slot(ctx, other, STUB, 2, "ownerType"),
                ) else {
                    return Ok(false);
                };
                if !type_value_equals(ctx, oa, ob)? {
                    return Ok(false);
                }
                let this = ctx.read_native_pin(this_pin, this);
                let other = ctx.read_native_pin(other_pin, other);
                let (Some(aa), Some(ab)) = (
                    reflect_type_slot(ctx, this, STUB, 1, "actualTypeArguments"),
                    reflect_type_slot(ctx, other, STUB, 1, "actualTypeArguments"),
                ) else {
                    return Ok(false);
                };
                type_array_equals(ctx, aa, ab)
            })();
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Int(i32::from(equal?))))
        },
    );
    registry.register(
        "java/lang/reflect/ParameterizedType",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            const STUB: &str = "java/lang/reflect/ParameterizedType";
            // gen r4w3/rooting: every hash step runs Java `hashCode` (a GC
            // point); pin `this` and read each slot only right before its
            // own step (same values, same fold order as before).
            let this_pin = ctx.pin_native_root(this);
            let hash = (|| -> Result<i32, cratonvm_types::error::MethodCallFailed> {
                let a = reflect_type_slot(ctx, this, STUB, 1, "actualTypeArguments")
                    .unwrap_or(Value::Object(None));
                let ha = type_array_hash(ctx, a)?;
                let this = ctx.read_native_pin(this_pin, this);
                let o = reflect_type_slot(ctx, this, STUB, 2, "ownerType")
                    .unwrap_or(Value::Object(None));
                let ho = type_value_hash(ctx, o)?;
                let this = ctx.read_native_pin(this_pin, this);
                let r =
                    reflect_type_slot(ctx, this, STUB, 0, "rawType").unwrap_or(Value::Object(None));
                let hr = type_value_hash(ctx, r)?;
                Ok(ha ^ ho ^ hr)
            })();
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Int(hash?)))
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
            // gen r4w3/rooting: the upper-bound compare runs Java `equals`
            // (a GC point); pin `this`/`other` and re-read before the
            // lower-bound slot reads.
            let this_pin = ctx.pin_native_root(this);
            let other_pin = ctx.pin_native_root(other);
            let equal = (|| -> Result<bool, cratonvm_types::error::MethodCallFailed> {
                let (Some(ua), Some(ub)) = (
                    reflect_type_slot(ctx, this, STUB, 0, "upperBounds"),
                    reflect_type_slot(ctx, other, STUB, 0, "upperBounds"),
                ) else {
                    return Ok(false);
                };
                if !type_array_equals(ctx, ua, ub)? {
                    return Ok(false);
                }
                let this = ctx.read_native_pin(this_pin, this);
                let other = ctx.read_native_pin(other_pin, other);
                let (Some(la), Some(lb)) = (
                    reflect_type_slot(ctx, this, STUB, 1, "lowerBounds"),
                    reflect_type_slot(ctx, other, STUB, 1, "lowerBounds"),
                ) else {
                    return Ok(false);
                };
                type_array_equals(ctx, la, lb)
            })();
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Int(i32::from(equal?))))
        },
    );
    registry.register(
        "java/lang/reflect/WildcardType",
        "hashCode",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            const STUB: &str = "java/lang/reflect/WildcardType";
            // gen r4w3/rooting: hashing `l` runs Java `hashCode` (a GC
            // point); pin `this` and read `upperBounds` only afterwards.
            let this_pin = ctx.pin_native_root(this);
            let hash = (|| -> Result<i32, cratonvm_types::error::MethodCallFailed> {
                let l = reflect_type_slot(ctx, this, STUB, 1, "lowerBounds")
                    .unwrap_or(Value::Object(None));
                let hl = type_array_hash(ctx, l)?;
                let this = ctx.read_native_pin(this_pin, this);
                let u = reflect_type_slot(ctx, this, STUB, 0, "upperBounds")
                    .unwrap_or(Value::Object(None));
                let hu = type_array_hash(ctx, u)?;
                Ok(hl ^ hu)
            })();
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Int(hash?)))
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

    /// Interpreter round i6 wave 116, lane L3: a hidden class keeps its
    /// `/0x<hex>` tail in `Method.toString`, as `Class.getName()` spells it.
    #[test]
    fn i116_l3_type_names_keep_a_hidden_class_tail() {
        assert_eq!(
            class_name_to_type_name("p/Host$$Lambda/0x0000020001001400"),
            "p.Host$$Lambda/0x0000020001001400"
        );
        assert_eq!(
            class_name_to_type_name("[[Lp/H/0x1f;"),
            "p.H/0x1f[][]"
        );
        assert_eq!(class_name_to_type_name("java/lang/String"), "java.lang.String");
        assert_eq!(class_name_to_type_name("p/0xnot/Hex"), "p.0xnot.Hex");
        assert_eq!(class_name_to_type_name("int"), "int");
    }

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

    /// Interpreter round i3 wave 79, lane L3: the nested `Method.invoke`
    /// guard measures the stack left, not a count of 100.
    #[test]
    fn i79_reflective_guard_trips_only_near_the_stack_end() {
        let (low, high) = (0x1000_0000usize, 0x1000_0000 + (8 << 20));
        let floor = reflective_stack_floor(low, high);
        assert_eq!(floor, low + (1 << 20), "an 8 MiB stack keeps 1 MiB");
        assert!(!reflective_stack_exhausted_at(high - 4096, low, high));
        assert!(!reflective_stack_exhausted_at(floor + 1, low, high));
        assert!(reflective_stack_exhausted_at(floor, low, high));
        assert!(reflective_stack_exhausted_at(low + 16, low, high));
        // A small stack keeps a quarter of itself.
        let small_high = low + (2 << 20);
        assert_eq!(reflective_stack_floor(low, small_high), low + (512 << 10));
        // Unknown bounds, or a frame outside the cached band, never trip.
        assert!(!reflective_stack_exhausted_at(low + 16, 0, 0));
        assert!(!reflective_stack_exhausted_at(low - 4096, low, high));
        assert!(!reflective_stack_exhausted_at(high + 4096, low, high));
        // The last-resort count is far above the old 100.
        assert!(REFLECTIVE_INVOKE_DEPTH_CEILING > 1000);
        // The live probe on this test thread is not near its end.
        assert!(!reflective_native_stack_exhausted());
    }

    /// JIT round 21 wave 8, lane indy (page
    /// `r21-hidden-class-own-private-ctor-access-refused-20261019`): JDK 25's
    /// `StringConcatFactory.InlineHiddenClassStrategy` spins
    /// `java/lang/String$$StringConcat` as a hidden class from `String`'s
    /// lookup and asks `findConstructor` for its PACKAGE-PRIVATE `<init>`. The
    /// hidden class is in `java/lang` (JEP 371), so the access holds; the old
    /// last-`/` split put it in `java/lang/String$$StringConcat` and refused.
    #[test]
    fn r21_hidden_class_package_private_member_reachable_from_its_lookup_package() {
        let mut ctx = crate::test_utils::mock_ctx();
        let caller = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("caller class");
        let declaring = ctx
            .ensure_class_initialized("java/lang/String$$StringConcat/0x0000000000000000")
            .expect("hidden class");
        ctx.set_class_hidden(declaring);
        // Package-private (no ACC_PUBLIC / PRIVATE / PROTECTED).
        assert!(caller_may_access_member(&mut ctx, caller, declaring, 0, None));
        // Private stays nest-scoped: not a NESTMATE-defined hidden class here.
        assert!(!caller_may_access_member(
            &mut ctx, caller, declaring, 0x0002, None
        ));
        // The falsifier: a NON-hidden class of that spelling really is in the
        // package `java/lang/String$$StringConcat`, and stays refused.
        let plain = ctx
            .ensure_class_initialized("java/lang/String$$StringConcat/0x1")
            .expect("plain class");
        assert!(!caller_may_access_member(&mut ctx, caller, plain, 0, None));
    }

    #[test]
    fn r21_hidden_class_names_split_and_spell_like_hotspot() {
        assert_eq!(
            hidden_class_file_name("java/lang/String$$StringConcat/0x0000000000000000"),
            "java/lang/String$$StringConcat"
        );
        assert_eq!(hidden_class_file_name("p/Host$$Lambda/0x1f"), "p/Host$$Lambda");
        assert_eq!(hidden_class_file_name("p/Host/0xzz"), "p/Host/0xzz");
        assert_eq!(hidden_class_file_name("p/Host"), "p/Host");
        assert_eq!(
            external_name_of_maybe_hidden("java/lang/String$$StringConcat/0x0", true),
            "java.lang.String$$StringConcat/0x0"
        );
        assert_eq!(external_name_of_maybe_hidden("p/Host/0x0", false), "p.Host.0x0");
    }

    /// JIT round 21 wave 13 (review rev-native item 5): the hidden-package
    /// switch and the `Lookup.in` relation switch are read once per VM (the
    /// door caches), never from the flag table per call, and the runtime
    /// package asks the name's shape, then the hidden bit, then the switch.
    #[test]
    fn r21_w13_relation_switches_are_read_once_per_vm() {
        let reflect = include_str!("lang_reflect.rs").replace("\r\n", "\n");
        let body = &reflect[reflect
            .find("\npub(crate) fn runtime_package_of(")
            .expect("runtime_package_of")..];
        let body = &body[..body.find("\n}\n").expect("its end")];
        let shape = body.find("hidden_class_file_name(&name)").expect("the shape");
        let hidden = body.find("ctx.is_class_hidden(class_id)").expect("the hidden bit");
        let switch = body.find("hidden_runtime_package_enabled(ctx)").expect("the switch");
        assert!(shape < hidden && hidden < switch);
        let reader = &reflect[reflect
            .find("\nfn hidden_runtime_package_enabled(")
            .expect("the reader")..];
        let reader = &reader[..reader.find("\n}\n").expect("its end")];
        assert!(reader.contains("reflect_hidden_runtime_package_per_vm(ctx)"));
        let loader = include_str!("classloader.rs").replace("\r\n", "\n");
        let lk = &loader[loader
            .find("\nfn lk_in_package_member_enabled(")
            .expect("the Lookup.in reader")..];
        let lk = &lk[..lk.find("\n}\n").expect("its end")];
        assert!(lk.contains("lookup_in_package_member_per_vm(ctx)"));
        assert!(!lk.contains("runtime_flag_default_on"));
        // The mock context has no cache slot: the flag table answers (default on).
        let ctx = crate::test_utils::mock_ctx();
        assert!(crate::lang_class::reflect_hidden_runtime_package_per_vm(&ctx)
            || !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECT_HIDDEN_RUNTIME_PACKAGE"
            ));
    }
}
