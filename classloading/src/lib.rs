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

pub mod access_control;
pub mod annotations;
pub mod builtin_loaders;
pub mod bytecode_verifier;
mod class;
mod class_manager;
mod class_path;
pub(crate) mod fx_hash;
pub mod jar_signer;
pub mod loaders;
pub mod module;
pub mod proxy_gen;
pub mod resolution;
pub mod verifier;
pub mod verify_frame;
pub mod verify_insn;
pub mod vtype;

pub use class::{
    find_field_recursive, find_method_recursive, invokespecial_selection_start, ArrayInfo, Class,
    ClassId, ClassLoaderId, ClassState, ClassStore, CodeSource, RecordComponentInfo,
};
pub use class_manager::is_bootstrap_appended_class;
pub use class_manager::{
    any_class_redefined,
    bump_jit_supersede_epoch,
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
    register_builtin_classloaders,
    static_common_superclass_lookup,
    ClassFileLoadHook,
    ClassManager,
    DefineClassOptions,
    JitInvalidateHook,
    JvmtiClassHook,
    RedefineOptions,
    ResolutionInvalidateHook,
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
// Round 5 audit fix (LOW #11) / Round 7 carry-over: pre-flattened
// built-in loader delegation chain. Used by VM-side callers that
// want to walk parent-delegation without chasing trait-object
// parent pointers (the slice walk is cache-friendly and avoids
// 3 vtable dispatches per probe).
pub use loaders::{BUILTIN_LOADER_DELEGATION_CHAIN, MAX_BUILTIN_LOADER_DEPTH};
pub use module::{
    descriptor_from_module_attribute, package_of, ModuleDescriptor, ModuleRegistry, JAVA_BASE,
    UNNAMED_MODULE,
};
