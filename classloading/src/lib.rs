// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class loading subsystem.
//!
//! Handles locating, loading, caching, linking, and verifying Java classes.
//! The key types are:
//! - [`ClassId`] — a unique identifier for each loaded class
//! - [`Class`] — runtime representation of a loaded Java class
//! - [`ClassState`] — the initialization lifecycle state machine
//! - [`ClassStore`] — indexed storage for all loaded classes
//! - [`ClassPath`] — locates `.class` files on the filesystem
//! - [`ResolutionCache`](resolution::ResolutionCache) — caches resolved symbolic references
//! - [`access_control`] — JVM spec 5.4.4 access checking
//! - [`verifier`] — structural verification (Pass 2)
//! - [`bytecode_verifier`] — bytecode type-checking verification (Pass 3)
//! - [`vtype`] — verification type lattice for Pass 3

/// The class-loading slice of the process-wide typed configuration.
///
/// Every `CRATONVM_*` flag this crate reads is a field on
/// [`cratonvm_types::LoaderFlags`], parsed once at first use. See
/// `docs/flag-census.md` for the inventory and
/// `cratonvm_types::flags` for the latching rules.
#[inline]
pub(crate) fn loader_flags() -> &'static cratonvm_types::LoaderFlags {
    &cratonvm_types::flags().loader
}

pub mod access_control;
pub mod annotations;
pub mod builtin_loaders;
pub mod bytecode_verifier;
mod class;
mod class_manager;
/// Class provenance (`ClassOrigin`) — the `--jdk-only` policy's view of where
/// each loaded class came from. See `docs/feature-designs/jdk-only-mode.md` §5.
///
/// Public as a module (not just re-exported) because `--dump-class-origins`
/// consumers in `vm-cli` / `difftest` name the type through
/// `cratonvm_classloading::class_origin::…`.
pub mod class_origin;
mod class_path;
pub(crate) mod fx_hash;
pub mod jar_signer;
pub mod metadata_handle;
pub mod loaders;
pub mod module;
pub mod proxy_gen;
pub mod resolution;
pub mod type_maps;
pub mod verifier;
pub mod verify_frame;
pub mod verify_insn;
pub mod vtype;

pub use class::{
    find_field_recursive, find_method_recursive, invokespecial_selection_start, ArrayInfo, Class,
    ClassId, ClassLoaderId, ClassState, ClassStore, CodeSource, RecordComponentInfo,
    RECORD_OBJ_COMPUTED, RECORD_OBJ_EQUALS, RECORD_OBJ_HASH_CODE, RECORD_OBJ_TO_STRING,
};
// JDK-only mode (contract §5): every `Class` carries a `ClassOrigin`, and the
// `--dump-class-origins` census is a `Vec<ClassOriginEntry>`. Both are named at
// the crate root because the consumers (`vm`, `vm-cli`, `difftest`) already
// import `Class` from here.
pub use class_origin::{ClassOrigin, ClassOriginEntry};
pub use class_manager::is_bootstrap_appended_class;
pub use class_manager::synthetic_stub_instance_field_count;
pub use class_manager::{
    any_class_redefined,
    class_definition_epoch,
    bump_jit_supersede_epoch,
    drain_pending_class_hooks,
    install_class_file_load_hook,
    install_class_load_hook,
    install_class_prepare_hook,
    install_jit_invalidate_hook,
    install_resolution_invalidate_hook,
    install_vtable_install_hook,
    install_vtable_override_hook,
    is_builtin_classloader_name,
    jdk_superclass_lookup,
    jit_supersede_epoch,
    // Loader-identity consolidation: the single-source-of-truth
    // `CRATONVM_LOADER_AWARE_RESOLUTION` gate. `vm::runtime::env_cache` and
    // `native-builtins::classloader` both delegate to this instead of
    // keeping their own `OnceLock`-cached env-var copy — see
    // `docs/internal/loader-identity.md`.
    loader_aware_resolution,
    register_builtin_classloaders,
    set_current_thread_id,
    static_common_superclass_lookup,
    ClassFileLoadHook,
    ClassManager,
    DefineClassOptions,
    JitInvalidateHook,
    JvmtiClassHook,
    // C2 review P1 (classloading identity): the three-way answer that keeps
    // "nobody has this name" apart from "several loaders each have their own
    // class under it". Every `Option`-returning lookup collapses the two, and a
    // caller that reads the collapse as "absent" loads a second copy — see
    // `docs/known-issues/classloading-identity-audit.md`.
    NameResolution,
    RedefineOptions,
    ResolutionInvalidateHook,
    UnloadedClass,
    VtableInstallHook,
    VtableMethodSnapshot,
    VtableOverrideHook,
    VtableSlotDescriptor,
    CLASS_INIT_INITIALIZED,
    // Round 5 audit fix (HIGH): AtomicU8 class-init fast-path constants
    // consumed by `vm_util::ensure_class_initialized_shared`.
    CLASS_INIT_IN_PROGRESS,
    CLASS_INIT_UNINITIALIZED,
};
pub use class_path::{ClassPath, ManifestInfo};
// Round 5 audit fix (MED #10) / Round 7 audit fix (MED #11): expose the
// reflective `(class, name, descriptor)` cache so VM-side reflective
// resolvers (`Class.getMethod`, `MethodHandles.Lookup.findVirtual`,
// JNI `GetMethodID`/`GetFieldID`, Spring's `ReflectionUtils.findMethod`)
// can dedupe their per-call hierarchy walk. Invalidated in lockstep
// with `ResolutionCache::invalidate_class` from the JVMTI
// `RedefineClasses` path.
pub use resolution::{LinkResolver, ResolvedMember};
// Verification-derived per-method oop maps (see `type_maps`). Produced by the
// same walk that verifies, on the default build path — no feature gate, no
// env var. Re-exported here because the eventual consumers (GC root scan,
// interpreter fast path, JIT) live outside this crate.
// (`store_heap_bytes` / `store_class_count` are deliberately NOT re-exported
// at the crate root — their names are too generic there; reach them as
// `type_maps::store_heap_bytes`.)
pub use type_maps::{
    class_type_maps, mark_class_verification_skipped, publish_class_type_maps,
    replace_class_type_maps, type_maps_for, type_maps_for_named, verification_status,
    ClassTypeMaps, CompactBitmapArray, FastPathVeto, FrameOopMap, LocalOopBits, MethodTypeMaps,
    MethodTypeMapsBuilder, OopBits, SetBitIter, StackOopBits, VerificationStatus,
};
// Round 5 audit fix (LOW #11) / Round 7 carry-over: pre-flattened
// built-in loader delegation chain. Used by VM-side callers that
// want to walk parent-delegation without chasing trait-object
// parent pointers (the slice walk is cache-friendly and avoids
// 3 vtable dispatches per probe).
pub use loaders::{BUILTIN_LOADER_DELEGATION_CHAIN, MAX_BUILTIN_LOADER_DEPTH};
// 2026-07-30: user-defined loaders finally have their delegation parents
// modelled Rust-side, so resolution against already-defined classes walks
// them instead of falling through to the loader-blind global path.
pub use loaders::{
    dbg_loader_chain, has_user_loader_parents, loader_parent_chain_enabled,
    register_user_loader_parent, user_loader_ancestors, user_loader_parent_known,
    MAX_USER_LOADER_DEPTH,
};
pub use module::{
    descriptor_from_module_attribute, package_of, ModuleDescriptor, ModuleRegistry, JAVA_BASE,
    UNNAMED_MODULE,
};
