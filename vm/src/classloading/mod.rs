//! Class loading subsystem — thin re-export shim.
//!
//! The implementation lives in the `rustjvm-classloading` crate.
//! This module re-exports everything so that the rest of the `vm` crate
//! can continue using `crate::classloading::*` paths unchanged.

pub use rustjvm_classloading::access_control;
pub use rustjvm_classloading::bytecode_verifier;
pub use rustjvm_classloading::loaders;
pub use rustjvm_classloading::module;
pub use rustjvm_classloading::resolution;
pub use rustjvm_classloading::verifier;
pub use rustjvm_classloading::verify_frame;
pub use rustjvm_classloading::verify_insn;
pub use rustjvm_classloading::vtype;

pub use rustjvm_classloading::{
    find_field_recursive, find_method_recursive, jdk_superclass_lookup, Class, ClassId,
    ClassLoaderId, ClassManager, ClassPath, ClassState, ClassStore, ManifestInfo, ModuleDescriptor,
    ModuleRegistry, RecordComponentInfo,
};
pub use rustjvm_classloading::{
    descriptor_from_module_attribute, package_of, JAVA_BASE, UNNAMED_MODULE,
};
