// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Field reference resolution and invoke argument/return plumbing.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: Field resolution`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::site_cache::{site_stats, FieldSiteCache, MethodSiteCache, MethodSiteInfo};
use super::*;

/// Resolve a constant-pool field reference.
///
/// **This is the resolution core, not the public entry point.** New callers
/// outside `crate::runtime::interpreter` go through
/// [`crate::runtime::resolve::MemberResolver::field_ref`], which tags the
/// answer with the VM that produced it and converts failures into the
/// structured [`crate::runtime::resolve::ResolveError`]. It is `pub(crate)`
/// only so that `MemberResolver` can delegate here; `runtime::resolve::guard`
/// fails the build if anything else names it.
pub(crate) fn resolve_field_ref(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<ResolvedField, MethodCallFailed> {
    // Before the constant pool is read: see `resolve_field_in_class`'s fill.
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (field_class_name, field_name, class_idx) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        let (class_idx, nat_idx) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::FieldReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => {
                return Err(VmError::Internal {
                    message: format!("invalid field ref at cp#{cp_index}"),
                }
                .into());
            }
        };
        let class_name = class
            .constant_pool
            .get_class_name(class_idx)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{class_idx}"),
            })?
            .to_string();
        let (field_name, _descriptor) =
            class
                .constant_pool
                .get_name_and_type(nat_idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!("invalid name_and_type at cp#{nat_idx}"),
                })?;
        (class_name, field_name.to_string(), class_idx)
    };

    // JVMS §5.4.3: an owner whose class entry failed to resolve stays failed
    // (see `resolve_field_ref_loader_aware`). This threadless core cannot
    // build the recorded throwable, so it only declines: its callers are the
    // compiler's field peeks, which then leave the site to the interpreter,
    // and the interpreter rethrows the record (interpreter round i1 wave 25,
    // lane L5). Without it a peek could bake a field of an owner another
    // entry loaded since, and compiled code would read it where the
    // interpreter throws.
    if resolution_failure_is_recorded(shared, current_class_id, class_idx) {
        return Err(crate::error::LinkageError::NoClassDefFoundError {
            class_name: field_class_name,
        }
        .into());
    }

    // A background compiler has no `JvmThread`, so it cannot drive the
    // referencing class's defining loader on a cold symbolic reference. Do
    // not let it seed this per-(ClassId, cp-index) cache through the flat
    // global store: the interpreter would later accept that stale result
    // instead of applying JVMS initiating-loader resolution. Once the owner
    // is known to this loader, using the cache is safe again.
    let loader_sensitive = should_use_loader_initiated_resolution(shared, current_class_id)
        && !field_class_name.starts_with('[')
        && !is_global_resolution_namespace(&field_class_name)
        && matches!(
            shared
                .classes
                .class_manager
                .read()
                .get_loader_id(current_class_id),
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        );
    // A `javax/`-style owner name is not loader-sensitive (the global route
    // answers it), except where the referencing user loader has its OWN record
    // of it — a class it defined under the name, or its memoised `loadClass`
    // answer — which is what the interpreter's resolver answers
    // (`lookup_loader_initiated`); the flat store below would hand the JIT
    // java.base's same-named class (interpreter round i1 wave 27, lane L5;
    // `L5W27JdkNamedOwnClass`). `None` for every other caller.
    // A hidden class's own class-file name is the hidden class (JIT round 21
    // wave 20, lane hidden-self-resolve, `CRATONVM_HIDDEN_SELF_CP_BY_ID`):
    // the flat load below LOADED the class path's ordinary copy of the bytes
    // for a compile of the hidden class's `getstatic <ITSELF>.sCount`, and
    // bound that third class's static.
    let hidden_self =
        super::constants::hidden_self_cp_owner(shared, current_class_id, &field_class_name);
    let loader_local_id = if hidden_self.is_some() {
        hidden_self
    } else if loader_sensitive || is_user_loader_global_name(&field_class_name) {
        lookup_loader_initiated(shared, current_class_id, &field_class_name)
    } else {
        None
    };
    // Cloned out (plain data): the decline below takes the class manager's
    // read lock, which must not nest inside this cache's guard.
    let cached_entry = shared
        .classes
        .resolution_cache
        .read()
        .get_field(current_class_id, cp_index)
        .cloned();
    if let Some(cached) = cached_entry.as_ref() {
        // A per-callsite field entry can have been seeded before this loader
        // defined its own owner class (for example while a forked Spring test
        // context is being prepared). It is not valid merely because the
        // loader has since initiated that name: the cached declaring identity
        // must also be that exact owner. Otherwise getstatic returns the app
        // copy's enum singleton from a fork-loaded caller. (`loader_local_id`
        // is `None` for a caller that is not loader-sensitive and has no own
        // record, so such a caller takes the cached entry, as before.)
        if loader_local_id.map_or(true, |id| cached.declaring_class_id == id) {
            return Ok(cached.clone());
        }
        // The site has a resolution and the loader's record now names an
        // owner it does not cover (the interpreter's own test: the owner or
        // a superclass of it declares the cached field; an inherited field
        // named through a subclass is the common case, and is answered from
        // the entry). An isolated URL loader's site keeps its entry
        // (`resolve_field_ref_loader_aware`), any other re-resolves on its
        // next execution; this peek cannot tell which (the isolation
        // predicate needs a thread), so it leaves the site to the
        // interpreter rather than bake an owner the interpreter may not use.
        // Round 15 wave 5, lane fieldowner2.
        let covered = loader_local_id.is_some_and(|id| {
            shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(id, cached.declaring_class_id)
        });
        // A covered entry is what the interpreter answers too; re-resolving
        // it only re-filled the same answer under the cache's write lock.
        if covered && isolated_field_site_sticky_enabled() {
            return Ok(cached.clone());
        }
        if !covered
            && loader_specific_peek(
                shared,
                current_class_id,
                &field_class_name,
                loader_sensitive,
            )
        {
            return Err(VmError::Internal {
                message: format!(
                    "field {field_class_name}.{field_name}: the site's resolution and its loader's record disagree"
                ),
            }
            .into());
        }
    }

    let field_class_id = match loader_local_id {
        Some(id) => id,
        None if loader_sensitive => {
            return Err(VmError::Internal {
                message: format!(
                    "field {field_class_name}.{field_name} requires loader-aware execution resolution"
                ),
            }
            .into());
        }
        // FO4-3 (round 15 wave 5, lane fieldowner2; opt-in): a user loader's
        // `javax/`-style owner nobody has answered for it is not LOADED by
        // this peek (a class-load event compile timing caused, which HotSpot's
        // compilers never have); only a class already known in the loader's
        // namespace answers, and anything else leaves the site to the
        // interpreter. Every caller of this core is a compile door.
        None if (field_peek_no_load_enabled()
            || (field_class_name.starts_with("java/")
                && loader_global_name_ask_enabled(shared, &field_class_name)))
            && is_user_loader_global_name(&field_class_name)
            && user_defined_loader_of(shared, current_class_id).is_some() =>
        {
            let known = {
                let cm = shared.classes.class_manager.read();
                super::constants::class_resolved_without_loading(
                    shared,
                    &cm,
                    current_class_id,
                    &field_class_name,
                )
            };
            known.ok_or_else(|| VmError::Internal {
                message: format!(
                    "field {field_class_name}.{field_name}: owner not loaded for this loader"
                ),
            })?
        }
        // Interpreter round i4 wave 94, lane L2
        // (`i93-L2-compile-door-field-peek-loads-class-path-owners`): wherever
        // the doors can defer a site (`field_peek_never_loads_enabled`), the
        // peek loads NO owner, for any referencing loader. HotSpot's compilers
        // never load a class (C1 patches, C2 traps; JVMS §5.4.3 loads it when
        // the site first runs), so a class-path owner of a cold site must not
        // appear in `-verbose:class`, a JVMTI `ClassLoad`, or the loader's
        // record because a method compiled. The unresolved site is deferred
        // by the door (`DirectHelperTable::field_site_cp`) or trapped by the
        // optimizing tier (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`); on aarch64
        // (wave 96) the backend declines the method instead.
        None if field_peek_never_loads_enabled() => {
            let known = {
                let cm = shared.classes.class_manager.read();
                super::constants::class_resolved_without_loading(
                    shared,
                    &cm,
                    current_class_id,
                    &field_class_name,
                )
            };
            if known.is_none() && crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] field peek: owner {field_class_name} not loaded, \
                     not loaded by the compile (class_id={} cp={cp_index})",
                    current_class_id.as_u32()
                );
            }
            known.ok_or_else(|| VmError::Internal {
                message: format!(
                    "field {field_class_name}.{field_name}: owner not loaded, and a compile loads none"
                ),
            })?
        }
        None => shared.load_class_concurrent(&field_class_name)?,
    };

    // JVMS §5.4.4 for the class the reference names (see
    // `member_owner_access_census`). This threadless core cannot build or
    // record the throwable; the interpreter's loader-aware resolver, reaching
    // the same entry, records it.
    if let Some(message) =
        member_owner_access_census(shared, current_class_id, &field_class_name, field_class_id)
    {
        return Err(LinkageError::IllegalAccessError { message }.into());
    }

    let fill = !threadless_fill_withheld(
        shared,
        current_class_id,
        &field_class_name,
        field_class_id,
        loader_sensitive,
    );
    let nest_unsettled = std::cell::Cell::new(false);
    let resolved = resolve_field_in_class(
        shared,
        current_class_id,
        cp_index,
        field_class_id,
        field_class_name,
        field_name,
        fill_as_of,
        fill,
        if field_constraint_before_fill_enabled() {
            FieldConstraintCheck::Peek
        } else {
            FieldConstraintCheck::Skip
        },
        &nest_unsettled,
    );
    // The answer rests on a `NestHost` claim whose host no door has loaded
    // yet (JIT round 20 wave 16, lane nest): this threadless core cannot load
    // it, so the peek is declined and the interpreter's loader-aware resolver
    // settles the claim at the site's first execution.
    if nest_unsettled.get() {
        return Err(VmError::Internal {
            message: "field access rests on an unsettled nest host claim; left to the interpreter"
                .to_string(),
        }
        .into());
    }
    resolved
}

/// `CRATONVM_FIELD_SITE_ISOLATED_STICKY` (default on; `0` / `false` / `off` /
/// `no` turns it off). Round 15 wave 5, lane fieldowner2
/// (`r15w4-fieldowner-isolated-loader-field-sites-reask-the-loader-FIXED-20260930.md`):
/// an isolated URL loader's field site keeps its first resolution (JVMS
/// §5.4.3), so its `loadClass` is asked once per site, not once per
/// execution. Two halves, one switch: [`resolve_field_ref_loader_aware`]
/// takes the site's per-`(class, cp)` entry before re-deriving the owner, and
/// [`resolve_field_ref`] no longer fills that entry with an answer the
/// interpreter would not give (so every entry such a site can see is one the
/// loader-aware resolver produced, or the loader's own definition). Read on
/// the resolution-miss path only.
pub(super) fn isolated_field_site_sticky_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FIELD_SITE_ISOLATED_STICKY")
}

/// `CRATONVM_FIELD_CONSTRAINT_BEFORE_FILL` (default on; `0` / `false` / `off` /
/// `no` restores the old order): the loader-aware field resolver asks the JVMS
/// §5.3.4 loader-constraint question of a reference-typed field BEFORE the
/// per-`(class, cp)` fill instead of after it, so a `--jdk-only` refusal is
/// thrown at every execution (as HotSpot does) rather than once; and the
/// compile doors' peek declines a field whose type both loaders already see
/// as two classes, so compiled code does not read what the interpreter
/// refuses ([`FieldConstraintCheck`]). Round 15 wave 5, lane fieldowner2.
/// Round 15 wave 6 (lane sweepA): under `--jdk-only` the peek also leaves the
/// per-site entry of such a field unfilled, so the interpreter's first
/// execution settles the question itself (`resolve_field_in_class`).
/// Read once per resolution miss and once per compile-door field peek.
fn field_constraint_before_fill_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FIELD_CONSTRAINT_BEFORE_FILL")
}

/// `CRATONVM_JIT_FIELD_PEEK_NO_LOAD` (default OFF; any value but `0` /
/// `false` / `off` / `no` turns it on): FO4-3 of
/// `jit-r15w4-fieldowner-proposals-RETIRED-20261003.md` -- the compile doors' field peek does
/// not load a user loader's unanswered `javax/`-style owner. Off until a
/// census of the compile refusals it adds (the methods such a site keeps
/// interpreted until it runs) says it is free; see `resolve_field_ref`.
fn field_peek_no_load_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_FIELD_PEEK_NO_LOAD")
}

/// Does the compile doors' field peek ([`resolve_field_ref`]) load NO owner
/// class at all? Interpreter round i4 wave 94, lane L2
/// (`interpreter-i93-L2-compile-door-field-peek-loads-class-path-owners-FIXED-20261127.md`).
///
/// `CRATONVM_JIT_DEFERRED_FIELD_PEEK_NO_LOAD` (HotSpot parity, default ON;
/// `0` / `false` / `off` / `no` restores the compile-time load) -- coupled to
/// `CRATONVM_JIT_DEFERRED_FIELD_SITES` (default ON; `=0` restores the load
/// too). On x86-64 the doors defer the unresolved site
/// (`DirectHelperTable::field_site_cp`) and the method still compiles.
///
/// On every other target (aarch64) the rule holds as well -- interpreter
/// round i4 wave 96, lane L2: it used to be x86-64 only, so an aarch64
/// compile door LOADED a cold site's class-path owner (the class-load-timing
/// divergence `L2W94FieldPeekLoadsNothing` shows). The aarch64 backend has no
/// deferred site, so there the declined site is a clean decline: the backend
/// refuses any field site absent from its maps (`jit/src/lib.rs`, the
/// `target_arch = "aarch64"` block), the method stays interpreted, and the
/// owner is loaded by the interpreter when the site first runs (JVMS §5.4.3),
/// as on HotSpot. That costs such a method its aarch64 compile (a
/// performance gap, recorded on
/// `interpreter-i92-L2-deferred-field-sites-only-at-the-method-entry-door-FIXED-20261016.md`),
/// never Java-visible behaviour.
/// Read per compile-door field peek that misses every loaded class (cold).
pub(crate) fn field_peek_never_loads_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DEFERRED_FIELD_SITES")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DEFERRED_FIELD_PEEK_NO_LOAD")
}

/// `class_id`'s defining loader when it is a user-defined one.
fn user_defined_loader_of(
    shared: &SharedVm,
    class_id: ClassId,
) -> Option<cratonvm_types::ClassLoaderId> {
    match shared.classes.class_manager.read().get_loader_id(class_id) {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => Some(l),
        _ => None,
    }
}

/// Must [`resolve_field_ref`] (a compile door's threadless peek) keep its
/// answer out of the per-`(class, cp)` resolution cache? Yes when the
/// referencing class belongs to a user-defined loader, the owner came from a
/// loader-specific source (a loader-sensitive name, or a `javax/`-style name
/// the loader may define), and the owner is NOT a class that loader defined
/// itself: the answer is then the initiating memo's or the flat store's, which
/// the interpreter does not necessarily give. An isolated URL loader ignores
/// its memo (`lookup_loader_defined_exact`) and drives the loader, and since
/// round 15 wave 5 it takes the cached entry as the site's resolution
/// ([`isolated_field_site_sticky_enabled`]), so an entry a peek filled from a
/// stale memo would become the site's answer. The peek still answers; only
/// the fill is skipped (the loader-aware resolver fills on its first miss).
/// (An entry the site already has is never replaced by a peek either: one the
/// peek disagrees with makes it decline, above.) The trivial-getter fast path
/// (`dispatch_virtual.rs`), which reads the cache raw, sees fewer guessed
/// entries for the same reason.
fn threadless_fill_withheld(
    shared: &SharedVm,
    current_class_id: ClassId,
    field_class_name: &str,
    field_class_id: ClassId,
    loader_sensitive: bool,
) -> bool {
    if !owner_answer_is_the_loaders(shared, field_class_name, loader_sensitive) {
        return false;
    }
    if !isolated_field_site_sticky_enabled() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    match cm.get_loader_id(current_class_id) {
        Some(referencing @ cratonvm_types::ClassLoaderId::UserDefined(_)) => {
            cm.get_loader_id(field_class_id) != Some(referencing)
        }
        _ => false,
    }
}

/// Does the referencing user loader's own answer decide the owner named
/// `field_class_name` (so a threadless peek's answer may not be the
/// interpreter's)? A loader-sensitive name always; a global-namespace name
/// (`javax/`, `jdk/`, ...) because the loader may define or memoise it; a
/// `java/` name only while the interpreter ASKS the loader for it
/// ([`loader_global_name_ask_enabled`], `HotSpotAlignment::JavaNameAsk`, on
/// by default since dev i47: JVMS §5.3.2 resolves it through the loader's
/// `loadClass`, an observable call a filled entry would skip). With that ask
/// off the interpreter answers a `java/` name globally, the answer every path
/// gives, and the peek fills it as before i47. nio21 wave 13 L5.
fn owner_answer_is_the_loaders(
    shared: &SharedVm,
    field_class_name: &str,
    loader_sensitive: bool,
) -> bool {
    loader_sensitive
        || (is_user_loader_global_name(field_class_name)
            && (!field_class_name.starts_with("java/")
                || loader_global_name_ask_enabled(shared, field_class_name)))
}

/// Is [`resolve_field_ref`]'s answer for this site one the referencing user
/// loader's own records decide (a loader-sensitive name, or a `javax/`-style
/// name it may define), with [`isolated_field_site_sticky_enabled`] on? Then
/// a cached entry the peek disagrees with is the site's resolution, not a
/// stale guess (the peek no longer fills guesses: [`threadless_fill_withheld`]).
fn loader_specific_peek(
    shared: &SharedVm,
    current_class_id: ClassId,
    field_class_name: &str,
    loader_sensitive: bool,
) -> bool {
    owner_answer_is_the_loaders(shared, field_class_name, loader_sensitive)
        && isolated_field_site_sticky_enabled()
        && user_defined_loader_of(shared, current_class_id).is_some()
}

/// `CRATONVM_JIT=field-site-cache` — the per-thread resolved-field site cache
/// ([`FieldSiteCache`]). Read once and cached; this sits on the interpreter's
/// field path.
///
/// # Default-ON since 2026-08-18; `CRATONVM_JIT_FIELD_SITE_CACHE=0` opts out
///
/// It shipped default-OFF on 2026-08-05 and stayed there, which made it a
/// feature that could not be measured by anyone who did not already know the
/// variable's name. `site_stats` reports the state plainly: over 38 million
/// interpreted `getfield`s the field counters read `hit=0 miss=0 fill=0` — not
/// a cache that missed, a cache never CONSULTED. The module docs anticipated
/// exactly this reading ("an inert gate shows up here immediately as `hit=0`").
///
/// What the default was costing, marginal ns per opcode over an `iadd`
/// control, one binary, `--nojit`, arms interleaved (`probes/InterpDecodedOpcodeCostProbe.java`):
///
/// | opcode      | OFF       | ON        |
/// |-------------|-----------|-----------|
/// | `getfield`  | 549 / 720 | 251 / 245 |
/// | `putfield`  | 557 / 653 | 222 / 240 |
/// | `getstatic` | 474 / 545 | 174 / 200 |
/// | `putstatic` | 424 / 505 | 176 / 180 |
///
/// That reproduces the Azure figures the lever was accepted on
/// (1.9-2.7x per pass, and 12.7% off the real Tomcat annotation scan; see
/// docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md),
/// which is why the default moved rather than the measurement being retaken.
///
/// The correctness argument is unchanged and lives in `site_cache`'s module
/// docs — three epochs, checked per entry on every hit and every fill, any of
/// which wipes the entry. Nothing here relaxes it; only the default moved.
/// `field-site-cache-loader` (the wider-surface arm that also admits
/// loader-sensitive sites) followed in interpreter round i1 wave 25 (below);
/// `method-site-cache`, which measured nothing, stays opt-in.
///
/// Same opt-out spelling as `CRATONVM_JIT_OSR` (`env_cache`), so one kill
/// switch reads the same way across the interpreter's default-ON levers.
/// Read once per VM into `VmConfig::field_site_cache` (interpreter round i2
/// wave 58, lane L7; a process `OnceLock` before).
#[inline(always)]
fn field_site_cache_enabled(shared: &SharedVm) -> bool {
    shared.config.field_site_cache
}

