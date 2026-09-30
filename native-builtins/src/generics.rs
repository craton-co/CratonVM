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
    static GENERIC_DECL_SCOPE: Cell<Option<ScopedDecl>> = const { Cell::new(None) };
    static TYPE_PARAM_BUILD_SCOPE: Cell<Option<ScopedDecl>> = const { Cell::new(None) };
}

/// gen r4w3/rooting: a thread-local is NOT a GC root, and the scoped decl is
/// held across `loadClass` / `getTypeParameters()` / `toArray` upcalls (all
/// GC points) of a moving young GC. A scope entered with a `ctx`
/// ([`GenericDeclScope::new_pinned`]) therefore stores a native pin HANDLE,
/// and readers resolve the decl's CURRENT address through
/// `ctx.read_native_pin`. The pin is taken when the scope is entered, so it
/// sits ABOVE every pin its creator took earlier and BELOW every pin taken
/// inside the scope: LIFO holds as long as the creating function does not
/// unpin one of its own earlier handles while the scope is alive. `Drop` has
/// no `ctx`, so it only restores the previous entry; the pin itself is
/// released by the creator's next lower `unpin_native_roots`, by
/// [`GenericDeclScope::release`], or by `safe_native_call`'s watermark
/// restore when the native returns. gc-common w30-b removed the unrooted
/// `Raw` form and its ctx-less `GenericDeclScope::new` entry, which had no
/// caller left.
#[derive(Clone, Copy)]
enum ScopedDecl {
    Pinned(usize, ObjectRef),
}

impl ScopedDecl {
    fn current(self, ctx: &dyn NativeContext) -> ObjectRef {
        match self {
            ScopedDecl::Pinned(handle, fallback) => ctx.read_native_pin(handle, fallback),
        }
    }

    fn pin(ctx: &mut dyn NativeContext, decl: Value) -> Option<(ScopedDecl, usize)> {
        match decl {
            Value::Object(Some(o)) => {
                let handle = ctx.pin_native_root(o);
                Some((ScopedDecl::Pinned(handle, o), handle))
            }
            _ => None,
        }
    }
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
///
/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 15): keyed `(weak lock key of decl, name)` -- see
/// [`TypeParamCacheKey`]. The key used to be `(vm, identity hash of decl,
/// name)`, so two live declarations with one hash (the per-heap counter
/// wraps) shared their `T` -- B's `getTypeParameters()` answered A's
/// `TypeVariable`, with A's bounds and generic declaration.
///
/// gc-common w30-b (`common-w29e-reflection-type-variable-caches-pin-their-class-loaders`):
/// the row holds the `TypeVariable` ITSELF ([`TypeParamRow`]), reported by the
/// `type-variables` row of `vm/src/memory/native_roots.rs`
/// ([`gc_scan_type_parameter_roots`]) and re-addressed after every moving
/// collection ([`gc_update_type_parameter_refs`]). It used to be a strong JNI
/// global root, and a `TypeVariable`'s field 2 is its declaration, so every
/// class it touched kept its loader -- and so every class that loader defined
/// -- for the life of the VM. Now, on a cycle licensed for loader-conditional
/// metadata, the value is pinned to the loader of the row's owner class
/// ([`TypeParamRow::owner`], through `metadata_pin`) and lives exactly as long
/// as that loader, as a `ClassValue` result does; a built-in-loader owner, an
/// unknown owner and every unlicensed cycle root it outright. A dead loader's
/// rows go twice over: with their declaration's weak lock key
/// ([`forget_type_parameter_keys`]; a declaration reaches its loader, so it
/// dies in the same collection) and with the owner class's unload hint
/// ([`forget_unloaded_type_parameter_rows`]).
fn type_parameter_build_cache() -> &'static Mutex<TypeParamTable> {
    static CACHE: OnceLock<Mutex<TypeParamTable>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The build cache: per VM, [`TypeParamCacheKey`] -> [`TypeParamRow`]. The keys
/// are unique across VMs already; the VM level keeps a collection's scan and
/// remap to its own rows (the `TypeVariable` is an address in that VM's heap).
type TypeParamTable = HashMap<usize, HashMap<TypeParamCacheKey, TypeParamRow>>;

/// `(weak lock key of the GenericDeclaration, type-variable name)`. The lock
/// key (`crate::gc_stable_weak_lock_key`) names one live object of one VM and
/// follows it across moves; it is never minted again once freed.
type TypeParamCacheKey = (usize, String);

