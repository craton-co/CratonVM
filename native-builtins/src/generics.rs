//! WP2.8 — Runtime conversion: parsed signature AST → Java reflective Type objects.
//!
//! The parser itself lives in `cratonvm_reader::signature`. This module
//! contains the runtime layer that consumes parsed nodes and builds
//! `java.lang.reflect.{ParameterizedType, TypeVariable, WildcardType,
//! GenericArrayType}` heap objects via `NativeContext`.

use cratonvm_native_api::registry::NativeContext;
use cratonvm_types::Value;

// Re-export the AST + parser entry points so existing callers can keep
// importing from `crate::generics::...`. Internally everything routes
// through `cratonvm_reader::signature`.
pub use cratonvm_reader::signature::{
    parse_class_signature, parse_field_signature, parse_method_signature, ClassSig, MethodSig,
    TypeArg, TypeParam, TypeSig,
};

use crate::alloc_concurrent_synthetic;

/// Build the JVM internal name for an array whose component is a concrete
/// (non-generic) type sig. Returns None for type-variable components, wildcards,
/// or other generics that cannot collapse into a plain `Class<?>`.
fn component_array_name(sig: &TypeSig) -> Option<String> {
    match sig {
        TypeSig::Base(ch) => Some(format!("[{}", ch)),
        TypeSig::Class { name, type_args } if type_args.is_empty() => {
            Some(format!("[L{};", name))
        }
        TypeSig::Class { .. } => {
            // Parameterized component (e.g. List<T>) -> erase to raw class.
            // Real JDK reifier produces a GenericArrayType here, but Spring
            // (and most callers) accept the erased Class<?> for `.resolve()`.
            // We still return None so the caller falls back to GenericArrayType
            // for parametric components — preserving prior behavior.
            None
        }
        TypeSig::TypeVar(_) => None,
        TypeSig::Array(inner) => {
            let inner_name = component_array_name(inner)?;
            Some(format!("[{}", inner_name))
        }
    }
}

/// Convert a TypeSig into a java.lang.reflect.Type runtime object.
pub fn type_sig_to_java(ctx: &mut dyn NativeContext, sig: &TypeSig) -> Value {
    match sig {
        TypeSig::Base(ch) => {
            // Primitive types -> Class mirror for the primitive
            let prim_name = match ch {
                'B' => "byte",
                'C' => "char",
                'D' => "double",
                'F' => "float",
                'I' => "int",
                'J' => "long",
                'S' => "short",
                'Z' => "boolean",
                'V' => "void",
                _ => return Value::Object(None),
            };
            if let Some(cid) = ctx.class_id_by_name(prim_name) {
                let mirror = ctx.get_class_mirror(cid);
                Value::Object(Some(mirror))
            } else {
                Value::Object(None)
            }
        }
        TypeSig::Class { name, type_args } if type_args.is_empty() => {
            // Non-parameterized class -> Class mirror.
            //
            // Use `load_class` (loads but does NOT trigger <clinit>) — calling
            // `ensure_class_initialized` here cascades through Joda's
            // `DateTimeZone.<clinit>` (which fails on default-tz lookup),
            // surfacing as `arg.resolve()==null` in Spring's
            // `GenericConversionService.getRequiredTypeInfo` and the IAE
            // "Unable to determine source type <S> and target type <T>".
            // Real-JDK reifier likewise returns Class mirrors without forcing
            // initialization.
            if let Some(cid) = ctx.class_id_by_name(name) {
                let mirror = ctx.get_class_mirror(cid);
                Value::Object(Some(mirror))
            } else if let Ok(Some(v)) = ctx.load_class(name) {
                v
            } else {
                Value::Object(None)
            }
        }
        TypeSig::Class { name, type_args } => {
            // Parameterized type -> ParameterizedType object
            // field 0 = rawType (Class mirror), field 1 = actualTypeArguments (Type[])
            let pt =
                alloc_concurrent_synthetic(ctx, "java/lang/reflect/ParameterizedType", 2);
            let raw_val = if let Some(cid) = ctx.class_id_by_name(name) {
                let m = ctx.get_class_mirror(cid);
                Value::Object(Some(m))
            } else if let Ok(Some(v)) = ctx.load_class(name) {
                v
            } else {
                Value::Object(None)
            };
            if !matches!(raw_val, Value::Object(None)) {
                ctx.set_field(pt, 0, raw_val);
            }
            let args_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), type_args.len());
            for (i, arg) in type_args.iter().enumerate() {
                let val = type_arg_to_java(ctx, arg);
                ctx.set_array_element(args_arr, i, val);
            }
            ctx.set_field(pt, 1, Value::Object(Some(args_arr)));
            Value::Object(Some(pt))
        }
        TypeSig::TypeVar(name) => {
            // TypeVariable: field 0 = name (String), field 1 = bounds (Type[])
            let tv = alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 2);
            let name_str = ctx.create_string(name);
            ctx.set_field(tv, 0, Value::Object(Some(name_str)));
            // Bounds: default to Object if no bounds known
            let bounds_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
            Value::Object(Some(tv))
        }
        TypeSig::Array(component) => {
            // For concrete component types (primitive or non-generic class), the
            // real JDK reifier returns a plain `Class<?>` for the array type
            // (e.g. signature `[C` => `char[].class`, not a GenericArrayType).
            // Only `T[]` / `List<T>[]` etc. become GenericArrayType.
            //
            // Spring's `ResolvableType.resolve()` returns null for any non-Class
            // generic type — so for `Formatter<char[]>` this caused
            // "Unable to extract the parameterized field type from Formatter
            //  [CharArrayFormatter]".
            let array_name = component_array_name(component);
            if let Some(name) = array_name {
                if let Some(cid) = ctx.class_id_by_name(&name) {
                    return Value::Object(Some(ctx.get_class_mirror(cid)));
                }
                if let Ok(Some(v)) = ctx.load_class(&name) {
                    return v;
                }
            }
            // Fallback: GenericArrayType for unresolvable / type-variable components.
            let gat = alloc_concurrent_synthetic(ctx, "java/lang/reflect/GenericArrayType", 1);
            let comp_val = type_sig_to_java(ctx, component);
            ctx.set_field(gat, 0, comp_val);
            Value::Object(Some(gat))
        }
    }
}

