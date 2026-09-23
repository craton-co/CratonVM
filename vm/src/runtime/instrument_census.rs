// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The workspace's diagnostic counters that no other report reads.
//!
//! # What this is
//!
//! Round 10 took a census of every atomic-backed instrument in this workspace
//! and found 112 whose only references were inside the file that defines them:
//! numbers that a shipped run computes and nobody can see. The register is
//! `docs/internal/retired/r10-diagread-orphan-instrument-debt-register-20260921-RETIRED-20260922.md`
//! and the gate that stops it growing is
//! `scripts/check-orphan-instruments.sh`.
//!
//! The one sentence that makes it a defect rather than untidiness: **zero is
//! indistinguishable from "this never happened".** Every other kind of bug
//! here produces a wrong answer, and a wrong answer has a test. An instrument
//! with no wiring produces a correct-looking answer forever, and the person
//! reading it concludes the hazard it watches has not occurred.
//!
//! This module is the "wire it" half of the register's disposition, for every
//! instrument that survived the review. The other halves are recorded where
//! they happened: an instrument with no caller and no question left to answer
//! was deleted, with a comment in its place saying which question it was for
//! and where that question is answered now; an instrument whose only caller was
//! in its own file was narrowed to `pub(crate)`, because that is over-broad
//! visibility rather than a missing reader.
//!
//! # Why it is opt-in
//!
//! The register's own objection to bulk-wiring was that "a 112-line diagnostic
//! wall that nobody reads is the same defect with more characters". That is an
//! objection about the DEFAULT run. So this report has its own switch, is mixed
//! into no other report, names the subsystem beside every number, and prints
//! every row **including the zeros** — a zero a reader can reach is the whole
//! point, and a report that hides its zeros cannot distinguish "not wired" from
//! "did not happen".
//!
//! ```text
//! CRATONVM_INSTRUMENT_CENSUS=1 cratonvm -cp . Main
//! ```
//!
//! # What it does not prove
//!
//! That a number is printed does not make it correct, and it does not make its
//! FEEDER exist: the gate this serves proves "no textual reader", never "no
//! reader", and never "the thing it watches happened". A row that is zero on a
//! workload that certainly exercises its subsystem is the next thing to
//! investigate, not a clean bill of health.

