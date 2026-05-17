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
pub mod loaders;
pub mod module;
pub mod proxy_gen;
pub mod resolution;
pub mod verifier;
pub mod verify_frame;
pub mod verify_insn;
pub mod vtype;

pub use class::{
    find_field_recursive, find_method_recursive, ArrayInfo, Class, ClassId, ClassLoaderId,
    ClassState, ClassStore, CodeSource, RecordComponentInfo,
};
pub use class_manager::{
    install_class_file_load_hook, install_class_load_hook, install_class_prepare_hook,
    install_jit_invalidate_hook, install_resolution_invalidate_hook,
    install_vtable_install_hook, install_vtable_override_hook,
    is_builtin_classloader_name, jdk_superclass_lookup, register_builtin_classloaders,
    ClassFileLoadHook, ClassManager, DefineClassOptions, JitInvalidateHook, JvmtiClassHook,
    RedefineOptions, ResolutionInvalidateHook, VtableInstallHook, VtableMethodSnapshot,
    VtableOverrideHook, VtableSlotDescriptor,
};
pub use class_path::{ClassPath, ManifestInfo};
pub use module::{
    descriptor_from_module_attribute, package_of, ModuleDescriptor, ModuleRegistry,
    JAVA_BASE, UNNAMED_MODULE,
};