/// Convert a TypeArg into a Type object.
fn type_arg_to_java(ctx: &mut dyn NativeContext, arg: &TypeArg) -> Value {
    match arg {
        TypeArg::Exact(sig) => type_sig_to_java(ctx, sig),
        TypeArg::Extends(sig) => {
            // WildcardType: field 0 = upperBounds, field 1 = lowerBounds
            let wt = alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2);
            let upper = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            let bound_val = type_sig_to_java(ctx, sig);
            ctx.set_array_element(upper, 0, bound_val);
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            Value::Object(Some(wt))
        }
        TypeArg::Super(sig) => {
            let wt = alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2);
            let upper = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            let bound_val = type_sig_to_java(ctx, sig);
            ctx.set_array_element(lower, 0, bound_val);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            Value::Object(Some(wt))
        }
        TypeArg::Unbounded => {
            // ? => WildcardType with upper=Object, lower=empty
            let wt = alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2);
            let upper = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            Value::Object(Some(wt))
        }
    }
}

/// Convert a TypeParam into a TypeVariable runtime object, using its bounds.
pub fn type_param_to_java(ctx: &mut dyn NativeContext, tp: &TypeParam) -> Value {
    let tv = alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 2);
    let name_str = ctx.create_string(&tp.name);
    ctx.set_field(tv, 0, Value::Object(Some(name_str)));

    // Collect bounds
    let mut bound_sigs: Vec<&TypeSig> = Vec::new();
    if let Some(ref cb) = tp.class_bound {
        bound_sigs.push(cb);
    }
    for ib in &tp.interface_bounds {
        bound_sigs.push(ib);
    }
    if bound_sigs.is_empty() {
        // Default bound is Object
        let bounds_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
        if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
            let obj_mirror = ctx.get_class_mirror(obj_cid);
            ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
        }
        ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
    } else {
        let bounds_arr =
            ctx.new_ref_array(cratonvm_types::ClassId::new(0), bound_sigs.len());
        for (i, bs) in bound_sigs.iter().enumerate() {
            let val = type_sig_to_java(ctx, bs);
            ctx.set_array_element(bounds_arr, i, val);
        }
        ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
    }
    Value::Object(Some(tv))
}
