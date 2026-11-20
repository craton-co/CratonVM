#[allow(unused_imports)]
pub(super) use super::*;
#[cfg(test)]
mod i116_l2_by_id_body_tests;
#[cfg(test)]
mod r20_prefl_mh_compiled_link_tests;
#[cfg(test)]
mod r11w11_superseded_body_memo_tests;
#[cfg(test)]
mod r9w4_vm4_collection_leaf_kind_tests;
#[cfg(test)]
mod i79_l3_rebase_self_call_floor_tests;

#[cfg(test)]
mod r13w2_mega_suspended_floor_tests;

#[cfg(test)]
mod r13w4_mega3_floor_word_tests;

#[cfg(test)]
mod r16w17_mega5_blind_late_probe_tests;
#[cfg(test)]
mod r12w3_hfix_dispatch_stack_overflow_tests;

#[cfg(test)]
mod r12w4_exc2_helper_failure_tests;
#[cfg(all(test, feature = "gpu-offload"))]
mod compiled_offload_site_tests;
#[cfg(test)]
mod t1w2_site_access_guard_tests;
#[cfg(test)]
mod r16w8_sync_special_door_tests;
/// Round 15 wave 4 (lane walk2): the site cache's owner test.
#[cfg(test)]
mod r15w4_walk2_site_owner_tests;
#[cfg(test)]
mod r19_funnel3_lean_table_tests;
#[cfg(test)]
mod r23_w8_reflect_new_array_hint_kind_tests;
/// JIT r23 wave 5, lane L5: the one-validation arraycopy door answers (and
/// declines) exactly as the two self-validating attempts it replaces.
#[cfg(test)]
mod r23_w5_arraycopy_one_validation_tests;

#[cfg(test)]
mod r23_w4_reflect_new_array_direct_tests;
#[cfg(test)]
mod r25_varhandle_array_element_fast_switch_tests;
// ---------------------------------------------------------------------------
// `VarHandle` read-mode thin direct-call helpers.
//
// The JIT half is `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS` and the two
// recognitions that read it (`jit::try_compile`'s single-pass ladder and
// `jit_bridge::compile_osr_artifact`). This half is the callee: a

#[cfg(test)]
mod jr21_reflect2_lean_link_tests;

/// JIT round 21 lane reflect4 (REFLECT3-21-2, `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`):
/// the hidden-owner static leaf of a compiled `invokeBasic` / `linkToStatic`
/// link is resolved by its class id, and no other site is.
#[cfg(test)]
mod jr21_reflect4_hidden_leaf_tests;

/// JIT round 21 lane reflect4 (REFLECT4-21-1 / REFLECT4-21-3): every census
/// row a site can count names a reason, and the served rows are followed by
/// their ladder twins.
#[cfg(test)]
mod jr21_reflect4_census_tests;

#[cfg(test)]
mod jr21_reflect3_agree_layout_tests;
#[cfg(test)]
mod j27_virtual_dispatch_counter_probe_only_tests;

#[cfg(test)]
mod j27_dispatch_counter_via_manager_tests;

#[cfg(test)]
mod j27_mh_real_converting_tests;

/// j27 wave 6 (lane T1LAM2; `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`):
/// the cold fill records a bound STATIC compile row (`invoke_kind 6`) only for
/// an unconverted real bound handle of a static member.
#[cfg(test)]
mod j27_t1lam2_bound_static_compile_row_tests;

pub(super) mod mh_constant_bind_enabled;
#[allow(unused_imports)]
pub use mh_constant_bind_enabled::*;
#[allow(unused_imports)]
use mh_constant_bind_enabled::*;
mod mh_constant_filter_compile_fill;
#[allow(unused_imports)]
pub use mh_constant_filter_compile_fill::*;
#[allow(unused_imports)]
use mh_constant_filter_compile_fill::*;
pub(super) mod jit_invoke_dispatch_body;
#[allow(unused_imports)]
pub use jit_invoke_dispatch_body::*;
#[allow(unused_imports)]
use jit_invoke_dispatch_body::*;
mod try_jit_site_cached_native_dispatch;
#[allow(unused_imports)]
pub use try_jit_site_cached_native_dispatch::*;
#[allow(unused_imports)]
use try_jit_site_cached_native_dispatch::*;
mod static_dispatch_counter_compiles;
#[allow(unused_imports)]
pub use static_dispatch_counter_compiles::*;
#[allow(unused_imports)]
use static_dispatch_counter_compiles::*;

mod mh_constant_bind_fill;
#[allow(unused_imports)]
pub use mh_constant_bind_fill::*;
#[allow(unused_imports)]
use mh_constant_bind_fill::*;