/// `CRATONVM_JIT=field-site-cache-loader` — additionally admit
/// **loader-sensitive** sites resolved from the loader-local lookup, guarded by
/// the loader-resolution epoch as well. Implies `field-site-cache`; on its own
/// it does nothing.
///
/// Default-ON since interpreter round i1 wave 25 (lane L5), with the
/// `field-site-cache` spelling (unset → on; `0` / `false` / `off` / `no` → off,
/// `CRATONVM_JIT=-field-site-cache-loader`). Until then a class defined by a
/// user loader (a webapp, a Spring forked-loader test, a Groovy script, a
/// platform-parented `URLClassLoader`) paid the full loader-aware field
/// resolution on EVERY `getfield` / `putfield` / `getstatic` / `putstatic`
/// execution — two `class_manager` reads, a `resolution_cache` read and the
/// loader-local lookup — and, because `field_fast`'s quickened sites admit only
/// what `field_sites` holds, none of its sites was ever quickened either. The
/// correctness argument (every input of the answer is constant for the
/// referencing class or covered by the entry's two epochs; the arm fills only
/// from the loader-LOCAL lookup, never from a `loadClass` upcall) was written
/// down in wave 16
/// (`interpreter-L4-proposal-static-field-quickening-FIXED-20260930.md`, "Progress (wave
/// 16)"); it is wave 24's `site_fill_admitted` rule for `new` / cast sites
/// (answer = the loader's own record) applied to field sites. Measure with
/// `tools/probes/interp/L5/L5W25LoaderFieldSiteBench.java`.
///
/// Read once per VM into `VmConfig::field_site_cache_loader` (wave 58, lane
/// L7; a process `OnceLock` before).
#[inline(always)]
fn field_site_cache_loader_enabled(shared: &SharedVm) -> bool {
    shared.config.field_site_cache_loader
}

/// Loader-aware variant of [`resolve_field_ref`] for use at call sites that
/// have a `&mut JvmThread` available (the primary `getstatic`/`putstatic`
/// opcode handlers).
///
/// `resolve_field_ref`'s field-owning-class lookup only consults the
/// `initiating_resolution_cache`/`class_defined_by_loader_exact` fast paths
/// inside [`lookup_loader_initiated`] and, on a miss, falls straight to the
/// flat, loader-blind [`SharedVm::load_class_concurrent`] — unlike
/// [`resolve_class_loader_aware`] (used for `CONSTANT_Class` references:
/// `ldc`/`new`/`checkcast`/`instanceof`), it never drives the referencing
/// class's OWN defining loader's `loadClass()` on a cache miss. Under
/// `@CompileWithForkedClassLoader`-style scenarios this silently binds a
/// `getstatic` in a freshly fork-loaded class to some OTHER (earlier-loaded,
/// typically application-loader) copy of the same-named field-owning class —
/// e.g. a fork-loaded `TypeMappedAnnotation`'s `getstatic
/// MergedAnnotation$Adapt.CLASS_TO_STRING` binding to the application
/// loader's `Adapt.CLASS_TO_STRING` singleton, which then compares `!=` (by
/// reference) against a *correctly* fork-loaded `Adapt` array built
/// elsewhere in the same call chain — silently corrupting `Adapt.isIn(...)`
/// and, downstream, `ConfigurationClassParser$SourceClass
/// .getAnnotationAttributes`'s `Class[]`→`String[]` conversion (manifests as
/// `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;`).
///
/// This variant uses the full, re-entrant [`resolve_class_loader_aware`]
/// resolution for the field-owning class instead, so a cache miss for a
/// user-loader-owned referencing class drives that loader's own `loadClass`
/// (JVMS §5.4.3 initiating-loader semantics) before ever falling back to the
/// global store. Behaviour is unchanged whenever the referencing class is
/// NOT user-loader-owned (gate off, or a built-in defining loader) — that
/// case still resolves via the same global `load_class_concurrent` path.
pub(super) fn resolve_field_ref_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<ResolvedField, MethodCallFailed> {
    // ---- Resolved constant pool: per-thread site cache ------------------
    // A hit here answers the whole function with an array index, two integer
    // tag compares and two relaxed atomic loads — no lock, no allocation, no
    // class resolution. Only sites whose loader-aware revalidation was proven
    // redundant at fill time are ever stored; see `FieldSiteCache`.
    let epochs_at_entry = if field_site_cache_enabled(shared) {
        if let Some(hit) = thread.field_sites.get(current_class_id, cp_index) {
            let hit = hit.clone();
            site_stats::bump_if(shared.config.site_census, site_stats::FIELD_HIT);
            return Ok(hit);
        }
        site_stats::bump(site_stats::FIELD_MISS);
        // Read BEFORE resolving; see `SiteCache::put`.
        FieldSiteCache::epochs_for(shared)
    } else {
        (0, 0)
    };
    // A field cache entry may have been populated by a loader-blind helper
    // (verification, JIT metadata, or an earlier legacy path) before this
    // opcode reaches its loader-aware resolver. Do not trust such an entry
    // blindly for a user-loader caller: first resolve the symbolic owner in
    // the caller's initiating-loader namespace, then validate that the cached
    // declaring class is that owner or one of its actual ancestors.
    let cached = shared
        .classes
        .resolution_cache
        .read()
        .get_field(current_class_id, cp_index)
        .cloned();

    // Before the constant pool is read: see `resolve_field_in_class`'s fill.
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (field_class_name, field_name, class_idx, field_descriptor) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        let (class_idx, nat_idx) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::FieldReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => {
                return Err(VmError::Internal {
                    message: format!("invalid field ref at cp#{cp_index}"),
                }
                .into());
            }
        };
        let class_name = class
            .constant_pool
            .get_class_name(class_idx)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{class_idx}"),
            })?
            .to_string();
        let (field_name, descriptor) =
            class
                .constant_pool
                .get_name_and_type(nat_idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!("invalid name_and_type at cp#{nat_idx}"),
                })?;
        (
            class_name,
            field_name.to_string(),
            class_idx,
            descriptor.to_string(),
        )
    };

    // JVMS §5.4.3: a failed resolution of the field's CLASS entry — by this
    // reference, another member reference naming the same entry, or a class
    // opcode on it — is rethrown, even when the class has become loadable
    // since (interpreter round i1 wave 25, lane L5; probes
    // `L5W24OwnerFailureRecord` / `L5W25OwnerFailureRecordInvoke`). Checked
    // before the loader-local lookup below, which would answer from the later
    // success of some other entry.
    if let Some(recorded) = recorded_resolution_failure(shared, thread, current_class_id, class_idx)
    {
        return Err(recorded);
    }

    // A loader-local cache lookup (no re-entrant loadClass) may already know
    // the field-owning class for a custom-loader caller; skip the expensive
    // re-entrant resolver in that case, matching resolve_field_ref's fast
    // path. Only when it doesn't (or the caller isn't loader-sensitive) do
    // we pay for the full resolve_class_loader_aware call below.
    let loader_sensitive = should_use_loader_initiated_resolution(shared, current_class_id)
        && !field_class_name.starts_with('[')
        && !is_global_resolution_namespace(&field_class_name)
        && matches!(
            shared
                .classes
                .class_manager
                .read()
                .get_loader_id(current_class_id),
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        );
    // An isolated URL loader must not accept an initiating-cache entry here:
    // it may predate the loader's private definition and point at the
    // application copy. A getstatic against that stale owner shares static
    // annotation metadata caches across otherwise isolated frameworks.
    let isolated_url_definition =
        loader_sensitive && is_isolated_url_loader_definition(shared, thread, current_class_id);
    // JVMS §5.4.3: this site's earlier resolution is its answer. The rule
    // above keeps the MEMO out, and without this the site had no record at
    // all: for an owner the loader did not define itself every execution
    // scanned the class store twice and ran the loader's `loadClass` again
    // (HotSpot asks once), and `fill_field_site` could not keep the driven
    // answer. With `threadless_fill_withheld`, the entry for such a site is
    // the loader-aware resolver's own fill (it drove this loader, or took
    // its definition) or the loader's definition, never a peek's memo or
    // flat-store guess. The entry's sweeps (redefinition, unloading, layout
    // change) move the epochs the per-thread fill is tagged with. Round 15
    // wave 5, lane fieldowner2.
    if isolated_url_definition {
        if let Some(sticky) = cached.as_ref() {
            if isolated_field_site_sticky_enabled() {
                // `loader_local = true`: the answer no longer depends on a
                // lookup the epochs do not cover (see `fill_field_site`).
                fill_field_site(
                    shared,
                    thread,
                    current_class_id,
                    cp_index,
                    loader_sensitive,
                    true,
                    epochs_at_entry,
                    sticky,
                );
                return Ok(sticky.clone());
            }
        }
    }
    // A hidden class's own class-file name skips the loader-local lookups,
    // which answer an ordinary class of that name: the class-entry
    // resolution below answers the hidden class itself (JIT round 21 wave 20,
    // lane hidden-self-resolve, `CRATONVM_HIDDEN_SELF_CP_BY_ID`).
    let loader_local_id = if loader_sensitive
        && super::constants::hidden_self_cp_owner(shared, current_class_id, &field_class_name)
            .is_none()
    {
        if isolated_url_definition {
            lookup_loader_defined_exact(shared, current_class_id, &field_class_name)
        } else {
            lookup_loader_initiated(shared, current_class_id, &field_class_name)
        }
    } else {
        None
    };

    // Whether the owner came from the loader-LOCAL lookup rather than the
    // re-entrant resolver. Only the former has a writable validity condition
    // (the two epochs); see `fill_field_site`.
    let loader_local = loader_local_id.is_some();
    let field_class_id = match loader_local_id {
        Some(id) => id,
        // A failed owner resolution is recorded against the class entry, as
        // the class opcodes record theirs; converted here (not only by the
        // opcode that called) so the record holds the Java-visible error.
        // The referencing class is named explicitly: this resolver is not
        // only reached from an opcode whose frame is on top.
        None => {
            // Through the class entry's record (JVMS §5.4.3.2 resolves the
            // Fieldref's class through `class_idx`, the entry `new` / casts
            // name too): an isolated URL loader's `loadClass` is asked once
            // per entry, not once per instruction kind. Every other class
            // resolves exactly as `resolve_class_loader_aware`. Round 15
            // wave 5, lane classsite2 (`r15w5-classsite-class-entry-shared-
            // by-every-instruction-patch-20260930.md`, part A).
            let id = super::constants::resolve_class_entry_loader_aware(
                shared,
                thread,
                (current_class_id, class_idx),
                &field_class_name,
                fill_as_of,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(current_class_id),
                    &field_class_name,
                    e,
                )
            })
            .map_err(|e| {
                record_resolution_failure_as_of(shared, current_class_id, class_idx, e, fill_as_of)
            })?;
            // JVMS §5.4.3: a failure another thread recorded for the class
            // entry meanwhile is its outcome (interpreter round i1 wave 38,
            // lane L5; `--jdk-only`).
            if let Some(recorded) = recorded_resolution_failure_after_success(
                shared,
                thread,
                current_class_id,
                class_idx,
            ) {
                return Err(recorded);
            }
            id
        }
    };
    if let Some(cached) = cached {
        let cache_matches_owner = {
            let cm = shared.classes.class_manager.read();
            cached.declaring_class_id == field_class_id
                || cm.is_subclass_of(field_class_id, cached.declaring_class_id)
        };
        if cache_matches_owner {
            fill_field_site(
                shared,
                thread,
                current_class_id,
                cp_index,
                loader_sensitive,
                loader_local,
                epochs_at_entry,
                &cached,
            );
            return Ok(cached);
        }
    }

    // JVMS §5.4.4 for the CLASS the reference names: §5.4.3.2 resolves it
    // first, and §5.4.3.1 applies the class access check to it, so a public
    // field of a package-private class of another package is an
    // `IllegalAccessError` recorded against the class entry, as HotSpot's
    // `ConstantPool::klass_at_impl` records it (interpreter round i1 wave 26,
    // lane L5; probe `L5W25MemberAccessProbe`, `hidden-owner` row). On the
    // resolution miss only: the cached answer above was checked when filled.
    if let Some(message) =
        member_owner_access_census(shared, current_class_id, &field_class_name, field_class_id)
    {
        return Err(raise_member_owner_access_refusal(
            shared,
            thread,
            current_class_id,
            class_idx,
            &message,
            fill_as_of,
        ));
    }

    let constraint_member = field_name.clone();
    let constraint_before_fill = field_constraint_before_fill_enabled();
    let resolve_in_class =
        |field_class_name: String, field_name: String, nest_unsettled: &std::cell::Cell<bool>| {
            resolve_field_in_class(
                shared,
                current_class_id,
                cp_index,
                field_class_id,
                field_class_name,
                field_name,
                fill_as_of,
                true,
                if constraint_before_fill {
                    FieldConstraintCheck::Settle(field_descriptor.as_str())
                } else {
                    FieldConstraintCheck::Skip
                },
                nest_unsettled,
            )
        };
    let nest_unsettled = std::cell::Cell::new(false);
    let nest_mark = nest_host_claim_mark(shared);
    let first = resolve_in_class(
        field_class_name.clone(),
        field_name.clone(),
        &nest_unsettled,
    );
    // HotSpot loads a `NestHost` claim's host at the first private access
    // check that needs it (`InstanceKlass::nest_host`); the core cannot, so
    // it left the claim pending and filled nothing. Load it here, with the
    // thread, and resolve again (JIT round 20 wave 16, lane nest). Nothing
    // is held across the load: this is the resolution miss, before the
    // opcode pops anything.
    let resolved = if nest_unsettled.get() {
        drop(first);
        settle_nest_host_claims(shared, thread, nest_mark)?;
        resolve_in_class(field_class_name, field_name, &std::cell::Cell::new(false))?
    } else {
        first?
    };
    // JVMS §5.3.4 (interpreter round i1 wave 31): a reference
    // field's type against the declaring class's loader. Asked inside
    // `resolve_field_in_class`, before its fill, since round 15 wave 5.
    if resolved.is_reference && !constraint_before_fill {
        let violation = {
            let cm = shared.classes.class_manager.read();
            loader_constraint_violation(
                shared,
                &cm,
                "field",
                current_class_id,
                resolved.declaring_class_id,
                &constraint_member,
                &field_descriptor,
            )
        };
        if let Some(message) = violation {
            loader_constraint_census(shared, message)?;
        }
    }
    fill_field_site(
        shared,
        thread,
        current_class_id,
        cp_index,
        loader_sensitive,
        loader_local,
        epochs_at_entry,
        &resolved,
    );
    Ok(resolved)
}

/// Offer a freshly-resolved site to the per-thread [`FieldSiteCache`].
///
/// A **loader-blind** site (`loader_sensitive == false`) is always offered: its
/// owner resolution reads the global name → `ClassId` mapping and nothing else,
/// so the class-definition epoch alone is a complete validity condition.
///
/// A **loader-sensitive** site is offered only under
/// `CRATONVM_JIT=field-site-cache-loader` (default-on since interpreter round
/// i1 wave 25), and only when the owner came back
/// from the loader-local lookup (`loader_local` — `class_defined_by_loader_exact`
/// or the initiating-resolution memo) rather than from a re-entrant
/// `resolve_class_loader_aware`. That restriction is what makes the validity
/// condition writable: those two sources are covered exactly by the
/// class-definition and loader-resolution epochs. A site that needed the
/// re-entrant resolver keeps paying full revalidation every time, as before.
///
/// This split exists because the two cases have different reach. Tomcat's
/// annotation scan run from a plain classpath is loader-blind, but the same
/// code inside a real webapp deploy runs under a user-defined loader — so a
/// fix that only covered the first would be measurable on the probe and inert
/// where it matters. `CRATONVM_DBG=field-site`'s `reject_loader` counter is
/// what says which case a given workload is in.
#[inline]
#[allow(clippy::too_many_arguments)]
fn fill_field_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    cp_index: u16,
    loader_sensitive: bool,
    loader_local: bool,
    epochs_at_entry: (u64, u64),
    resolved: &ResolvedField,
) {
    if !field_site_cache_enabled(shared) {
        return;
    }
    if loader_sensitive && !(loader_local && field_site_cache_loader_enabled(shared)) {
        site_stats::bump(site_stats::FIELD_REJECT_LOADER);
        return;
    }
    thread.field_sites.put(
        current_class_id,
        cp_index,
        epochs_at_entry,
        resolved.clone(),
    );
    site_stats::bump(site_stats::FIELD_FILL);
}

/// `CRATONVM_JIT=method-site-cache` — enable the per-thread resolved-method site
/// cache, which removes `resolve_method_ref` from the inline-cache HIT path of
/// `invokevirtual`/`invokespecial`/`invokeinterface`/`invokestatic` through a
/// cached native or intrinsic. Default-OFF → behaviour byte-for-byte unchanged.
/// Read once per VM into `VmConfig::method_site_cache` (wave 58, lane L7; a
/// process `OnceLock` before).
#[inline(always)]
fn method_site_cache_enabled(shared: &SharedVm) -> bool {
    shared.config.method_site_cache
}

/// The `(descriptor, num_params)` pair the argument-popping helpers need, from
/// the per-thread site cache when it can supply it and from `resolve_method_ref`
/// otherwise.
///
/// Only the two values are cached, never the resolved owner or callback: those
/// have dispatch semantics attached (loader identity, native-shadow
/// suppression, redefine gates) that this table deliberately knows nothing
/// about. A descriptor and a parameter count are properties of the
/// constant-pool `NameAndType` alone, which is why they are safe to answer here
/// under the module's epoch condition — see `site_cache`'s docs.
#[inline]
fn method_site_info(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_class_id: ClassId,
    cp_index: u16,
) -> Result<MethodSiteInfo, MethodCallFailed> {
    if !method_site_cache_enabled(shared) {
        let (_cn, _mn, descriptor, num_params) =
            resolve_method_ref(shared, caller_class_id, cp_index)?;
        return Ok(MethodSiteInfo {
            descriptor,
            num_params: u16::try_from(num_params).unwrap_or(u16::MAX),
        });
    }
    if let Some(hit) = thread.method_sites.get(caller_class_id, cp_index) {
        let hit = hit.clone();
        site_stats::bump_if(shared.config.site_census, site_stats::METHOD_HIT);
        return Ok(hit);
    }
    site_stats::bump(site_stats::METHOD_MISS);
    // Read BEFORE resolving; see `SiteCache::put`.
    let epochs_at_entry = MethodSiteCache::epochs_for(shared);
    let (_cn, _mn, descriptor, num_params) = resolve_method_ref(shared, caller_class_id, cp_index)?;
    let info = MethodSiteInfo {
        descriptor,
        // A descriptor cannot declare more than 255 parameter slots (JVMS
        // §4.3.3), so the clamp is unreachable for anything the verifier let
        // through; saturating rather than truncating keeps a malformed
        // descriptor from silently popping the wrong number of operands.
        num_params: u16::try_from(num_params).unwrap_or(u16::MAX),
    };
    thread
        .method_sites
        .put(caller_class_id, cp_index, epochs_at_entry, info.clone());
    site_stats::bump(site_stats::METHOD_FILL);
    Ok(info)
}

