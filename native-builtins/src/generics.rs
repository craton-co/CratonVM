// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.8 — Runtime conversion: parsed signature AST → Java reflective Type objects.
//!
//! The parser itself lives in `cratonvm_reader::signature`. This module
//! contains the runtime layer that consumes parsed nodes and builds
//! `java.lang.reflect.{ParameterizedType, TypeVariable, WildcardType,
//! GenericArrayType}` heap objects via `NativeContext`.

use cratonvm_native_api::registry::NativeContext;
use cratonvm_types::{ObjectRef, Value};

use std::cell::Cell;

thread_local! {
    /// The `GenericDeclaration` (Class / Method / Constructor mirror) that owns
    /// the type parameters referenced by type-variable USES in the signature
    /// currently being converted. A type-variable use (`E` inside `Iterator<E>`)
    /// has no declaration site of its own; ByteBuddy's mock generation calls
    /// `TypeVariable.getGenericDeclaration()` and throws
    /// `IllegalStateException: Unknown declaration: null` if it is null. The
    /// enclosing reflection native (`Method.getGenericReturnType`,
    /// `Class.getGenericInterfaces`, …) sets this to the declaring class/method
    /// for the duration of its conversion via [`GenericDeclScope`].
    static GENERIC_DECL_SCOPE: Cell<Option<ObjectRef>> = const { Cell::new(None) };
}

/// RAII guard installing the current [`GENERIC_DECL_SCOPE`] and restoring the
/// previous value on drop (so nested conversions don't leak scope).
pub struct GenericDeclScope(Option<ObjectRef>);

impl GenericDeclScope {
    pub fn new(decl: Value) -> Self {
        let r = match decl {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        GenericDeclScope(GENERIC_DECL_SCOPE.with(|c| c.replace(r)))
    }
}

impl Drop for GenericDeclScope {
    fn drop(&mut self) {
        GENERIC_DECL_SCOPE.with(|c| c.set(self.0));
    }
}

/// The current generic-declaration scope as a `Value` (null when unset).
fn current_generic_decl() -> Value {
    GENERIC_DECL_SCOPE
        .with(|c| c.get())
        .map(|o| Value::Object(Some(o)))
        .unwrap_or(Value::Object(None))
}

/// Resolve a type-variable USE named `name` to the REAL `TypeVariable` object
/// declared by `decl` (a `Class`/`Method`/`Constructor` mirror) via its
/// `getTypeParameters()`. The returned object is the same
/// `sun.reflect…TypeVariableImpl` reflection hands out elsewhere, so a resolver
/// substituting the variable across a hierarchy sees identity equality (as on
/// HotSpot). Returns `None` when `decl` declares no parameter of that name
/// (the variable belongs to an outer scope) — the caller falls back to a
/// synthetic stand-in. `getTypeParameters()` runs the real JDK reflection
/// (it returns real `TypeVariableImpl`s), so this does not re-enter the
/// signature converter.
fn resolve_declared_type_variable(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> Option<Value> {
    let arr = match ctx.invoke_virtual(
        decl,
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return None,
    };
    let len = ctx.array_length(arr);
    for i in 0..len {
        if let Value::Object(Some(tv)) = ctx.get_array_element(arr, i) {
            let tv_name = match ctx.get_field_by_name(tv, "name") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            };
            if tv_name.as_deref() == Some(name) {
                return Some(Value::Object(Some(tv)));
            }
        }
    }
    None
}

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
            // Primitive types -> Class mirror for the primitive.
            //
            // Primitive mirrors have no regular `ClassId`, so `class_id_by_name`
            // returns None for "int"/"long"/etc. — that previously collapsed a
            // primitive parameter in a *generic* signature to `null` (e.g. the
            // `int hashIterations` arg of a `@JsonCreator (int, String,
            // Map<String,List<String>>)` ctor), and Jackson then threw
            // "Unrecognized Type: [null]" deserializing the POJO. Use the
            // dedicated primitive-mirror accessor instead (cf. jmx_openmbean.rs).
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
            let mirror = ctx.primitive_class_mirror(prim_name);
            Value::Object(Some(mirror))
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
            // A type-variable USE (`T` inside `ConstraintValidator<Max, T>`)
            // refers to a type parameter DECLARED by the enclosing generic
            // declaration. Resolve it to that declaration's REAL type-parameter
            // object (the same `sun.reflect…TypeVariableImpl` that
            // `getTypeParameters()` returns) so it is identity-equal to it,
            // exactly as on HotSpot. A synthetic stand-in compares unequal to
            // the real `TypeVariableImpl` and breaks any resolver that
            // substitutes the variable across a class hierarchy — Hibernate
            // Validator then fails to discover a constraint's validated type
            // (`HV000030 No validator found` / `HV000150 multiple validators`).
            if let Value::Object(Some(decl)) = current_generic_decl() {
                if let Some(real) = resolve_declared_type_variable(ctx, decl, name) {
                    return real;
                }
            }
            // Fallback (no resolvable declaration in scope): synthetic
            // TypeVariable — field 0 = name, field 1 = bounds (Type[]),
            // field 2 = genericDeclaration. Always 3 fields so the
            // `getGenericDeclaration` native's slot-2 read is in bounds.
            let tv = alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 3);
            let name_str = ctx.create_string(name);
            ctx.set_field(tv, 0, Value::Object(Some(name_str)));
            // Bounds: default to Object if no bounds known
            let bounds_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
            }
            ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
            ctx.set_field(tv, 2, current_generic_decl());
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
///
/// `generic_decl` is the declaring `Class`/`Executable` mirror (the
/// `GenericDeclaration` that owns this type parameter). It is stored in field 2
/// and returned by `TypeVariable.getGenericDeclaration()`; ByteBuddy's mock
/// generation (`OfTypeVariable$ForLoadedType.getTypeVariableSource`) requires a
/// non-null declaration or it throws `IllegalStateException: Unknown
/// declaration: null`.
pub fn type_param_to_java(
    ctx: &mut dyn NativeContext,
    tp: &TypeParam,
    generic_decl: Value,
) -> Value {
    // Bounds may reference type variables (e.g. `<T extends Comparable<T>>`);
    // their declaration is this same generic declaration.
    let _scope = GenericDeclScope::new(generic_decl);
    let tv = alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 3);
    let name_str = ctx.create_string(&tp.name);
    ctx.set_field(tv, 0, Value::Object(Some(name_str)));
    ctx.set_field(tv, 2, generic_decl);

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
