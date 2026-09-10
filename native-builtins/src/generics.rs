// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.8 — Runtime conversion: parsed signature AST → Java reflective Type objects.
//!
//! The parser itself lives in `cratonvm_reader::signature`. This module
//! contains the runtime layer that consumes parsed nodes and builds
//! `java.lang.reflect.{ParameterizedType, TypeVariable, WildcardType,
//! GenericArrayType}` heap objects via `NativeContext`.

use cratonvm_native_api::registry::NativeContext;
use cratonvm_types::{ClassId, ObjectRef, Value};

use cratonvm_types::error::MethodCallFailed;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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
    static TYPE_PARAM_BUILD_SCOPE: Cell<Option<ObjectRef>> = const { Cell::new(None) };
}
/// A recursive generic bound must reuse the TypeVariable currently being
/// constructed. For example, while building L extends T, resolving T through
/// Class.getTypeParameters() would otherwise re-enter the native builder; the
/// guard below then creates a fallback T with Object as its bound.
/// Cache key's middle component: `class_id_from_mirror(decl)` only resolves
/// a `Class` mirror (it's a reverse lookup into `class_mirrors_reverse`,
/// keyed on Class mirrors specifically). For a `Method`/`Constructor`
/// `GenericDeclaration`, it always returns `None` -- see the CRITICAL note
/// on [`cached_building_type_parameter`] below for why silently skipping
/// the cache there is NOT a harmless miss.
fn type_parameter_build_cache() -> &'static Mutex<HashMap<(usize, i32, String), i32>> {
    static CACHE: OnceLock<Mutex<HashMap<(usize, i32, String), i32>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keys in [`type_parameter_build_cache`] whose value is a PLACEHOLDER: a
/// `TypeVariable` synthesized by the `TypeSig::TypeVar` fallback arm for a
/// variable whose declaration could not be consulted yet, and which therefore
/// carries the default `Object` bound instead of its declared one.
///
/// This happens whenever a type parameter's bound mentions a LATER parameter of
/// the same list -- `<T extends Thing<S>, S extends Something>`. Building `T`
/// resolves the nested `S`, `resolve_declared_type_variable` refuses to
/// re-enter `getTypeParameters()` while the list is under construction, and the
/// fallback publishes an `Object`-bounded stand-in for `S` under `(decl, "S")`.
/// Without this marker `Class.getTypeParameters()` then served that stand-in as
/// the real `S` forever: `Base.class.getTypeParameters()[1].getBounds()` came
/// back `[Object]` where HotSpot answers `[Something]`, and every consumer that
/// resolves a type variable to its bound saw the wrong type. Spring's
/// `ResolvableType.forField(field, declaringClass).resolve()` returned `null`
/// instead of the bound, so `@MockitoBean S something` in
/// `AbstractMockitoBeanAndGenericsIntegrationTests` matched by type against
/// EVERY bean in the context ("found 17 beans of type ?") and AOT processing of
/// the class failed outright.
///
/// A marked entry is a cache MISS for lookups that want the finished article;
/// [`type_param_to_java`] patches the stand-in in place (preserving identity, so
/// the reference already baked into `T`'s bound is corrected too) and clears the
/// mark.
fn type_parameter_placeholder_set(
) -> &'static Mutex<std::collections::HashSet<(usize, i32, String)>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<(usize, i32, String)>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn placeholder_key(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> (usize, i32, String) {
    (
        ctx.vm_identity(),
        ctx.identity_hash_code(decl),
        name.to_string(),
    )
}

/// Whether the cached `TypeVariable` for `(decl, name)` is an `Object`-bounded
/// stand-in rather than the declared parameter. See
/// [`type_parameter_placeholder_set`].
pub(crate) fn is_placeholder_type_parameter(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> bool {
    let key = placeholder_key(ctx, decl, name);
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key)
}

fn mark_placeholder_type_parameter(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) {
    let key = placeholder_key(ctx, decl, name);
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key);
}