/// How [`resolve_field_in_class`] asks JVMS §5.3.4 of a reference-typed field
/// before its fill (round 15 wave 5, lane fieldowner2;
/// `CRATONVM_FIELD_CONSTRAINT_BEFORE_FILL`).
#[derive(Clone, Copy)]
pub(super) enum FieldConstraintCheck<'a> {
    /// Not here: the caller asks after the fill, or not at all (the old order).
    Skip,
    /// The loader-aware resolver, with the field's descriptor: the finding is
    /// settled ([`loader_constraint_census`]: pending constraints recorded,
    /// a violation thrown under `--jdk-only`) and nothing is filled on a throw.
    Settle(&'a str),
    /// A compile door's threadless peek ([`resolve_field_ref`]): a violation
    /// both sides already see ([`peek_field_constraint_violated`]) that
    /// enforced constraints would throw (`ClassRealm::loader_constraints_on`)
    /// declines the peek, so no compiled site reads a
    /// field the interpreter refuses at every execution. Records nothing, and
    /// (round 15 wave 6, lane sweepA) fills nothing for a field §5.3.4 could
    /// still refuse, so the interpreter's own `Settle` runs at its first miss.
    Peek,
}

/// Do `accessor_id` and `declaring_id` (two loaders) already see the class a
/// field `descriptor` names as two different classes? The direct half of
/// `loader_constraints::member_constraint_finding`, answered through
/// [`owner_in_referencing_namespace`] on both sides, with no side effect: a
/// violation only the recorded constraint table knows is not seen (the
/// interpreter's `settle` finds it at the site's first execution). `java/`
/// names cannot differ between loaders. Under the caller's class-manager guard.
fn peek_field_constraint_violated(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    declaring_id: ClassId,
    descriptor: &str,
) -> bool {
    if cm.get_loader_id(accessor_id) == cm.get_loader_id(declaring_id) {
        return false;
    }
    let Some(name) = descriptor
        .trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|n| n.strip_suffix(';'))
    else {
        return false;
    };
    if name.starts_with("java/") {
        return false;
    }
    match (
        owner_in_referencing_namespace(shared, cm, accessor_id, name),
        owner_in_referencing_namespace(shared, cm, declaring_id, name),
    ) {
        (Some(seen_by_accessor), Some(seen_by_declaring)) => seen_by_accessor != seen_by_declaring,
        _ => false,
    }
}

/// Can JVMS §5.3.4 constrain a field of this `descriptor`? Its element type is
/// a class a user loader may define, i.e. not a `java/` name (the rule of
/// `loader_constraints::constrainable`) and not a primitive.
fn field_type_constrainable(descriptor: &str) -> bool {
    descriptor
        .trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|n| n.strip_suffix(';'))
        .is_some_and(|n| !n.starts_with("java/"))
}

/// Shared tail of [`resolve_field_ref`] / [`resolve_field_ref_loader_aware`]:
/// given an already-resolved field-owning `field_class_id`, look up
/// `field_name` (own fields first, then the superclass/superinterface chain),
/// cache the result under `(current_class_id, cp_index)` (unless `fill` is
/// false: [`threadless_fill_withheld`]), and return it. `constraint`: how the
/// JVMS §5.3.4 question of a reference-typed field is asked before the fill.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_field_in_class(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    field_class_id: ClassId,
    field_class_name: String,
    field_name: String,
    fill_as_of: u64,
    fill: bool,
    constraint: FieldConstraintCheck<'_>,
    nest_unsettled: &std::cell::Cell<bool>,
) -> Result<ResolvedField, MethodCallFailed> {
    // First check: is this a static field? Look in the declaring class's own fields.
    //
    // Lock-order discipline (the H2 TestScript class-resolution DEADLOCK):
    // NEVER hold `class_manager` while acquiring `resolution_cache` — the
    // cache write below used to live INSIDE the `cm` read guard, while the
    // invokedynamic string-concat path holds resolution-side state and then
    // takes `class_manager.write()` (`alloc_java_string_object`'s
    // `load_class("java/lang/String")`) — a textbook ABBA inversion that
    // wedged main-vm (this fn, lock_exclusive) against a concat worker,
    // with every other resolver piling up behind the queued writer.
    // `resolve_method_ref` already documents and follows this rule
    // ("Drop the read lock before acquiring write lock"); mirror it here:
    // resolve under `cm`, DROP it at block end, then cache + return.
    //
    // C2 P0 — the own-fields scan, the superclass/superinterface walk, the JPMS
    // access check and both FIELD-TRACE diagnostics that used to be written out
    // twice here now live in `runtime::resolve::MemberResolver::locate_field`.
    // The trace moved with them because only `locate_field` knows which of the
    // two search paths produced the answer; keeping the second trace here would
    // have had to guess from `declaring_class_id`, and would have guessed wrong
    // for a field the recursive walk finds on the owner itself. Both messages,
    // and the conditions that emit them, are unchanged.
    //
    // This function keeps the parts that are genuinely its own: the `cm` guard
    // (so the lock order at this call site is unchanged — `locate_field`
    // borrows the guard rather than taking its own) and the
    // `(current_class_id, cp_index)` cache write, which must happen with `cm`
    // DROPPED per the ABBA note above.
    //
    // `AccessPolicy::ModuleOnly` is exactly what the two inlined checks did:
    // `check_module_access_by_id` and nothing else. The member half of JVMS
    // §5.4.4 (private / protected / package-private) is asked right after,
    // by `field_member_access_refusal` under the same guard, and enforced
    // under `--jdk-only` (interpreter round i1 wave 26, lane L5): on this
    // resolution-miss path only, before the fill, so a cached answer is one
    // that passed it.
    let (resolved, refusal, peek_refused, peek_withholds_fill) = {
        let cm = shared.classes.class_manager.read();
        // JVMS §5.4.3.2 — the OTHER half of the fieldref key.
        //
        // Resolution is by name AND descriptor, and §4.5 forbids only the pair
        // from repeating: one class may legally declare several fields sharing
        // a name. Matching on the name alone returns whichever comes first in
        // declaration order, which can be a field of an entirely different
        // type — and the caller then gets its slot index and its
        // static/instance flag.
        //
        // Read straight back out of the referencing class's own constant pool
        // rather than threading it down from `resolve_field_ref`: this function
        // already holds `cp_index` and the `cm` guard, and both of its callers
        // reach it only on a resolution-cache MISS, so the lookup is cold.
        // `None` (a malformed or non-fieldref entry) keeps the historical
        // name-only key, which is what those callers got before.
        let descriptor: Option<&str> =
            cm.get_class(current_class_id)
                .and_then(|c| match c.constant_pool.get(cp_index) {
                    Some(ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) => c
                        .constant_pool
                        .get_name_and_type(*name_and_type_index)
                        .map(|(_, d)| d),
                    _ => None,
                });
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let accessor = resolver.scope(current_class_id);
        let owner = resolver.scope(field_class_id);
        let scoped = resolver.locate_field(
            &cm,
            accessor,
            owner,
            &field_class_name,
            &field_name,
            descriptor,
            crate::runtime::resolve::AccessPolicy::ModuleOnly,
        )?;
        let resolved = resolver.adopt(scoped)?;
        let refusal = field_member_access_refusal(
            shared,
            &cm,
            current_class_id,
            field_class_id,
            &resolved,
            &field_name,
            descriptor,
            nest_unsettled,
        );
        // A compile door's peek of a field JVMS §5.3.4 would refuse (see
        // `FieldConstraintCheck::Peek`), wherever constraints are enforced:
        // `--jdk-only`, and `--compatible` with the `loader-constraints` row
        // (the i50-L5 `--compatible` gates page; it asked `is_jdk_only`, so
        // under `--compatible` a compiled site baked a violating field).
        let peek_refused = matches!(constraint, FieldConstraintCheck::Peek)
            && resolved.is_reference
            && shared.classes.loader_constraints_on
            && descriptor.is_some_and(|d| {
                peek_field_constraint_violated(
                    shared,
                    &cm,
                    current_class_id,
                    resolved.declaring_class_id,
                    d,
                )
            });
        // L2 of `r15w5-fieldowner2-review-findings-FIXED-20260930.md` (round 15
        // wave 6, lane sweepA): the peek asks only the both-loaded half of
        // JVMS §5.3.4, and the interpreter's revalidating path accepts what
        // the peek filled without settling the rest -- so under `--jdk-only`
        // a violation only the recorded table knows was never thrown, and a
        // pending constraint for a not-yet-loaded type was never recorded. A
        // reference field whose declaring class has another loader and whose
        // type a user loader may define (the only shape §5.3.4 can refuse) is
        // therefore answered but not filled; the loader-aware resolver fills
        // it after its own `Settle`. `java/` types (`System.out`) still fill,
        // and so does a pair of built-in loaders (parent-first delegation
        // gives both the one class).
        let peek_withholds_fill = matches!(constraint, FieldConstraintCheck::Peek)
            && resolved.is_reference
            && shared.classes.loader_constraints_on
            && {
                let accessor = cm.get_loader_id(current_class_id);
                let declaring = cm.get_loader_id(resolved.declaring_class_id);
                let user = |l: Option<cratonvm_types::ClassLoaderId>| {
                    matches!(l, Some(cratonvm_types::ClassLoaderId::UserDefined(_)))
                };
                accessor != declaring && (user(accessor) || user(declaring))
            }
            && descriptor.is_some_and(field_type_constrainable);
        (resolved, refusal, peek_refused, peek_withholds_fill)
    };
    if let Some(message) = refusal.and_then(|m| member_access_census(shared, "field", m)) {
        return Err(LinkageError::IllegalAccessError { message }.into());
    }
    if peek_refused {
        return Err(VmError::Internal {
            message: format!(
                "field {field_class_name}.{field_name}: loader constraint violated; left to the interpreter"
            ),
        }
        .into());
    }

    // JVMS §5.3.4 before the fill (round 15 wave 5, lane fieldowner2): the
    // loader-aware resolver used to ask it AFTER this fill, so its refusal
    // (`--jdk-only`) was thrown once and the next execution took the filled
    // entry through the cached path without asking again. HotSpot records no
    // field resolution that failed its constraint check and throws at every
    // execution; a refusal here now leaves nothing behind.
    if let FieldConstraintCheck::Settle(field_descriptor) = constraint {
        if resolved.is_reference {
            let violation = {
                let cm = shared.classes.class_manager.read();
                loader_constraint_violation(
                    shared,
                    &cm,
                    "field",
                    current_class_id,
                    resolved.declaring_class_id,
                    &field_name,
                    field_descriptor,
                )
            };
            if let Some(message) = violation {
                loader_constraint_census(shared, message)?;
            }
        }
    }

    // `fill_as_of` was taken before the caller read the constant pool: a
    // redefinition of this class since then (it can even run on this thread,
    // from an agent's transformer while the owner loads) swept this cache
    // before the fill, so the fill would outlive the pool it came from
    // (`ResolutionCache::fill_snapshot`, interpreter round i1 wave 22).
    // Nor an answer that rests on an unsettled nest host claim (JIT round 20
    // wave 16, lane nest): the claim is settled and the field resolved again.
    if fill && !peek_withholds_fill && !nest_unsettled.get() {
        shared.classes.resolution_cache.write().put_field_as_of(
            current_class_id,
            cp_index,
            resolved.clone(),
            fill_as_of,
        );
    }

    Ok(resolved)
}

// ---------------------------------------------------------------------------
// JVMS §5.4.4 at field and method resolution (interpreter round i1 wave 26,
// lane L5; `i25-L5-member-access-is-not-checked-at-field-and-method-resolution`)
// ---------------------------------------------------------------------------
//
// HotSpot checks, after resolving a `Fieldref` / `Methodref`, that the
// referencing class may access the CLASS it names (`LinkResolver::resolve_klass`
// -> `ConstantPool::klass_at_impl` -> `Reflection::verify_class_access`, the
// failure recorded against the class entry) and the MEMBER it resolved to
// (`check_field_accessability` / `check_method_accessability` ->
// `Reflection::verify_member_access`, with the nestmate test and the
// protected clause against the class the reference names). Until wave 26 this
// VM asked JPMS readability only. Both checks now run on every resolution
// MISS (never on a cached answer, which is only filled after they passed):
// `--jdk-only` refuses, `--compatible` admits and counts
// (`ClassRealm::member_access_refusals`, `CRATONVM_DBG=access`) for the census
// the next stage is decided on.

/// Is a JVMS §5.4.4 refusal at field / method resolution thrown, or only
/// counted? `--jdk-only` since interpreter round i1 wave 26 (lane L5);
/// `--compatible` since wave 29, census-decided: `refusals=0 unverified=0`
/// over 151 `--compatible` runs of the core suite and the jdk-only corpus,
/// the Spring Boot `simple` fat jar and four Tomcat loader test classes
/// (`i25-L5-member-access-is-not-checked-at-field-and-method-resolution`).
/// One place to change.
#[inline]
pub(crate) fn member_access_enforced(_shared: &SharedVm) -> bool {
    true
}

/// A class whose bytecode or member flags this VM made, not read from a class
/// file HotSpot would have loaded too: lambda proxies, `$ProxyN`, reflection /
/// serialization accessors, VM-internal shapes and compatibility stubs (whose
/// flags are guessed). No HotSpot rule speaks for them, so neither side of a
/// member reference that involves one is refused. Hidden classes are NOT
/// exempt: `access_control` gives them their host's nest and package.
fn access_exempt_origin(class: &crate::classloading::Class) -> bool {
    matches!(
        class.origin,
        cratonvm_classloading::ClassOrigin::GeneratedLambda { .. }
            | cratonvm_classloading::ClassOrigin::GeneratedProxy { .. }
            | cratonvm_classloading::ClassOrigin::ReflectionAccessor { .. }
            | cratonvm_classloading::ClassOrigin::VmInternal
            | cratonvm_classloading::ClassOrigin::CompatibilityStub { .. }
    )
}

/// HotSpot's link-time relaxation (`Reflection::can_relax_access_check_for`
/// with `classloader_only`, `Verifier::relax_access_for`): a member access
/// between two pre-Java-8 class files (major < 52) defined by the same
/// TRUSTED loader — the application or platform loader; the bootstrap loader
/// is not "trusted" there — from the same code source is admitted.
fn old_class_files_relaxed(a: &crate::classloading::Class, b: &crate::classloading::Class) -> bool {
    a.version.major < 52
        && b.version.major < 52
        && a.loader_id == b.loader_id
        && matches!(
            a.loader_id,
            cratonvm_types::ClassLoaderId::Application | cratonvm_types::ClassLoaderId::Extension
        )
        && a.code_source.as_ref().and_then(|c| c.url.as_deref())
            == b.code_source.as_ref().and_then(|c| c.url.as_deref())
}

/// A private access `access_control::are_nestmates` refused only because the
/// nest host both classes claim is not loaded: HotSpot's
/// `has_nestmate_access_to` LOADS the host to validate the claim, and this
/// check cannot load. Two sibling nested classes can run before their outer
/// class ever does (`Outer$A` calling `Outer$B`'s private method), so a
/// refusal here would be a false one; admitted instead. A host that IS loaded
/// and does not list the member was already decided by `are_nestmates`.
///
/// Only while the claims are UNSETTLED (JIT round 20 wave 16, lane nest;
/// `jr20-private-access-admitted-when-the-common-nest-host-is-unloaded`):
/// the admission is then recorded as pending ([`note_unsettled_nest_host`],
/// which raises `nest_unsettled`, so the answer is not cached and a door
/// with a thread loads the host and resolves again,
/// [`settle_nest_host_claims`]). Once a claim is settled, the host either
/// loaded (the class store judges it: a host its loader delegated to another
/// defining loader is another run-time package) or failed to load (the
/// member is its own host): no admission here either way.
fn unvalidated_common_nest_host(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    a: &crate::classloading::Class,
    b: &crate::classloading::Class,
    nest_unsettled: &std::cell::Cell<bool>,
) -> bool {
    let host_a = a.nest_host.as_deref().unwrap_or(&*a.name);
    let host_b = b.nest_host.as_deref().unwrap_or(&*b.name);
    // The host of THEIR loader (interpreter round i1 wave 27, lane L5):
    // `are_nestmates` confirms only through that one now, so another
    // loader's same-named host being loaded says nothing here. One hash
    // probe, where the loader-blind `find_by_name` was a scan of the store.
    if !(a.loader_id == b.loader_id
        && host_a == host_b
        && cm
            .loaded_class_under_exact_key(host_a, a.loader_id)
            .is_none())
    {
        return false;
    }
    if a.hidden
        || b.hidden
        || settled_nest_host_claim(shared, a.id).is_some()
        || settled_nest_host_claim(shared, b.id).is_some()
    {
        return false;
    }
    note_unsettled_nest_host(shared, a, host_a, nest_unsettled);
    note_unsettled_nest_host(shared, b, host_b, nest_unsettled);
    true
}

/// `member`'s settled `NestHost` claim ([`crate::vm::realms::class_realm::NestHostClaim::Settled`]):
/// `Some(Some(text))` when loading its host failed (HotSpot's `Nest host
/// resolution of ... failed: ...`), `Some(None)` when the host loaded,
/// `None` when no door has loaded it yet. JIT round 20 wave 16, lane nest.
fn settled_nest_host_claim(
    shared: &SharedVm,
    member: ClassId,
) -> Option<Option<std::sync::Arc<str>>> {
    match shared.classes.nest_host_claims.lock().get(&member) {
        Some(crate::vm::realms::class_realm::NestHostClaim::Settled(failure)) => {
            Some(failure.clone())
        }
        _ => None,
    }
}

/// Record that a private access check needed `member`'s claim of the nest
/// host `host` (internal name), which no door has loaded through `member`'s
/// loader yet, and raise `nest_unsettled` (the verdict or its text is not
/// final). Counted in `ClassRealm::nest_host_unsettled_reads`, whose value
/// before the count is the claim's `seq` ([`nest_host_claim_mark`]). Under
/// the caller's class-manager guard (the claims lock is taken inside it,
/// never the other way round). JIT round 20 wave 16, lane nest.
fn note_unsettled_nest_host(
    shared: &SharedVm,
    member: &crate::classloading::Class,
    host: &str,
    nest_unsettled: &std::cell::Cell<bool>,
) {
    if member.hidden || host == &*member.name {
        return;
    }
    {
        let mut claims = shared.classes.nest_host_claims.lock();
        if matches!(
            claims.get(&member.id),
            Some(crate::vm::realms::class_realm::NestHostClaim::Settled(_))
        ) {
            return;
        }
        // JIT round 20 wave 17 (lane rev15): the claim carries the count of
        // the check that (last) needed it, so that check's door settles it
        // and no other door does (`settle_nest_host_claims`'s `since`).
        let seq = shared
            .classes
            .nest_host_unsettled_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match claims.get_mut(&member.id) {
            Some(
                crate::vm::realms::class_realm::NestHostClaim::Pending { seq: noted, .. }
                | crate::vm::realms::class_realm::NestHostClaim::Loading { seq: noted, .. },
            ) => *noted = seq,
            Some(crate::vm::realms::class_realm::NestHostClaim::Settled(_)) => {}
            None => {
                claims.insert(
                    member.id,
                    crate::vm::realms::class_realm::NestHostClaim::Pending {
                        host: std::sync::Arc::from(host),
                        member: std::sync::Arc::from(member.name.replace('/', ".")),
                        seq,
                    },
                );
            }
        }
    }
    nest_unsettled.set(true);
}