/// A cached `TypeVariable` (gc-common w30-b: the object itself, no longer a
/// JNI global-root handle).
#[derive(Clone, Copy)]
struct TypeParamRow {
    /// The class whose defining loader owns the row ([`type_parameter_owner`]):
    /// the declaration's own class for a `Class` declaration, the declaring
    /// class of a `Method` / `Constructor`. `None` when neither resolves; the
    /// row is then rooted outright, as before w30-b.
    owner: Option<u32>,
    /// The `TypeVariable`, current after every moving collection
    /// ([`gc_update_type_parameter_refs`]).
    tv: ObjectRef,
}

/// The class whose loader owns `decl`'s type-parameter rows: `decl`'s own
/// class for a `Class` mirror, the declaring class (`clazz`) of a `Method` /
/// `Constructor`. `None` when that does not resolve through the VM's mirror
/// map -- the row is then rooted, which over-retains and is never unsound. The
/// authoritative `class_id_from_mirror` only, never a layout-slot fallback: a
/// WRONG owner could pin the value to a loader other than the one the
/// declaration keeps alive. `decl` must be CURRENT; reads only.
fn type_parameter_owner(ctx: &dyn NativeContext, decl: ObjectRef) -> Option<u32> {
    let decl_class = ctx.class_name_of_id(ctx.class_id_of_object(decl));
    let mirror = match decl_class.as_deref() {
        Some("java/lang/reflect/Method") | Some("java/lang/reflect/Constructor") => {
            match crate::lang_class::method_clazz_value(ctx, decl) {
                Value::Object(Some(m)) => m,
                _ => return None,
            }
        }
        _ => decl,
    };
    ctx.class_id_from_mirror(mirror).map(|id| id.as_u32())
}

/// gc-common w29-e: drop the `TypeVariable` cache rows and placeholder marks
/// filed under a weak lock key the registry has just FREED
/// (`lib.rs::sweep_lock_keys`, after every collection): their declaration
/// died, and the key is never minted again. Dropping the row is all it takes
/// to release the `TypeVariable` (gc-common w30-b: the row was its only native
/// reference). `keys` holds every weak key the sweep freed, of every kind.
/// One lock at a time, never under the lock-key registry's; nothing here
/// reaches Java. Answers the rows dropped.
pub(crate) fn forget_type_parameter_keys(keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    let set: std::collections::HashSet<usize> = keys.iter().copied().collect();
    let dropped = {
        let mut cache = type_parameter_build_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut dropped = 0usize;
        cache.retain(|_, rows| {
            let before = rows.len();
            rows.retain(|(key, _), _| !set.contains(key));
            dropped += before - rows.len();
            !rows.is_empty()
        });
        dropped
    };
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(key, _)| !set.contains(key));
    dropped
}

/// [`forget_type_parameter_keys`] at VM `vm`'s teardown
/// (`lib.rs::forget_vm_lock_keys`): the rows go with their freed keys, and
/// any row of `vm` still left (a key the teardown did not free) goes too.
pub(crate) fn forget_vm_type_parameter_keys(vm: usize, keys: &[usize]) {
    forget_type_parameter_keys(keys);
    type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);
}

/// gc-common w30-b: drop VM `vm`'s rows owned by a class the collection just
/// UNLOADED (`lib.rs::forget_unloaded_proxy_classes`, from
/// `vm::memory::gc::unload_dead_class_metadata`'s unloading path). Their
/// `TypeVariable`s were pinned to the dead loader, not rooted, so this
/// collection did not mark them: a row left behind would name a reclaimed
/// object, and with the class's loader pin gone the next root scan would
/// report that address as a ROOT. The declaration's weak lock key normally
/// drops the row in the same epilogue ([`forget_type_parameter_keys`]); this
/// covers a declaration that outlived its loader. The placeholder marks
/// filed under the dropped keys go too. One lock at a time; nothing here
/// reaches Java. Answers the rows dropped.
pub fn forget_unloaded_type_parameter_rows(vm: usize, class_ids: &[u32]) -> usize {
    if class_ids.is_empty() {
        return 0;
    }
    let ids: std::collections::HashSet<u32> = class_ids.iter().copied().collect();
    let dropped: Vec<TypeParamCacheKey> = {
        let mut cache = type_parameter_build_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(rows) = cache.get_mut(&vm) else {
            return 0;
        };
        let mut dropped = Vec::new();
        rows.retain(|key, row| {
            if row.owner.is_some_and(|cid| ids.contains(&cid)) {
                dropped.push(key.clone());
                false
            } else {
                true
            }
        });
        if rows.is_empty() {
            cache.remove(&vm);
        }
        dropped
    };
    if !dropped.is_empty() {
        let mut marks = type_parameter_placeholder_set()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in &dropped {
            marks.remove(key);
        }
    }
    dropped.len()
}