fn clear_placeholder_type_parameter(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) {
    let key = placeholder_key(ctx, decl, name);
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

/// CRITICAL for termination, not just an optimization: `com.sun.beans
/// .TypeResolver.resolve(TypeVariable, Map)` (real JDK bytecode) detects a
/// type variable that maps to itself with `map.get(tv) == tv` --
/// **reference** equality, not `.equals()`. If two calls to
/// [`type_param_to_java`] for "the same" conceptual type variable (same
/// declaration + name) return two DIFFERENT (non-`==`) `TypeVariable`
/// objects, that self-check can never fire, and `TypeResolver` walks the
/// bound chain forever instead of terminating -- observed as
/// `createLayoutFromConfigClass` calling `Arrays.hashCode`
/// (`ParameterizedTypeImpl.hashCode` -> `WeakCache.get` ->
/// `TypeResolver.resolve`, itself recursing only 2-3 levels deep, so the
/// interpreter's call stack never grows) upward of 574,000 times in 5
/// minutes with no sign of terminating.
///
/// The original implementation keyed this cache on
/// `class_id_from_mirror(decl)`, which only resolves `Class` mirrors --
/// for a **method- or constructor-scoped** type variable (`decl` is a
/// `Method`/`Constructor` mirror, e.g. `<T> T getAttribute(String name)`),
/// that lookup always returns `None`, the `?`/early-return below skips the
/// cache silently, and every reference to that type variable builds a
/// FRESH, non-identical `TypeVariable` object. Use the `decl` object's own
/// identity hash instead of `class_id_from_mirror` -- it works uniformly
/// for a Class, Method, or Constructor declaration (all are real,
/// individually-addressable heap objects with a stable identity hash), and
/// is exactly the same identity-stability property this cache already
/// relies on for its cached VALUES (`ctx.identity_hash_code(tv)` below).
///
/// `pub(crate)` so `native_class_get_type_parameters`
/// (`native-builtins/src/lang_class.rs`) can consult it directly: real
/// `Class.getTypeParameters()` calls hit this same non-identity gap for the
/// **successfully-declared** parameter case (as opposed to the
/// unresolvable-name fallback this file's `type_sig_to_java` arm already
/// guards), which independently hung Hibernate Validator's
/// `ConstraintHelper`/`TypeHelper` reflection — see
/// `docs/known-issues/springboot/embedded-tomcat-loopback-self-connect-silent-hang-20260719.md`.
pub(crate) fn cached_building_type_parameter(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> Option<Value> {
    let key = (
        ctx.vm_identity(),
        ctx.identity_hash_code(decl),
        name.to_string(),
    );
    let ident = *type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)?;
    ctx.read_var_handle_root(ident)
        .map(|tv| Value::Object(Some(tv)))
}

fn cache_building_type_parameter(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
    tv: ObjectRef,
) {
    let decl_ident = ctx.identity_hash_code(decl);
    ctx.register_var_handle_root(tv);
    let ident = ctx.identity_hash_code(tv);
    type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((ctx.vm_identity(), decl_ident, name.to_string()), ident);
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

struct TypeParamBuildScope(Option<ObjectRef>);

impl TypeParamBuildScope {
    fn new(decl: Value) -> Self {
        let r = match decl {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        TypeParamBuildScope(TYPE_PARAM_BUILD_SCOPE.with(|c| c.replace(r)))
    }
}

impl Drop for TypeParamBuildScope {
    fn drop(&mut self) {
        TYPE_PARAM_BUILD_SCOPE.with(|c| c.set(self.0));
    }
}

fn is_building_type_params_for(decl: ObjectRef) -> bool {
    TYPE_PARAM_BUILD_SCOPE.with(|c| c.get() == Some(decl))
}

/// The current generic-declaration scope as a `Value` (null when unset).
fn current_generic_decl() -> Value {
    GENERIC_DECL_SCOPE
        .with(|c| c.get())
        .map(|o| Value::Object(Some(o)))
        .unwrap_or(Value::Object(None))
}

/// Resolve a symbolic signature class from the loader that owns the current
/// generic declaration.  A global name lookup is insufficient once a forked
/// test or generated class has defined another legitimate copy of the same
/// binary name: its reflective `ParameterizedType` must contain raw classes
/// from that declaration's own loader, otherwise generic resolvers compare
/// incompatible class identities.
fn class_id_in_generic_scope(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Option<cratonvm_types::ClassId> {
    let dbg = crate::nbflags().dbg_lambda_generic && name.contains("ApplicationContextInitializer");
    let decl_opt = GENERIC_DECL_SCOPE.with(|scope| scope.get());
    let near_opt = decl_opt.and_then(|decl| ctx.class_id_from_mirror(decl));
    // `class_id_by_name_near` only looks among classes that are already
    // registered. If the declaring class belongs to an isolated URL loader and
    // its signature-only dependency has not been loaded yet, that lookup falls
    // back to the first global copy. Loading it afterwards cannot repair the
    // already-created `ParameterizedType`, so JAXB/Spring can end up walking a
    // graph containing both the application and child-loader copies. Resolve
    // through the declaring class first: this performs normal parent delegation
    // and defines the child copy when that is what the declaration sees.
    let scoped = near_opt.and_then(|near| {
        ctx.class_id_by_name_via_referencing_class(near, name)
            .ok()
            .or_else(|| ctx.class_id_by_name_near(name, near))
    });
    let global = ctx.class_id_by_name(name);
    if dbg {
        eprintln!(
            "[LAMBDA-GENERIC] class_id_in_generic_scope name={name} decl_present={} near={near_opt:?} scoped={scoped:?} global={global:?}",
            decl_opt.is_some()
        );
    }
    scoped.or(global)
}

/// Resolve a signature class through the declaring class's loader before the
/// process-global fallback. A read-only nearby lookup cannot find a
/// child-loader class until something else has loaded it; reflective generic
/// signatures such as `List<ServiceType>` are often the first use. Falling
/// straight through to the global loader then splits a JAXB/Spring model graph
/// by class identity.
fn resolve_class_id_in_generic_scope(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Option<cratonvm_types::ClassId> {
    if let Some(cid) = class_id_in_generic_scope(ctx, name) {
        return Some(cid);
    }

    let declaring_class = GENERIC_DECL_SCOPE
        .with(|scope| scope.get())
        .and_then(|decl| ctx.class_id_from_mirror(decl));
    if let Some(declaring_class) = declaring_class {
        let loader_id = ctx.loader_id_of_class(declaring_class);
        let loader =
            crate::classloader::defining_loader_for(ctx.vm_identity(), declaring_class.as_u32())
                .or_else(|| {
                    (loader_id >= 3)
                        .then(|| {
                            crate::classloader::loader_object_for_namespace_id(loader_id as u32)
                        })
                        .flatten()
                });
        if let Some(loader) = loader {
            if let Some(mirror) =
                crate::classloader::find_loaded_class_for_loader(ctx, loader, name)
            {
                if let Some(cid) = ctx.class_id_from_mirror(mirror) {
                    return Some(cid);
                }
            }
            let loader_pin = ctx.pin_native_root(loader);
            let name_obj = ctx.create_string(&name.replace('/', "."));
            let name_pin = ctx.pin_native_root(name_obj);
            let loader = ctx.read_native_pin(loader_pin, loader);
            let name_obj = ctx.read_native_pin(name_pin, name_obj);
            let loaded = ctx.invoke_virtual(
                loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj))],
            );
            ctx.unpin_native_roots(name_pin);
            ctx.unpin_native_roots(loader_pin);
            if let Ok(Some(Value::Object(Some(mirror)))) = loaded {
                if let Some(cid) = ctx.class_id_from_mirror(mirror) {
                    return Some(cid);
                }
            }
        }
    }

    ctx.load_class(name)
        .ok()
        .flatten()
        .and_then(|value| match value {
            Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
            _ => None,
        })
}

fn reflective_type_variable_name(ctx: &mut dyn NativeContext, tv: ObjectRef) -> Option<String> {
    let cname = ctx
        .class_name_of_id(ctx.class_id_of_object(tv))
        .unwrap_or_default();
    if cname == "java/lang/reflect/TypeVariable" {
        if let Value::Object(Some(s)) = ctx.get_field(tv, 0) {
            return ctx.read_string(s);
        }
    }
    match ctx.get_field_by_name(tv, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(tv, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
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
    if let Some(type_variable) = cached_building_type_parameter(ctx, decl, name) {
        return Some(type_variable);
    }
    if is_building_type_params_for(decl) {
        return None;
    }
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
            if reflective_type_variable_name(ctx, tv).as_deref() == Some(name) {
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

use crate::try_alloc_concurrent_synthetic;

/// Build the JVM internal name for an array whose component is a concrete
/// (non-generic) type sig. Returns None for type-variable components, wildcards,
/// or other generics that cannot collapse into a plain `Class<?>`.
fn component_array_name(sig: &TypeSig) -> Option<String> {
    match sig {
        TypeSig::Base(ch) => Some(format!("[{}", ch)),
        TypeSig::Class {
            name, type_args, ..
        } if type_args.is_empty() => Some(format!("[L{};", name)),
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
pub fn type_sig_to_java(
    ctx: &mut dyn NativeContext,
    sig: &TypeSig,
) -> Result<Value, MethodCallFailed> {
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
                _ => return Ok(Value::Object(None)),
            };
            let mirror = ctx.primitive_class_mirror(prim_name);
            Ok(Value::Object(Some(mirror)))
        }
        TypeSig::Class {
            name,
            type_args,
            owner,
        } if type_args.is_empty() && owner.is_none() => {
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
            if let Some(cid) = resolve_class_id_in_generic_scope(ctx, name) {
                let mirror = ctx.get_class_mirror(cid);
                Ok(Value::Object(Some(mirror)))
            } else {
                Ok(Value::Object(None))
            }
        }
        TypeSig::Class {
            name,
            type_args,
            owner,
        } => {
            // Parameterized type -> ParameterizedType object.
            // field 0 = rawType (Class mirror), field 1 = actualTypeArguments
            // (Type[]), field 2 = ownerType (Type or null).
            //
            // The owner is reified only when the signature used the nested
            // `Outer<...>.Inner<...>` form, in which case HotSpot returns a
            // ParameterizedType whose getOwnerType() is the enclosing type.
            // Type-variable resolvers (Spring ResolvableType / GenericType-
            // Resolver) walk this owner chain to bind variables declared by an
            // enclosing generic class — e.g. `class TypedInnerTyped extends
            // InnerTyped<Long>` resolves the field `T` (declared on the OUTER
            // `EnclosedInParameterizedType<T>`) only via the owner
            // `EnclosedInParameterizedType<Integer>`.
            //
            // Note: this arm also fires when the inner has NO type args but an
            // owner is present (`type_args` empty, `owner` Some) — that still
            // reifies as a ParameterizedType on HotSpot, so build one here.
            // GC-safety (2026-07-16): `pt` is a freshly-allocated object not
            // yet reachable from any Java-visible root. Every allocating call
            // below (`get_class_mirror`/`load_class` for `raw_val`, the
            // per-element `type_arg_to_java` factory calls building
            // `args_arr`, and the recursive `type_sig_to_java` for the owner
            // type) can trigger a GC that relocates it — pin it for the
            // whole arm and re-read the forwarded reference before every
            // use, matching the established `create_annotation_proxy`
            // pattern. `args_arr` gets the same treatment across its own
            // fill loop (mirrors `build_mirror_array`).
            let pt = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/ParameterizedType", 3)?;
            let pt_pin = ctx.pin_native_root(pt);
            let raw_val = if let Some(cid) = resolve_class_id_in_generic_scope(ctx, name) {
                let m = ctx.get_class_mirror(cid);
                Value::Object(Some(m))
            } else {
                Value::Object(None)
            };
            let pt = ctx.read_native_pin(pt_pin, pt);
            if !matches!(raw_val, Value::Object(None)) {
                ctx.set_field(pt, 0, raw_val);
            }
            let args_arr = {
                let mut args_arr = new_type_array(ctx, type_args.len());
                let args_pin = ctx.pin_native_root(args_arr);
                for (i, arg) in type_args.iter().enumerate() {
                    let val = type_arg_to_java(ctx, arg)?;
                    // ParameterizedType arguments are never null in the JDK
                    // reflection contract. An absent optional dependency is
                    // represented by its erased Object type instead.
                    let val = if matches!(val, Value::Object(None)) {
                        ctx.class_id_by_name("java/lang/Object")
                            .map(|id| Value::Object(Some(ctx.get_class_mirror(id))))
                            .unwrap_or(val)
                    } else {
                        val
                    };
                    args_arr = ctx.read_native_pin(args_pin, args_arr);
                    ctx.set_array_element(args_arr, i, val);
                }
                ctx.read_native_pin(args_pin, args_arr)
            };
            let pt = ctx.read_native_pin(pt_pin, pt);
            ctx.set_field(pt, 1, Value::Object(Some(args_arr)));
            // Owner type (field 2): reify the enclosing type node, or null.
            let owner_val = match owner {
                Some(o) => type_sig_to_java(ctx, o),
                None => Ok(Value::Object(None)),
            }?;
            let pt = ctx.read_native_pin(pt_pin, pt);
            ctx.set_field(pt, 2, owner_val);
            ctx.unpin_native_roots(pt_pin);
            Ok(Value::Object(Some(pt)))
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
                // A type-variable USE resolves by walking the ENCLOSING generic
                // declarations, exactly like Java's lexical scope: the immediate
                // decl (method/constructor/class), then its declaring class, then
                // any outer classes. A method type-parameter bound such as
                // `<S extends T> withType(Class<S>)` references the CLASS's `T`,
                // which the immediate (method) decl does not declare — without the
                // walk-up we fell back to a synthetic stub whose `getName()` is
                // right but which is NOT identity-equal to the class's real `T`
                // and whose bound defaults to `Object`. ByteBuddy's mock builder
                // (`TypeVariableSource.findExpectedVariable`) then fails with
                // "Cannot resolve T", breaking Mockito mocks of any generic type
                // (e.g. Gradle `RepositoryHandler`). Resolving up the scope hands
                // back the real `TypeVariableImpl`, matching HotSpot.
                let mut scope = decl;
                let scope_pin = ctx.pin_native_root(scope);
                for _ in 0..16 {
                    let mut scope = ctx.read_native_pin(scope_pin, scope);
                    if let Some(real) = resolve_declared_type_variable(ctx, scope, name) {
                        return Ok(real);
                    }
                    // Climb one lexical level. `getDeclaringClass` is the right
                    // question for a method/constructor decl and for a MEMBER
                    // class, but it answers NULL for an ANONYMOUS or LOCAL class
                    // (JLS: those are not members of their enclosing class), so
                    // the walk used to stop dead on the first anonymous scope and
                    // fall through to the synthetic stand-in below — whose
                    // `genericDeclaration` is then the anonymous class itself
                    // rather than the class that actually declares the variable.
                    // `new U<E>() { }` inside `class V<E>` is exactly that shape:
                    // HotSpot reports `E`'s declaration as `V`, CratonVM reported
                    // the anonymous `V$1`. netty's
                    // `ReflectionUtil.resolveTypeParameter` then asks
                    // `V$1.isAssignableFrom(V$1)` (true instead of false), loops,
                    // walks off the end of the superclass chain and throws
                    // "cannot determine the type of the type parameter 'E'" —
                    // `io.netty.util.internal.TypeParameterMatcherTest.testInnerClass`.
                    //
                    // Only consulted when `getDeclaringClass` yields nothing, so a
                    // `Method`/`Constructor` scope (which always has a declaring
                    // class, and has no `getEnclosingClass`) never reaches it.
                    let next = match ctx.invoke_virtual(
                        scope,
                        "getDeclaringClass",
                        "()Ljava/lang/Class;",
                        &[],
                    ) {
                        Ok(Some(Value::Object(Some(enclosing)))) if enclosing != scope => {
                            Some(enclosing)
                        }
                        _ => None,
                    };
                    let next = match next {
                        Some(n) => Some(n),
                        None => match ctx.invoke_virtual(
                            scope,
                            "getEnclosingClass",
                            "()Ljava/lang/Class;",
                            &[],
                        ) {
                            Ok(Some(Value::Object(Some(enclosing)))) if enclosing != scope => {
                                Some(enclosing)
                            }
                            _ => None,
                        },
                    };
                    match next {
                        Some(enclosing) => scope = enclosing,
                        None => break,
                    }
                }
            }
            // Fallback (no resolvable declaration in scope): synthetic
            // TypeVariable — field 0 = name, field 1 = bounds (Type[]),
            // field 2 = genericDeclaration. Always 3 fields so the
            // `getGenericDeclaration` native's slot-2 read is in bounds.
            //
            // MUST be cached per (enclosing decl, name), exactly like
            // `type_param_to_java`'s own synthetic-TypeVariable path —
            // without it, EVERY use of an unresolvable-name type variable
            // (one whose name doesn't match any declared type parameter
            // walking up to 16 enclosing scopes) allocates a brand new,
            // non-identical object. `com.sun.beans.TypeResolver.resolve`'s
            // self-reference check for a type variable that maps to itself
            // in its substitution map relies on the SAME object recurring
            // for "the same" variable across repeated resolution passes;
            // a fresh object every time defeats that check and the real
            // bytecode never terminates. Confirmed empirically: this exact
            // gap (there, in `type_param_to_java`'s cache, which only
            // covered the SUCCESSFULLY-resolved-declaration case) let
            // `Arrays.hashCode` get called 574,867+ times in 5 minutes with
            // no sign of terminating, hanging Spring Boot's Thymeleaf
            // `createLayoutFromConfigClass` test — see
            // thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md.
            if let Value::Object(Some(decl)) = current_generic_decl() {
                if let Some(cached) = cached_building_type_parameter(ctx, decl, name) {
                    return Ok(cached);
                }
            }
            // GC-safety (2026-07-16): same unrooted-across-allocation pattern
            // as the ParameterizedType arm above — pin `tv` immediately and
            // re-read the forwarded reference after each allocating call
            // (`create_string`, the `bounds_arr` allocation) before using it.
            let tv = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 3)?;
            let tv_pin = ctx.pin_native_root(tv);
            let name_str = ctx.create_string(name);
            let tv = ctx.read_native_pin(tv_pin, tv);
            ctx.set_field(tv, 0, Value::Object(Some(name_str)));
            // Bounds: default to Object if no bounds known
            let bounds_arr = {
                let bounds_arr = new_type_array(ctx, 1);
                let bounds_pin = ctx.pin_native_root(bounds_arr);
                if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                    let obj_mirror = ctx.get_class_mirror(obj_cid);
                    let bounds_arr = ctx.read_native_pin(bounds_pin, bounds_arr);
                    ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
                }
                ctx.read_native_pin(bounds_pin, bounds_arr)
            };
            let tv = ctx.read_native_pin(tv_pin, tv);
            ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
            ctx.set_field(tv, 2, current_generic_decl());
            // Store into the SAME cache checked at the top of this arm —
            // without this write, the read-side lookup added above is a
            // permanent no-op (every call misses and rebuilds). Only
            // possible to key this when an enclosing decl was in scope.
            if let Value::Object(Some(decl)) = current_generic_decl() {
                cache_building_type_parameter(ctx, decl, name, tv);
                // This stand-in carries the DEFAULT `Object` bound, not the
                // declared one - mark it so `Class.getTypeParameters()` does not
                // serve it as the finished parameter.
                mark_placeholder_type_parameter(ctx, decl, name);
            }
            ctx.unpin_native_roots(tv_pin);
            Ok(Value::Object(Some(tv)))
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
                    return Ok(Value::Object(Some(ctx.get_class_mirror(cid))));
                }
                if let Ok(Some(v)) = ctx.load_class(&name) {
                    return Ok(v);
                }
            }
            // Fallback: GenericArrayType for unresolvable / type-variable components.
            // GC-safety: `gat` held across the recursive (allocating)
            // `type_sig_to_java` call — pin/re-read as above.
            let gat = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/GenericArrayType", 1)?;
            let gat_pin = ctx.pin_native_root(gat);
            let comp_val = type_sig_to_java(ctx, component);
            let gat = ctx.read_native_pin(gat_pin, gat);
            ctx.set_field(gat, 0, comp_val?);
            ctx.unpin_native_roots(gat_pin);
            Ok(Value::Object(Some(gat)))
        }
    }
}