/// The mark a door with a thread takes BEFORE the resolution whose
/// `nest_unsettled` it may then settle: [`settle_nest_host_claims`] loads
/// only the claims noted at or after it (JIT round 20 wave 17, lane rev15).
pub(crate) fn nest_host_claim_mark(shared: &SharedVm) -> u64 {
    shared
        .classes
        .nest_host_unsettled_reads
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// HotSpot's `InstanceKlass::nest_host` for every claim a member-access check
/// left pending ([`note_unsettled_nest_host`]): load the host through the
/// member's defining loader (the `CONSTANT_Class` door,
/// `constants::resolve_class_loader_aware`) and record the outcome. A failed
/// load is swallowed and recorded as `Nest host resolution of <member> with
/// host <host> failed: <throwable>`; a `VirtualMachineError` (or an internal
/// error) propagates, as HotSpot propagates a VME. The caller then resolves
/// again, and the class store (a loaded host) or the record (a failed one)
/// decides. JIT round 20 wave 16, lane nest.
///
/// GC-safety: runs the loaders' `loadClass` (Java), so the caller holds no
/// raw reference and no class-manager guard: it is called at a door's
/// resolution miss, before anything is popped.
///
/// Only the claims noted since `since` ([`nest_host_claim_mark`], taken by
/// the door before its resolution) -- the accessor's and the holder's, as
/// HotSpot's `has_nestmate_access_to` loads exactly those two hosts (JIT
/// round 20 wave 17, lane rev15). It used to load EVERY pending claim of
/// the VM: another loader's `loadClass` ran on this thread at an unrelated
/// access (a lock HotSpot never takes there: a non-parallel-capable
/// loader's monitor, so a deadlock with a thread holding it and waiting on
/// a lock this one holds), and that load's `VirtualMachineError` was thrown
/// from this access. A claim noted by a racing check in the same window is
/// still loaded here; its own door then finds it settled.
#[cold]
#[inline(never)]
pub(crate) fn settle_nest_host_claims(
    shared: &SharedVm,
    thread: &mut JvmThread,
    since: u64,
) -> Result<(), MethodCallFailed> {
    use crate::vm::realms::class_realm::NestHostClaim;
    let me = thread.thread_id.0;
    // Every claim not settled yet, except those THIS thread is loading
    // already (a `loadClass` that meets its own claim again must not recurse:
    // the inner resolution keeps the old answer, uncached). Another thread's
    // load in progress is loaded here too (the loader answers both alike), so
    // no door acts on an unsettled claim while a racing one loads.
    let pending: Vec<(ClassId, std::sync::Arc<str>, std::sync::Arc<str>, u64)> = {
        let mut claims = shared.classes.nest_host_claims.lock();
        claims
            .iter_mut()
            .filter_map(|(&id, claim)| {
                let (host, member, seq) = match &*claim {
                    NestHostClaim::Pending { host, member, seq } if *seq >= since => (
                        std::sync::Arc::clone(host),
                        std::sync::Arc::clone(member),
                        *seq,
                    ),
                    NestHostClaim::Loading {
                        host,
                        member,
                        by,
                        seq,
                    } if *by != me && *seq >= since => {
                        return Some((
                            id,
                            std::sync::Arc::clone(host),
                            std::sync::Arc::clone(member),
                            *seq,
                        ));
                    }
                    NestHostClaim::Pending { .. }
                    | NestHostClaim::Loading { .. }
                    | NestHostClaim::Settled(_) => return None,
                };
                *claim = NestHostClaim::Loading {
                    host: std::sync::Arc::clone(&host),
                    member: std::sync::Arc::clone(&member),
                    by: me,
                    seq,
                };
                Some((id, host, member, seq))
            })
            .collect()
    };
    for (at, (member_id, host, member_name, _)) in pending.iter().enumerate() {
        let failure =
            match super::constants::resolve_class_loader_aware(shared, thread, *member_id, host) {
                Ok(_) => None,
                Err(error) => match nest_host_load_failure_text(shared, error, member_name, host) {
                    Ok(text) => Some(text),
                    Err(propagated) => {
                        // A `VirtualMachineError`: nothing is recorded, and
                        // the claims this thread took are pending again.
                        let mut claims = shared.classes.nest_host_claims.lock();
                        for (id, host, member, seq) in pending.iter().skip(at) {
                            if let Some(claim) = claims.get_mut(id) {
                                if matches!(claim, NestHostClaim::Loading { by, .. } if *by == me) {
                                    *claim = NestHostClaim::Pending {
                                        host: std::sync::Arc::clone(host),
                                        member: std::sync::Arc::clone(member),
                                        seq: *seq,
                                    };
                                }
                            }
                        }
                        return Err(propagated);
                    }
                },
            };
        if cratonvm_types::flags().loader.dbg_access {
            eprintln!(
                "[ACCESS-DBG] NEST-HOST {member_name} -> {host}: {}",
                failure.as_deref().unwrap_or("loaded")
            );
        }
        let mut claims = shared.classes.nest_host_claims.lock();
        if let Some(claim) = claims.get_mut(member_id) {
            // A racing door may have settled it first: its outcome stands.
            if !matches!(claim, NestHostClaim::Settled(_)) {
                *claim = NestHostClaim::Settled(failure);
            }
        }
    }
    Ok(())
}

/// HotSpot's record of a failed nest host load (`InstanceKlass::nest_host`):
/// `Nest host resolution of <member> with host <host> failed: ` followed by
/// `java_lang_Throwable::print` of the throwable (its class's external name,
/// then `: <message>` when it has one). `Err(error)` hands back a failure
/// HotSpot propagates instead: a `VirtualMachineError`, and here also an
/// internal VM error (not a Java throwable at all).
fn nest_host_load_failure_text(
    shared: &SharedVm,
    error: MethodCallFailed,
    member: &str,
    host: &str,
) -> Result<std::sync::Arc<str>, MethodCallFailed> {
    let (class_name, message) = match error {
        MethodCallFailed::ExceptionThrown(exc) => {
            let (name, vme) = {
                let cm = shared.classes.class_manager.read();
                match cm.get_class(shared.mem.heap.class_id_of(exc)) {
                    Some(class) => (
                        class.name.replace('/', "."),
                        class.is_assignable_to_name(
                            "java/lang/VirtualMachineError",
                            &cm.class_store,
                        ),
                    ),
                    None => (String::new(), false),
                }
            };
            if vme || name.is_empty() {
                return Err(MethodCallFailed::ExceptionThrown(exc));
            }
            (
                name,
                super::constants::throwable_detail_message(shared, exc),
            )
        }
        MethodCallFailed::InternalError(VmError::Linkage(error)) => {
            let (name, message) = crate::runtime::exceptions::linkage_throwable(&error);
            (name.replace('/', "."), Some(message))
        }
        other => return Err(other),
    };
    let throwable = match message {
        Some(message) => format!("{class_name}: {message}"),
        None => class_name,
    };
    Ok(std::sync::Arc::from(format!(
        "Nest host resolution of {member} with host {host} failed: {throwable}"
    )))
}

/// The member a field or method reference resolved to, with its flags.
#[derive(Clone, Copy)]
enum ResolvedMemberAccess<'a> {
    Field {
        name: &'a str,
        flags: cratonvm_reader::class_access_flags::FieldAccessFlags,
    },
    Method {
        name: &'a str,
        descriptor: &'a str,
        flags: cratonvm_reader::class_access_flags::MethodAccessFlags,
    },
}

/// JVMS §5.4.4 (`Reflection::verify_member_access`) for a member of `holder_id`
/// reached through a reference naming `referenced_id`: HotSpot's
/// `IllegalAccessError` message when `accessor_id` may not access it, `None`
/// when it may or when this cannot tell (a class missing from the store, a
/// VM-made class on either side, a refusal that is JPMS's and was already
/// answered by the `ModuleOnly` check). Pure: counts nothing, loads nothing.
///
/// The protected clause's `T` is the class the reference NAMES
/// (`referenced_id`), as HotSpot passes `resolved_class`; the receiver's
/// run-time type is the verifier's business (JVMS §4.10.1.8), so passing it
/// is not a vacuous `None` (the trap `access_control` warns about). A static
/// member passes `None`: HotSpot admits a protected static from any subclass.
///
/// `nest_unsettled` is raised when the verdict on a PRIVATE member, or its
/// text, rests on a `NestHost` claim no door has loaded the host of yet
/// ([`unvalidated_common_nest_host`], [`note_unsettled_nest_host`]): the
/// caller must not cache the answer, and a caller with a thread settles the
/// claims ([`settle_nest_host_claims`]) and asks again.
fn member_access_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    referenced_id: ClassId,
    holder_id: ClassId,
    member: ResolvedMemberAccess<'_>,
    nest_unsettled: &std::cell::Cell<bool>,
) -> Option<String> {
    use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
    if accessor_id == holder_id {
        return None;
    }
    let (is_public, is_private, is_static, flags) = match member {
        ResolvedMemberAccess::Field { flags, .. } => (
            flags.contains(FieldAccessFlags::PUBLIC),
            flags.contains(FieldAccessFlags::PRIVATE),
            flags.contains(FieldAccessFlags::STATIC),
            crate::runtime::resolve::MemberFlags::Field(flags),
        ),
        ResolvedMemberAccess::Method { flags, .. } => (
            flags.contains(MethodAccessFlags::PUBLIC),
            flags.contains(MethodAccessFlags::PRIVATE),
            flags.contains(MethodAccessFlags::STATIC),
            crate::runtime::resolve::MemberFlags::Method(flags),
        ),
    };
    if is_public {
        return None;
    }
    let accessor = cm.get_class(accessor_id)?;
    let holder = cm.get_class(holder_id)?;
    if access_exempt_origin(accessor) || access_exempt_origin(holder) {
        return None;
    }
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    // JPMS readability of the HOLDER is not a JVMS §5.4.4 member rule (HotSpot
    // checks the module boundary on the class the reference names only):
    // when it alone refuses, this is not the refusal being introduced here.
    if resolver
        .check_member_access(
            cm,
            resolver.scope(accessor_id),
            resolver.scope(holder_id),
            flags,
            None,
            crate::runtime::resolve::AccessPolicy::ModuleOnly,
        )
        .is_err()
    {
        return None;
    }
    let receiver = (!is_static).then(|| resolver.scope(referenced_id));
    match resolver.check_member_access(
        cm,
        resolver.scope(accessor_id),
        resolver.scope(holder_id),
        flags,
        receiver,
        crate::runtime::resolve::AccessPolicy::Full,
    ) {
        Err(crate::runtime::resolve::ResolveError::IllegalAccess { .. }) => {}
        Ok(_) | Err(_) => return None,
    }
    if is_private && unvalidated_common_nest_host(shared, cm, accessor, holder, nest_unsettled) {
        return None;
    }
    if old_class_files_relaxed(accessor, holder) {
        return None;
    }
    // `Reflection::verify_member_access`: every access from a subclass of the
    // bootstrap `SerializationConstructorAccessorImpl` succeeds (by identity;
    // see `extends_serialization_constructor_accessor`).
    if extends_serialization_constructor_accessor(cm, accessor_id) {
        return None;
    }
    // HotSpot names a user-defined loader in the parenthetical
    // (`(g.A and g.B are in unnamed module of loader L @1dbd16a6)`), as the
    // class-constant text already did here; the member texts left it out
    // (JIT round 20 wave 15, lane huntLink; `R20HuntLinkAccess`).
    let naming = super::constants::messages_name_a_user_loader(shared, accessor, holder);
    let user_loader = |class: &crate::classloading::Class| {
        if naming {
            super::constants::user_loader_name_and_id_under_guard(shared, cm, class)
        } else {
            None
        }
    };
    let message = match member {
        ResolvedMemberAccess::Field { name, flags } => {
            crate::classloading::access_control::field_access_denied_message_naming(
                accessor,
                holder,
                flags,
                name,
                Some(&cm.module_registry),
                &user_loader,
            )
        }
        ResolvedMemberAccess::Method {
            name,
            descriptor,
            flags,
        } => crate::classloading::access_control::method_access_denied_message_naming(
            accessor,
            holder,
            flags,
            &crate::runtime::resolve::selection::external_method_name(
                &holder.name,
                name,
                descriptor,
            ),
            Some(&cm.module_registry),
            &user_loader,
        ),
    };
    if !is_private {
        return Some(message);
    }
    // A refused PRIVATE access names why a nest host claim failed
    // (`print_nest_host_error_on`): the accessor's, then the holder's (JIT
    // round 20 wave 15, lane huntLink; `R20HuntLinkNest`). A host its
    // member's loader knows is judged here; one it does not know is loaded
    // by a door with a thread first (JIT round 20 wave 16, lane nest:
    // `note_unsettled_nest_host` / `settle_nest_host_claims`), and a failed
    // load names itself (`Nest host resolution of ... failed: ...`).
    let nest_error = |member: &crate::classloading::Class| -> Option<String> {
        let host_name = member.nest_host.as_deref()?;
        if member.hidden || host_name == &*member.name {
            return None;
        }
        let settled = settled_nest_host_claim(shared, member.id);
        if let Some(Some(failure)) = &settled {
            return Some(failure.to_string());
        }
        let Some(host_id) = owner_in_referencing_namespace(shared, cm, member.id, host_name) else {
            if settled.is_none() {
                note_unsettled_nest_host(shared, member, host_name, nest_unsettled);
            }
            return None;
        };
        crate::classloading::access_control::nest_host_error_text(
            member,
            cm.get_class(host_id)?,
            Some(&cm.module_registry),
            &user_loader,
        )
    };
    let suffix = crate::classloading::access_control::nest_host_error_suffix(
        nest_error(accessor).as_deref(),
        nest_error(holder).as_deref(),
    );
    Some(message + &suffix)
}

/// [`member_access_refusal`] for the field `resolved` (declared by
/// `resolved.declaring_class_id`) that the reference `field_class_id.field_name`
/// resolved to. The field's flags are read back from its declaring class by
/// the reference's key (name and descriptor, as `locate_field` matched it).
fn field_member_access_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    field_class_id: ClassId,
    resolved: &ResolvedField,
    field_name: &str,
    descriptor: Option<&str>,
    nest_unsettled: &std::cell::Cell<bool>,
) -> Option<String> {
    if accessor_id == resolved.declaring_class_id {
        return None;
    }
    let holder = cm.get_class(resolved.declaring_class_id)?;
    let field = holder
        .fields
        .iter()
        .find(|f| &*f.name == field_name && descriptor.is_none_or(|d| &*f.descriptor == d))
        .or_else(|| holder.fields.iter().find(|f| &*f.name == field_name))?;
    if owner_foreign_to_referencing_loader(cm, accessor_id, field_class_id) {
        return None;
    }
    member_access_refusal(
        shared,
        cm,
        accessor_id,
        field_class_id,
        resolved.declaring_class_id,
        ResolvedMemberAccess::Field {
            name: field_name,
            flags: field.access_flags,
        },
        nest_unsettled,
    )
}

/// JVMS §5.4.4 for the field `owner_id.name:descriptor` (resolved from
/// `owner_id` by name and descriptor, JVMS §5.4.3.2) accessed from
/// `accessor_id`, as the resolution of a field `CONSTANT_MethodHandle`
/// checks it: `Some(true)` when the field is PRIVATE and refused, `Some(false)`
/// when refused for another reason, `None` when admitted or not decidable
/// here (the field does not resolve, an exempt origin, a loader question).
/// Interpreter round i1 wave 39, lane L4: the getters of a hand-assembled
/// `ObjectMethods.bootstrap` site (`invokedynamic::object_methods_resolve_getters`).
pub(crate) fn field_handle_access_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
    nest_unsettled: &std::cell::Cell<bool>,
) -> Option<bool> {
    use cratonvm_reader::class_access_flags::FieldAccessFlags;
    let (_, field, declaring) = crate::classloading::find_field_recursive_by_descriptor(
        owner_id,
        name,
        descriptor,
        &cm.class_store,
    )?;
    if declaring == accessor_id || owner_foreign_to_referencing_loader(cm, accessor_id, owner_id) {
        return None;
    }
    let flags = field.access_flags;
    member_access_refusal(
        shared,
        cm,
        accessor_id,
        owner_id,
        declaring,
        ResolvedMemberAccess::Field { name, flags },
        nest_unsettled,
    )?;
    Some(flags.contains(FieldAccessFlags::PRIVATE))
}

/// [`member_access_refusal`] for the method `owner_id.name descriptor`
/// resolves to (JVMS §5.4.3.3 / §5.4.3.4, `selection::resolve_declaring`).
/// `None` as well when it does not resolve through the class store (a
/// signature-polymorphic reference; `NoSuchMethodError` is decided elsewhere).
pub(super) fn method_member_access_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<String> {
    method_member_access_refusal_noting(
        shared,
        cm,
        accessor_id,
        owner_id,
        name,
        descriptor,
        &std::cell::Cell::new(false),
    )
}

/// [`method_member_access_refusal`], raising `nest_unsettled` when the answer
/// rests on an unsettled `NestHost` claim (see [`member_access_refusal`]):
/// the method resolution core's form (JIT round 20 wave 16, lane nest).
pub(super) fn method_member_access_refusal_noting(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
    nest_unsettled: &std::cell::Cell<bool>,
) -> Option<String> {
    if owner_foreign_to_referencing_loader(cm, accessor_id, owner_id) {
        return None;
    }
    let store = cm.class_store();
    let holder_id =
        crate::runtime::resolve::selection::resolve_declaring(store, owner_id, name, descriptor)?;
    if holder_id == accessor_id {
        return None;
    }
    let flags = store
        .get(holder_id)?
        .find_method(name, descriptor)?
        .access_flags;
    member_access_refusal(
        shared,
        cm,
        accessor_id,
        owner_id,
        holder_id,
        ResolvedMemberAccess::Method {
            name,
            descriptor,
            flags,
        },
        nest_unsettled,
    )
}

/// JVMS §5.4.4 for the CLASS a field or method reference names (§5.4.3.2 /
/// §5.4.3.3 resolve it first, and §5.4.3.1 checks it as HotSpot's
/// `klass_at_impl` does for every class entry): the class-constant verdict
/// (`class_constant_access_denial_in`: package, JPMS export, the
/// `SerializationConstructorAccessorImpl` exemption), with the VM-made classes
/// of [`access_exempt_origin`] admitted. Pure; `None` for an array owner
/// (`[I.clone()`), whose element the class opcodes already check.
pub(super) fn member_owner_access_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    owner_name: &str,
    owner_id: ClassId,
) -> Option<String> {
    if owner_id == accessor_id || owner_name.starts_with('[') {
        return None;
    }
    let accessor = cm.get_class(accessor_id)?;
    let owner = cm.get_class(owner_id)?;
    if access_exempt_origin(accessor) || access_exempt_origin(owner) {
        return None;
    }
    if owner_foreign_to_referencing_loader(cm, accessor_id, owner_id) {
        return None;
    }
    class_constant_access_denial_in(shared, cm, accessor_id, owner_name, Some(owner_id))
}

