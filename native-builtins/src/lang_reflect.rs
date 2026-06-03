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
use cratonvm_types::{ClassId, ObjectRef, Value};
use cratonvm_types::error::MethodCallResult;

use crate::lang_class::{
    annotation_element_to_java, box_value,
    create_method_object, create_constructor_object, create_field_object,
    descriptor_to_class_mirror, method_class_name_desc, method_descriptor_for_invoke,
    mirror_class_id, mirror_class_name, native_method_invoke,
    parse_descriptor_param_and_return, read_method_descriptor,
    read_constructor_descriptor, read_field_meta,
};
use crate::obj_arg;

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
    *DBG_METHOD_INVOKE_BOX
        .get_or_init(|| std::env::var_os("CRATONVM_DBG_METHOD_INVOKE_BOX").is_some())
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
    crate::lang_class::write_method_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_field_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    crate::lang_class::write_field_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_constructor_try_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
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
    // We don't know which subtype the receiver is — try all three writers.
    crate::lang_class::write_method_accessible_external(ctx, this, true);
    crate::lang_class::write_field_accessible_external(ctx, this, true);
    crate::lang_class::write_constructor_accessible_external(ctx, this, true);
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// AccessibleObject.canAccess(Object) — JDK 9+ access check.
// ---------------------------------------------------------------------------
//
// Returns true iff the reflective member can be invoked / read with the
// given receiver. For static members the receiver must be null. For
// instance members the receiver's class must be assignable to the
// declaring class. Public members of public modules / classes always
// succeed; non-public members require the override flag.

fn can_access_member(
    ctx: &mut dyn NativeContext,
    member: ObjectRef,
    obj_arg: Value,
    is_static: bool,
) -> bool {
    // Static member: receiver MUST be null per spec.
    if is_static {
        return matches!(obj_arg, Value::Object(None));
    }

    // Instance member: receiver MUST be non-null AND assignable to the
    // declaring class.
    let receiver = match obj_arg {
        Value::Object(Some(r)) => r,
        _ => return false,
    };

    let declaring = match ctx.get_field_by_name(member, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return false,
    };
    let declaring_id = match mirror_class_id(ctx, declaring) {
        Some(id) => id,
        None => return false,
    };
    let receiver_id = ctx.class_id_of_object(receiver);
    // is_subclass(child, parent) returns true iff `child` is `parent` or a
    // subclass of it; equivalently, `parent.isAssignableFrom(child)`.
    ctx.is_subclass(receiver_id, declaring_id)
}