/// Allocate a `java.lang.reflect.Type[]` (component type `Type`, **not**
/// `Object`). HotSpot's generic-reflection methods (`getActualTypeArguments`,
/// `getBounds`, `getUpperBounds`/`getLowerBounds`) all return `Type[]`; code
/// that reflectively invokes them then does `(Type[]) result` — e.g. Spring's
/// `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke`. Allocating the
/// backing array as `Object[]` made that cast throw
/// `ClassCastException: …Object[] / FieldTypeSignature cannot be cast to
/// [Ljava/lang/reflect/Type;` across spring-beans/AOP/binding (bug-05).
fn new_type_array(ctx: &mut dyn NativeContext, len: usize) -> cratonvm_types::ObjectRef {
    // `ensure_class_initialized` force-loads + returns the ClassId directly.
    // `class_id_by_name` alone returns `None` when `java/lang/reflect/Type`
    // has not been loaded yet — which is the common case during the very first
    // `getGenericType()` call — silently degrading the array to `Object[]`.
    let cid = ctx
        .ensure_class_initialized("java/lang/reflect/Type")
        .ok()
        .or_else(|| ctx.class_id_by_name("java/lang/reflect/Type"))
        .unwrap_or(cratonvm_types::ClassId::new(0));
    ctx.new_ref_array(cid, len)
}