/// Whether `owner_id` — the class a member reference of `accessor_id` was
/// resolved to — is provably NOT what `accessor_id`'s defining loader maps
/// the owner's name to: `accessor_id`'s direct superclass or superinterface of
/// that name is another class (JVMS §5.3.5: linking resolved exactly those
/// names through the defining loader), or that user-defined loader has itself
/// defined another class under the name.
///
/// That happens where CratonVM resolves a name outside the loader's namespace
/// (the `jdk/`, `javax/`, `sun/`, `com/sun/` names `is_global_resolution_namespace`
/// routes globally, and the loader-blind fallbacks): the reference is then
/// bound to a same-named class of ANOTHER loader, which is a resolution
/// defect of its own (see
/// `docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md`).
/// The JVMS §5.4.4 checks must not compound it by judging access to a class
/// the reference does not name in its own namespace, so they admit: a user
/// loader's `jdk/internal/reflect/SerializationConstructorAccessorImpl`
/// subclass's `super()` resolved to java.base's class and was refused as a
/// non-exported package (interpreter round i1 wave 26, lane L5b; probe
/// `L5W26SerializationAccessorSpoof`). Pure, cold (a refusal path's input).
fn owner_foreign_to_referencing_loader(
    cm: &crate::classloading::ClassManager,
    accessor_id: ClassId,
    owner_id: ClassId,
) -> bool {
    if accessor_id == owner_id {
        return false;
    }
    let (Some(accessor), Some(owner)) = (cm.get_class(accessor_id), cm.get_class(owner_id)) else {
        return false;
    };
    let name: &str = &owner.name;
    if &*accessor.name == name {
        // The reference names the accessor's own name, answered by another class.
        return true;
    }
    let supertype_elsewhere = accessor
        .superclass
        .into_iter()
        .chain(accessor.interfaces.iter().copied())
        .any(|sup| sup != owner_id && cm.get_class(sup).is_some_and(|c| &*c.name == name));
    supertype_elsewhere
        || (matches!(
            accessor.loader_id,
            cratonvm_types::ClassLoaderId::UserDefined(_)
        ) && cm
            .loaded_class_under_exact_key(name, accessor.loader_id)
            .is_some_and(|own| own != owner_id))
}

/// The class `name` names in `referencing_class_id`'s namespace, known
/// without loading: the class itself, then its direct superclass or
/// superinterface of that name (JVMS §5.3.5: resolved through its defining
/// loader when it was linked), then `find_class_by_name_for_class` (the
/// loader's own definitions first, then its recorded parents and the built-in
/// chain), then — for a user-defined loader — its initiating memo, the
/// validated answers of its own `loadClass` (`drive_defining_loader_load`;
/// interpreter round i1 wave 27, lane L5: a class the loader's `loadClass`
/// delegated to a user-loader parent CratonVM has not recorded is in the
/// loader's namespace, but only the memo says so, and without it the checks
/// were skipped). The owner the JVMS §5.4.4 checks of a method reference are
/// asked against (`resolve_method_metadata`); interpreter round i1 wave 26,
/// lane L5b: the plain lookup alone answered java.base's class for a user
/// loader's same-named superclass.
///
/// Takes the initiating memo's read lock under the caller's class-manager
/// guard, as `class_resolved_without_loading` does (the memo's writers hold no
/// class-manager lock).
pub(crate) fn owner_in_referencing_namespace(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    let class = cm.get_class(referencing_class_id)?;
    if &*class.name == name {
        return Some(referencing_class_id);
    }
    // A hidden class's own class-file name is the hidden class (JVMS 5.4.3.1;
    // JIT round 21 wave 20, lane hidden-self-resolve,
    // `CRATONVM_HIDDEN_SELF_CP_BY_ID`): the lookups below answered an
    // ordinary class of that name once one was loaded, and the JVMS 5.4.4
    // checks then refused the hidden class's calls to its own privates
    // (`R21Fz5HiddenOsr1`).
    if let Some(own) = super::constants::hidden_self_cp_owner_in(cm, referencing_class_id, name) {
        return Some(own);
    }
    class
        .superclass
        .into_iter()
        .chain(class.interfaces.iter().copied())
        .find(|&sup| cm.get_class(sup).is_some_and(|c| &*c.name == name))
        .or_else(|| {
            // `--jdk-only`, a user-defined referencing loader: only what that
            // loader has DEFINED or ANSWERED is known
            // (`class_resolved_without_loading`), never
            // `find_class_by_name_for_class`'s guess ("its recorded parents,
            // then the built-in chain"). A child-first loader defines its own
            // copy of a name its parent already has once asked: JaCoCo's
            // agent loader (`AgentModule$1`) defines `InjectedClassRuntime`
            // and its nested `$Lookup`, whose application-loader copy
            // `getDeclaredClasses()` had loaded; the guess judged
            // `InjectedClassRuntime`'s `invokestatic $Lookup.lookup` against
            // that copy, a different runtime package, and refused it before
            // `execute_invokestatic` asked the loader (interpreter round i1
            // wave 29, lane L5; probe `L5/L5W29ChildFirstAgentLoader`). An
            // owner not known yet is not a denial: the resolution is not
            // cached, the door asks the loader, and the resolution that
            // follows judges the loader's answer. Every other caller is
            // unchanged. Both modes since wave 29, when `--compatible` began
            // enforcing member access (without it JaCoCo's agent loader would
            // meet the same false `IllegalAccessError` there).
            if matches!(
                class.loader_id,
                cratonvm_types::ClassLoaderId::UserDefined(_)
            ) {
                return super::constants::class_resolved_without_loading(
                    shared,
                    cm,
                    referencing_class_id,
                    name,
                );
            }
            cm.find_class_by_name_for_class(name, referencing_class_id)
        })
        .or_else(|| {
            let loader @ cratonvm_types::ClassLoaderId::UserDefined(_) = class.loader_id else {
                return None;
            };
            // The memo, or the initiating record under
            // `CRATONVM_RECORD_REPLACES_MEMO` (wave 49, lane L5).
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                shared, loader, name,
            )
        })
}

/// Count a JVMS §5.4.4 refusal at field / method resolution in this VM's
/// `member_access_refusals` and trace it under `CRATONVM_DBG=access`;
/// `Some(message)` when this VM enforces it ([`member_access_enforced`]).
#[cold]
pub(super) fn member_access_census(
    shared: &SharedVm,
    what: &'static str,
    message: String,
) -> Option<String> {
    let n = shared
        .classes
        .member_access_refusals
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    let enforced = member_access_enforced(shared);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] MEMBER {} #{n} {what}: {message}",
            if enforced {
                "DENY"
            } else {
                "ADMIT (compatible, census)"
            }
        );
    }
    enforced.then_some(message)
}

/// Count (and, under `CRATONVM_DBG=access`, trace) a JVMS §5.4.4 refusal met
/// against an owner that only the loader-blind fallback knew
/// (`resolve_method_metadata`, interpreter round i1 wave 27, lane L5). Never
/// enforced, in any mode: see `ClassRealm::member_access_unverified`.
#[cold]
#[inline(never)]
pub(super) fn member_access_unverified_census(
    shared: &SharedVm,
    what: &'static str,
    message: &str,
) {
    let n = shared
        .classes
        .member_access_unverified
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] MEMBER UNVERIFIED (loader-blind owner, admitted) #{n} {what}: {message}"
        );
    }
}

/// The `CRATONVM_DBG=access` run total of this VM's JVMS §5.4.4 member-access
/// census, as one line — the number the `--compatible` enforcement stage of
/// `i25-L5-member-access-is-not-checked-at-field-and-method-resolution` is
/// decided on, read per run instead of by counting trace lines:
///
/// `[ACCESS-DBG] MEMBER census: mode=compatible refusals=3 (admitted) unverified=0 receiver-owner-unknown=0`
///
/// `refusals` is `ClassRealm::member_access_refusals` (enforced under
/// `--jdk-only`, admitted under `--compatible`), `unverified` is
/// `member_access_unverified`, `receiver-owner-unknown` is
/// `receiver_owner_unresolved` (resolved under `--jdk-only`, counted only
/// under `--compatible`). Printed once per VM, zeros included (a zero is
/// the reading the stage needs, and silence cannot be told from an unarmed
/// run); nothing when the category is off. The launcher calls it on both exit
/// arms (`cratonvm_vm::report_member_access_census_at_exit`).
pub(crate) fn report_member_access_census_at_exit(shared: &SharedVm) {
    if !cratonvm_types::flags().loader.dbg_access
        || shared
            .classes
            .member_access_census_reported
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let refusals = shared
        .classes
        .member_access_refusals
        .load(std::sync::atomic::Ordering::Relaxed);
    let unverified = shared
        .classes
        .member_access_unverified
        .load(std::sync::atomic::Ordering::Relaxed);
    let receiver_owner_unresolved = shared
        .classes
        .receiver_owner_unresolved
        .load(std::sync::atomic::Ordering::Relaxed);
    let loader_constraints = shared
        .classes
        .loader_constraint_violations
        .load(std::sync::atomic::Ordering::Relaxed);
    let mode = if shared.config.is_jdk_only() {
        "jdk-only"
    } else {
        "compatible"
    };
    let verdict = if member_access_enforced(shared) {
        "enforced"
    } else {
        "admitted"
    };
    eprintln!(
        "[ACCESS-DBG] MEMBER census: mode={mode} refusals={refusals} ({verdict}) unverified={unverified} receiver-owner-unknown={receiver_owner_unresolved} loader-constraint-violations={loader_constraints}"
    );
}

/// JVMS §5.3.4 at member resolution: when `accessor_id` resolves a member of
/// `declaring_id` (a class of ANOTHER loader) whose `descriptor` names a type
/// N, both loaders must see the same class for N. `Some` when both already
/// see one and the two differ (HotSpot's `check_method_loader_constraints` /
/// `check_field_loader_constraints` refuse it with `LinkageError`), or, under
/// `--jdk-only`, when a side has not loaded N yet (the constraint HotSpot
/// records for the later load; interpreter round i1 wave 37, lane L5). `None`
/// when the loaders are the same or every name agrees.
///
/// Each side's view is [`owner_in_referencing_namespace`], the view the
/// member-access checks judge by. Resolution misses only; no loading. The
/// finding is settled by [`loader_constraint_census`] after the caller drops
/// its class-manager guard (`vm/src/runtime/resolve/loader_constraints.rs`).
pub(super) fn loader_constraint_violation(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    what: &'static str,
    accessor_id: ClassId,
    declaring_id: ClassId,
    member: &str,
    descriptor: &str,
) -> Option<crate::runtime::resolve::loader_constraints::MemberConstraintFinding> {
    crate::runtime::resolve::loader_constraints::member_constraint_finding(
        shared,
        cm,
        what,
        accessor_id,
        declaring_id,
        member,
        descriptor,
    )
}

/// Settle a [`loader_constraint_violation`] finding (record its pending
/// constraints, `--jdk-only`), then count (and, under `CRATONVM_DBG=access`,
/// trace) a violation; `Err(LinkageError)` when this VM enforces it: under
/// `--jdk-only` (interpreter round i1 wave 31, after a census of zero
/// violations over the suite, the jdk-only corpus, the Spring Boot fat jar and
/// three Tomcat loader test classes, in both modes). `--compatible` counts
/// only. Not under a class-manager guard.
#[cold]
#[inline(never)]
pub(super) fn loader_constraint_census(
    shared: &SharedVm,
    finding: crate::runtime::resolve::loader_constraints::MemberConstraintFinding,
) -> Result<(), MethodCallFailed> {
    let Some(message) = crate::runtime::resolve::loader_constraints::settle(shared, finding) else {
        return Ok(());
    };
    let n = shared
        .classes
        .loader_constraint_violations
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    // `--compatible` too with `CRATONVM_COMPAT_LOADER_CONSTRAINTS`
    // (`ClassRealm::loader_constraints_on`; wave 49, lane L5).
    let enforced = shared.classes.loader_constraints_on;
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] LOADER-CONSTRAINT {} #{n}: {message}",
            if enforced {
                "DENY"
            } else {
                "(counted, admitted)"
            }
        );
    }
    if enforced {
        return Err(crate::error::LinkageError::LoaderConstraintViolation { message }.into());
    }
    Ok(())
}

/// [`member_owner_access_refusal`] under this class-manager read, then
/// [`member_access_census`]: `Some(message)` when the refusal is enforced.
pub(super) fn member_owner_access_census(
    shared: &SharedVm,
    accessor_id: ClassId,
    owner_name: &str,
    owner_id: ClassId,
) -> Option<String> {
    let refusal = {
        let cm = shared.classes.class_manager.read();
        member_owner_access_refusal(shared, &cm, accessor_id, owner_name, owner_id)
    }?;
    member_access_census(shared, "owner class", refusal)
}

/// The `IllegalAccessError` for an owner-class refusal ([`member_owner_access_census`]),
/// recorded against the reference's CLASS entry `class_index` under `as_of`
/// (JVMS §5.4.3: every later resolution through that entry — a `new`, an
/// `ldc`, another member reference — rethrows it, as on HotSpot).
#[cold]
pub(super) fn raise_member_owner_access_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_index: u16,
    message: &str,
    as_of: u64,
) -> MethodCallFailed {
    // HotSpot's `unnamed module @0x<hash>` (lane L4's shared helper; the
    // refusal was built under the class-manager guard, released by now;
    // interpreter round i5 wave 100, lane L2). Idempotent for a text it
    // already rewrote.
    let message =
        crate::vm::access_denial_with_unnamed_module_hash(shared, class_id, message.to_string());
    let error = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalAccessError",
        Some(&message),
    ) {
        Ok(obj) => MethodCallFailed::ExceptionThrown(obj),
        Err(e) => e,
    };
    record_resolution_failure_as_of(shared, class_id, class_index, error, as_of)
}

/// Kill switch for the duplicate-class-name gate below
/// (`CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE=1`, or
/// `CRATONVM_LOADER=-dup-name-field-gate`), so the change can be A/B'd on one
/// binary. Set, the gate is skipped and every call walks the authoritative
/// path exactly as it did before the gate existed — a cross-binary comparison
/// is not an A/B. Read once per VM into `VmConfig::dup_name_field_gate`
/// (interpreter round i2 wave 59, lane L7; a process `OnceLock` before).
#[inline(always)]
fn dup_name_field_gate_disabled(shared: &SharedVm) -> bool {
    !shared.config.dup_name_field_gate
}

pub(super) fn retarget_instance_field_to_receiver(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    receiver_class_id: ClassId,
    field: &ResolvedField,
) -> Option<ResolvedField> {
    hotpath_counts::bump(&hotpath_counts::RETARGET_FIELD_CALLS);
    // ── Loader-split gate (2026-09-02) ───────────────────────────────────
    //
    // Everything below this function's early-return block exists for ONE
    // situation: the receiver class and the resolved declaring class have the
    // SAME NAME under DIFFERENT `ClassId`s, so the cached field index belongs
    // to the wrong copy of the class. The very first thing the slow half does
    // is `if &*receiver_class.name != &*resolved_decl.name { return None; }`.
    //
    // Without this gate, every `getfield`/`putfield` whose receiver class
    // merely DIFFERS from the declaring class — the ordinary shape of an
    // inherited field, and most field accesses in real OO bytecode — reached
    // that test by way of a `class_manager` read lock, three `get_class`
    // lookups and a constant-pool walk. The `FieldSiteCache` above answers
    // resolution in an array index and two integer compares; this function
    // then threw that away on the majority of accesses.
    //
    // `any_duplicate_class_name()` is a single relaxed load of a latch raised
    // by `loaded_classes_insert` the first time any binary name resolves to
    // two distinct `ClassId`s. False ⇒ no receiver/declaring pair can be a
    // same-name-different-id pair ⇒ the slow half is provably a no-op.
    //
    // Measured (`probes/FieldShape.java`, `--nojit`, min-of-9, arms
    // interleaved both ways): the inherited-over-own-class delta for one
    // get+put pair was 54 / 78 / 103 ns across three runs.
    //
    // One diagnostic consequence, stated so it is not rediscovered as a bug:
    // the `[RETARGET-SKIP]` trace in the early-return block below cannot fire
    // while the gate short-circuits. Set `CRATONVM_NO_DUP_NAME_FIELD_GATE=1`
    // alongside `CRATONVM_DBG_FIELD_WATCH` to get it back.
    if !dup_name_field_gate_disabled(shared) && !crate::classloading::any_duplicate_class_name() {
        return None;
    }
    if field.is_static
        || receiver_class_id == ClassId::new(0)
        || receiver_class_id == field.declaring_class_id
        || !should_use_loader_initiated_resolution(shared, current_class_id)
    {
        if crate::runtime::env_cache::dbg_field_watch()
            && !field.is_static
            && receiver_class_id != ClassId::new(0)
            && receiver_class_id != field.declaring_class_id
        {
            // Fired the mismatch condition but bailed on
            // should_use_loader_initiated_resolution — worth knowing this
            // gate is the reason retargeting was skipped for a genuinely
            // mismatched receiver.
            let cm = shared.classes.class_manager.read();
            let decl_name = cm
                .get_class(field.declaring_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let recv_name = cm
                .get_class(receiver_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            if crate::runtime::env_cache::field_watch_class_matches(&decl_name) {
                eprintln!(
                    "[RETARGET-SKIP] cp_index={} cached_decl={}({:?}) actual_recv={}({:?}) cached_field_index={} loader_initiated_gate=false",
                    cp_index, decl_name, field.declaring_class_id, recv_name, receiver_class_id, field.field_index
                );
            }
        }
        return None;
    }

    let cm = shared.classes.class_manager.read();
    let current_class = cm.get_class(current_class_id)?;
    let name_and_type_index = match current_class.constant_pool.get(cp_index)? {
        ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        } => *name_and_type_index,
        _ => return None,
    };
    let (field_name, descriptor) = current_class
        .constant_pool
        .get_name_and_type(name_and_type_index)?;
    let resolved_decl = cm.get_class(field.declaring_class_id)?;
    let receiver_class = cm.get_class(receiver_class_id)?;
    if &*receiver_class.name != &*resolved_decl.name {
        return None;
    }

    let dbg = crate::runtime::env_cache::dbg_field_watch()
        && crate::runtime::env_cache::field_watch_class_matches(&resolved_decl.name);
    let cached_field_index = field.field_index;
    let cached_decl_name = resolved_decl.name.to_string();
    let recv_name_for_dbg = receiver_class.name.to_string();

    let mut cursor = Some(receiver_class_id);
    while let Some(cid) = cursor {
        let class = cm.class_store.get(cid)?;
        let mut instance_idx = 0usize;
        for f in &class.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == field_name && &*f.descriptor == descriptor {
                let new_index = class.first_field_index + instance_idx;
                if dbg {
                    eprintln!(
                        "[RETARGET-HIT] cp_index={} field={} cached_decl={}({:?}) cached_index={} actual_recv={}({:?}) new_decl_cid={:?} new_index={} {}",
                        cp_index, field_name, cached_decl_name, field.declaring_class_id,
                        cached_field_index, recv_name_for_dbg, receiver_class_id, cid, new_index,
                        if new_index != cached_field_index { "**INDEX CHANGED**" } else { "(same index)" }
                    );
                }
                return Some(ResolvedField {
                    declaring_class_id: cid,
                    field_index: new_index,
                    is_static: false,
                    is_volatile: f.is_volatile(),
                    is_final: f.is_final(),
                    is_reference: f.descriptor.starts_with('L') || f.descriptor.starts_with('['),
                    desc_byte: f.descriptor.as_bytes().first().copied().unwrap_or(0),
                });
            }
            instance_idx += 1;
        }
        cursor = class.superclass;
    }

    if dbg {
        eprintln!(
            "[RETARGET-MISS] cp_index={} field={} cached_decl={}({:?}) actual_recv={}({:?}) — walked full hierarchy, no matching field found",
            cp_index, field_name, cached_decl_name, field.declaring_class_id, recv_name_for_dbg, receiver_class_id
        );
    }
    None
}

