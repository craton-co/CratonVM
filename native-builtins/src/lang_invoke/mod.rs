// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! MethodHandle, MethodType, Lookup, CallSite, and VarHandle native method implementations.
//!
//! Session 4: Full MethodHandle invocation with type resolution, VarHandle field
//! access with real get/set/CAS, and proper Lookup.find* resolution.

use super::*;

pub(crate) use std::borrow::Cow;
pub(crate) use std::sync::Arc;

pub(crate) use cratonvm_native_api::{
    MhWmteRaise, NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
pub(crate) use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
pub(crate) use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

pub(crate) use crate::lang_class::{box_value_canonical, mirror_class_id, mirror_class_name, ClassTextName};
pub(crate) use crate::{obj_arg, try_alloc_concurrent_synthetic};

pub(crate) use member_name::{
    VH_CLASS, VH_CLASS_ID, VH_FIELD, VH_FIELD_COUNT, VH_FIELD_DESC, VH_FIELD_INDEX, VH_KIND,
    VH_KIND_ARRAY, VH_KIND_INSTANCE, VH_KIND_STATIC,
};

pub mod member_name;
pub mod call_site;
pub mod direct_dispatch;
pub mod var_handles;
pub mod adapters;
pub mod combinators;
pub mod astype_bind;

pub use member_name::*;
pub use call_site::*;
pub use direct_dispatch::*;
pub use var_handles::*;
pub use adapters::*;
pub use combinators::*;
pub use astype_bind::*;

pub(crate) use member_name::*;
pub(crate) use call_site::*;
pub(crate) use direct_dispatch::*;
pub(crate) use var_handles::*;
pub(crate) use adapters::*;
pub(crate) use combinators::*;
pub(crate) use astype_bind::*;

#[cfg(test)]
pub(crate) const LANG_INVOKE_SRC: &str = concat!(
    include_str!("member_name.rs"),
    include_str!("call_site.rs"),
    include_str!("direct_dispatch.rs"),
    include_str!("var_handles.rs"),
    include_str!("adapters.rs"),
    include_str!("combinators.rs"),
);

#[cfg(test)]
mod tests;