/// Every instrument in this workspace that no other report reads, as
/// `(qualified name, rendered value)`.
///
/// Ordered by subsystem so the report groups without sorting, and named with
/// the instrument's own function name so `rg <name>` lands on the definition
/// and on this read.
#[must_use]
pub fn census() -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::with_capacity(64);

    // ── types ────────────────────────────────────────────────────────────
    let (alloc_a, alloc_b, alloc_c) = cratonvm_types::gpu_event_census::alloc_totals();
    out.push((
        "types.gpu_event_census.alloc_totals",
        format!("{alloc_a}/{alloc_b}/{alloc_c}"),
    ));
    out.push((
        "types.cell_census.reported",
        cratonvm_types::cell_census::reported().to_string(),
    ));
    out.push((
        "types.foreign_layout_refusals",
        cratonvm_types::field_layout::foreign_layout_refusals().to_string(),
    ));
    out.push((
        "types.narrow_oop.out_of_range_hits",
        cratonvm_types::narrow_oop::out_of_range_hits().to_string(),
    ));

    // ── reader ───────────────────────────────────────────────────────────
    let (q1, q2, q3, q4, q5) = cratonvm_reader::quicken_stats();
    out.push(("reader.quicken_stats", format!("{q1}/{q2}/{q3}/{q4}/{q5}")));

    // ── classloading ─────────────────────────────────────────────────────
    let (pe1, pe2, pe3) = cratonvm_classloading::proxy_gen::proxy_emit_counts();
    out.push((
        "classloading.proxy_emit_counts",
        format!("{pe1}/{pe2}/{pe3}"),
    ));
    out.push((
        "classloading.store_class_count",
        cratonvm_classloading::type_maps::store_class_count().to_string(),
    ));
    out.push((
        "classloading.store_heap_bytes",
        cratonvm_classloading::type_maps::store_heap_bytes().to_string(),
    ));

    // ── gc ───────────────────────────────────────────────────────────────
    out.push((
        "gc.small_allocations_in_large_region",
        cratonvm_gc::arena::small_allocations_in_large_region().to_string(),
    ));
    out.push((
        "gc.expected_primitive_into_reference_count",
        cratonvm_gc::autobox::expected_primitive_into_reference_count().to_string(),
    ));
    let (bo1, bo2, bo3) = cratonvm_gc::g1::block_offset_audit_census();
    out.push(("gc.block_offset_audit_census", format!("{bo1}/{bo2}/{bo3}")));
    let (bnj1, bnj2) = cratonvm_gc::g1::block_offset_no_jump_census();
    out.push(("gc.block_offset_no_jump_census", format!("{bnj1}/{bnj2}")));
    let (bp1, bp2, bp3) = cratonvm_gc::g1::block_offset_process_census();
    out.push((
        "gc.block_offset_process_census",
        format!("{bp1}/{bp2}/{bp3}"),
    ));
    out.push((
        "gc.drain_controller_samples_refused",
        cratonvm_gc::g1::drain_controller_samples_refused().to_string(),
    ));
    let (fr1, fr2, fr3, fr4, fr5, fr6) = cratonvm_gc::g1::free_region_scan_counts();
    out.push((
        "gc.free_region_scan_counts",
        format!("{fr1}/{fr2}/{fr3}/{fr4}/{fr5}/{fr6}"),
    ));
    let (ev1, ev2, ev3, ev4, ev5, ev6) = cratonvm_gc::g1::g1_evac_copy_span();
    out.push((
        "gc.g1_evac_copy_span",
        format!("{ev1}/{ev2}/{ev3}/{ev4}/{ev5}/{ev6}"),
    ));
    out.push((
        "gc.parallel_shared_dest_proved_empty",
        cratonvm_gc::g1::parallel_shared_dest_proved_empty().to_string(),
    ));
    out.push((
        "gc.current_sweep_cycle",
        cratonvm_gc::gen_heap::current_sweep_cycle().to_string(),
    ));
    out.push((
        "gc.jit_ref_store_armed_markers",
        cratonvm_gc::gen_heap::jit_ref_store_armed_markers().to_string(),
    ));
    out.push((
        "gc.array_receiver_field_accesses",
        cratonvm_gc::heap::array_receiver_field_accesses().to_string(),
    ));
    let (cc1, cc2, cc3, cc4) = cratonvm_gc::mark_bitmap::clear_census();
    out.push((
        "gc.mark_bitmap.clear_census",
        format!("{cc1}/{cc2}/{cc3}/{cc4}"),
    ));
    out.push((
        "gc.process_committed_bytes",
        cratonvm_gc::reservation::process_committed_bytes().to_string(),
    ));
    let (dc1, dc2, dc3, dc4) = cratonvm_gc::young_mark::drain_census();
    out.push((
        "gc.young_mark.drain_census",
        format!("{dc1}/{dc2}/{dc3}/{dc4}"),
    ));

    // ── native-builtins / native-collections ─────────────────────────────
    out.push((
        "native_builtins.proxy_instances_created",
        cratonvm_native_builtins::reflect_annotations::proxy_instances_created().to_string(),
    ));
    out.push((
        "native_collections.collection_refresh_moved_count",
        cratonvm_native_collections::collection_refresh_moved_count().to_string(),
    ));
    out.push((
        "native_collections.owner_class_filter_drop_count",
        cratonvm_native_collections::owner_class_filter_drop_count().to_string(),
    ));

    // ── jit ──────────────────────────────────────────────────────────────
    cratonvm_jit::instrument_census::instrument_census(&mut out);

    // ── vm ───────────────────────────────────────────────────────────────
    let (a5a, a5b, a5c, a5d) = crate::jit::conservative_roots::a5_fallback_census();
    out.push(("vm.a5_fallback_census", format!("{a5a}/{a5b}/{a5c}/{a5d}")));
    out.push((
        "vm.register_image_remap_words",
        crate::jit::conservative_roots::register_image_remap_words().to_string(),
    ));
    out.push((
        "vm.stale_after_remap_hits",
        crate::jit::conservative_roots::stale_after_remap_hits().to_string(),
    ));
    out.push((
        "vm.stale_after_remap_resumed_hits",
        crate::jit::conservative_roots::stale_after_remap_resumed_hits().to_string(),
    ));
    out.push((
        "vm.uncovered_precise_frame_count",
        crate::jit::conservative_roots::uncovered_precise_frame_count().to_string(),
    ));
    let (ud1, ud2) = crate::jit::conservative_roots::unreg_declined_census();
    out.push(("vm.unreg_declined_census", format!("{ud1}/{ud2}")));
    out.push((
        "vm.jit_collection_leaf_hit_count",
        crate::jit::helpers::jit_collection_leaf_hit_count().to_string(),
    ));
    out.push((
        "vm.jit_corrupt_value_cells",
        crate::jit::helpers::jit_corrupt_value_cells().to_string(),
    ));
    out.push((
        "vm.jit_direct_helper_jdk_only_refusals",
        crate::jit::helpers::jit_direct_helper_jdk_only_refusals().to_string(),
    ));
    out.push((
        "vm.ambiguous_process_vm_resolutions",
        crate::native::jni::ambiguous_process_vm_resolutions().to_string(),
    ));
    out.push((
        "vm.lambda_jit_adapter_installs",
        crate::runtime::interpreter::lambda::lambda_jit_adapter_installs().to_string(),
    ));
    out.push((
        "vm.lambda_jit_capture_adapter_installs",
        crate::runtime::interpreter::lambda::lambda_jit_capture_adapter_installs().to_string(),
    ));
    let (ld1, ld2) = crate::runtime::interpreter::lambda::lambda_site_deopt_outcomes();
    out.push(("vm.lambda_site_deopt_outcomes", format!("{ld1}/{ld2}")));
    out.push((
        "vm.proxy.instances_created",
        crate::runtime::proxy::stats::instances_created().to_string(),
    ));
    out.push((
        "vm.proxy.dispatches",
        crate::runtime::proxy::stats::dispatches().to_string(),
    ));
    out.push((
        "vm.access_checks_run",
        crate::runtime::resolve::access_checks_run().to_string(),
    ));
    out.push((
        "vm.illegal_transition_count",
        crate::threading::thread_state::illegal_transition_count().to_string(),
    ));
    out.push((
        "vm.thread_state_roster",
        format!(
            "{} thread(s)",
            crate::threading::thread_state::thread_state_roster().len()
        ),
    ));
    out.push((
        "vm.check_override_redefine_suppressed",
        crate::vm::vm_exec::check_override_redefine_suppressed().to_string(),
    ));
    out.push((
        "vm.native_shadow_dropped_by_redefine",
        crate::vm::vm_exec::native_shadow_dropped_by_redefine().to_string(),
    ));

    out
}

/// `true` when `CRATONVM_INSTRUMENT_CENSUS` asks for the report.
#[must_use]
pub fn enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_INSTRUMENT_CENSUS")
}

/// Print the census to stderr, one instrument per line, zeros included.
///
/// Called from the launcher's shutdown report. Does nothing unless
/// [`enabled`], so a caller may invoke it unconditionally.
pub fn dump_to_stderr() {
    if !enabled() {
        return;
    }
    let rows = census();
    eprintln!(
        "[cratonvm] instrument census ({} instruments with no other reader; \
         a zero here means the counter was not incremented, which is NOT the \
         same as the subsystem being idle -- see \
         vm/src/runtime/instrument_census.rs)",
        rows.len()
    );
    for (name, value) in rows {
        eprintln!("[instrument] {name} = {value}");
    }
}