/// Extract the declaring class name from a constant pool FieldReference.
///
/// Used at the getstatic/putstatic opcode boundary to build a
/// `NoClassDefFoundError` message when class resolution fails.
pub(super) fn field_ref_class_name(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
) -> Option<String> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    if let Some(ConstantPoolEntry::FieldReference { class_index, .. }) =
        class.constant_pool.get(cp_index)
    {
        class
            .constant_pool
            .get_class_name(*class_index)
            .map(|s| s.to_string())
    } else {
        None
    }
}

/// Extract the field name from a constant pool FieldReference (for enhanced NPE messages).
pub fn resolve_field_name(shared: &SharedVm, class_id: ClassId, cp_index: u16) -> Option<String> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    if let Some(ConstantPoolEntry::FieldReference {
        name_and_type_index,
        ..
    }) = class.constant_pool.get(cp_index)
    {
        let (name, _) = class
            .constant_pool
            .get_name_and_type(*name_and_type_index)?;
        Some(name.to_string())
    } else {
        None
    }
}

/// JVMS §6.5 `getfield` / `putfield` / `getstatic` / `putstatic`, linking
/// exceptions: "if the resolved field is a static field, `getfield` throws an
/// `IncompatibleClassChangeError`", and the mirror rule for the static
/// opcodes. `expect_static` is `true` for `getstatic` / `putstatic`.
///
/// Resolution (`resolve_field_ref_loader_aware`) is shared by all four
/// opcodes and answers the same `ResolvedField` for one constant-pool entry
/// whichever opcode asked, so the check has to be made by the opcode, on
/// every execution, after resolution. Before it existed a mismatch was not an
/// error at all: `getfield` on a static field read the INSTANCE slot at the
/// static field's index (and `getstatic` on an instance field the static slot
/// at the instance index) -- a silent wrong-value read on exactly the
/// binary-incompatible class change the error exists to report.
///
/// The hot path is one `bool` compare; the message is built only on the cold
/// arm, in HotSpot's `LinkResolver::resolve_field` wording ("Expected static
/// field p.C.f" / "Expected non-static field p.C.f", naming the class the
/// field reference names).
#[inline(always)]
pub(super) fn check_field_staticness(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    field: &ResolvedField,
    expect_static: bool,
) -> Result<(), MethodCallFailed> {
    if field.is_static == expect_static {
        return Ok(());
    }
    Err(field_staticness_mismatch(
        shared,
        current_class_id,
        cp_index,
        expect_static,
    ))
}

#[cold]
#[inline(never)]
fn field_staticness_mismatch(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    expect_static: bool,
) -> MethodCallFailed {
    let class_name = field_ref_class_name(shared, current_class_id, cp_index).unwrap_or_default();
    let field_name = resolve_field_name(shared, current_class_id, cp_index).unwrap_or_default();
    LinkageError::IncompatibleClassChangeError {
        message: field_staticness_message(expect_static, &class_name, &field_name),
    }
    .into()
}

/// HotSpot's message for a static/instance field-kind mismatch. `class_name`
/// is the internal (slash) form; the message uses the external (dot) form.
fn field_staticness_message(expect_static: bool, class_name: &str, field_name: &str) -> String {
    format!(
        "Expected {} field {}.{}",
        if expect_static {
            "static"
        } else {
            "non-static"
        },
        crate::runtime::exceptions::hotspot_external_name(&class_name),
        field_name
    )
}

/// Why a `putfield` / `putstatic` may not store to a `final` field. See
/// [`final_field_put_violation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinalPutViolation {
    /// The field is declared by a class other than the writing method's.
    DifferentClass,
    /// Class file version 53+: the writing method is not the declaring
    /// class's `<init>` (instance field) or `<clinit>` (static field).
    DifferentMethod,
}

/// JVMS §6.5 `putfield` / `putstatic` linking rule for `final` fields, in
/// HotSpot's shape (`LinkResolver::resolve_field`):
///
/// 1. a final field may be stored only by a method of the class that
///    declares it, for every class file version;
/// 2. when that class's file version is 53 or later, only from its `<init>`
///    (instance field) or `<clinit>` (static field).
///
/// `current_major` is asked only when rule 2 has to be decided, so a legal
/// constructor store never pays for the class-version lookup. Pure, so the
/// interpreter, the quickened `putfield` arm and the JIT compile doors share
/// one decision.
#[inline]
pub(crate) fn final_field_put_violation(
    field: &ResolvedField,
    current_class_id: ClassId,
    method_name: &str,
    current_major: impl FnOnce() -> u16,
) -> Option<FinalPutViolation> {
    if !field.is_final {
        return None;
    }
    if field.declaring_class_id != current_class_id {
        return Some(FinalPutViolation::DifferentClass);
    }
    let initializer = if field.is_static {
        "<clinit>"
    } else {
        "<init>"
    };
    if method_name == initializer || current_major() < 53 {
        return None;
    }
    Some(FinalPutViolation::DifferentMethod)
}

/// Major version of `class_id`'s class file; 0 (the permissive answer) for a
/// class the manager does not know. Takes the class-manager read lock, so it
/// must not be called with a class-manager guard held.
fn class_major_version(shared: &SharedVm, class_id: ClassId) -> u16 {
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map_or(0, |c| c.version.major)
}

/// The interpreter's `putfield` / `putstatic` final-field check, called right
/// after [`check_field_staticness`] (so `field.is_static` is the opcode's
/// kind). One `bool` test on the hot path; everything else is cold.
#[inline(always)]
pub(super) fn check_final_field_put(
    shared: &SharedVm,
    current_class_id: ClassId,
    method_name: &str,
    cp_index: u16,
    field: &ResolvedField,
) -> Result<(), MethodCallFailed> {
    if !field.is_final {
        return Ok(());
    }
    final_field_put_slow(shared, current_class_id, method_name, cp_index, field)
}

/// The linking half of a DEFERRED compiled field site (interpreter round i4
/// wave 92, lane L2; `vm::jit::helpers::jit_field_site_cp`, switch
/// `CRATONVM_JIT_DEFERRED_FIELD_SITES`): what the interpreter's `getstatic` /
/// `putstatic` / `getfield` / `putfield` arm does before it touches the field,
/// in the same order -- the loader-aware resolution through the holder's
/// loader (JVMS §5.4.3, a recorded failure rethrown), the §6.5 static /
/// instance kind check, and for a store the final-field rule against the
/// writing method `writer`. Initialisation and the access itself are the
/// caller's. A resolution error is converted as the interpreter's opcode
/// boundary converts it, with `holder` as the referencing class (compiled
/// code has no interpreter frame of its own).
pub(crate) fn link_deferred_field_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    holder: ClassId,
    cp_index: u16,
    opcode: u8,
    writer: &str,
) -> Result<ResolvedField, MethodCallFailed> {
    let expect_static = matches!(opcode, 0xb2 | 0xb3);
    let field = match resolve_field_ref_loader_aware(shared, thread, holder, cp_index) {
        Ok(field) => field,
        Err(e) => {
            let name = field_ref_class_name(shared, holder, cp_index).unwrap_or_default();
            return Err(crate::runtime::exceptions::convert_class_not_found_for(
                shared,
                thread,
                Some(holder),
                &name,
                e,
            ));
        }
    };
    check_field_staticness(shared, holder, cp_index, &field, expect_static)?;
    if matches!(opcode, 0xb3 | 0xb5) {
        check_final_field_put(shared, holder, writer, cp_index, &field)?;
    }
    Ok(field)
}