/// Convert a TypeArg into a Type object.
fn type_arg_to_java(ctx: &mut dyn NativeContext, arg: &TypeArg) -> Result<Value, MethodCallFailed> {
    match arg {
        TypeArg::Exact(sig) => {
            let value = type_sig_to_java(ctx, sig);
            // A signature may mention an optional dependency absent from the
            // active class path (Hibernate Validator's monetary validators
            // are the concrete case). Reflection must never expose a null
            // Type entry: callers dereference every argument. Erasure to
            // Object is the conservative non-null representation when the
            // referenced class cannot be resolved.
            if matches!(value, Ok(Value::Object(None))) {
                Ok(ctx
                    .class_id_by_name("java/lang/Object")
                    .map(|id| Value::Object(Some(ctx.get_class_mirror(id))))
                    .unwrap_or(value?))
            } else {
                value
            }
        }
        // GC-safety (2026-07-16): every arm below holds a freshly-allocated
        // `wt`/`upper`/`lower` in a Rust local across further allocating
        // calls (`new_type_array`, `get_class_mirror`, the recursive
        // `typesig_to_real_type` bound resolution) before it becomes
        // reachable via `set_field`/`set_array_element`. Pin each local
        // immediately and re-read the forwarded reference before use,
        // matching the established `pin_native_root` contract.
        TypeArg::Extends(sig) => {
            // WildcardType: field 0 = upperBounds, field 1 = lowerBounds
            let wt = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2)?;
            let wt_pin = ctx.pin_native_root(wt);
            let upper = new_type_array(ctx, 1);
            let upper_pin = ctx.pin_native_root(upper);
            let bound_val = typesig_to_real_type(ctx, sig);
            let upper = ctx.read_native_pin(upper_pin, upper);
            ctx.set_array_element(upper, 0, bound_val?);
            let wt = ctx.read_native_pin(wt_pin, wt);
            let upper = ctx.read_native_pin(upper_pin, upper);
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = new_type_array(ctx, 0);
            let wt = ctx.read_native_pin(wt_pin, wt);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            ctx.unpin_native_roots(wt_pin);
            Ok(Value::Object(Some(wt)))
        }
        TypeArg::Super(sig) => {
            let wt = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2)?;
            let wt_pin = ctx.pin_native_root(wt);
            let upper = new_type_array(ctx, 1);
            let upper_pin = ctx.pin_native_root(upper);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                let upper = ctx.read_native_pin(upper_pin, upper);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            let wt = ctx.read_native_pin(wt_pin, wt);
            let upper = ctx.read_native_pin(upper_pin, upper);
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = new_type_array(ctx, 1);
            let lower_pin = ctx.pin_native_root(lower);
            let bound_val = typesig_to_real_type(ctx, sig);
            let lower = ctx.read_native_pin(lower_pin, lower);
            ctx.set_array_element(lower, 0, bound_val?);
            let wt = ctx.read_native_pin(wt_pin, wt);
            let lower = ctx.read_native_pin(lower_pin, lower);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            ctx.unpin_native_roots(wt_pin);
            Ok(Value::Object(Some(wt)))
        }
        TypeArg::Unbounded => {
            // ? => WildcardType with upper=Object, lower=empty
            let wt = try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/WildcardType", 2)?;
            let wt_pin = ctx.pin_native_root(wt);
            let upper = new_type_array(ctx, 1);
            let upper_pin = ctx.pin_native_root(upper);
            if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
                let obj_mirror = ctx.get_class_mirror(obj_cid);
                let upper = ctx.read_native_pin(upper_pin, upper);
                ctx.set_array_element(upper, 0, Value::Object(Some(obj_mirror)));
            }
            let wt = ctx.read_native_pin(wt_pin, wt);
            let upper = ctx.read_native_pin(upper_pin, upper);
            ctx.set_field(wt, 0, Value::Object(Some(upper)));
            let lower = new_type_array(ctx, 0);
            let wt = ctx.read_native_pin(wt_pin, wt);
            ctx.set_field(wt, 1, Value::Object(Some(lower)));
            ctx.unpin_native_roots(wt_pin);
            Ok(Value::Object(Some(wt)))
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
) -> Result<Value, MethodCallFailed> {
    // Bounds may reference type variables (e.g. `<T extends Comparable<T>>`);
    // their declaration is this same generic declaration.
    let _scope = GenericDeclScope::new(generic_decl);
    let _build_scope = TypeParamBuildScope::new(generic_decl);
    // GC-safety (2026-07-16): this is the "enum/type-var builder" residual
    // gap flagged (but never swept) in
    // jit-junit-discovery-reflection-corruption.md
    // — `tv` is freshly-allocated and not yet reachable from any Java-visible
    // root; `create_string` and the `bounds_arr` construction below (which
    // itself calls the allocating `typesig_to_real_type` per bound) can each
    // trigger a GC that relocates it. Pin it for the whole function and
    // re-read the forwarded reference before every use.
    // Reuse the `Object`-bounded stand-in the fallback arm may already have
    // published for this name (see `type_parameter_placeholder_set`) and patch
    // it in place. Allocating a fresh object here instead would leave the
    // earlier parameter's bound - which baked the stand-in in by reference -
    // pointing at an object whose bounds stay wrong forever, and would break the
    // reference identity `com.sun.beans.TypeResolver` depends on.
    let placeholder = match generic_decl {
        Value::Object(Some(decl)) if is_placeholder_type_parameter(ctx, decl, &tp.name) => {
            match cached_building_type_parameter(ctx, decl, &tp.name) {
                Some(Value::Object(Some(existing))) => Some(existing),
                _ => None,
            }
        }
        _ => None,
    };
    let tv = match placeholder {
        Some(existing) => existing,
        None => try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/TypeVariable", 3)?,
    };
    let tv_pin = ctx.pin_native_root(tv);
    let name_str = ctx.create_string(&tp.name);
    let tv = ctx.read_native_pin(tv_pin, tv);
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
    let bounds_arr = if bound_sigs.is_empty() {
        // Default bound is Object
        let mut bounds_arr = new_type_array(ctx, 1);
        let bounds_pin = ctx.pin_native_root(bounds_arr);
        if let Some(obj_cid) = ctx.class_id_by_name("java/lang/Object") {
            let obj_mirror = ctx.get_class_mirror(obj_cid);
            bounds_arr = ctx.read_native_pin(bounds_pin, bounds_arr);
            ctx.set_array_element(bounds_arr, 0, Value::Object(Some(obj_mirror)));
        }
        ctx.read_native_pin(bounds_pin, bounds_arr)
    } else {
        let mut bounds_arr = new_type_array(ctx, bound_sigs.len());
        let bounds_pin = ctx.pin_native_root(bounds_arr);
        for (i, bs) in bound_sigs.iter().enumerate() {
            let val = typesig_to_real_type(ctx, bs)?;
            bounds_arr = ctx.read_native_pin(bounds_pin, bounds_arr);
            ctx.set_array_element(bounds_arr, i, val);
        }
        ctx.read_native_pin(bounds_pin, bounds_arr)
    };
    let tv = ctx.read_native_pin(tv_pin, tv);
    ctx.set_field(tv, 1, Value::Object(Some(bounds_arr)));
    // Publish only after this variable's own bounds are complete. A self-bound
    // such as T extends Comparable<T> must still use the bounded fallback while
    // its recursive graph is being materialized; subsequent parameters (for
    // example L extends T) reuse this canonical completed object.
    if let Value::Object(Some(decl)) = generic_decl {
        let tv = ctx.read_native_pin(tv_pin, tv);
        cache_building_type_parameter(ctx, decl, &tp.name, tv);
        // Bounds are now the declared ones, so this is no longer a stand-in.
        clear_placeholder_type_parameter(ctx, decl, &tp.name);
    }
    ctx.unpin_native_roots(tv_pin);
    Ok(Value::Object(Some(tv)))
}

