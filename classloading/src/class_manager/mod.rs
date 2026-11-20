// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Class manager — loading, parsing, and caching of Java classes.
//!
//! The `ClassManager` is the central component that:
//! 1. Locates `.class` files via the parent-delegation model (bootstrap → extension → application)
//! 2. Parses them via `cratonvm_reader::read_class`
//! 3. Recursively loads superclasses and interfaces
//! 4. Stores them in a `ClassStore` indexed by `ClassId`
//! 5. Provides lookup by name and id
//!
//! **Synthetic bootstrap:** `ClassManager::ensure_synthetic_class` registers minimal classes with
//! `crate::class::Class::is_synthetic_stub` set — a legacy shortcut when no real classfile exists.
//! Project policy is to prefer real JDK/app `.class` bytes and shrink synthetic paths over time; see
//! `docs/feature-designs/native-dispatch-architecture.md`.

// T10.9.E: removed `use std::collections::{HashMap, HashSet};` — every
// internal map/set in this file now uses `FxHashMap` / `FxHashSet`
// (faster non-cryptographic hash, safe because keys are trusted internal
// data: ClassId, interned class names, etc.).
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_native_api::VmId;
use cratonvm_reader::attribute::{force_decode_all, Attribute};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::field::ClassFileField;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ThinArcStr;
use tracing::debug;

use crate::class::{
    ArrayInfo, Class, ClassId, ClassLoaderId, ClassState, ClassStore, CodeSource,
    EnclosingMethodInfo, InnerClassEntry, RecordComponentInfo,
};
use crate::class_origin::{ClassOrigin, ClassOriginEntry};
use crate::class_path::ClassPath;
use crate::loader_flags;
use crate::loaders::{
    ApplicationClassFinder, BootstrapClassFinder, ClassFinder, ExtensionClassFinder,
    BUILTIN_LOADER_DELEGATION_CHAIN,
};
use crate::metadata_handle::{
    ClassHandle, FieldHandle, MetadataKind, MetadataRealm, MethodHandle, StaleMetadata,
};
use crate::module::{
    descriptor_from_module_attribute, is_platform_module_name, package_of,
    packages_from_module_packages_attribute, ModuleRegistry,
};
use crate::redefinition_refusal::{RedefinitionRefusal, RedefinitionRefused};
use crate::vtype::ClassHierarchy;
use cratonvm_reader::SharedBytes;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::{
    ClassFileError, JdkOnlyViolation, LinkageError, RuntimeError, VmError,
};

/// Return a class only when exactly one loader has defined `name`.
///
/// This is the fail-closed operation for genuinely context-free diagnostic or
/// metadata paths. It must never be used where an initiating class/loader is
/// available: ambiguity is information loss, not permission to choose the
/// first map entry.
/// Kept as the executable specification of what `ClassManager::
/// name_definitions` must answer: `unique_definition_index_matches_linear_scan`
/// asserts the O(1) index agrees with this scan. Production reads the index.
#[cfg(test)]
fn unique_loaded_class(map: &LoadedClassesMap, name: &str) -> Option<ClassId> {
    let mut found = None;
    for ((_, stored_name), &id) in map {
        if stored_name.as_ref() != name {
            continue;
        }
        if found.is_some_and(|prior| prior != id) {
            return None;
        }
        found = Some(id);
    }
    found
}

#[cfg(test)]
mod loader_lookup_tests;
#[cfg(test)]
mod i106_l5_class_name_tests;
#[cfg(test)]
mod r23w10_method_repeated_attribute_tests;
#[cfg(test)]
mod i109_l5_member_name_and_signature_tests;

#[cfg(test)]
mod i84_l5_modifier_refusal_tests;
#[cfg(test)]
mod native_constant_surface_raw_slot_layout_audit_tests;
/// Interpreter round i1 wave 45, lane L3: the class attributes a
/// redefinition may not change, compared as HotSpot 25 compares them
/// (`L3W45RedefineShapes.java`).
#[cfg(test)]
mod i45_l3_redefinition_nesting_tests;

#[cfg(test)]
mod w23_l5_bootstrap_appended_tests;
#[cfg(test)]
mod r9w5_typecheck5_duplicate_name_latch_tests;

#[cfg(test)]
mod w20_l2_store_epoch_tests;

#[cfg(test)]
mod w20_l2_redefinition_ring_tests;

/// zs s4/rp: class and `InnerClasses` flags are stored as HotSpot's
/// `ClassFileParser` stores them (unrecognized bits dropped).
#[cfg(test)]
mod zs4_rp_stored_class_flags_tests;

/// zs s6/rv4: the redefinition's class-modifier comparison reads the new
/// bytes the way `define` stored the old ones (`hotspot_stored_class_flags`).
#[cfg(test)]
mod zs6_rv4_redefinition_class_flags_tests;

#[cfg(test)]
mod zt16df6_unbound_define_tests;


mod ambiguous_stand_in_refused;
#[allow(unused_imports)]
pub use ambiguous_stand_in_refused::*;
#[allow(unused_imports)]
use ambiguous_stand_in_refused::*;
mod class_init_bits;
#[allow(unused_imports)]
pub use class_init_bits::*;
#[allow(unused_imports)]
use class_init_bits::*;
mod with_app_fat_jar_emulation;
#[allow(unused_imports)]
pub use with_app_fat_jar_emulation::*;
#[allow(unused_imports)]
use with_app_fat_jar_emulation::*;
mod define_class_shared_parsed;
#[allow(unused_imports)]
pub use define_class_shared_parsed::*;
#[allow(unused_imports)]
use define_class_shared_parsed::*;
mod jdk_superclass_lookup;
#[allow(unused_imports)]
pub use jdk_superclass_lookup::*;
#[allow(unused_imports)]
use jdk_superclass_lookup::*;
mod jdk_superclass;
#[allow(unused_imports)]
pub use jdk_superclass::*;
#[allow(unused_imports)]
use jdk_superclass::*;
mod synthetic_stub_fields;
#[allow(unused_imports)]
pub use synthetic_stub_fields::*;
#[allow(unused_imports)]
use synthetic_stub_fields::*;
mod synthetic_stub_ctor_methods;
#[allow(unused_imports)]
pub use synthetic_stub_ctor_methods::*;
#[allow(unused_imports)]
use synthetic_stub_ctor_methods::*;