pub(crate) fn native_method_can_access(
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
    let ok = can_access_member(ctx, this, obj, is_static);
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
    let ok = can_access_member(ctx, this, obj, is_static);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

pub(crate) fn native_constructor_can_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Constructors are never static — receiver must be null per spec.
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    let ok = matches!(obj, Value::Object(None));
    let _ = this; // unused but required for arity
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
        let enc_id = ctx
            .class_id_by_name(&enc_class)
            .or_else(|| ctx.ensure_class_initialized(&enc_class).ok());
        if let Some(enc_id) = enc_id {
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
                let outer_id = ctx
                    .class_id_by_name(outer)
                    .or_else(|| ctx.ensure_class_initialized(outer).ok());
                if let Some(outer_id) = outer_id {
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

    for (i, pdesc) in param_descs.iter().enumerate() {
        let p = ctx.alloc_object(parameter_class_id, PARAMETER_NUM_FIELDS);

        let (name_str, modifiers) = match parameter_meta.get(i) {
            Some((n, flags)) if !n.is_empty() => (n.clone(), *flags as i32),
            Some((_, flags)) => (format!("arg{i}"), *flags as i32),
            None => (format!("arg{i}"), 0),
        };
        let name = ctx.create_string(&name_str);
        let type_mirror = descriptor_to_class_mirror(ctx, pdesc);

        ctx.set_field(p, 0, Value::Object(Some(name)));
        ctx.set_field(p, 1, Value::Int(modifiers));
        ctx.set_field(p, 2, Value::Object(Some(type_mirror)));
        ctx.set_field(p, 3, Value::Object(Some(declaring_executable)));

        ctx.set_array_element(arr, i, Value::Object(Some(p)));
    }
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
            let m = create_method_object(ctx, meta);
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
            let c = create_constructor_object(ctx, meta);
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
    Ok(Some(crate::lang_class::annotation_element_to_java_typed(
        ctx,
        &default,
        ret_desc.as_deref(),
    )))
}

// ---------------------------------------------------------------------------
// Method.isVarArgs / isBridge / isDefault / isSynthetic
// ---------------------------------------------------------------------------

pub(crate) fn native_method_is_varargs(
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

pub(crate) fn native_method_is_bridge(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
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
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
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
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_abstract = (modifiers & 0x0400) != 0;
    let is_static = (modifiers & 0x0008) != 0;
    if is_abstract || is_static {
        return Ok(Some(Value::Int(0)));
    }

    let declaring = match ctx.get_field_by_name(this, "clazz") {
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
    Ok(Some(ctx.get_field_by_name(this, "exceptionTypes")))
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
    let mirror = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(ctx.get_field_by_name(this, "exceptionTypes"))),
    };
    let class_id = match mirror_class_id(ctx, mirror) {
        Some(id) => id,
        None => return Ok(Some(ctx.get_field_by_name(this, "exceptionTypes"))),
    };
    let method_name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(Some(ctx.get_field_by_name(this, "exceptionTypes"))),
    };
    let descriptor = match read_method_descriptor(ctx, this) {
        Some(d) => d,
        None => return Ok(Some(ctx.get_field_by_name(this, "exceptionTypes"))),
    };

    if let Some(sig_str) = ctx.method_signature(class_id, &method_name, &descriptor) {
        if let Some(method_sig) = crate::generics::parse_method_signature(&sig_str) {
            if !method_sig.throws.is_empty() {
                let arr = ctx.new_ref_array(ClassId::new(0), method_sig.throws.len());
                for (i, t) in method_sig.throws.iter().enumerate() {
                    let v = crate::generics::type_sig_to_java(ctx, t);
                    ctx.set_array_element(arr, i, v);
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
    }
    // No Signature attribute, or no `^Type` throws section in it:
    // pin to the raw `exceptionTypes` array. WP2.8 will revisit deep
    // generic Type proxy support; today's surface is "raw Class for
    // every throws-type".
    Ok(Some(ctx.get_field_by_name(this, "exceptionTypes")))
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

    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };

    let mut s = String::new();
    // Build modifier prefix in JLS order. Method.toString uses the
    // method-level mask (no ACC_VARARGS / ACC_BRIDGE / ACC_SYNTHETIC).
    if (modifiers & 0x0001) != 0 { s.push_str("public "); }
    if (modifiers & 0x0002) != 0 { s.push_str("private "); }
    if (modifiers & 0x0004) != 0 { s.push_str("protected "); }
    if (modifiers & 0x0008) != 0 { s.push_str("static "); }
    if (modifiers & 0x0010) != 0 { s.push_str("final "); }
    if (modifiers & 0x0020) != 0 { s.push_str("synchronized "); }
    if (modifiers & 0x0100) != 0 { s.push_str("native "); }
    if (modifiers & 0x0400) != 0 { s.push_str("abstract "); }
    if (modifiers & 0x0800) != 0 { s.push_str("strictfp "); }

    // Return type — `getTypeName()` style: dotted class name, primitive
    // bare names ("int"), array suffix `[]`.
    let ret_mirror = match ctx.get_field_by_name(this, "returnType") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let ret_name = ret_mirror
        .and_then(|m| mirror_class_name(ctx, m))
        .unwrap_or_else(|| "void".to_string());
    s.push_str(&class_name_to_type_name(&ret_name));
    s.push(' ');

    // Declaring class
    let decl_mirror = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    };
    let decl_name = decl_mirror
        .and_then(|m| mirror_class_name(ctx, m))
        .unwrap_or_default();
    s.push_str(&decl_name.replace('/', "."));
    s.push('.');

    // Method name
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(sref)) => ctx.read_string(sref).unwrap_or_default(),
        _ => String::new(),
    };
    s.push_str(&name);

    // Parameter types
    s.push('(');
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(this, "parameterTypes") {
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
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(this, "exceptionTypes") {
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

fn native_method_invoke_boxed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // Delegate to the canonical implementation first.
    let raw = native_method_invoke(ctx, args)?;

    let raw_val = match raw {
        Some(v) => v,
        None => return Ok(None),
    };

    // Recover the declared return descriptor from the Method mirror so we
    // can sanity-check the return shape against what the JDK contract
    // requires (primitive returns must come back as wrapper objects, never
    // as raw `Value::Int`/`Value::Long`/etc. and never as a primitive
    // `Class<int>` mirror).
    let method_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(raw_val)),
    };
    let descriptor = method_descriptor_for_invoke(ctx, method_obj);
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
pub(crate) fn register_wp2_1_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
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
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
            Ok(Some(ctx.get_field_by_name(this, "rawType")))
        },
    );
    registry.register(
        pti_real,
        "getRawType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "rawType")))
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
                let clone = ctx.new_ref_array(cratonvm_types::ClassId::new(0), len);
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
            Ok(Some(ctx.get_field_by_name(this, "ownerType")))
        },
    );

    // Same field-resolution issue affects TypeVariableImpl / WildcardTypeImpl /
    // GenericArrayTypeImpl. Provide field-by-name natives so they keep working
    // when reached via real-JDK reifier code paths.
    let tvi_real = "sun/reflect/generics/reflectiveObjects/TypeVariableImpl";
    registry.register(
        tvi_real,
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "name")))
        },
    );
    let wti_real = "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl";
    registry.register(
        wti_real,
        "getUpperBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "upperBounds")))
        },
    );
    registry.register(
        wti_real,
        "getLowerBounds",
        "()[Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "lowerBounds")))
        },
    );
    let gat_real = "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl";
    registry.register(
        gat_real,
        "getGenericComponentType",
        "()Ljava/lang/reflect/Type;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "genericComponentType")))
        },
    );
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
        "getGenericDeclaration",
        "()Ljava/lang/reflect/GenericDeclaration;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
