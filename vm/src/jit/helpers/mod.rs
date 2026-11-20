// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//!
//! Decomposed from `vm/src/jit/helpers.rs` into modular runtime components:
//! - `thread.rs`: JIT thread state, stack guards, mic_prof, disp_census, callee guards.
//! - `alloc.rs`: Object, array, multi-array allocations, TLAB bump-pointer, ZGC tracking.
//! - `access.rs`: Field access, static field access, array loads/stores, write barriers.
//! - `typecheck.rs`: Checkcast, instanceof, site refusals, exception throwing & handling.
//! - `dispatch.rs`: Invoke dispatch, monomorphic inline caching, indy bridge, lambda bridges.
//! - `fast_native.rs`: Direct fast-native implementations (arraycopy, VarHandle).
//! - `deopt.rs`: Callee deoptimization, not-entrant stubs, uncommon traps.
//! - `table.rs`: JIT runtime helper table construction and miscellaneous entry points.

#[allow(unused_imports)]
pub(in crate::jit::helpers) use std::cell::Cell;

pub(in crate::jit::helpers) use cratonvm_jit::{DescriptorParamIter, JitInvokeInfo, JitMICSlot, JitPICSlot, JitRuntimeHelpers};
pub(in crate::jit::helpers) use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
pub(in crate::jit::helpers) use cratonvm_types::{
    ArrayElementType, ClassId, ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};

pub(in crate::jit::helpers) use crate::jit::helper_guard::{contain, OnPanic};
pub(in crate::jit::helpers) use crate::memory::vm_heap::VmHeap;
// `class_was_redefined` is this file's memoized wrapper of the
// `redefine_state` function of that name (round 12 wave 2, lane rt).
pub(in crate::jit::helpers) use crate::runtime::redefine_state::{class_id_or_name_was_redefined, named_class_was_redefined};
pub(in crate::jit::helpers) use crate::threading::jvm_thread::JvmThread;
pub(in crate::jit::helpers) use crate::threading::thread_state::{self, ThreadExecState};
pub(in crate::jit::helpers) use crate::vm::SharedVm;
pub(in crate::jit::helpers) use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};
// ---------------------------------------------------------------------------
// WS1 diagnostic profiling for the JIT dispatch helpers
// (env-gated: CRATONVM_DBG_MIC_PROF=1; zero-cost when off beyond one cached
// bool branch). Dumps cumulative path counts + rdtsc cycle totals to stderr
// every 2^24 `jit_invoke_virtual_mic` entries, so the last dump before exit
// approximates the run's totals.
// ---------------------------------------------------------------------------


#[macro_use]
mod thread;
#[allow(unused_imports)]
pub use thread::*;
#[macro_use]
mod alloc;
#[allow(unused_imports)]
pub use alloc::*;
#[macro_use]
mod access;
#[allow(unused_imports)]
pub use access::*;
#[macro_use]
mod typecheck;
#[allow(unused_imports)]
pub use typecheck::*;
#[macro_use]
mod dispatch;
#[allow(unused_imports)]
pub use dispatch::*;
#[macro_use]
mod fast_native;
#[allow(unused_imports)]
pub use fast_native::*;
#[macro_use]
mod deopt;
#[allow(unused_imports)]
pub use deopt::*;
#[macro_use]
mod table;
#[allow(unused_imports)]
pub use table::*;

// j27 wave 7 (lane T1LAM3): a trap out of a stale body makes that body not
// entrant (`CRATONVM_JIT_STALE_TRAP_NOT_ENTRANT`, opt-in); and the one way the
// VM takes the stash's trapping body (`taken_trapping_body_retained`).
pub(crate) mod stale_trap_not_entrant;

/// Full concatenated source code of all JIT runtime helper components, preserving
/// the exact source layout of the original `vm/src/jit/helpers.rs` monolith for
/// source-pinning tests across the workspace.
pub const HELPERS_SRC: &str = concat!(
    concat!(include_str!("thread/mod.rs"), "\n", include_str!("thread/direct_static_compiled_callee_entry.rs"), "\n", include_str!("thread/class_was_redefined.rs"), "\n", include_str!("thread/jit_service_callee_deopt_body.rs"), "\n", include_str!("thread/decode_dispatch_values_into_shaped.rs")),
    include_str!("alloc.rs"),
    include_str!("access.rs"),
    include_str!("typecheck.rs"),
    concat!(include_str!("dispatch/mod.rs"), "\n", include_str!("dispatch/mh_constant_bind_enabled.rs"), "\n", include_str!("dispatch/mh_constant_bind_fill.rs"), "\n", include_str!("dispatch/mh_constant_filter_compile_fill.rs"), "\n", include_str!("dispatch/jit_invoke_dispatch_body.rs"), "\n", include_str!("dispatch/try_jit_site_cached_native_dispatch.rs"), "\n", include_str!("dispatch/static_dispatch_counter_compiles.rs")),
    concat!(include_str!("fast_native/mod.rs"), "\n", include_str!("fast_native/varhandle_read_direct_counts.rs"), "\n", include_str!("fast_native/jit_integer_value_of_direct_body_on.rs"), "\n", include_str!("fast_native/jit_scoped_memory_put_int_direct.rs"), "\n", include_str!("fast_native/nio_charset_ir_layout_words.rs"), "\n", include_str!("fast_native/jit_concurrent_hashmap_get_direct.rs"), "\n", include_str!("fast_native/note_ic_profile_seed.rs")),
    include_str!("deopt.rs"),
    include_str!("table.rs"),
);

