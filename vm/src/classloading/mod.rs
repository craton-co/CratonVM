// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class loading subsystem — thin re-export shim.
//!
//! The implementation lives in the `cratonvm-classloading` crate.
//! This module re-exports everything so that the rest of the `vm` crate
//! can continue using `crate::classloading::*` paths unchanged.

pub use cratonvm_classloading::access_control;
pub use cratonvm_classloading::bytecode_verifier;
pub use cratonvm_classloading::loaders;
pub use cratonvm_classloading::module;
pub use cratonvm_classloading::resolution;
pub use cratonvm_classloading::shadow_layout;
pub use cratonvm_classloading::verifier;
pub use cratonvm_classloading::verify_frame;
pub use cratonvm_classloading::verify_insn;
pub use cratonvm_classloading::vtype;

pub use cratonvm_classloading::{
    any_class_redefined, bump_jit_supersede_epoch, class_definition_epoch,
    find_field_recursive, find_field_recursive_by_descriptor, find_method_recursive,
    invokespecial_selection_start, invokevirtual_final_declaring_class,
    invokevirtual_private_declaring_class,
    jdk_superclass_lookup, jit_supersede_epoch,
    static_common_superclass_lookup, Class, ClassId, ClassLoaderId, ClassManager, ClassPath,
    ClassState, ClassStore, ManifestInfo, ModuleDescriptor, ModuleRegistry, RecordComponentInfo,
    RECORD_OBJ_COMPUTED, RECORD_OBJ_EQUALS, RECORD_OBJ_HASH_CODE, RECORD_OBJ_TO_STRING,
};
pub use cratonvm_classloading::{
    descriptor_from_module_attribute, package_of, JAVA_BASE, UNNAMED_MODULE,
};
