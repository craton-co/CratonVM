// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! MethodHandle, MethodType, Lookup, CallSite, and VarHandle native method implementations.
//!
//! Session 4: Full MethodHandle invocation with type resolution, VarHandle field
//! access with real get/set/CAS, and proper Lookup.find* resolution.

use super::*;

pub use std::borrow::Cow;
pub use std::sync::Arc;

pub use cratonvm_native_api::{
    MhWmteRaise, NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
pub use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
pub use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

pub use crate::lang_class::{
    box_value_canonical, mirror_class_id, mirror_class_name, ClassTextName,
};
pub use crate::lib_parts::arrays_range_or_throw::obj_arg;
pub use crate::util_concurrent_ext::try_alloc_concurrent_synthetic;

pub use member_name::{
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

pub use member_name::*;
pub use call_site::*;
pub use direct_dispatch::*;
pub use var_handles::*;
pub use adapters::*;
pub use combinators::*;
pub use astype_bind::*;

#[cfg(test)]
pub const LANG_INVOKE_SRC: &str = concat!(
    concat!(include_str!("member_name/mod.rs"), "\n", include_str!("member_name/vh_array_call.rs"), "\n", include_str!("member_name/mh_foreign_door_type.rs"), "\n", include_str!("member_name/mh_invoke_exact_type_refusal.rs"), "\n", include_str!("member_name/vh_indirect_mode_handle.rs")),
    include_str!("call_site.rs"),
    include_str!("direct_dispatch.rs"),
    concat!(include_str!("var_handles/mod.rs"), "\n", include_str!("var_handles/native_record_support.rs"), "\n", include_str!("var_handles/invoke_reference_cast_refusal.rs"), "\n", include_str!("var_handles/mh_real_field_converting_serve.rs")),
    concat!(include_str!("adapters/mod.rs"), "\n", include_str!("adapters/mh_direct_lane_enabled.rs"), "\n", include_str!("adapters/mh_bindto_prelink_bound_field.rs"), "\n", include_str!("adapters/method_finder_mint_file.rs"), "\n", include_str!("adapters/mh_invoke_with_arguments_list_door.rs")),
    include_str!("combinators.rs"),
);

#[cfg(test)]
mod tests;