/// The split helper files, relative to `vm/src/jit/helpers/`, in exactly the
/// order [`HELPERS_SRC`] concatenates them. Keep the two lists in step.
#[cfg(test)]
pub(crate) const HELPERS_PARTS: &[&str] = &[
    "thread/mod.rs", "thread/direct_static_compiled_callee_entry.rs", "thread/class_was_redefined.rs", "thread/jit_service_callee_deopt_body.rs", "thread/decode_dispatch_values_into_shaped.rs",
    "alloc.rs", "access.rs", "typecheck.rs",
    "dispatch/mod.rs", "dispatch/mh_constant_bind_enabled.rs", "dispatch/mh_constant_bind_fill.rs", "dispatch/mh_constant_filter_compile_fill.rs", "dispatch/jit_invoke_dispatch_body.rs", "dispatch/try_jit_site_cached_native_dispatch.rs", "dispatch/static_dispatch_counter_compiles.rs",
    "fast_native/mod.rs", "fast_native/varhandle_read_direct_counts.rs", "fast_native/jit_integer_value_of_direct_body_on.rs", "fast_native/jit_scoped_memory_put_int_direct.rs", "fast_native/nio_charset_ir_layout_words.rs", "fast_native/jit_concurrent_hashmap_get_direct.rs", "fast_native/note_ic_profile_seed.rs",
    "deopt.rs", "table.rs",
];

/// Working-tree read of the helper sources: the same text as [`HELPERS_SRC`],
/// but read from disk at run time rather than baked in at compile time.
///
/// For the source witnesses that read `vm/src/jit/helpers.rs` at run time on
/// purpose (`source-witness-tests-read-the-working-tree`: an unbuilt edit must
/// still fail them). `CARGO_MANIFEST_DIR` is `<repo>/vm`.
#[cfg(test)]
pub(crate) fn read_helpers_src_from_disk() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/jit/helpers");
    let mut out = String::new();
    for part in HELPERS_PARTS {
        let path = dir.join(part);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        out.push_str(&text);
    }
    out
}

