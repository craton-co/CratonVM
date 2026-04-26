//! WP2.8 — Runtime conversion: parsed signature AST → Java reflective Type objects.
//!
//! The parser itself lives in `rustjvm_reader::signature`. This module
//! contains the runtime layer that consumes parsed nodes and builds
//! `java.lang.reflect.{ParameterizedType, TypeVariable, WildcardType,
//! GenericArrayType}` heap objects via `NativeContext`.

use rustjvm_native_api::registry::NativeContext;
use rustjvm_types::Value;

// Re-export the AST + parser entry points so existing callers can keep
// importing from `crate::generics::...`. Internally everything routes
// through `rustjvm_reader::signature`.
pub use rustjvm_reader::signature::{
    parse_class_signature, parse_field_signature, parse_method_signature, ClassSig, MethodSig,
    TypeArg, TypeParam, TypeSig,
};

use crate::alloc_concurrent_synthetic;

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
            // Non-parameterized class -> Class mirror
            let cid = ctx
                .class_id_by_name(name)
                .or_else(|| ctx.ensure_class_initialized(name).ok());
            if let Some(cid) = cid {
                let mirror = ctx.get_class_mirror(cid);
                Value::Object(Some(mirror))
            } else {
                Value::Object(None)
            }
        }
        TypeSig::Class { name, type_args } => {
            // Parameterized type -> ParameterizedType object
            // field 0 = rawType (Class mirror), field 1 = actualTypeArguments (Type[])
            let pt =
                alloc_concurrent_synthetic(ctx, "java/lang/reflect/ParameterizedType", 2);
            let cid = ctx
                .class_id_by_name(name)
                .or_else(|| ctx.ensure_class_initialized(name).ok());
            if let Some(cid) = cid {
                let raw_mirror = ctx.get_class_mirror(cid);
                ctx.set_field(pt, 0, Value::Object(Some(raw_mirror)));
            }
            let args_arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), type_args.len());
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
            let bounds_arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
            Value::Object(Some(tv))
        }
        TypeSig::Array(component) => {
            // GenericArrayType: field 0 = componentType
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
            let upper = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
            let bound_val = type_sig_to_java(ctx, sig);
            ctx.set_array_element(upper, 0, bound_val);
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            Value::Object(Some(wt))
        }
        TypeArg::Super(sig) => {
            let wt = alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2);
            let upper = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
            let bound_val = type_sig_to_java(ctx, sig);
            ctx.set_array_element(lower, 0, bound_val);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            Value::Object(Some(wt))
        }
        TypeArg::Unbounded => {
            // ? => WildcardType with upper=Object, lower=empty
            let wt = alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2);
            let upper = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
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
        let bounds_arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
        if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
            let obj_mirror = ctx.get_class_mirror(obj_cid);
            ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
        }
        ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
    } else {
        let bounds_arr =
            ctx.new_ref_array(rustjvm_types::ClassId::new(0), bound_sigs.len());
        for (i, bs) in bound_sigs.iter().enumerate() {
            let val = type_sig_to_java(ctx, bs);
            ctx.set_array_element(bounds_arr, i, val);
        }
        ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
    }
    Value::Object(Some(tv))
}