/// Root scan for the `type-variables` row of `vm/src/memory/native_roots.rs`
/// (gc-common w30-b): hand each of VM `vm_identity`'s cached `TypeVariable`s
/// to `visit` with its owner class id. The caller roots it, or -- under THIS
/// VM's loader-conditional licence -- pins it to the owner's user loader
/// (`native_roots::defer_or_root`). The rows are copied out and the table
/// lock dropped before the first `visit`, so the lock never nests over the
/// `metadata_pin` / `loader_pin` locks.
pub fn gc_scan_type_parameter_roots(
    vm_identity: usize,
    visit: &mut dyn FnMut(Option<u32>, ObjectRef),
) {
    let rows: Vec<TypeParamRow> = {
        let cache = type_parameter_build_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match cache.get(&vm_identity) {
            Some(rows) => rows.values().copied().collect(),
            None => return,
        }
    };
    for row in rows {
        visit(row.owner, row.tv);
    }
}

/// Post-move remap for [`gc_scan_type_parameter_roots`]'s rows: re-address
/// each of VM `vm_identity`'s `TypeVariable`s the collection moved. The keys
/// (weak lock keys) are stable across moves. A dead `TypeVariable` is absent
/// from the map and keeps its stale address until its row is dropped in the
/// same epilogue ([`forget_type_parameter_keys`] /
/// [`forget_unloaded_type_parameter_rows`]); nothing reads a row in between.
pub fn gc_update_type_parameter_refs(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let mut cache = type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(rows) = cache.get_mut(&vm_identity) else {
        return;
    };
    for row in rows.values_mut() {
        if let Some(&new_addr) = pointer_map.get(&(row.tv.as_ptr() as usize)) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: the collector's forwarding address for a live object.
            row.tv = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
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
///
/// Keyed like [`type_parameter_build_cache`] since gc-common w29-e.
fn type_parameter_placeholder_set(
) -> &'static Mutex<std::collections::HashSet<TypeParamCacheKey>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<TypeParamCacheKey>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// The [`TypeParamCacheKey`] of `(decl, name)` if `decl` already has a weak
/// lock key -- WITHOUT minting one (the read and remove paths: a declaration
/// never cached has no row). `decl` must be CURRENT.
fn existing_type_param_key(
    ctx: &dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> Option<TypeParamCacheKey> {
    crate::existing_weak_lock_key(ctx, decl).map(|key| (key, name.to_string()))
}

/// The [`TypeParamCacheKey`] of `(decl, name)`, minting `decl`'s weak lock key
/// (the write paths). `decl` must be CURRENT.
fn minted_type_param_key(
    ctx: &dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> Option<TypeParamCacheKey> {
    crate::gc_stable_weak_lock_key(ctx, decl)
        .ok()
        .map(|key| (key, name.to_string()))
}

/// Whether the cached `TypeVariable` for `(decl, name)` is an `Object`-bounded
/// stand-in rather than the declared parameter. See
/// [`type_parameter_placeholder_set`].
pub(crate) fn is_placeholder_type_parameter(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
) -> bool {
    let Some(key) = existing_type_param_key(&*ctx, decl, name) else {
        return false;
    };
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key)
}

fn mark_placeholder_type_parameter(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) {
    let Some(key) = minted_type_param_key(&*ctx, decl, name) else {
        return;
    };
    type_parameter_placeholder_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key);
}

fn clear_placeholder_type_parameter(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) {
    let Some(key) = existing_type_param_key(&*ctx, decl, name) else {
        return;
    };
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
/// FRESH, non-identical `TypeVariable` object. Key on the `decl` object
/// itself instead of `class_id_from_mirror` -- it works uniformly for a
/// Class, Method, or Constructor declaration (all are real,
/// individually-addressable heap objects). The key was the decl's identity
/// hash until gc-common w29-e, which two live declarations can share; it is
/// the decl's weak lock key now (see [`type_parameter_build_cache`]).
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
    // Never mints: a declaration nothing was cached for has no row.
    let key = existing_type_param_key(&*ctx, decl, name)?;
    let vm = ctx.vm_identity();
    // The row's `TypeVariable` is current: every moving collection re-addresses
    // it before any mutator resumes ([`gc_update_type_parameter_refs`]).
    let tv = type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .and_then(|rows| rows.get(&key))
        .map(|row| row.tv)?;
    Some(Value::Object(Some(tv)))
}

/// Cache `tv` as `(decl, name)`'s `TypeVariable`, owned by `decl`'s class
/// ([`type_parameter_owner`]). Both must be CURRENT. Neither allocates nor
/// runs Java.
fn cache_building_type_parameter(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
    tv: ObjectRef,
) {
    let owner = type_parameter_owner(&*ctx, decl);
    cache_type_parameter_row(ctx, decl, name, tv, owner);
}

/// [`cache_building_type_parameter`] with the owner class given. The key is
/// minted before the table guard. A replaced row (the placeholder patch
/// re-caches the same `TypeVariable`; a racing builder may have cached its
/// own) needs no release: the row was its only native reference.
fn cache_type_parameter_row(
    ctx: &mut dyn NativeContext,
    decl: ObjectRef,
    name: &str,
    tv: ObjectRef,
    owner: Option<u32>,
) {
    let Some(key) = minted_type_param_key(&*ctx, decl, name) else {
        return;
    };
    let vm = ctx.vm_identity();
    type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(vm)
        .or_default()
        .insert(key, TypeParamRow { owner, tv });
}

/// RAII guard installing the current [`GENERIC_DECL_SCOPE`] and restoring the
/// previous value on drop (so nested conversions don't leak scope).
pub struct GenericDeclScope {
    prev: Option<ScopedDecl>,
    /// Native pin handle of this scope's decl (`None` for a null scope).
    pin: Option<usize>,
}

impl GenericDeclScope {
    /// gen r4w3/rooting: enter the scope with `decl` pinned as a native root,
    /// so readers see its current address after a moving GC.
    pub fn new_pinned(ctx: &mut dyn NativeContext, decl: Value) -> Self {
        let (r, pin) = match ScopedDecl::pin(ctx, decl) {
            Some((s, h)) => (Some(s), Some(h)),
            None => (None, None),
        };
        GenericDeclScope {
            prev: GENERIC_DECL_SCOPE.with(|c| c.replace(r)),
            pin,
        }
    }

    /// Leave the scope AND release its pin (plus every pin taken after it).
    /// Only call when nothing pinned after this scope is still needed.
    pub fn release(self, ctx: &mut dyn NativeContext) {
        if let Some(h) = self.pin {
            ctx.unpin_native_roots(h);
        }
        // `self` drops here, restoring the previous entry.
    }
}

impl Drop for GenericDeclScope {
    fn drop(&mut self) {
        GENERIC_DECL_SCOPE.with(|c| c.set(self.prev));
    }
}

struct TypeParamBuildScope(Option<ScopedDecl>);

impl TypeParamBuildScope {
    /// gen r4w3/rooting: pinned like [`GenericDeclScope::new_pinned`].
    fn new_pinned(ctx: &mut dyn NativeContext, decl: Value) -> Self {
        let r = ScopedDecl::pin(ctx, decl).map(|(s, _)| s);
        TypeParamBuildScope(TYPE_PARAM_BUILD_SCOPE.with(|c| c.replace(r)))
    }
}

impl Drop for TypeParamBuildScope {
    fn drop(&mut self) {
        TYPE_PARAM_BUILD_SCOPE.with(|c| c.set(self.0));
    }
}

fn is_building_type_params_for(ctx: &dyn NativeContext, decl: ObjectRef) -> bool {
    TYPE_PARAM_BUILD_SCOPE
        .with(|c| c.get())
        .map(|s| s.current(ctx))
        == Some(decl)
}

/// The current generic-declaration scope's decl (current address) or `None`.
fn current_generic_decl_ref(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    GENERIC_DECL_SCOPE.with(|c| c.get()).map(|s| s.current(ctx))
}

/// The current generic-declaration scope as a `Value` (null when unset).
fn current_generic_decl(ctx: &dyn NativeContext) -> Value {
    current_generic_decl_ref(ctx)
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
    // gen r4w3/rooting: resolve the scope's decl through its pin (current address).
    let decl_opt = current_generic_decl_ref(ctx);
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

    // gen r4w3/rooting: re-read through the pin -- `class_id_in_generic_scope`
    // above may have loaded a class (a GC point).
    let declaring_class =
        current_generic_decl_ref(ctx).and_then(|decl| ctx.class_id_from_mirror(decl));
    if let Some(declaring_class) = declaring_class {
        let loader_id = ctx.loader_id_of_class(declaring_class);
        let loader =
            crate::classloader::defining_loader_for(ctx.vm_identity(), declaring_class.as_u32())
                .or_else(|| {
                    (loader_id >= 3)
                        .then(|| {
                            crate::classloader::loader_object_for_namespace_id_in(ctx.vm_identity(), loader_id as u32)
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
            // Under the loader's monitor when it is not parallel-capable
            // (`--jdk-only`), as a VM-initiated load (wave 29, lane L5).
            let loaded =
                crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_obj);
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
    if is_building_type_params_for(ctx, decl) {
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
            if ctx.is_jdk_only() {
                return real_parameterized_type(ctx, name, type_args, owner);
            }
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
            // gen r4w3/rooting: the scope's decl is read through its pin, so
            // `decl` is current here (it was a stale thread-local ref before).
            if let Value::Object(Some(decl)) = current_generic_decl(ctx) {
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
                // The pinned root has to FOLLOW the walk. Before 2026-09-10
                // this loop opened with `let mut scope =
                // ctx.read_native_pin(scope_pin, scope);`, which SHADOWED the
                // outer binding: `scope = enclosing` at the bottom wrote the
                // shadow, the shadow died with the iteration, and the next pass
                // re-read the ORIGINAL `decl` out of the one pin that was ever
                // taken. The walk therefore re-tested the immediate declaration
                // sixteen times and never climbed once, so every method-level
                // bound that names its CLASS's variable still fell through to
                // the synthetic stand-in below -- exactly the case the comment
                // above says this loop exists to handle. Re-pin on each climb
                // so `read_native_pin` returns the CURRENT scope, and release
                // the whole run of pins on the way out (the old code leaked one
                // pin per conversion, and every `return` inside the loop leaked
                // it unconditionally).
                let pin_base = ctx.pin_native_root(decl);
                let mut scope_pin = pin_base;
                let mut scope = decl;
                let mut resolved: Option<Value> = None;
                for _ in 0..16 {
                    scope = ctx.read_native_pin(scope_pin, scope);
                    if let Some(real) = resolve_declared_type_variable(ctx, scope, name) {
                        resolved = Some(real);
                        break;
                    }
                    // `resolve_declared_type_variable` invokes
                    // `getTypeParameters()`, which allocates.
                    scope = ctx.read_native_pin(scope_pin, scope);
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
                    let declaring =
                        ctx.invoke_virtual(scope, "getDeclaringClass", "()Ljava/lang/Class;", &[]);
                    // This call allocates too, and the `enclosing != scope`
                    // guard below compares `scope` against a reference the call
                    // just produced. Re-read it off the pin FIRST: a moved
                    // `scope` compares unequal to itself, the self-reference
                    // guard passes, and the walk climbs into its own starting
                    // point. The `getEnclosingClass` arm right below already
                    // re-reads for exactly this reason; this arm did not.
                    scope = ctx.read_native_pin(scope_pin, scope);
                    let next = match declaring {
                        Ok(Some(Value::Object(Some(enclosing)))) if enclosing != scope => {
                            Some(enclosing)
                        }
                        _ => None,
                    };
                    let next = match next {
                        Some(n) => Some(n),
                        None => {
                            let scope = ctx.read_native_pin(scope_pin, scope);
                            match ctx.invoke_virtual(
                                scope,
                                "getEnclosingClass",
                                "()Ljava/lang/Class;",
                                &[],
                            ) {
                                Ok(Some(Value::Object(Some(enclosing)))) if enclosing != scope => {
                                    Some(enclosing)
                                }
                                _ => None,
                            }
                        }
                    };
                    match next {
                        Some(enclosing) => {
                            scope = enclosing;
                            scope_pin = ctx.pin_native_root(scope);
                        }
                        None => break,
                    }
                }
                ctx.unpin_native_roots(pin_base);
                if let Some(real) = resolved {
                    return Ok(real);
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
            if let Value::Object(Some(decl)) = current_generic_decl(ctx) {
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
            // gen r4w3/rooting: re-read the scope's decl through its pin after
            // the allocations above (`try_alloc_concurrent_synthetic` can run
            // <clinit>), rather than storing/keying on a stale thread-local ref.
            let scope_decl = current_generic_decl(ctx);
            ctx.set_field(tv, 2, scope_decl);
            // Store into the SAME cache checked at the top of this arm —
            // without this write, the read-side lookup added above is a
            // permanent no-op (every call misses and rebuilds). Only
            // possible to key this when an enclosing decl was in scope.
            if let Value::Object(Some(decl)) = scope_decl {
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
            if ctx.is_jdk_only() {
                // Strict: the real `GenericArrayTypeImpl`, whose `toString`/
                // `equals`/`hashCode` are the JDK's own -- the interface-named
                // carrier below prints as `java.lang.reflect.GenericArrayType@2b0`
                // (`SerializableTypeWrapperTests.genericArrayType`).
                let comp_val = typesig_to_real_type(ctx, component)?;
                return match ctx.invoke(
                    "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl",
                    "make",
                    "(Ljava/lang/reflect/Type;)Lsun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl;",
                    &[comp_val],
                )? {
                    Some(v @ Value::Object(Some(_))) => Ok(v),
                    _ => Ok(Value::Object(None)),
                };
            }
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

/// `--jdk-only`: a REAL `sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl`
/// for a `Class<..>` signature node, via the JDK's own factory, in place of the
/// interface-named `java/lang/reflect/ParameterizedType` carrier.
fn real_parameterized_type(
    ctx: &mut dyn NativeContext,
    name: &str,
    type_args: &[TypeArg],
    owner: &Option<Box<TypeSig>>,
) -> Result<Value, cratonvm_types::error::MethodCallFailed> {
    let raw_mirror = match resolve_class_id_in_generic_scope(ctx, name) {
        Some(cid) => ctx.get_class_mirror(cid),
        None => return Ok(Value::Object(None)),
    };
    let raw_pin = ctx.pin_native_root(raw_mirror);
    let mut args_arr = new_type_array(ctx, type_args.len());
    let args_pin = ctx.pin_native_root(args_arr);
    for (i, arg) in type_args.iter().enumerate() {
        let val = typearg_to_real_type(ctx, arg)?;
        // Arguments are never null in the reflection contract; an absent
        // optional dependency is its erased `Object`.
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
    let owner_val = match owner {
        Some(o) => typesig_to_real_type(ctx, o)?,
        None => Value::Object(None),
    };
    let args_arr = ctx.read_native_pin(args_pin, args_arr);
    let raw_val = Value::Object(Some(ctx.read_native_pin(raw_pin, raw_mirror)));
    let made = ctx.invoke(
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "make",
        "(Ljava/lang/Class;[Ljava/lang/reflect/Type;Ljava/lang/reflect/Type;)Lsun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl;",
        &[raw_val, Value::Object(Some(args_arr)), owner_val],
    );
    ctx.unpin_native_roots(raw_pin);
    match made? {
        Some(v @ Value::Object(Some(_))) => Ok(v),
        _ => Ok(Value::Object(None)),
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
    // gen r4w3/rooting: both scopes pin `generic_decl`; below it is re-read
    // through the scope (`current_generic_decl`) after each GC point instead
    // of reusing the by-value argument. These pins are the lowest this
    // function takes; they are released by `scope.release` on the success
    // path (an early `?` return leaves them to the caller's watermark).
    let scope = GenericDeclScope::new_pinned(ctx, generic_decl);
    let _build_scope = TypeParamBuildScope::new_pinned(ctx, generic_decl);
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
    // gen r4w3/rooting: `try_alloc_concurrent_synthetic` above can run
    // <clinit>; store the decl's current address (read through the scope pin).
    let generic_decl = current_generic_decl(ctx);
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
    // gen r4w3/rooting: the bounds loop (`typesig_to_real_type`) is a GC
    // point; key the cache on the decl's CURRENT address.
    if let Value::Object(Some(decl)) = current_generic_decl(ctx) {
        let tv = ctx.read_native_pin(tv_pin, tv);
        cache_building_type_parameter(ctx, decl, &tp.name, tv);
        // Bounds are now the declared ones, so this is no longer a stand-in.
        clear_placeholder_type_parameter(ctx, decl, &tp.name);
    }
    let tv = ctx.read_native_pin(tv_pin, tv);
    ctx.unpin_native_roots(tv_pin);
    // gen r4w3/rooting: the scope pin is this function's lowest; releasing
    // it also drops the build-scope pin (nothing reads either past here).
    scope.release(ctx);
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

/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 15): the `TypeVariable` build cache and its placeholder marks are
/// keyed by the declaration's weak lock key. gc-common w30-b: the rows hold
/// the `TypeVariable` itself, owned by a class id, and are reported by
/// [`gc_scan_type_parameter_roots`] instead of a JNI global root. Mock
/// addresses 4 GiB apart share the mock identity hash (the address truncated
/// to `i32`); each test has its own VM identity.
#[cfg(test)]
mod w29e_type_parameter_cache_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock hashes the address and the
        // lock-key registry compares it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Forgets exactly the test's own VM's rows and keys, even on a failure.
    struct Teardown(usize);
    impl Drop for Teardown {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(self.0);
            forget_vm_type_parameter_keys(self.0, &[]);
        }
    }

    fn tv_of(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) -> Option<ObjectRef> {
        match cached_building_type_parameter(ctx, decl, name) {
            Some(Value::Object(Some(tv))) => Some(tv),
            _ => None,
        }
    }

    /// What the `type-variables` root source is handed for `vm`, sorted.
    fn scanned(vm: usize) -> Vec<(Option<u32>, usize)> {
        let mut out = Vec::new();
        gc_scan_type_parameter_roots(vm, &mut |owner, tv| out.push((owner, tv.as_ptr() as usize)));
        out.sort_unstable();
        out
    }

    fn rows_of(vm: usize) -> usize {
        type_parameter_build_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&vm)
            .map_or(0, |rows| rows.len())
    }

    /// Two live declarations of ONE VM with one identity hash keep their own
    /// `T`; the placeholder mark of one is not the other's.
    #[test]
    fn same_hash_declarations_keep_their_own_type_variables() {
        const VM: usize = 0x29E0_1501;
        let _t = Teardown(VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (decl_a, decl_b) = (at(0x1_29E0_1500), at(0x2_29E0_1500));
        assert_eq!(ctx.identity_hash_code(decl_a), ctx.identity_hash_code(decl_b));
        let (tv_a, tv_b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());

        cache_type_parameter_row(&mut ctx, decl_a, "T", tv_a, None);
        mark_placeholder_type_parameter(&mut ctx, decl_a, "T");
        assert_eq!(tv_of(&mut ctx, decl_b, "T"), None, "B has no T yet");
        assert!(!is_placeholder_type_parameter(&mut ctx, decl_b, "T"));

        cache_type_parameter_row(&mut ctx, decl_b, "T", tv_b, None);
        assert_eq!(tv_of(&mut ctx, decl_a, "T"), Some(tv_a));
        assert_eq!(tv_of(&mut ctx, decl_b, "T"), Some(tv_b));
        assert!(is_placeholder_type_parameter(&mut ctx, decl_a, "T"));
        assert!(!is_placeholder_type_parameter(&mut ctx, decl_b, "T"));

        // Re-caching the same `TypeVariable` (the placeholder patch) replaces
        // the row in place and takes no JNI global root (gc-common w30-b).
        let roots = ctx.global_root_count();
        cache_type_parameter_row(&mut ctx, decl_a, "T", tv_a, None);
        clear_placeholder_type_parameter(&mut ctx, decl_a, "T");
        assert_eq!(ctx.global_root_count(), roots);
        assert_eq!(rows_of(VM), 2);
        assert!(!is_placeholder_type_parameter(&mut ctx, decl_a, "T"));
        assert_eq!(tv_of(&mut ctx, decl_a, "T"), Some(tv_a));
    }

    /// The lock-key sweep frees a dead declaration's key: its rows and marks
    /// go, and its `TypeVariable` is no longer reported to the collector. The
    /// live collider keeps its row.
    #[test]
    fn a_dead_declarations_rows_go_and_its_root_is_released() {
        const VM: usize = 0x29E0_1502;
        let _t = Teardown(VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (dead, live) = (at(0x1_29E0_1508), at(0x2_29E0_1508));
        let (tv_dead, tv_live) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        cache_type_parameter_row(&mut ctx, dead, "E", tv_dead, Some(0x30B0));
        mark_placeholder_type_parameter(&mut ctx, dead, "E");
        cache_type_parameter_row(&mut ctx, live, "E", tv_live, Some(0x30B0));
        assert_eq!(scanned(VM).len(), 2);

        let live_addr = live.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM, &|x| x == live_addr);
        assert_eq!(tv_of(&mut ctx, dead, "E"), None);
        assert!(!is_placeholder_type_parameter(&mut ctx, dead, "E"));
        assert_eq!(tv_of(&mut ctx, live, "E"), Some(tv_live));
        assert_eq!(
            scanned(VM),
            vec![(Some(0x30B0), tv_live.as_ptr() as usize)],
            "the dead declaration's TypeVariable is no longer a root"
        );
    }

    /// gc-common w30-b: a row whose owner class unloaded (its loader died) is
    /// dropped by the unload hint with its placeholder mark, and is no longer
    /// reported; a row of another class, and another VM's row of the same
    /// class id, stay.
    #[test]
    fn a_dead_loaders_rows_go_with_the_unload_hint() {
        const VM: usize = 0x30B0_0001;
        const OTHER_VM: usize = 0x30B0_0002;
        let _t = Teardown(VM);
        let _o = Teardown(OTHER_VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (gone_decl, kept_decl) = (at(0x30B0_0010), at(0x30B0_0018));
        let (tv_gone, tv_kept) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        cache_type_parameter_row(&mut ctx, gone_decl, "K", tv_gone, Some(0x30B1));
        mark_placeholder_type_parameter(&mut ctx, gone_decl, "K");
        cache_type_parameter_row(&mut ctx, kept_decl, "K", tv_kept, Some(0x30B2));
        let mut other = crate::test_utils::mock_ctx();
        other.set_vm_identity(OTHER_VM);
        let (other_decl, tv_other) = (at(0x30B0_0020), other.fresh_object_ref());
        cache_type_parameter_row(&mut other, other_decl, "K", tv_other, Some(0x30B1));

        assert_eq!(forget_unloaded_type_parameter_rows(VM, &[0x30B1]), 1);
        assert_eq!(tv_of(&mut ctx, gone_decl, "K"), None);
        assert!(!is_placeholder_type_parameter(&mut ctx, gone_decl, "K"));
        assert_eq!(tv_of(&mut ctx, kept_decl, "K"), Some(tv_kept));
        assert_eq!(scanned(VM), vec![(Some(0x30B2), tv_kept.as_ptr() as usize)]);
        assert_eq!(
            scanned(OTHER_VM),
            vec![(Some(0x30B1), tv_other.as_ptr() as usize)]
        );
        assert_eq!(forget_unloaded_type_parameter_rows(VM, &[]), 0);
    }

    /// gc-common w30-b: a live row keeps its value across a relocation -- the
    /// remap re-addresses the `TypeVariable`, the lookup answers the new
    /// address and the scan reports it. Another VM's row is not touched by
    /// this VM's pointer map.
    #[test]
    fn a_live_row_keeps_its_value_across_a_relocation() {
        const VM: usize = 0x30B0_0003;
        const OTHER_VM: usize = 0x30B0_0004;
        let _t = Teardown(VM);
        let _o = Teardown(OTHER_VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let decl = at(0x30B0_0030);
        let tv = at(0x30B0_0038);
        cache_type_parameter_row(&mut ctx, decl, "V", tv, None);
        let mut other = crate::test_utils::mock_ctx();
        other.set_vm_identity(OTHER_VM);
        cache_type_parameter_row(&mut other, at(0x30B0_0040), "V", tv, None);

        let moved_to = 0x1_30B0_0038usize;
        let mut map = cratonvm_types::PointerMap::new();
        map.insert(tv.as_ptr() as usize, moved_to);
        gc_update_type_parameter_refs(VM, &map);
        assert_eq!(tv_of(&mut ctx, decl, "V"), Some(at(moved_to)));
        assert_eq!(scanned(VM), vec![(None, moved_to)]);
        assert_eq!(scanned(OTHER_VM), vec![(None, tv.as_ptr() as usize)]);
        // An empty map is a no-op.
        gc_update_type_parameter_refs(VM, &cratonvm_types::PointerMap::new());
        assert_eq!(scanned(VM), vec![(None, moved_to)]);
    }

    /// gc-common w30-b: the owner of a `Class` declaration is its class, of a
    /// `Method` its declaring class (`clazz`); an object that is neither a
    /// resolvable mirror nor an executable has none, so its rows are rooted
    /// outright (as bootstrap-loader rows are, by `defer_or_root`).
    #[test]
    fn the_owner_is_the_declarations_class() {
        const VM: usize = 0x30B0_0005;
        let _t = Teardown(VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let mirror = ctx.get_class_mirror(ClassId::new(0x30B5));
        assert_eq!(type_parameter_owner(&ctx, mirror), Some(0x30B5));

        let method = match ctx.new_object("java/lang/reflect/Method") {
            Ok(Some(Value::Object(Some(m)))) => m,
            other => panic!("mock new_object: {other:?}"),
        };
        ctx.set_field(method, 0, Value::Object(Some(mirror)));
        assert_eq!(type_parameter_owner(&ctx, method), Some(0x30B5));

        let clazzless = match ctx.new_object("java/lang/reflect/Constructor") {
            Ok(Some(Value::Object(Some(c)))) => c,
            other => panic!("mock new_object: {other:?}"),
        };
        assert_eq!(type_parameter_owner(&ctx, clazzless), None);

        // The public write path records that owner.
        let tv = ctx.fresh_object_ref();
        cache_building_type_parameter(&mut ctx, method, "M", tv);
        assert_eq!(scanned(VM), vec![(Some(0x30B5), tv.as_ptr() as usize)]);
    }
}