#[cfg(test)]
mod compact_field_proof_tests;
#[cfg(test)]
mod t1y3_direct_helper_tally_tests;
#[cfg(test)]
mod jit_counter_block_tests;
#[cfg(test)]
mod j27_boot_final_ref_tests;
#[cfg(test)]
mod r23_l9_display_at_init_start_tests;
#[cfg(test)]
mod tc21_ensure_display_tests;
#[cfg(test)]
mod r21_hash_identity_class_tests;
#[cfg(test)]
mod jr21_hash2_identity_class_table_tests;
#[cfg(test)]
mod jr21_tcarray_array_typecheck_tests;
#[cfg(test)]
mod jr21_refs2_referent_query_tests;
#[cfg(test)]
mod hl21_identity_verdict_backfill_tests;
#[cfg(test)]
mod jr21_hashlock_identity_equals_table_tests;
#[cfg(test)]
mod vhc21_reference_field_plan_veto_tests;
#[cfg(test)]
mod gcd_d2j_jit_alloc_oom_tests;
#[cfg(test)]
mod gcd_d1b_overhead_verdict_tests;
#[cfg(test)]
mod gch_h1fr_census_door_tests;
#[cfg(test)]
mod gcf_f1o_census_frames_tests;
#[cfg(test)]
mod zt11ca3_split_post_init_fields_tests;
#[cfg(test)]
mod r11w18_hashmap_new_cp_site_memo_tests;
#[cfg(test)]
mod r13w10_sync6_class_mirror_slot_tests;
#[cfg(test)]
mod r13w11_misc11_ldc_slot_teardown_tests;
#[cfg(test)]
mod deopt_tests;
#[cfg(test)]
mod r23_w6_self_call_budget_tests;
#[cfg(test)]
mod r12w3_hfix_self_call_floor_margin_tests;
#[cfg(test)]
mod decode_receiver_routes;
#[cfg(test)]
mod jit_native_dispatch_profile;
#[cfg(test)]
mod jit_monitor_jmx_pairing;
#[cfg(test)]
mod g12_compiled_aastore_asks_the_shared_predicate;
#[cfg(test)]
mod direct_helper_gate_witness;
#[cfg(test)]
mod direct_bind_source_witness;
#[cfg(test)]
mod varhandle_read_direct_helper_tables;
#[cfg(test)]
mod r23w4_ffm_unaligned_offset_coord_tests;
#[cfg(test)]
mod r23w3_ffm_element_site_kind_index_tests;
#[cfg(test)]
mod ffm_layout_kind_tests;
#[cfg(test)]
mod unsafe_inline_admitted_mask_tests;
#[cfg(test)]
mod nio21_w8_direct_helper_gate_sites_tests;
#[cfg(test)]
mod direct_helper_table_tests;
#[cfg(test)]
mod r9w3_vmhelpers3_negative_typecheck_memo;
#[cfg(test)]
mod r10_help2_positive_typecheck_memo_epoch;
#[cfg(test)]
mod w21_l5_per_vm_class_epoch_memos;
#[cfg(test)]
mod r9w3_vmhelpers3_ldc_slot_memo;
#[cfg(test)]
mod r9w3_vmhelpers3_post_alloc_init_skip;
#[cfg(test)]
mod r9w10_exc10_division_guard_retier;
#[cfg(test)]
mod oomjit5_compiled_catch_tests;
#[cfg(test)]
mod w36f_native_return_handoff_tests;
#[cfg(test)]
mod r11_helper_entry_native_return_tests;
#[cfg(test)]
mod r11w4_rt_compile_arg_pins;
#[cfg(test)]
mod i1w14_l2_substituted_owner_tests;
#[cfg(test)]
mod i17_l1_polling_body_id_tests;
#[cfg(test)]
mod i24_l6_unpublished_osr_body_tests;
#[cfg(test)]
mod i18_l4_redefinition_memo_tests;
#[cfg(test)]
mod i20_l1_cp_site_index_tests;
#[cfg(test)]
mod w36a_md_class_verdict_tests;
#[cfg(test)]
mod r12_rt_quiesce_verdict_memo_tests;
#[cfg(test)]
mod r12w2_osrfix_lambda_get_service_tests;
#[cfg(test)]
mod i43_l2_cp_site_tests;
#[cfg(test)]
mod r16_mh_constant_bind_tests;
#[cfg(test)]
mod r16w12_throw3_callee_verdict_identity;
#[cfg(test)]
mod gcj3rt3_mh_site_identity_tests;
#[cfg(test)]
mod byte_array_view_read_abi_tests;
#[cfg(test)]
mod r18_dispatch_null_receiver_witness;
#[cfg(test)]
mod nio20_getclass_mirror_words_tests;
#[cfg(test)]
mod ffm_direct_door_tests;
#[cfg(test)]
mod r19_throws19_local_handler_lean_tests;
#[cfg(test)]
mod lambda_cast_thunk_install_tests;
#[cfg(test)]
mod t1w12_refs4_referent_row_query_tests;
#[cfg(test)]
mod nio21w19_thread_local_ir_layout_tests;
#[cfg(test)]
mod i81_l3_reflect_door_direct_tests;
#[cfg(test)]
mod infallible_direct_native_tests;
#[cfg(test)]
mod r21_reflect_accessor_link_tests;
#[cfg(test)]
mod r21_indy_clinit_barrier_answer_tests;
#[cfg(test)]
mod r21_intrinsics_vectorized_hash_unroll_tests;
#[cfg(test)]
mod r21_compat_thin_helper_asks_the_resolver;
#[cfg(test)]
mod r22_inline_mh_bind_customization_tests;
#[cfg(test)]
mod r21_stress_poll_arrival_tests;
#[cfg(test)]
mod upd21_ref_get_and_set_admission_tests;
#[cfg(test)]
mod rt22_helper_table_pin_tests;
#[cfg(test)]
mod gen_owed_poll_table_tests;
#[cfg(test)]
mod j27_typecheck_census_tests;
#[cfg(test)]
mod r21w26_hiddenunload_typecheck_site_tests;
#[cfg(test)]
mod r13w7_predicates_proxy_shim_typecheck_tests;
#[cfg(test)]
mod i11_l2_unbound_typecheck_raises;
#[cfg(test)]
mod r19_mega6_native_door_tests;
#[cfg(test)]
mod zt29_jd_native_site_owner_tests;

#[cfg(all(test, feature = "zgc"))]
mod zgc_store_plan_tests;