/// Build a REAL `sun.reflect.generics.reflectiveObjects.*` Type from a `TypeSig`.
///
/// Unlike the bare-interface synthetic objects [`type_sig_to_java`] allocates
/// (whose inherited `Type.getTypeName()` / `Object.toString()` default-method
/// dispatch cannot be intercepted by a native, so a nested generic like
/// `Map<String, List<String>>` renders as `java.lang.reflect.ParameterizedType@…`),
/// real `*Impl` objects carry working `getTypeName()`/`toString()` bytecode and
/// match what the real-JDK reifier hands out for `Method.get­GenericReturnType`.
/// Used for the reflective-Type natives (`Field.getGenericType`,
/// `Class.getGenericSuperclass`/`getGenericInterfaces`) so they render
/// identically to HotSpot. Non-parameterized / type-variable / array / primitive
/// shapes fall back to [`type_sig_to_java`] (a raw `Class<?>` mirror renders
/// fine; a synthetic `TypeVariable` is resolved via the generic-decl scope).
pub(crate) fn typesig_to_real_type(
    ctx: &mut dyn NativeContext,
    sig: &TypeSig,
) -> Result<Value, MethodCallFailed> {
    match sig {
        TypeSig::Class {
            name,
            type_args,
            owner,
        } if !type_args.is_empty() || owner.is_some() => {
            let slashed = name.replace('.', "/");
            let raw_cid = resolve_class_id_in_generic_scope(ctx, &slashed);
            // GC-safety (2026-07-16): `raw_mirror` is a resolved Class mirror
            // held in a Rust local across every allocation below (`args` and
            // its per-element `typearg_to_real_type` factory calls, the
            // recursive `owner_val` build, and the final `pti` alloc)
            // before it is written into `pti` via `set_field_by_name`. Pin
            // it now and re-read the forwarded reference right before use.
            let mut raw_mirror = match raw_cid {
                Some(cid) => ctx.get_class_mirror(cid),
                None => return type_sig_to_java(ctx, sig),
            };
            let raw_pin = ctx.pin_native_root(raw_mirror);
            let pti_cid = match ctx.ensure_class_initialized(
                "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
            ) {
                Ok(c) => c,
                Err(_) => return type_sig_to_java(ctx, sig),
            };
            let mut args = new_type_array(ctx, type_args.len());
            let args_pin = ctx.pin_native_root(args);
            for (i, a) in type_args.iter().enumerate() {
                let v = typearg_to_real_type(ctx, a);
                args = ctx.read_native_pin(args_pin, args);
                ctx.set_array_element(args, i, v?);
            }
            args = ctx.read_native_pin(args_pin, args);
            if crate::nbflags().trace_pti_args {
                let observed_len = ctx.array_length(args);
                if observed_len != type_args.len() {
                    eprintln!(
                        "PTI-TRACE: MISMATCH building {} — type_args.len()={} but array_length(args)={}",
                        slashed,
                        type_args.len(),
                        observed_len
                    );
                } else if observed_len > 8 {
                    eprintln!(
                        "PTI-TRACE: unusually large actualTypeArguments building {} — len={}",
                        slashed, observed_len
                    );
                }
            }
            // A nested generic class has an owner even when the classfile
            // signature uses a flattened binary name and carries no
            // parameterized owner node. HotSpot returns the raw enclosing
            // Class in that case; returning null loses the declaration context
            // and makes Spring resolve same-named variables against a sibling
            // interface (Create.I versus Search.I).
            let owner_val = match owner {
                Some(o) => typesig_to_real_type(ctx, o),
                None => Ok(match slashed.rsplit_once('$') {
                    Some((outer, _)) => {
                        let owner_id = resolve_class_id_in_generic_scope(ctx, outer);
                        owner_id
                            .map(|cid| Value::Object(Some(ctx.get_class_mirror(cid))))
                            .unwrap_or(Value::Object(None))
                    }
                    None => Value::Object(None),
                }),
            }?;
            let owner_pin = match owner_val {
                Value::Object(Some(owner)) => Some(ctx.pin_native_root(owner)),
                _ => None,
            };
            args = ctx.read_native_pin(args_pin, args);
            raw_mirror = ctx.read_native_pin(raw_pin, raw_mirror);
            let nfields = ctx.class_num_total_fields(pti_cid).max(3);
            let pti = ctx.alloc_object(pti_cid, nfields);
            let owner_val = match (owner_val, owner_pin) {
                (Value::Object(Some(owner)), Some(pin)) => {
                    Value::Object(Some(ctx.read_native_pin(pin, owner)))
                }
                _ => owner_val,
            };
            ctx.set_field_by_name(pti, "rawType", Value::Object(Some(raw_mirror)));
            ctx.set_field_by_name(pti, "actualTypeArguments", Value::Object(Some(args)));
            ctx.set_field_by_name(pti, "ownerType", owner_val);
            ctx.unpin_native_roots(raw_pin);
            Ok(Value::Object(Some(pti)))
        }
        _ => type_sig_to_java(ctx, sig),
    }
}