#[cold]
#[inline(never)]
fn final_field_put_slow(
    shared: &SharedVm,
    current_class_id: ClassId,
    method_name: &str,
    cp_index: u16,
    field: &ResolvedField,
) -> Result<(), MethodCallFailed> {
    let Some(violation) = final_field_put_violation(field, current_class_id, method_name, || {
        class_major_version(shared, current_class_id)
    }) else {
        return Ok(());
    };
    let class_name = field_ref_class_name(shared, current_class_id, cp_index).unwrap_or_default();
    let field_name = resolve_field_name(shared, current_class_id, cp_index).unwrap_or_default();
    let current_class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(current_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    Err(LinkageError::IllegalAccessError {
        message: final_field_put_message(
            violation,
            field.is_static,
            &class_name,
            &field_name,
            &current_class_name,
            method_name,
        ),
    }
    .into())
}

/// HotSpot's `IllegalAccessError` text for a rejected final-field store
/// (`LinkResolver::resolve_field`). `class_name` is the class the field
/// reference names and `current_class_name` the writer's, both in internal
/// form. The `DifferentMethod` wording ends in a space in HotSpot's format
/// string, and is reproduced with it.
fn final_field_put_message(
    violation: FinalPutViolation,
    is_static: bool,
    class_name: &str,
    field_name: &str,
    current_class_name: &str,
    method_name: &str,
) -> String {
    let kind = if is_static { "static" } else { "non-static" };
    let class_name = crate::runtime::exceptions::hotspot_external_name(&class_name);
    match violation {
        FinalPutViolation::DifferentClass => format!(
            "Update to {kind} final field {class_name}.{field_name} attempted from a different \
             class ({}) than the field's declaring class",
            crate::runtime::exceptions::hotspot_external_name(&current_class_name)
        ),
        FinalPutViolation::DifferentMethod => format!(
            "Update to {kind} final field {class_name}.{field_name} attempted from a different \
             method ({method_name}) than the initializer method {} ",
            if is_static { "<clinit>" } else { "<init>" }
        ),
    }
}

/// Whether `code` (a method of `class_id` named `method_name`) contains a
/// `putfield` / `putstatic` through constant-pool entry `cp_index` that
/// [`final_field_put_violation`] rejects for `field`.
///
/// For the JIT compile doors: their field resolvers see only a constant-pool
/// index, which `getfield` and `putfield` share, so a final field that the
/// method may read but not write cannot be refused per site without looking
/// at the opcodes. Refusing such a site leaves the method to the interpreter,
/// which throws. Walks the code only for a final field this method may not
/// write; must not be called with a class-manager guard held.
pub(crate) fn method_puts_final_field_illegally(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    code: &[u8],
    cp_index: u16,
    field: &ResolvedField,
) -> bool {
    final_field_put_illegal(shared, class_id, method_name, field)
        && code_has_field_put(code, cp_index, field.is_static)
}

/// Whether a `putfield` / `putstatic` of `field` from method `method_name` of
/// `class_id` would be rejected by [`final_field_put_violation`], for a
/// compile door that knows the site's opcode. Must not be called with a
/// class-manager guard held.
pub(crate) fn final_field_put_illegal(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    field: &ResolvedField,
) -> bool {
    field.is_final
        && final_field_put_violation(field, class_id, method_name, || {
            class_major_version(shared, class_id)
        })
        .is_some()
}

/// Whether `code` holds a `putstatic` (`is_static`) or `putfield` whose
/// operand is `cp_index`, at an instruction start.
fn code_has_field_put(code: &[u8], cp_index: u16, is_static: bool) -> bool {
    let opcode = if is_static { 0xb3 } else { 0xb5 };
    let [hi, lo] = cp_index.to_be_bytes();
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == opcode && code.get(pc + 1) == Some(&hi) && code.get(pc + 2) == Some(&lo) {
            return true;
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// JVMS field-store/load semantics for sub-int fields: `byte`/`boolean`/`char`/
/// `short` fields hold a value narrowed to the field's declared width even
/// though the operand stack and locals carry them as a full 32-bit `int`.
///
/// On a `putfield`/`putstatic` the stored `int` is narrowed (and, for the
/// signed types, sign-extended back into an `i32`); on a `getfield`/`getstatic`
/// the read value is re-narrowed so a field whose backing slot was widened by
/// some other path still reads back the spec-mandated value (`B` sign-extends an
/// `i8`, `S` an `i16`, `C` zero-extends a `u16`, `Z` masks to bit 0). `I` and
/// every non-int descriptor are returned unchanged.
///
/// Only the `Value::Int` carrier is narrowed; any other `Value` shape is passed
/// through untouched (defensive — a non-int slot on a sub-int field is a
/// separate upstream issue and must not be silently reshaped here).
#[inline]
pub(super) fn narrow_int_to_field_type(value: Value, desc_byte: u8) -> Value {
    match value {
        Value::Int(x) => {
            let narrowed = match desc_byte {
                // JVM spec: narrow to sub-int width then sign/zero-extend back to i32 (i2b/i2s/i2c)
                b'B' => x as i8 as i32, // byte: sign-extend low 8 bits
                // JVM spec: narrow to sub-int width then sign/zero-extend back to i32 (i2b/i2s/i2c)
                b'S' => x as i16 as i32, // short: sign-extend low 16 bits
                // JVM spec: narrow to sub-int width then sign/zero-extend back to i32 (i2b/i2s/i2c)
                b'C' => x as u16 as i32, // char: zero-extend low 16 bits
                b'Z' => x & 1,           // boolean: JVMS stores only bit 0
                _ => return value,       // I and non-sub-int: unchanged
            };
            Value::Int(narrowed)
        }
        _ => value,
    }
}

/// T10.9.D K3 — Push a static-field `Value` onto the operand stack using
/// the tag-exact path for category-2 primitives (J/D).
///
/// `Value::Long(x)` / `Value::Double(x)` would lose their tag on the
/// generic `push(Value)` boundary (the raw i64 bits are stored untagged
/// and later decoded as `Value::Double` via `to_value()`), so J and D
/// fields are pushed via `push_compact` with an explicit `CompactValue::long`
/// / `CompactValue::double` constructor keyed off the declared descriptor.
/// All other descriptors keep the legacy reference-aware coercion.
pub(super) fn push_static_field_value(
    stack: &mut crate::runtime::ValueStack,
    value: Value,
    is_reference: bool,
    desc_byte: Option<u8>,
) -> Result<(), MethodCallFailed> {
    match desc_byte {
        Some(b'J') => {
            // Long: accept any primitive (zero-init defaults to Int(0)).
            let lv = match value {
                Value::Long(x) => x,
                // Widening: i32 -> i64 (sign-extended, JVM i2l)
                Value::Int(x) => x as i64,
                // A freshly zero-initialized static slot decodes as
                // Object(None) through the Value boundary.  Treat it as
                // the JVMS default of 0L for a long field.
                Value::Object(None) => 0,
                other => {
                    return Err(VmError::Internal {
                        message: format!(
                            "getstatic: expected long for J-descriptor field, got {other:?}"
                        ),
                    }
                    .into());
                }
            };
            // Mark KIND_LONG so a downstream `pop_long` reads the slot
            // bit-exact (collision-shaped longs keep their high bits).
            stack.push_compact_long(crate::types::CompactValue::long(lv));
            Ok(())
        }
        Some(b'D') => {
            let dv = match value {
                Value::Double(x) => x,
                // Cast: integer word reinterpreted as float/double bit pattern
                Value::Long(x) => f64::from_bits(x as u64),
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                Value::Int(x) => x as f64,
                Value::Object(None) => 0.0,
                other => {
                    return Err(VmError::Internal {
                        message: format!(
                            "getstatic: expected double for D-descriptor field, got {other:?}"
                        ),
                    }
                    .into());
                }
            };
            stack.push_compact_double(crate::types::CompactValue::double_raw(dv));
            Ok(())
        }
        _ => {
            // Legacy path for I/F/Z/B/S/C and reference descriptors —
            // same coercion rules as before T10.9.D K3.
            let mut v = value;
            if is_reference {
                match v {
                    Value::Int(0) | Value::Long(0) => v = Value::Object(None),
                    _ => {}
                }
            } else {
                match v {
                    Value::Object(None) => v = Value::Int(0),
                    Value::Object(Some(raw)) => {
                        // Cast: object/code pointer to integer address
                        let bits = raw.as_ptr() as usize as u64;
                        // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
                        v = Value::Int(bits as i32);
                    }
                    _ => {}
                }
            }
            if remap_trace_on() {
                if let Value::Object(Some(o)) = &v {
                    push_prov_record(o.as_ptr() as usize, "getstatic");
                }
            }
            stack.push(v)?;
            Ok(())
        }
    }
}

/// T10.9.D K3 — Pop the operand-stack top as a static-field value using
/// the tag-exact path for category-2 primitives (J/D).
///
/// For J/D descriptors this reads the raw `CompactValue` and decodes it
/// as the requested 64-bit primitive rather than going through the
/// generic `to_value()` boundary (which would misinterpret untagged long
/// bits as Double).  A tag that cannot be reasonably coerced returns a
/// typed `VmError::Internal` instead of panicking.
pub(super) fn pop_static_field_value(
    stack: &mut crate::runtime::ValueStack,
    desc_byte: Option<u8>,
) -> Result<Value, MethodCallFailed> {
    use crate::types::CompactTag;
    match desc_byte {
        Some(b'J') => {
            // Kinds-aware bit-exact pop (mirrors Putfield-J): a KIND_LONG slot
            // is read verbatim so a collision-shaped long (SHA-512 working
            // variables, BC safegcd `0xFFFC_…` accumulators) keeps its high
            // bits instead of being truncated by the SUB_INT i2l-widening
            // heuristic. The KIND_UNKNOWN fallback in `pop_long` preserves the
            // widening contract for synthetic int-where-long.
            Ok(Value::Long(stack.pop_long()?))
        }
        Some(b'D') => {
            // Kinds-aware bit-exact pop (mirrors the J arm above): a slot the
            // push marked KIND_DOUBLE IS the double's 64 bits, so a NaN
            // payload that collides with the NaN-box tag space survives
            // putstatic instead of being decoded as the sub-tag it matches.
            //
            // The slot is decoded BEFORE it is popped (i66-L7): an empty
            // stack reports the underflow as the other arms do, and an odd
            // tag's `Err` leaves the operand stack as it was, so a caller
            // that declines on `Err` (`field_fast::putstatic_site_hit`, the
            // dispatch loop's `putstatic_inline_hit`) leaves nothing undone.
            let cv = stack.peek_compact_checked()?;
            if stack.peek_kind_is_double() {
                stack.pop_compact();
                return Ok(Value::Double(f64::from_bits(cv.raw_bits())));
            }
            let dv = match cv.tag() {
                CompactTag::Double => f64::from_bits(cv.raw_bits()),
                // Cast: integer word reinterpreted as float/double bit pattern
                CompactTag::Long => f64::from_bits(cv.as_long_unchecked() as u64),
                CompactTag::Int => match cv.to_value() {
                    // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                    Value::Int(x) => x as f64,
                    _ => 0.0,
                },
                CompactTag::Null | CompactTag::Uninitialized => 0.0,
                other => {
                    return Err(VmError::Internal {
                        message: format!(
                            "putstatic: tag {other:?} incompatible with D-descriptor field",
                        ),
                    }
                    .into());
                }
            };
            stack.pop_compact();
            Ok(Value::Double(dv))
        }
        Some(d @ (b'L' | b'[')) => {
            let v = stack.pop()?;
            Ok(coerce_value_for_return(v, d))
        }
        // JVMS §6.5 `putstatic`: a `boolean` static stores `value & 1`, a
        // `byte`/`char`/`short` static only its own width (the `putfield`
        // twin already narrows). `narrow_int_to_field_type` leaves `I`/`F`
        // and every non-`Int` carrier untouched. The single-pass JIT narrows
        // the same way before `jit_putstatic_int` (`op_field.rs` `0xb3`).
        Some(d) => Ok(narrow_int_to_field_type(stack.pop()?, d)),
        None => Ok(stack.pop()?),
    }
}

/// T18.K4 — Push an invoke* return value onto the caller's operand stack
/// using the tag-exact path for category-2 primitives (J/D).
///
/// Background: when a J or D return value is pushed via the generic
/// `ValueStack::push(Value)` boundary it goes through
/// `CompactValue::from_value`, which stores the raw 64-bit bits in an
/// untagged slot.  For longs whose bit pattern happens to set the
/// NaN-tagged marker bits (`NANBOX_BITS`), the slot is subsequently
/// decoded by `to_value()` as `Value::Uninitialized` — manifesting at
/// `pop_long` as "expected long on stack, got <uninitialized>" on the
/// KC26 boot path.
///
/// This helper routes `Value::Long(x)` / `Value::Double(d)` through
/// `push_compact(CompactValue::long / double)` which makes the semantic
/// intent explicit at the return-value-push site and shields category-2
/// primitives from the `Value` boundary round-trip.  Non-J/D returns
/// keep the legacy `push(Value)` path — it is already correct for
/// Int/Float/Object/etc., and preserving it avoids disturbing the hot
/// path for the common case.
///
/// A `Void` return (descriptor `V`) is represented by `None` at the
/// call site; this helper is only invoked on `Some(_)` so it never
/// mis-pushes an empty slot.
#[inline]
pub(super) fn push_invoke_return_value(
    stack: &mut crate::runtime::ValueStack,
    value: Value,
) -> Result<(), RuntimeError> {
    if remap_trace_on() {
        if let Value::Object(Some(o)) = &value {
            push_prov_record(o.as_ptr() as usize, "invoke-ret");
        }
    }
    match value {
        Value::Long(x) => {
            // Mark KIND_LONG so the caller's subsequent `pop_long` reads the
            // return value bit-exact. Without this a collision-shaped long
            // return (e.g. `Long.rotateRight` in SHA-512) is decoded as a
            // tagged int and truncated.
            stack.push_compact_long(crate::types::CompactValue::long(x));
            Ok(())
        }
        Value::Double(d) => {
            stack.push_compact_double(crate::types::CompactValue::double_raw(d));
            Ok(())
        }
        other => stack.push(other),
    }
}

/// JNI-style handles sometimes surface as `Value::Long` on the operand stack.
/// When popping `invoke*` arguments, coerce reference-typed parameters (and
/// the receiver) so downstream bytecode and natives see `Value::Object`.
///
/// Phase 9 #1 follow-up — also coerce `J` / `D` primitives so a long that
/// landed on the operand stack as an untagged 64-bit slot (e.g. via
/// `CompactValue::long`, whose raw bits coincide with the NaN-tag-free
/// region for any "normal" long value) is forwarded as `Value::Long`
/// rather than `Value::Double`. The bug surfaced as
/// `Native.futureSynchronize(1L)` arriving in the shim as
/// `Double(5e-324)` (the f64 reinterpretation of bit pattern 0x1),
/// which `arg_long` then quietly read as 0 — so `f.get()` queried a
/// non-existent submission, never ran the kernel, and left output
/// arrays at their pre-launch zero values.
#[inline]
pub(super) fn coerce_invoke_arg_for_descriptor(b: u8, v: Value) -> Value {
    match b {
        b'L' | b'[' => coerce_value_for_return(v, b),
        b'J' => match v {
            // Already a Long — fast path.
            Value::Long(_) => v,
            // Untagged-long-as-Double: reinterpret the bit pattern. This
            // is the common case for "normal" longs (any value whose
            // upper 16 bits aren't all 1s).
            // Cast: float/double raw bit pattern stored in integer word (no value conversion)
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            // i2l widening for upstream bytecode that forgot the cast.
            // Widening: i32 -> i64 (sign-extended, JVM i2l)
            Value::Int(i) => Value::Long(i as i64),
            // Null / Uninitialized → JVMS §2.3 default 0L.
            Value::Object(None) | Value::Uninitialized => Value::Long(0),
            _ => v,
        },
        b'D' => match v {
            Value::Double(_) => v,
            // A long that hit `Self::Long(x)` via descriptor-aware decode
            // path elsewhere would have raw bits — reinterpret.
            // Cast: integer word reinterpreted as float/double bit pattern
            Value::Long(x) => Value::Double(f64::from_bits(x as u64)),
            // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
            Value::Int(i) => Value::Double(i as f64),
            Value::Object(None) | Value::Uninitialized => Value::Double(0.0),
            _ => v,
        },
        _ => v,
    }
}

/// Decode a popped argument slot to a `Value`, honoring the operand stack's
/// `KIND_LONG` / `KIND_DOUBLE` mark.
///
/// When `kind` is a category-2 mark the slot was pushed by a genuine long or
/// double producer, so a
/// `J`-descriptor parameter reads the raw 64 bits verbatim — a collision-shaped
/// long (top bits `0xFFFC_…`, low 32-bit payload, indistinguishable from a
/// tagged int by bit pattern alone) keeps its high bits instead of being
/// truncated. All other cases (including `D` reinterpreting the long bits, and
/// any non-long-marked slot) fall through to the descriptor-aware decode, which
/// preserves the legacy i2l-widening behavior for synthetic int-where-long.
#[inline]
pub(super) fn decode_arg_kind_aware(cv: CompactValue, kind: u8, pd_byte: u8) -> Value {
    // A slot the push marked as a category-2 primitive IS its own 64 bits:
    // `CompactValue::long` and `CompactValue::double_raw` both store verbatim,
    // so whichever NaN-box sub-tag those bits happen to match says nothing
    // about them. Read them directly instead of letting
    // `decode_by_descriptor`'s int / null / uninitialized heuristics interpret
    // a collision — that is how a `double` argument carrying a NaN payload
    // used to arrive at `Double.doubleToRawLongBits` as `1.0`.
    if kind == crate::runtime::ValueStack::KIND_MARK_LONG
        || kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE
    {
        match pd_byte {
            b'J' => return Value::Long(cv.as_long_unchecked()),
            // Cast: integer word reinterpreted as float/double bit pattern
            b'D' => return Value::Double(f64::from_bits(cv.to_bits())),
            _ => {}
        }
    }
    cv.decode_by_descriptor(pd_byte)
}

/// Refresh every `Value::Object` reference in `args` via `load_and_forward`.
///
/// Mirrors the barrier `execute_invoke_kind` (the slow dispatch path)
/// already applies to its popped args, with the same rationale: args popped
/// off the operand stack land in a plain buffer that is invisible to the
/// collector's root scan. If a moving-GC evacuation runs in the gap between
/// popping and copying into the callee's locals (e.g. a shared-pool refill
/// or a monitor acquisition below, both of which can allocate), a stale
/// from-space address would otherwise reach the callee — and if that
/// address's old object has since been reclaimed and its memory reused by
/// an unrelated object, dereferencing it silently returns the WRONG
/// object's data instead of crashing. `execute_invoke_kind` already had
/// this barrier; the cached/fast dispatch paths below (`execute_invoke*_cached`,
/// `execute_invokevirtual_vtable_fast`) popped args the same way but never
/// re-validated them before building the callee frame. Root-caused via
/// `org.h2.test.unit.TestUpgrade`'s residual `NoSuchMethodError` — see
/// `bug-h2-suite-residual-fail-triage-FIXED.md`.
#[inline]
pub(super) fn refresh_stale_object_args(shared: &SharedVm, args: &mut [Value]) {
    for value in args.iter_mut() {
        if let Value::Object(Some(obj)) = value {
            *obj = shared.mem.heap.load_and_forward(*obj);
        }
    }
}

/// Keeps interpreter invoke arguments live while the slow dispatcher resolves
/// classes, checks bridges, and may re-enter Java before it builds the callee
/// frame.
///
/// `execute_invoke_kind` pops its arguments into a Rust `Vec<Value>`. That Vec
/// is not part of the Java root set. A moving collection during the relatively
/// long slow-dispatch path therefore updates the object everywhere except in
/// `args`; the later method lookup sees the zeroed from-space header and can
/// mis-dispatch `Map.get` as `Object.get`. Pin each non-null object in the
/// thread's collector-scanned native roots and copy the forwarded values back
/// at every boundary where the dispatcher consumes the Vec again.
///
/// The guard uses a raw thread pointer solely so it does not hold a Rust borrow
/// across the dispatcher. Its lifetime is lexical inside `execute_invoke_kind`,
/// and Drop restores the exact pin watermark on every return/error path.
pub(super) struct InvokeArgsRootGuard {
    pub(super) thread: *mut JvmThread,
    pub(super) pin_base: usize,
    pub(super) object_count: usize,
}

impl InvokeArgsRootGuard {
    pub(super) fn new(thread: &mut JvmThread, args: &[Value]) -> Self {
        // `CRATONVM_DBG_DEADREF_STORE`: were the arguments dead BEFORE the
        // guard existed?
        //
        // A pin preserves whatever it is given. If the values popped off the
        // operand stack were already stale, every `refresh` below hands them
        // back faithfully and the guard looks like it is working. Splitting
        // "pinned dead" from "died while pinned" is the whole question here:
        // the first is a caller that popped its arguments too early, the
        // second would be a gap in the `native_pin_roots` remap.
        crate::runtime::frame::note_dead_arg_pub(args, "InvokeArgsRootGuard::new");
        let pin_base = thread.native_pin_roots.len();
        for value in args {
            if let Value::Object(Some(obj)) = value {
                thread.native_pin_roots.push(*obj);
            }
        }
        Self {
            thread: thread as *mut JvmThread,
            pin_base,
            object_count: thread.native_pin_roots.len() - pin_base,
        }
    }

    pub(super) fn refresh(&self, args: &mut [Value]) {
        // SAFETY: the guard is scoped to the active interpreter invocation;
        // `thread` remains alive and exclusively owned by that invocation.
        let thread = unsafe { &*self.thread };
        let mut pin = self.pin_base;
        for value in args.iter_mut() {
            if matches!(value, Value::Object(Some(_))) {
                *value = Value::Object(Some(thread.native_pin_roots[pin]));
                pin += 1;
            }
        }
        // And the other half: a pin slot that is itself dead means the value
        // died WHILE pinned, which the pin is supposed to make impossible.
        crate::runtime::frame::note_dead_arg_pub(args, "InvokeArgsRootGuard::refresh");
        debug_assert_eq!(pin, self.pin_base + self.object_count);
    }

    pub(super) fn replace_object_arg(&self, args: &[Value], index: usize, obj: ObjectRef) {
        // Both callers replace a receiver they have just proven live, so the
        // ordinal is always present. Return rather than `.expect()` on the
        // counterfactual: this file's `#![cfg_attr(not(test), deny(...))]`
        // header forbids a production panic on the invoke path, and skipping
        // the pin refresh degrades to the argument's existing root instead of
        // tearing the VM down. `debug_assert!` keeps the invariant enforced
        // under test.
        let Some(ordinal) = args[..=index]
            .iter()
            .filter(|value| matches!(value, Value::Object(Some(_))))
            .count()
            .checked_sub(1)
        else {
            debug_assert!(
                false,
                "replacement invoke argument must already be a non-null object"
            );
            return;
        };
        // SAFETY: same lexical-lifetime argument as `refresh`; the computed
        // slot belongs to this guard's contiguous pin range.
        unsafe {
            let thread = &mut *self.thread;
            thread.native_pin_roots[self.pin_base + ordinal] = obj;
        }
    }
}

impl Drop for InvokeArgsRootGuard {
    fn drop(&mut self) {
        // SAFETY: see `refresh`; nested native calls restore their own later
        // watermarks before control returns here.
        unsafe {
            let thread = &mut *self.thread;
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }
}

/// Pop `invokevirtual` / `invokespecial` / `invokeinterface` arguments from
/// the operand stack (slow-path order) and apply `coerce_invoke_arg_for_descriptor`
/// so cached fast paths match `execute_invoke`.
pub(super) fn pop_coerced_invoke_args_virtual(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    frame_idx: usize,
    thread: &mut JvmThread,
) -> Result<(Vec<Value>, Arc<str>), MethodCallFailed> {
    // PERF (2026-08-04): this is the inline-cache HIT path — `execute_invoke*_
    // cached` reaches it after a 99.7%-warm `InvokeCache` lookup — and it used
    // to call `resolve_method_ref` unconditionally for two of the four values
    // that returns, paying a `resolution_cache` read lock, a hash probe and
    // three `Arc<str>` clone/drop pairs per invoke. That drop glue alone was
    // 1.5% of the Tomcat annotation-scan profile. `method_site_info` answers
    // from a per-thread direct-mapped table instead; default-OFF.
    let MethodSiteInfo {
        descriptor: method_descriptor,
        num_params,
    } = method_site_info(shared, thread, caller_class_id, cp_index)?;
    let num_params = num_params as usize;
    // PERF (2026-07-21): `nth_param_tag_byte` replaces `split_method_descriptor`
    // here — every caller of this Vec<String>/String-allocating parse only
    // ever read the first byte of each parameter token (see
    // `coerce_invoke_arg_for_descriptor`/`decode_arg_kind_aware`, both
    // `u8`-only). This was the single dominant hot spot (confirmed via cdb
    // stack sampling) behind a ~197x CratonVM-vs-HotSpot slowdown on
    // method-call-heavy interpreted workloads (Xerces SAX parsing —
    // see docs/known-issues/repros/xerces-sax-manysmallfiles-slowdown/).
    // BC SM2 fix (2026-05-28): use raw CompactValue + descriptor-aware
    // decode so a Long-collision-with-SUB_OBJECT bit pattern doesn't
    // round-trip through Value::Object and lose bits.
    // Pop slots as (CompactValue, is_KIND_LONG). The long-mark lets a
    // `J`-descriptor argument that is a collision-shaped long (`0xFFFC_…`
    // top bits, low 32-bit payload — e.g. a SHA-512 working variable passed
    // to `Long.rotateRight`) decode bit-exact instead of being truncated by
    // `decode_by_descriptor(b'J')`'s i2l-widening fallback.
    let mut tmp_cv: Vec<(CompactValue, u8)> = Vec::with_capacity(num_params + 1);
    for _ in 0..num_params {
        tmp_cv.push(thread.frames[frame_idx].stack.pop_with_kind()?);
    }
    tmp_cv.push(thread.frames[frame_idx].stack.pop_with_kind()?);
    tmp_cv.reverse();
    let mut args = Vec::with_capacity(num_params + 1);
    args.push(coerce_invoke_arg_for_descriptor(
        b'L',
        tmp_cv[0].0.decode_by_descriptor(b'L'),
    ));
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::of(&method_descriptor, shared.config.param_tag_scan);
    for i in 0..num_params {
        let pd_byte = param_tags.get(&method_descriptor, i);
        let (cv, kind) = tmp_cv[i + 1];
        let v = decode_arg_kind_aware(cv, kind, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }
    refresh_stale_object_args(shared, &mut args);
    Ok((args, method_descriptor))
}

/// The widest argument list the facts-driven pops below will serve, receiver
/// included. `DescriptorFacts` holds eight parameter tags inline, so nine is
/// already more than either helper can use; the round number keeps the buffer
/// one cache-line pair and leaves headroom if `INLINE_PARAMS` ever grows.
pub(super) const MAX_CACHED_NATIVE_ARGS: usize = 16;

/// Whether a cached-native call site may answer its descriptor questions from
/// the [`DescriptorFacts`](cratonvm_jit_api::DescriptorFacts) its inline-cache
/// entry was filled with, rather than re-resolving the constant pool.
///
/// `facts` is tokenised from the SAME `resolve_method_metadata` result that
/// `resolve_method_ref` returns on the hit path, and an inline-cache entry is
/// keyed by `(caller class, cp index)` — so the answer is a constant of the
/// entry. What this guards is the two shapes the tag array cannot describe: a
/// descriptor with more parameters than `INLINE_PARAMS`, and any disagreement
/// between the entry's `num_params` and the tokenised tag count. Either falls
/// back to the general helper, which re-derives everything.
#[inline]
pub(super) fn native_site_facts_usable(
    facts: &cratonvm_jit_api::DescriptorFacts,
    num_params: usize,
    with_receiver: bool,
) -> bool {
    !crate::runtime::env_cache::no_cached_native_facts()
        && !cratonvm_jit_api::descriptor_facts_disabled()
        && !facts.param_tags_overflow
        && facts.param_tag_len as usize == num_params
        && num_params + usize::from(with_receiver) <= MAX_CACHED_NATIVE_ARGS
}

/// [`pop_coerced_invoke_args_virtual`] answered from a call site's cached
/// [`DescriptorFacts`]: no constant-pool re-resolution, no descriptor scan and
/// no heap allocation.
///
/// Byte-for-byte the same arguments as the general helper. The receiver is
/// decoded with `decode_by_descriptor(b'L')` — NOT `decode_arg_kind_aware` —
/// because that is what the general helper does, and the two differ for a slot
/// carrying a category-2 kind mark.
///
/// Returns how many slots of `buf` were filled. On a mid-pop error the stack is
/// left exactly as the general helper leaves it: partially popped, which is
/// the caller's existing contract for a stack that was too shallow.
#[inline]
pub(super) fn pop_coerced_invoke_args_virtual_facts(
    shared: &SharedVm,
    frame_idx: usize,
    thread: &mut JvmThread,
    facts: &cratonvm_jit_api::DescriptorFacts,
    num_params: usize,
    buf: &mut [Value; MAX_CACHED_NATIVE_ARGS],
) -> Result<usize, MethodCallFailed> {
    for i in (0..num_params).rev() {
        let (cv, kind) = thread.frames[frame_idx].stack.pop_with_kind()?;
        let pd_byte = facts.param_tags[i];
        buf[i + 1] =
            coerce_invoke_arg_for_descriptor(pd_byte, decode_arg_kind_aware(cv, kind, pd_byte));
    }
    let (recv_cv, _) = thread.frames[frame_idx].stack.pop_with_kind()?;
    buf[0] = coerce_invoke_arg_for_descriptor(b'L', recv_cv.decode_by_descriptor(b'L'));
    refresh_stale_object_args(shared, &mut buf[..num_params + 1]);
    Ok(num_params + 1)
}

/// [`pop_coerced_invoke_args_static`] answered from a call site's cached
/// [`DescriptorFacts`]. See [`pop_coerced_invoke_args_virtual_facts`].
#[inline]
pub(super) fn pop_coerced_invoke_args_static_facts(
    shared: &SharedVm,
    frame_idx: usize,
    thread: &mut JvmThread,
    facts: &cratonvm_jit_api::DescriptorFacts,
    num_params: usize,
    buf: &mut [Value; MAX_CACHED_NATIVE_ARGS],
) -> Result<usize, MethodCallFailed> {
    for i in (0..num_params).rev() {
        let (cv, kind) = thread.frames[frame_idx].stack.pop_with_kind()?;
        let pd_byte = facts.param_tags[i];
        buf[i] =
            coerce_invoke_arg_for_descriptor(pd_byte, decode_arg_kind_aware(cv, kind, pd_byte));
    }
    refresh_stale_object_args(shared, &mut buf[..num_params]);
    Ok(num_params)
}

/// Pop `invokestatic` arguments (no receiver) with the same JNI handle fix.
pub(super) fn pop_coerced_invoke_args_static(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    frame_idx: usize,
    thread: &mut JvmThread,
) -> Result<(Vec<Value>, Arc<str>), MethodCallFailed> {
    // PERF (2026-08-04): see `pop_coerced_invoke_args_virtual` — same removal
    // of `resolve_method_ref` from the inline-cache HIT path.
    let MethodSiteInfo {
        descriptor: method_descriptor,
        num_params,
    } = method_site_info(shared, thread, caller_class_id, cp_index)?;
    let num_params = num_params as usize;
    // PERF (2026-07-21): see `pop_coerced_invoke_args_virtual` — same
    // non-allocating `nth_param_tag_byte` swap for `split_method_descriptor`.
    // BC SM2 fix (2026-05-28): pop slots as raw CompactValue and decode
    // with the parameter descriptor. `CompactValue::to_value()` would
    // mis-decode a Long whose bits collide with SUB_OBJECT as
    // Value::Object — the descriptor-aware decode keeps the long bits.
    let mut tmp_cv: Vec<(CompactValue, u8)> = Vec::with_capacity(num_params);
    for _ in 0..num_params {
        tmp_cv.push(thread.frames[frame_idx].stack.pop_with_kind()?);
    }
    tmp_cv.reverse();
    let mut args = Vec::with_capacity(num_params);
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::of(&method_descriptor, shared.config.param_tag_scan);
    for (i, (cv, kind)) in tmp_cv.into_iter().enumerate() {
        let pd_byte = param_tags.get(&method_descriptor, i);
        let v = decode_arg_kind_aware(cv, kind, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }
    refresh_stale_object_args(shared, &mut args);
    Ok((args, method_descriptor))
}

#[cfg(test)]
mod r9w7_putstatic7_tests {
    use super::*;

    /// JVMS §6.5 `putstatic` narrowing, written out independently of
    /// `narrow_int_to_field_type` so the test pins the SPEC, not the helper.
    /// `jit/tests/r9w7_putstatic7_narrow.rs` pins the single-pass JIT against
    /// the same table (`jvms_static_store`), which is what makes the two tiers
    /// provably agree.
    fn jvms_static_store(tag: u8, x: i32) -> i32 {
        match tag {
            b'Z' => x & 1,
            b'B' => i32::from(x as i8),
            b'C' => i32::from(x as u16),
            b'S' => i32::from(x as i16),
            _ => x,
        }
    }

    const INPUTS: [i32; 12] = [
        0,
        1,
        2,
        3,
        -1,
        300,
        -129,
        0x0001_8000,
        0x0001_0000,
        0x1234_5680,
        i32::MIN,
        i32::MAX,
    ];

    /// `iconst_2; putstatic Flag:Z` must store 0 (and `3` store 1); a
    /// `byte`/`char`/`short` static keeps only its own width; `I` is
    /// untouched. Before round 9 wave 7 the catch-all arm stored the raw int.
    #[test]
    fn putstatic_narrows_sub_int_values_to_the_field_width() {
        for &tag in b"ZBCSI" {
            for &x in &INPUTS {
                let mut stack = crate::runtime::ValueStack::new(4);
                stack.push(Value::Int(x)).expect("push");
                let got = pop_static_field_value(&mut stack, Some(tag)).expect("pop");
                assert_eq!(
                    got,
                    Value::Int(jvms_static_store(tag, x)),
                    "putstatic {}:{x:#x}",
                    tag as char
                );
            }
        }
    }

    /// The static/instance mismatch reports HotSpot's wording, naming the
    /// class in external (dot) form and the direction the OPCODE expected.
    #[test]
    fn field_staticness_mismatch_uses_hotspot_wording() {
        assert_eq!(
            field_staticness_message(true, "p/q/C", "f"),
            "Expected static field p.q.C.f"
        );
        assert_eq!(
            field_staticness_message(false, "C$Inner", "count"),
            "Expected non-static field C$Inner.count"
        );
    }

    fn final_field(declaring: u32, is_static: bool, is_final: bool) -> ResolvedField {
        ResolvedField {
            declaring_class_id: ClassId::new(declaring),
            field_index: 0,
            is_static,
            is_volatile: false,
            is_final,
            is_reference: false,
            desc_byte: b'I',
        }
    }

    /// JVMS §6.5 / HotSpot `LinkResolver::resolve_field`: the declaring-class
    /// rule holds for every class file version, the initializer-method rule
    /// only from version 53, and a non-final field is never refused.
    #[test]
    fn final_field_put_violation_follows_hotspot_rules() {
        use FinalPutViolation::*;
        let c = ClassId::new(5);
        let v =
            |f: &ResolvedField, m: &str, major: u16| final_field_put_violation(f, c, m, || major);
        // Not final: anything goes, and the version is never asked.
        assert_eq!(
            final_field_put_violation(&final_field(9, false, false), c, "m", || {
                panic!("version asked for a non-final field")
            }),
            None
        );
        // Another class's final field: refused whatever the method or version.
        assert_eq!(
            v(&final_field(9, false, true), "<init>", 69),
            Some(DifferentClass)
        );
        assert_eq!(
            v(&final_field(9, true, true), "<clinit>", 50),
            Some(DifferentClass)
        );
        // Own instance final: `<init>` always; other methods only below 53.
        assert_eq!(v(&final_field(5, false, true), "<init>", 69), None);
        assert_eq!(v(&final_field(5, false, true), "reset", 52), None);
        assert_eq!(
            v(&final_field(5, false, true), "reset", 53),
            Some(DifferentMethod)
        );
        assert_eq!(
            v(&final_field(5, false, true), "<clinit>", 69),
            Some(DifferentMethod)
        );
        // Own static final: `<clinit>` only, from 53.
        assert_eq!(v(&final_field(5, true, true), "<clinit>", 69), None);
        assert_eq!(
            v(&final_field(5, true, true), "<init>", 69),
            Some(DifferentMethod)
        );
        assert_eq!(v(&final_field(5, true, true), "<init>", 52), None);
        // A legal constructor store never looks the version up.
        assert_eq!(
            final_field_put_violation(&final_field(5, false, true), c, "<init>", || {
                panic!("version asked for an <init> store")
            }),
            None
        );
    }

    /// JIT round 21 wave 14, lane hidden-text: a hidden class keeps its
    /// `/0x<hex>` tail in both class names (`Klass::external_name()`), and in
    /// the staticness text.
    #[test]
    fn hidden_class_names_keep_their_slash_0x_tail_in_field_texts() {
        assert_eq!(
            final_field_put_message(
                FinalPutViolation::DifferentClass,
                true,
                "p/H/0x0000000000000002",
                "TAG",
                "p/D/0x0000000000000003",
                "m"
            ),
            "Update to static final field p.H/0x0000000000000002.TAG attempted from a \
             different class (p.D/0x0000000000000003) than the field's declaring class"
        );
        assert_eq!(
            field_staticness_message(true, "p/H/0x2a", "f"),
            "Expected static field p.H/0x2a.f"
        );
    }

    /// HotSpot's two `IllegalAccessError` texts, including the trailing space
    /// of the initializer-method form.
    #[test]
    fn final_field_put_message_uses_hotspot_wording() {
        assert_eq!(
            final_field_put_message(
                FinalPutViolation::DifferentClass,
                false,
                "p/C",
                "f",
                "p/D",
                "m"
            ),
            "Update to non-static final field p.C.f attempted from a different class (p.D) \
             than the field's declaring class"
        );
        assert_eq!(
            final_field_put_message(
                FinalPutViolation::DifferentMethod,
                true,
                "p/C",
                "S",
                "p/C",
                "set"
            ),
            "Update to static final field p.C.S attempted from a different method (set) \
             than the initializer method <clinit> "
        );
        assert_eq!(
            final_field_put_message(
                FinalPutViolation::DifferentMethod,
                false,
                "C",
                "x",
                "C",
                "reset"
            ),
            "Update to non-static final field C.x attempted from a different method (reset) \
             than the initializer method <init> "
        );
    }

    /// The JIT doors' opcode scan matches only the requested opcode and
    /// operand at an instruction start, never operand bytes that happen to
    /// spell it.
    #[test]
    fn code_has_field_put_matches_only_instruction_starts() {
        // sipush 0xb5_00; pop; getfield #7; putfield #7; return
        let code = [
            0x11, 0xb5, 0x00, 0x57, 0xb4, 0x00, 0x07, 0xb5, 0x00, 0x07, 0xb1,
        ];
        assert!(code_has_field_put(&code, 7, false));
        assert!(!code_has_field_put(&code, 7, true));
        assert!(
            !code_has_field_put(&code, 0x57, false),
            "sipush operand is not a putfield"
        );
        // getstatic #3; putstatic #4; return
        let code = [0xb2, 0x00, 0x03, 0xb3, 0x00, 0x04, 0xb1];
        assert!(code_has_field_put(&code, 4, true));
        assert!(!code_has_field_put(&code, 3, true));
    }

    /// Non-int carriers and descriptor-less pops are passed through.
    #[test]
    fn putstatic_narrowing_leaves_other_descriptors_alone() {
        let mut stack = crate::runtime::ValueStack::new(4);
        stack.push(Value::Float(2.5)).expect("push");
        assert_eq!(
            pop_static_field_value(&mut stack, Some(b'F')).expect("pop"),
            Value::Float(2.5)
        );
        stack.push(Value::Int(0x1234_5680)).expect("push");
        assert_eq!(
            pop_static_field_value(&mut stack, None).expect("pop"),
            Value::Int(0x1234_5680)
        );
    }

    /// i66-L7: a `D` pop decodes before it pops. A tag no `double` producer
    /// leaves returns `Err` with the operand still on the stack, and an empty
    /// stack reports the underflow instead of wrapping `len`.
    #[test]
    fn a_d_pop_that_fails_leaves_the_operand_stack_as_it_was() {
        let mut stack = crate::runtime::ValueStack::new(4);
        stack.push(Value::Int(7)).expect("push");
        stack.push(Value::Float(2.5)).expect("push");
        assert!(pop_static_field_value(&mut stack, Some(b'D')).is_err());
        assert_eq!(stack.len(), 2, "the odd-tag error popped nothing");
        assert_eq!(stack.pop().expect("pop"), Value::Float(2.5));

        let mut empty = crate::runtime::ValueStack::new(4);
        assert!(pop_static_field_value(&mut empty, Some(b'D')).is_err());
        assert_eq!(empty.len(), 0);

        // An accepted tag still pops exactly one slot.
        stack.push(Value::Long(0)).expect("push");
        assert_eq!(
            pop_static_field_value(&mut stack, Some(b'D')).expect("pop"),
            Value::Double(0.0)
        );
        assert_eq!(stack.len(), 1);
    }
}

#[cfg(test)]
mod i8_l4_static_watch_order_tests {
    /// JVMTI `FieldAccess` / `FieldModification` for a static field fire AFTER
    /// the holder's initialization, as HotSpot posts them (its `getstatic` /
    /// `putstatic` templates resolve the field, which initializes the holder,
    /// and only then post): a `<clinit>` that throws produces no event. Until
    /// interpreter round i1 wave 8 both handlers fired first. A source witness,
    /// because the alternative is a JVMTI agent plus a class whose initializer
    /// throws, for an ORDERING claim about two statements.
    #[test]
    fn static_field_watch_events_fire_after_the_holder_is_initialized() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/runtime/interpreter/opcodes.rs"
        ))
        .expect("opcodes.rs is readable")
        .replace("\r\n", "\n");
        let body = |sig: &str| -> String {
            let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
            let rest = &src[start..];
            // `\u{7d}` is a closing brace, spelled so this line's braces
            // balance for the brace-counting production-panic scanner.
            let stop = rest
                .find("\n\u{7d}\n")
                .unwrap_or_else(|| panic!("end of {sig} not found"));
            rest[..stop].to_string()
        };

        let put = body("pub(super) fn op_putstatic(");
        let init = put
            .find("ensure_class_initialized_shared(")
            .expect("op_putstatic initializes the holder");
        let fire = put
            .find("fire_field_modification_if_watched_for_vm(")
            .expect("op_putstatic fires the FieldModification watchpoint");
        assert!(
            init < fire,
            "putstatic must initialize the holder before the watch event"
        );

        let get = body("pub(super) fn op_getstatic(");
        let init = get
            .find("ensure_class_initialized_shared(")
            .expect("op_getstatic initializes the holder on its ordinary arm");
        let after_init = &get[init..];
        let fire = after_init
            .find("fire_access_watch(")
            .expect("op_getstatic fires the FieldAccess watchpoint after initializing");
        let read = after_init
            .find("get_static_shared(")
            .expect("op_getstatic reads the static after initializing");
        assert!(
            fire < read,
            "getstatic must fire the watch event before the read"
        );
    }
}

#[cfg(test)]
mod r15w5_fieldowner2_tests {
    //! Round 15 wave 5, lane fieldowner2: the compile doors' threadless field
    //! peek keeps loader-specific guesses out of the per-site resolution
    //! cache, which an isolated URL loader's site now takes as its answer
    //! (`r15w4-fieldowner-isolated-loader-field-sites-reask-the-loader-FIXED-20260930.md`).
    use super::{loader_specific_peek, threadless_fill_withheld};
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
    use crate::vm::SharedVm;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    /// Define and register a field-less, method-less class (the
    /// `constants.rs` `i9_l2_class_access_tests` helper).
    fn define(shared: &SharedVm, name: &str, loader: ClassLoaderId) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: loader,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(loader, name, id);
        id
    }

    #[test]
    fn a_peek_fills_only_what_the_interpreter_would_answer() {
        if !super::isolated_field_site_sticky_enabled() {
            // The kill switch restores the historical fill everywhere.
            return;
        }
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let user = ClassLoaderId::UserDefined(4051);
        let app = ClassLoaderId::Application;
        let app_referencing = define(&shared, "fo2/AppCaller", app);
        let referencing = define(&shared, "fo2/Caller", user);
        let app_owner = define(&shared, "fo2/Shared", app);
        let own_owner = define(&shared, "fo2/Own", user);
        let jdk_owner = define(&shared, "javax/fo2/Named", ClassLoaderId::Bootstrap);

        // A built-in loader's class: every answer is the interpreter's too.
        assert!(!threadless_fill_withheld(
            &shared,
            app_referencing,
            "fo2/Shared",
            app_owner,
            false
        ));
        assert!(!loader_specific_peek(
            &shared,
            app_referencing,
            "fo2/Shared",
            true
        ));
        // A user loader's loader-sensitive name answered by another loader's
        // class (the initiating memo): withheld.
        assert!(threadless_fill_withheld(
            &shared,
            referencing,
            "fo2/Shared",
            app_owner,
            true
        ));
        assert!(loader_specific_peek(
            &shared,
            referencing,
            "fo2/Shared",
            true
        ));
        // Its own definition: the answer every path gives; filled.
        assert!(!threadless_fill_withheld(
            &shared,
            referencing,
            "fo2/Own",
            own_owner,
            true
        ));
        // A `javax/`-style name the loader may define, answered by the JDK's
        // class: withheld (the interpreter asks the loader under --jdk-only).
        assert!(threadless_fill_withheld(
            &shared,
            referencing,
            "javax/fo2/Named",
            jdk_owner,
            false
        ));
        assert!(loader_specific_peek(
            &shared,
            referencing,
            "javax/fo2/Named",
            false
        ));
        // A `java/` name: the user loader cannot define it, but while the
        // interpreter asks that loader's `loadClass` for it (JavaNameAsk, on
        // by default since dev i47; JVMS 5.3.2) the call is observable, so a
        // peek's global answer is withheld; with the ask off the interpreter
        // answers globally too and the fill is the answer every path gives.
        // (Until nio21 wave 13 this asserted "filled" unconditionally, stale
        // since i47 put `java/` in `is_user_loader_global_name`.)
        let asks =
            super::super::constants::loader_global_name_ask_enabled(&shared, "java/lang/Object");
        assert_eq!(
            threadless_fill_withheld(&shared, referencing, "java/lang/Object", jdk_owner, false),
            asks
        );
        assert_eq!(
            loader_specific_peek(&shared, referencing, "java/lang/Object", false),
            asks
        );
    }

    /// The compile doors' side-effect-free JVMS §5.3.4 question: two loaders
    /// that each defined `fo2c/T` see a field of that type differently.
    #[test]
    fn a_peek_sees_a_field_type_two_loaders_both_defined() {
        if !crate::runtime::env_cache::loader_aware_resolution() {
            // The user-loader view below is the loader-initiated one.
            return;
        }
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let a = ClassLoaderId::UserDefined(4061);
        let b = ClassLoaderId::UserDefined(4062);
        let holder = define(&shared, "fo2c/Holder", a);
        let reader = define(&shared, "fo2c/Reader", b);
        let same_loader_reader = define(&shared, "fo2c/Peer", a);
        define(&shared, "fo2c/T", a);
        define(&shared, "fo2c/T", b);
        let cm = shared.classes.class_manager.read();
        assert!(super::peek_field_constraint_violated(
            &shared, &cm, reader, holder, "Lfo2c/T;"
        ));
        assert!(super::peek_field_constraint_violated(
            &shared,
            &cm,
            reader,
            holder,
            "[[Lfo2c/T;"
        ));
        assert!(
            !super::peek_field_constraint_violated(
                &shared,
                &cm,
                same_loader_reader,
                holder,
                "Lfo2c/T;"
            ),
            "one loader: no constraint"
        );
        assert!(!super::peek_field_constraint_violated(
            &shared, &cm, reader, holder, "[I"
        ));
        assert!(!super::peek_field_constraint_violated(
            &shared,
            &cm,
            reader,
            holder,
            "Ljava/lang/String;"
        ));
        assert!(
            !super::peek_field_constraint_violated(&shared, &cm, reader, holder, "Lfo2c/Unknown;"),
            "a name one side has not seen is not a direct violation"
        );
    }

    /// Round 15 wave 6 (lane sweepA, L2): the peek withholds its fill only for
    /// a field type a user loader may define.
    #[test]
    fn only_a_definable_reference_type_is_constrainable() {
        use super::field_type_constrainable as c;
        assert!(c("Lfo2c/Shared;"));
        assert!(c("[[Lfo2c/Shared;"));
        assert!(c("Ljavax/sql/DataSource;"), "javax/ is definable");
        assert!(!c("Ljava/io/PrintStream;"), "System.out keeps its fill");
        assert!(!c("[Ljava/lang/String;"));
        assert!(!c("I"));
        assert!(!c("[J"));
        assert!(!c("Lmalformed"));
    }
}
