// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

use std::borrow::Cow;
use std::sync::Arc;

use cratonvm_native_api::{
    MhWmteRaise, NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
use cratonvm_types::error::{
    LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError,
};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

// `box_value` is deliberately NOT imported: after the split described below,
// every unqualified boxing call in this file is the canonical one, and the
// four sites that must stay fresh spell `crate::lang_class::box_value` in
// full so the exception is visible rather than inferred from the argument.
use crate::lang_class::{box_value_canonical, mirror_class_id, mirror_class_name, ClassTextName};
use crate::{obj_arg, try_alloc_concurrent_synthetic};
#[cfg(test)]
mod r15w4_vh6_cast_message_tests;

/// Round 15 wave 4 (lane vh5; VH4-1): the array coordinate is cast to the
/// handle's array type. Probe twin: `R15Vh4ArrayCoordCast` (`longArr`,
/// `refArrSet`, `bvIntArr`), `R15Vh5CoordCastModes`.
#[cfg(test)]
mod r15w4_vh5_array_coordinate_tests;

/// Round 15 wave 3 (lane vh4; VH3-2,
/// `r15w3-vh3-minted-handle-index-coordinate-kinds-FIXED-20260930.md`): the index
/// coordinate of the VM's array and byte-view handles is converted by the
/// call site's static type, and the byte views' values are adapted. Probe
/// twins: `R15Vh3MintedCoordKinds`, `R15Vh4ViewValueKinds`.
#[cfg(test)]
mod r15w3_vh4_index_coordinate_tests;
#[cfg(test)]
mod i57_w123_real_varform_tests;
#[cfg(test)]
mod j27_static_slot_memo_tests;

#[cfg(test)]
mod j27_vhcomp_static_note_tests;
#[cfg(test)]
mod r15w2_vh2_reference_return_tests;
#[cfg(test)]
mod r23w6_segment_vh_mode_tests;
#[cfg(test)]
mod r12w7_ffm4_segment_vh_tests;
/// The access natives' first step: `Some(answer)` when `args[0]` is an
/// `IndirectVarHandle`, `None` for every other handle. `method` is the access
/// mode's method name (`get`, `compareAndSet`, ...), as each native passes it.
///
/// Placed before the arity, read-only, null-coordinate and value-adaptation
/// checks: those judge the handle by this file's shapes, and an indirect
/// handle's own coordinates are the adapter's to check.
///
/// Cost for every other handle: the side-table memo the native reads anyway,
/// and a class-name compare only for a handle with no row.
///
/// With no parameter-site word (nothing armed). Since round 15 wave 3 (lane
/// vh3; VH1-3) every access-mode native, read modes included, takes the word
/// and calls [`vh_indirect_serve_site`]; this form is the tests' spelling.
#[cfg(test)]
pub fn vh_indirect_serve(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> Option<MethodCallResult> {
    vh_indirect_serve_site(ctx, args, method, 0, &None)
}

#[cfg(test)]
mod r15w1_vh_indirect_tests;

#[cfg(test)]
mod r14w7_ffm7_indirect_vh_tests;
#[cfg(test)]
mod nio21w6_vh_cas_rmw_tests;

#[cfg(test)]
mod mhn_set_call_site_target_tests;

mod vh_array_call;
#[allow(unused_imports)]
pub use vh_array_call::*;
#[allow(unused_imports)]
use vh_array_call::*;
mod mh_foreign_door_type;
#[allow(unused_imports)]
pub use mh_foreign_door_type::*;
#[allow(unused_imports)]
use mh_foreign_door_type::*;
mod mh_invoke_exact_type_refusal;
#[allow(unused_imports)]
pub use mh_invoke_exact_type_refusal::*;
#[allow(unused_imports)]
use mh_invoke_exact_type_refusal::*;
mod vh_indirect_mode_handle;
#[allow(unused_imports)]
pub use vh_indirect_mode_handle::*;
#[allow(unused_imports)]
use vh_indirect_mode_handle::*;
