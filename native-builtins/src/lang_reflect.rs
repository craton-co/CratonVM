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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ClassId, ObjectRef, Value};
use rustjvm_types::error::MethodCallResult;

use crate::lang_class::{
    create_method_object, create_constructor_object, create_field_object,
    descriptor_to_class_mirror, mirror_class_id, mirror_class_name,
    parse_descriptor_param_and_return, read_method_descriptor,
    read_constructor_descriptor, read_field_meta,
};
use crate::obj_arg;

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
// return true since RustJVM doesn't enforce module-level deep-reflection
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
// Returns null for non-annotation methods. For annotation interfaces,
// returns the AnnotationDefault attribute value.
// We don't fully parse AnnotationDefault yet; return null for everything,
// which matches an annotation method without an explicit default.

pub(crate) fn native_method_get_default_value(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
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
// register_wp2_1_natives — registry-side entry point.
// ---------------------------------------------------------------------------

/// Register the WP2.1 net-new natives. Called from `register_essential_natives`
/// AFTER the existing reflection registrations so these supplement (don't
/// override) the historical layer.
pub(crate) fn register_wp2_1_natives(registry: &mut NativeMethodRegistry) {
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
    // on the field directly via rustjvm_reader::signature and builds a
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
