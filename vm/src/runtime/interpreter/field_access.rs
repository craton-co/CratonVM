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
    let (field_class_name, field_name) = {
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
        (class_name, field_name.to_string())
    };

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
    let loader_local_id = if loader_sensitive {
        lookup_loader_initiated(shared, current_class_id, &field_class_name)
    } else {
        None
    };
    if let Some(cached) = shared
        .classes
        .resolution_cache
        .read()
        .get_field(current_class_id, cp_index)
    {
        // A per-callsite field entry can have been seeded before this loader
        // defined its own owner class (for example while a forked Spring test
        // context is being prepared). It is not valid merely because the
        // loader has since initiated that name: the cached declaring identity
        // must also be that exact owner. Otherwise getstatic returns the app
        // copy's enum singleton from a fork-loaded caller.
        if !loader_sensitive || loader_local_id.map_or(true, |id| cached.declaring_class_id == id) {
            return Ok(cached.clone());
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
        None => shared.load_class_concurrent(&field_class_name)?,
    };

    resolve_field_in_class(
        shared,
        current_class_id,
        cp_index,
        field_class_id,
        field_class_name,
        field_name,
    )
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
/// loader-sensitive sites) stays opt-in, and so does `method-site-cache`,
/// which measured nothing and is kept on that footing.
fn field_site_cache_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        // Same opt-out spelling as `CRATONVM_JIT_OSR` (`env_cache`), so one
        // kill switch reads the same way across the interpreter's default-ON
        // levers. Unset → enabled.
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_FIELD_SITE_CACHE")
            .ok()
            .map(|v| {
                let v = v.trim();
                !(v == "0"
                    || v.eq_ignore_ascii_case("false")
                    || v.eq_ignore_ascii_case("off")
                    || v.eq_ignore_ascii_case("no"))
            })
            .unwrap_or(true)
    })
}

/// `CRATONVM_JIT=field-site-cache-loader` — additionally admit
/// **loader-sensitive** sites resolved from the loader-local lookup, guarded by
/// the loader-resolution epoch as well. Implies `field-site-cache`; on its own
/// it does nothing. Default-OFF and measured separately, because it is the arm
/// with the wider correctness surface.
fn field_site_cache_loader_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_FIELD_SITE_CACHE_LOADER").is_some()
    })
}

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
    let epochs_at_entry = if field_site_cache_enabled() {
        if let Some(hit) = thread.field_sites.get(current_class_id, cp_index) {
            let hit = hit.clone();
            site_stats::bump(site_stats::FIELD_HIT);
            return Ok(hit);
        }
        site_stats::bump(site_stats::FIELD_MISS);
        // Read BEFORE resolving; see `SiteCache::put`.
        FieldSiteCache::epochs_now()
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

    let (field_class_name, field_name) = {
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
        (class_name, field_name.to_string())
    };

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
    let loader_local_id = if loader_sensitive {
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
        None => resolve_class_loader_aware(shared, thread, current_class_id, &field_class_name)?,
    };
    if let Some(cached) = cached {
        let cache_matches_owner = {
            let cm = shared.classes.class_manager.read();
            cached.declaring_class_id == field_class_id
                || cm.is_subclass_of(field_class_id, cached.declaring_class_id)
        };
        if cache_matches_owner {
            fill_field_site(
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

    let resolved = resolve_field_in_class(
        shared,
        current_class_id,
        cp_index,
        field_class_id,
        field_class_name,
        field_name,
    )?;
    fill_field_site(
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
/// `CRATONVM_JIT=field-site-cache-loader`, and only when the owner came back
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
    thread: &mut JvmThread,
    current_class_id: ClassId,
    cp_index: u16,
    loader_sensitive: bool,
    loader_local: bool,
    epochs_at_entry: (u64, u64),
    resolved: &ResolvedField,
) {
    if !field_site_cache_enabled() {
        return;
    }
    if loader_sensitive && !(loader_local && field_site_cache_loader_enabled()) {
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
fn method_site_cache_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_METHOD_SITE_CACHE").is_some()
    })
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
    if !method_site_cache_enabled() {
        let (_cn, _mn, descriptor, num_params) =
            resolve_method_ref(shared, caller_class_id, cp_index)?;
        return Ok(MethodSiteInfo {
            descriptor,
            num_params: u16::try_from(num_params).unwrap_or(u16::MAX),
        });
    }
    if let Some(hit) = thread.method_sites.get(caller_class_id, cp_index) {
        let hit = hit.clone();
        site_stats::bump(site_stats::METHOD_HIT);
        return Ok(hit);
    }
    site_stats::bump(site_stats::METHOD_MISS);
    // Read BEFORE resolving; see `SiteCache::put`.
    let epochs_at_entry = MethodSiteCache::epochs_now();
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

/// Shared tail of [`resolve_field_ref`] / [`resolve_field_ref_loader_aware`]:
/// given an already-resolved field-owning `field_class_id`, look up
/// `field_name` (own fields first, then the superclass/superinterface chain),
/// cache the result under `(current_class_id, cp_index)`, and return it.
pub(super) fn resolve_field_in_class(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    field_class_id: ClassId,
    field_class_name: String,
    field_name: String,
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
    // `check_module_access_by_id` and nothing else. Member access (private /
    // protected / package-private) is still not enforced on the bytecode path
    // — see `classloading::access_control`'s module docs. Naming the policy
    // does not change it.
    let resolved = {
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
        resolver.adopt(scoped)?
    };

    shared
        .classes
        .resolution_cache
        .write()
        .put_field(current_class_id, cp_index, resolved.clone());

    Ok(resolved)
}

/// Kill switch for the duplicate-class-name gate below
/// (`CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE=1`, or
/// `CRATONVM_LOADER=-dup-name-field-gate`), so the change can be A/B'd on one
/// binary. Set, the gate is skipped and every call walks the authoritative
/// path exactly as it did before the gate existed — a cross-binary comparison
/// is not an A/B.
#[inline]
fn dup_name_field_gate_disabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE").is_some()
    })
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
    if !dup_name_field_gate_disabled() && !crate::classloading::any_duplicate_class_name() {
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
            if stack.peek_kind_is_double() {
                let cv = stack.pop_compact();
                return Ok(Value::Double(f64::from_bits(cv.raw_bits())));
            }
            let cv = stack.pop_compact();
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
            Ok(Value::Double(dv))
        }
        Some(d @ (b'L' | b'[')) => {
            let v = stack.pop()?;
            Ok(coerce_value_for_return(v, d))
        }
        _ => Ok(stack.pop()?),
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
    let param_tags = ParamTags::of(&method_descriptor);
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
    let param_tags = ParamTags::of(&method_descriptor);
    for (i, (cv, kind)) in tmp_cv.into_iter().enumerate() {
        let pd_byte = param_tags.get(&method_descriptor, i);
        let v = decode_arg_kind_aware(cv, kind, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }
    refresh_stale_object_args(shared, &mut args);
    Ok((args, method_descriptor))
}