/// Build the REAL `Type` for a single `TypeArg` (used by [`typesig_to_real_type`]).
fn typearg_to_real_type(
    ctx: &mut dyn NativeContext,
    arg: &TypeArg,
) -> Result<Value, MethodCallFailed> {
    match arg {
        TypeArg::Exact(sig) => {
            let value = typesig_to_real_type(ctx, sig);
            // Optional signature-only dependencies can be absent at runtime.
            // Real reflective Type arrays must contain a non-null entry.
            if matches!(value, Ok(Value::Object(None))) {
                Ok(object_class_mirror(ctx))
            } else {
                value
            }
        }
        TypeArg::Extends(sig) => {
            let b = typesig_to_real_type(ctx, sig);
            Ok(real_wildcard_type(ctx, vec![b?], vec![]))
        }
        TypeArg::Super(sig) => {
            // GC-safety (2026-07-16): `b` is computed before `obj`
            // (`object_class_mirror` can allocate a lazily-created mirror),
            // so it sits unrooted in a Rust local across that call. Pin it
            // immediately and re-read the forwarded reference before use.
            let b = typesig_to_real_type(ctx, sig)?;
            let b_pin = match b {
                Value::Object(Some(r)) => Some(ctx.pin_native_root(r)),
                _ => None,
            };
            let obj = object_class_mirror(ctx);
            let b = match (b, b_pin) {
                (Value::Object(Some(r)), Some(pin)) => {
                    Value::Object(Some(ctx.read_native_pin(pin, r)))
                }
                _ => b,
            };
            Ok(real_wildcard_type(ctx, vec![obj], vec![b]))
        }
        TypeArg::Unbounded => {
            let obj = object_class_mirror(ctx);
            Ok(real_wildcard_type(ctx, vec![obj], vec![]))
        }
    }
}

fn object_class_mirror(ctx: &mut dyn NativeContext) -> Value {
    ctx.class_id_by_name("java/lang/Object")
        .map(|c| Value::Object(Some(ctx.get_class_mirror(c))))
        .unwrap_or(Value::Object(None))
}

/// Build a REAL `WildcardTypeImpl` with the given (already-reified) bounds.
///
/// GC-safety (2026-07-16): `upper`/`lower` already hold freshly-built (and,
/// for a recursive bound, freshly-allocated) `Type` objects handed in by the
/// caller. Every one of them sits in a plain `Vec`, invisible to the GC root
/// scan, across all the allocating calls below (`ensure_class_initialized`,
/// two `new_type_array` calls, and the final `alloc_object`). Pin each
/// element immediately on entry and re-read the forwarded reference right
/// before it is stored into `up`/`lo`.
fn real_wildcard_type(ctx: &mut dyn NativeContext, upper: Vec<Value>, lower: Vec<Value>) -> Value {
    let upper_pins: Vec<Option<usize>> = upper
        .iter()
        .map(|v| match v {
            Value::Object(Some(r)) => Some(ctx.pin_native_root(*r)),
            _ => None,
        })
        .collect();
    let lower_pins: Vec<Option<usize>> = lower
        .iter()
        .map(|v| match v {
            Value::Object(Some(r)) => Some(ctx.pin_native_root(*r)),
            _ => None,
        })
        .collect();

    let wti_cid = match ctx
        .ensure_class_initialized("sun/reflect/generics/reflectiveObjects/WildcardTypeImpl")
    {
        Ok(c) => c,
        Err(_) => return Value::Object(None),
    };
    let mut up = new_type_array(ctx, upper.len());
    let up_pin = ctx.pin_native_root(up);
    for (i, v) in upper.iter().enumerate() {
        let refreshed = match (v, upper_pins[i]) {
            (Value::Object(Some(r)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, *r)))
            }
            _ => *v,
        };
        up = ctx.read_native_pin(up_pin, up);
        ctx.set_array_element(up, i, refreshed);
    }
    up = ctx.read_native_pin(up_pin, up);

    let mut lo = new_type_array(ctx, lower.len());
    let lo_pin = ctx.pin_native_root(lo);
    for (i, v) in lower.iter().enumerate() {
        let refreshed = match (v, lower_pins[i]) {
            (Value::Object(Some(r)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, *r)))
            }
            _ => *v,
        };
        lo = ctx.read_native_pin(lo_pin, lo);
        ctx.set_array_element(lo, i, refreshed);
    }
    lo = ctx.read_native_pin(lo_pin, lo);
    up = ctx.read_native_pin(up_pin, up);

    let nfields = ctx.class_num_total_fields(wti_cid).max(2);
    let wti = ctx.alloc_object(wti_cid, nfields);
    ctx.set_field_by_name(wti, "upperBounds", Value::Object(Some(up)));
    ctx.set_field_by_name(wti, "lowerBounds", Value::Object(Some(lo)));
    Value::Object(Some(wti))
}
