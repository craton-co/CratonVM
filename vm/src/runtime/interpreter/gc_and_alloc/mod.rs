// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation, garbage collection, and the root protocol between them.
//!
//! The interpreter's side of the collector: TLAB allocation and its refill
//! wedge-break, the GC entry points (`maybe_gc`, `maybe_gc_forced`,
//! `maybe_concurrent_gc`, the G1 cycle drivers), reference processing,
//! cleaners and finalizers, `safepoint_check`, and the root snapshot the
//! collector walks.
//!
//! `stw_take_over_and_wait` is the one to read first if something hangs at
//! a safepoint: it is the protocol by which one thread becomes the STW
//! initiator and the others park, and every deadlock in this area has been
//! a thread that never reached it.
//!
//! `apply_pointer_map_to_thread` is the moving-collector half — after a
//! young collection relocates objects, every root channel has to be
//! rewritten, and a channel that is missed is a stale reference that will
//! be dereferenced later, somewhere else.
//!
//! The `remap_trace_*`, `push_prov_*`, `deposit_gap_*`, `nret_*` and
//! `getfield_ring_*` helpers are the provenance tracing built for exactly
//! those investigations: off by default, and the reason a reclaimed-object
//! report can name the site that pushed the reference.

use super::*;

// LANE W7-D — the mark-cycle door census. `note_mark_door` is called at most
// once per PAUSE (and once per `g1_force_full_cycle` call), never per
// allocation; see `g1_drive_mark_cycle`.
use cratonvm_gc::g1::{note_mark_door, MarkDoor, MarkDoorOutcome};
// gen r4/plumbing (2026-09-23): per-collection accounting, see `gc_events`.
use super::gc_events::{
    gc_event_finish_by, gc_event_seal, gc_event_start, non_collection_pause_finish,
    non_collection_pause_seal, non_collection_pause_start, run_gc_notifications, GcDoor,
    NonCollectionPause,
};

/// Interpreter round i1 wave 13, lane L4: the grace slice of the handshake
/// pause.
#[cfg(test)]
mod i13_l4_handshake_pause_tests;

#[cfg(test)]
mod frame_trace_request_tests;
#[cfg(test)]
mod zt31_zc11_mark_start_grace_tests;
#[cfg(test)]
mod gci_i9ds_frozen_peer_deopt_stash_tests;
#[cfg(test)]
mod takeover_verdict_unreadable_tests;

/// gce e2/t: the first-pass grace (`CRATONVM_XT_FIRST_PASS_GRACE_US`).
#[cfg(test)]
mod gce_e2t_first_pass_grace_tests;

/// gcg g6/xt: the helper-window JIT-only narrowing (gcf f1/r) is deleted.
#[cfg(test)]
mod gcg_g6xt_helper_window_reads_every_blocked_peer_tests;

/// gc-common w5-a — source ratchets for the pause drivers.
#[cfg(test)]
mod w5a_pause_driver_tests;
/// gcj j3/x2 R1: the door's split of the finalizable inputs.
#[cfg(test)]
mod gcj3x2_r1_queued_finalizable_root_tests;

#[cfg(test)]
mod w18a_resurrection_closure_tests;
#[cfg(test)]
mod i20_l4_occupancy_poll_tests;
#[cfg(test)]
mod r23_w1_l7_root_kind_census_door_tests;

#[cfg(test)]
mod oldmark_census_section_tests;
/// Allocate a dynamically-produced `java.lang.String` under the SAME
/// heap-exhaustion contract as `new`: collect, retry, and finally raise a
/// catchable `OutOfMemoryError` -- never abort the process.
///
/// `vm_object::create_java_string_uninterned` cannot do this: it takes only a
/// `&SharedVm`, and every GC entry point needs the calling thread (to retire
/// its TLAB and contribute its roots). So its exhaustion path was a bare
/// `eprintln!` + `std::process::abort()`, which turned a plain `"a" + b` on a
/// full heap into an un-catchable VM kill. HotSpot throws
/// `OutOfMemoryError: Java heap space` there, and a Java program is entitled to
/// catch it. Reproduced with a 25-line probe under `-Xmx64m -XX:+UseG1GC`:
/// `FATAL: heap exhausted allocating java/lang/String (46 units)` (from the
/// `System.out.println("iter=" + i)` in the allocation loop) followed by
/// SIGABRT, where HotSpot reports `OutOfMemoryError` and keeps running.
///
/// Test-only since gc-common w2 (2026-09-23): no production caller remains
/// (none at the round base either); the UTF-16 twin below is the live entry.
#[cfg(test)]
pub(crate) fn create_string_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_uninterned as try_new_string;
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    // The escalation every interpreter ladder shares — forced GC, the
    // GC-overhead limit, retry, G1's last-ditch complete mark cycle, retry.
    // See `collect_and_retry`.
    if let Some(obj) =
        collect_and_retry(shared, thread, "create-string", |s| try_new_string(s, text))
    {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            // `text.len()` is the UTF-8 byte length, not a char count; say so.
            // The parenthesised site detail never reaches Java -- see
            // `RuntimeError::as_java_throwable`.
            message: format!("Java heap space (String of {} UTF-8 bytes)", text.len()),
        },
    )))
}

#[cfg(test)]
mod gcf_f3o_tests;
#[cfg(test)]
mod gce_e2o_latched_grace_tests;

#[cfg(test)]
mod r5w5_overhead_exit_tests;
/// gcd d10/o (2026-09-28): the shared allocation ladder in the shape a
/// NATIVE can use -- collect, then answer whether its ONE retry is worth
/// making. The body of `NativeContextImpl::reclaim_before_alloc_retry`
/// (`vm/src/vm/vm_exec.rs`), which every collecting native factory calls
/// after a refused `try_new_array` / `try_new_ref_array` / `try_alloc_object`
/// / `init_string_from_units` (`ArrayList(int)` and its `add` grow,
/// `String.toCharArray`, `StringBuilder(int)`, `ByteBuffer.allocate`, the
/// `Arrays.copyOf` family, ...). Same precondition as that trait method: the
/// caller holds no unpinned `ObjectRef`.
///
/// # Why a second body
///
/// `reclaim_before_alloc_retry` carried its own copy of the ladder, written
/// before gen r4w4 and never brought along, so a native factory decided an
/// `OutOfMemoryError` on less than `new` / `newarray`
/// ([`collect_and_retry_with_thread`]) and the JIT helpers do:
///
/// * **latched overhead streak** -- it returned the soft-reference rung's
///   answer, so with no live `SoftReference` it threw after the forced
///   collection alone, without even the retry that collection may have made
///   room for: on Generational no major reached the old generation (where a
///   program's dropped data sits after an `OutOfMemoryError`), on G1 no
///   marking cycle reached a dead Old region;
/// * **otherwise** -- one `last_ditch_reclaim`, where the interpreter and the
///   JIT helpers have run `second_major_before_oome` since gcd d4/j (one
///   Generational major is no verdict: young->old nepotism).
///
/// # The ladder
///
/// A native cannot attempt between the rungs, so the rungs run back to back
/// and the retry follows the last one:
///
/// * not latched: the forced collection, `last_ditch_reclaim`, the second
///   major; retry;
/// * latched: the soft-reference rung, then the interpreter's exit
///   ([`overhead_limit_exit`]): on Generational the deciding majors
///   ([`majors_to_decide_oome`]) and the error when a major this thread ran
///   left the streak latched; on G1 [`g1_overhead_limit_full_cycle`] decides;
///   on ZGC the forced cycle was whole-heap and earns the retry, as it earns
///   the interpreter's one attempt.
///
/// `CRATONVM_GC_OVERHEAD_PROGRESS=0` restores the historical body byte for
/// byte ([`native_reclaim_before_alloc_retry_historical`]).
/// Every production caller now takes the graced verdict (gcf f2/o); this
/// boolean form is the unit tests' spelling.
#[cfg(test)]
pub(crate) fn native_reclaim_before_alloc_retry(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    native_reclaim_before_alloc_retry_graced(shared, thread).retry
}

#[cfg(test)]
mod gci_i13oa_native_door_and_soft_rung_tests;
#[cfg(test)]
mod zt8rv4_metaspace_backfill_line_tests;

#[cfg(test)]
mod zr8nr_room_skip_repay_tests;

#[cfg(test)]
mod zr5cr_refusal_tests;
#[cfg(test)]
mod zr3ja_stale_gc_request_tests;
#[cfg(test)]
mod gcg_g4oo_owed_cycle_tests;
#[cfg(test)]
mod gcd_d4j_oome_collection_tests;

#[cfg(test)]
mod gcd_d9b_oome_ladder_census_tests;

#[cfg(test)]
mod gci_i8_gs_door_label_tests;

#[cfg(test)]
mod gci_i10_dl_compaction_door_tests;

#[cfg(test)]
mod gci_i12_ge_door_tests;

#[cfg(test)]
mod gci_i11_go_compaction_door_tests;

#[cfg(test)]
mod gcd_d2j_futile_young_tests;
/// Close this forced collection's progress window and open the next: read the
/// window since the previous forced collection (if one was recorded), then
/// record the counters and `live_after` for the next. Per VM
/// (`HeapRealm::gc_progress`). `None` when there is no previous window or the
/// progress half is switched off. gen r4w5/thrash5 (2026-09-24). Test-only
/// since gci i2/oo: the verdict calls [`sample_mutator_progress_at`].
#[cfg(test)]
fn sample_mutator_progress(shared: &SharedVm, live_after: usize) -> Option<MutatorProgress> {
    sample_mutator_progress_at(shared, live_after, None)
}

#[cfg(test)]
mod w5_overhead_progress_tests;
#[cfg(test)]
mod gcg_g4oo_crawl_backstop_tests;
#[cfg(test)]
mod gcf_f2o_overhead_limit_tests;
#[cfg(test)]
mod zt23_zc6_mark_end_service_tests;
#[cfg(test)]
mod zt24_zc8_mark_continue_tests;
#[cfg(test)]
mod zt3_mk_full_request_door_tests;

#[cfg(test)]
mod zt21_zc2_concurrent_sweep_door_tests;

#[cfg(test)]
mod zt18_zc1_system_gc_concurrent_tests;
/// zt t15/x12: [`system_gc_waits_finalizer_step_only`] is ZGC-only, default
/// ON with a `=0` kill; the delivery thread's default-policy batch brackets
/// exactly its `finalize()` step, ended by a guard (an unwinding finalizer
/// included). The step's own wait is tested in `cratonvm_gc::reference`.
#[cfg(test)]
mod zt15x12_finalizer_step_wait_tests;

/// zt t18/x14: [`system_gc_skips_finalizer_wait`] is ZGC-only, default ON
/// with a `=0` kill; the collection door's drain is bounded and waits for
/// nothing, while `runFinalization` keeps w5-d's wait.
#[cfg(test)]
mod zt18x14_collection_door_finalizer_tests;
/// zt t26/x21: [`zgc_finalizers_off_caller`] is ZGC-only, default ON with a
/// `=0` kill; a lock-free ZGC thread with a finalizer queued asks for the
/// delivery thread, and a fixture VM that cannot start one (no owning `Arc`)
/// keeps the inline drain.
#[cfg(test)]
mod zt26x21_finalizers_off_caller_tests;

/// zt t28/rf: [`run_finalization_off_caller`] is ZGC-only, default ON with a
/// `=0` kill; a fixture VM that cannot start the delivery thread (no owning
/// `Arc`) is not handed off, so its caller drains as before.
#[cfg(test)]
mod zt28rf_run_finalization_off_caller_tests;
/// zt t27/fin1: [`zgc_delivery_slices_finalizers`] is ZGC-only, default ON
/// with a `=0` kill; a slice stops at the queue length it began with, and the
/// batch loops over slices only under the switch.
#[cfg(test)]
mod zt27fin1_delivery_slice_tests;
/// gci i32/ec: the pure halves and the wiring of the kept finalizables'
/// private closure. `cargo test -j 5 -p cratonvm-vm --lib gci_i32_ec_`.
#[cfg(test)]
mod gci_i32_ec_finalizable_closure_tests;

/// gci i31/gq (`gci-i30rp-g1-dead-finalizable-in-a-kept-young-region-is-not-finalized-at-the-pause-FIXED-20261016`):
/// the finalizable candidates, the split of the verdict and the door's wiring.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i31_gq_`.
#[cfg(test)]
mod gci_i31_gq_kept_young_finalizable_tests;
/// gci i30/rp: the finalizer-only referents of a kept young region reach the
/// reference round as resurrected. `cargo test -j 5 -p cratonvm-vm --lib gci_i30_rp_`.
#[cfg(test)]
mod gci_i30_rp_finalizer_only_tests;
/// gci i27/gr: the switch reader, the candidate selection and the wiring of
/// the per-object verdict into `process_references_after_gc`.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i27_gr_`.
#[cfg(test)]
mod gci_i27_gr_pinned_referent_tests;

/// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
/// the G1 kept-young-region verdict is taken once per pause, over the
/// reference rows AND the JNI weak globals, and carried to every weak
/// consumer of the epilogue on the pause's relocation set.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i28_wt_`.
#[cfg(test)]
mod gci_i28_wt_weak_tables_tests;



/// zt t31/zc11: the source text of the VM reference round, for the structure
/// tests -- [`process_references_after_gc`] through the end of
/// [`reference_round`] (the three functions are contiguous).
#[cfg(test)]
pub(super) fn process_references_text(src: &str) -> String {
    let at = src
        .find("\npub(in crate::runtime::interpreter) fn process_references_after_gc(")
        .expect("process_references_after_gc");
    let round = src
        .find("\npub(in crate::runtime::interpreter) fn reference_round(")
        .expect("reference_round");
    let end = round + src[round..].find("\n\u{7d}\n").expect("its end");
    src[at..end].to_string()
}

#[cfg(test)]
mod zt31_zc11_reference_round_after_release_tests;
/// The TLAB object allocation sites, as mask bits for
/// [`compact_tlab_site_mask`].
pub(crate) mod tlab_site {
    /// `gc_alloc_object` — the interpreter's own `new`.
    pub const INTERPRETER: u32 = 1;
    /// `jit_new_object`'s guarded-refill TLAB attempt.
    pub const JIT_NEW: u32 = 2;
    /// The two `tlab_alloc_object` sites inside the JIT helpers.
    pub const JIT_HELPER: u32 = 4;
    /// The native-call allocation site in `vm_exec`.
    pub const NATIVE: u32 = 8;
    /// The compact-`String` site in `vm_object`.
    pub const STRING: u32 = 16;
}

#[cfg(test)]
mod gci_i10al_oom_dump_own_thread_tests;

#[cfg(test)]
mod gci_i9hj_oom_dump_pause_tests;
#[cfg(test)]
mod w5_oom_report_tests;

#[cfg(test)]
mod w4_oom_dump_path_tests;
#[cfg(test)]
mod w1f_alloc_front_end_tests;

#[cfg(test)]
mod w2d_alloc_front_end_tests;

#[cfg(test)]
mod w3c_alloc_front_end_tests;

#[cfg(test)]
mod w4c_alloc_front_end_tests;

#[cfg(test)]
mod w8v_ldc_literal_oom_tests;

#[cfg(test)]
mod w6c_alloc_front_end_tests;

#[cfg(test)]
mod w7b_alloc_plan_tests;
#[cfg(test)]
mod root_snapshot_cache_tests;

#[cfg(test)]
mod safepoint_remap_tests;
#[cfg(test)]
mod gci_i23hs_handshake_poll_tests;
#[cfg(test)]
mod gcd_d1c_sweep_reference_row_tests;

/// `gengc-r5w1-refs5-g1-remark-leaves-enqueued-phantom-referent-dangling`:
/// the remark's reference pass (G1's `g1_remark_process_references`, and the
/// generational remark through the same `remark_reference_rows`) nulls slot 0
/// of a live phantom it enqueues, so `cleanup` freeing the referent's region
/// leaves nothing naming it.
#[cfg(test)]
mod refs5_remark_phantom_slot_tests;
#[cfg(test)]
mod zgc_concurrent_reference_policy_tests;
/// gci i31/gq (G-2): the switch, the carry's selection and the hold-out.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i31_gq_`.
#[cfg(test)]
mod gci_i31_gq_door_queued_tests;
#[cfg(test)]
mod zr1br_satb_window_tests;
/// gci i6/g1t: the deferral switch is default ON (`=0` kills) and never
/// engages without the remark sweep that makes it sound.
#[cfg(test)]
mod gci_i6_g1t_itl_defer_tests;

/// gci i14/gq G-1: the remark's weak-before-final switch is default ON (since
/// 2026-10-17, `=0` kills), and both G1 remark orders note the round through
/// the one helper.
#[cfg(test)]
mod gci_i14gq_g1_remark_weak_before_final_tests;

/// gci i48/zl2: `CRATONVM_G1_REMARK_SOFT_CLOCK_NOW` extended to the
/// Generational concurrent remark's reference round (default OFF kept).
#[cfg(test)]
mod gci_i48_zl2_gen_remark_soft_clock_tests;

/// gci i15/rf (G-9): the undiscovered-`Reference` switch is opt-in, and
/// `remark_reference_rows` wires it in the order the fix needs.
#[cfg(test)]
mod gci_i15rf_g1_remark_undiscovered_refs_tests;

/// gci i22/dr (P2/dr, `gcd-d10r-proposal-one-relocation-target-set-per-pause-20260928`):
/// the stop-the-world door builds ONE relocation-destination set and every
/// in-pause verdict borrows it; none of the three builds its own any more.
#[cfg(test)]
mod gci_i22dr_one_relocation_set_per_pause_tests;

/// gci i16/fr (`gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018`,
/// the stop-the-world half): the switch reader, the probe walk, the
/// dead-finalizer split with its candidate-only note, and the door's order.
#[cfg(test)]
mod gci_i16fr_stw_undiscovered_refs_tests;
/// gch h4/g1: the two G1 remark switches are default ON with a `=0` kill, and
/// the keep-closure marker is inert off G1 and on an empty list.
#[cfg(test)]
mod gch_h4g1_g1_remark_tests;
/// gci i42/rp: the remark hint, the delivery thread's unwind guard and the
/// `CRATONVM_DBG_NO_REFPROC` exclusion arm.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i42rp_`.
#[cfg(test)]
mod gci_i42rp_reference_delivery_tests;
/// The remark tracer's visited set and closure memo
/// (`in-process-javac-throughput-aot-test-classes`).
/// Filter: `cargo test -j 5 -p cratonvm-vm --lib remark_trace_tests`.
#[cfg(test)]
mod remark_trace_tests;
/// gci i48/rf48 (`gci-i15rf-...-20261014` residual): the default-path rule
/// and the wiring of the G1 full collection's undiscovered `Reference`s
/// (young pause probe -> carry -> remark -> stage -> arm -> compaction pass).
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i48_rf48_`.
#[cfg(test)]
mod gci_i48_rf48_full_gc_undiscovered_tests;
/// gci i27/gr: the carry's switch reader and its two doors (the stop-the-world
/// round carries, the remark reads). `cargo test -j 5 -p cratonvm-vm --lib gci_i27_gr_`.
#[cfg(test)]
mod gci_i27_gr_carry_wiring_tests;


/// gci i30/ob (`i28-dg-report.md` review item 1): the allocation-failure
/// cycle is stop-the-world by default, whatever `ExplicitGCInvokesConcurrent`.
#[cfg(test)]
mod gci_i30_ob_last_ditch_tests;
#[cfg(test)]
mod gcd_d10o_ladder_tests;
#[cfg(test)]
mod gci_i15ca_abort_pause_tests;
#[cfg(test)]
mod last_ditch_backoff_tests;

#[cfg(test)]
mod root_snapshot_screen_tests;
#[cfg(test)]
mod default_field_init_tests;

/// r9w3 vmhelpers3 —
/// `perf-compact-tlab-planner-reads-an-env-var-per-allocation-20260918.md`.
#[cfg(test)]
mod r9w3_vmhelpers3_tlab_planner_tests;

/// gen r4w3/alloc3 (2026-09-23): the opt-in JIT refill-gate probe size.
#[cfg(test)]
mod r4w3_alloc3_gate_probe_tests;

#[cfg(test)]
mod w3d_finalizer_thread_tests;

#[cfg(test)]
mod w4a_door_tests;

#[cfg(test)]
mod w7g_door_tests;



#[cfg(test)]
mod w8g_door_tests;

#[cfg(test)]
mod gcf_f1o_refill_trigger_line_tests;

#[cfg(test)]
mod gcg_g3o_native_factory_stage_tests;

/// JIT round 18 wave 10 (lane ffmeintr): FF3-2's no-roots claims and the
/// stop-the-world watchdog switch.
#[cfg(test)]
mod r18_ffmeintr_tests;

/// JIT round 19 wave 1 (lane eintr19): FFMEINTR18-1 (the frame-trace pause
/// claims), FFMEINTR18-4 (excuse only unblocked peers), FFMEINTR18-5 (the
/// last-resort stall report).
#[cfg(test)]
mod r19_eintr19_tests;

#[cfg(test)]
mod gch_h4g1_native_oome_debt_tests;

/// JIT round 20 wave 21 (lane g1ref):
/// `jr20-g1-compiled-reference-get-loop-never-sees-the-clear-FIXED-20261013`. A G1
/// `System.gc()` runs its marking cycle whole inside one pause, and a frozen
/// peer's `Reference` never greys its own hidden referent.
#[cfg(test)]
mod jr20_g1ref_tests;

/// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
/// the heap dump's frozen-peer words.
#[cfg(test)]
mod gci_i4hd_dump_words_tests;
#[cfg(test)]
mod gci_i9ng_takeover_refusal_label_tests;

#[cfg(test)]
mod gci_i13gp_g1_metadata_and_abort_tests;

#[cfg(test)]
mod gci_i14ox_g1_oome_sequence_tests;





#[cfg(test)]
mod gci_i23pk_referent_screen_tests;

/// gci i28/dg (i27/ob review item 4): under `-XX:+ExplicitGCInvokesConcurrent`
/// only `System.gc()` and `jcmd GC.run` run G1's forced cycle concurrently;
/// JVMTI `ForceGarbageCollection` and the other full-collection doors stay
/// stop-the-world, as HotSpot's `should_do_concurrent_full_gc`.
#[cfg(test)]
mod gci_i28_dg_force_full_door_tests;

#[cfg(test)]
mod gci_i35_jf_forced_drain_tests;

#[cfg(test)]
mod allocation_diagnostic_isolation_tests;

#[cfg(test)]
mod gci_i47w_sg47_door_tests;

/// gci i47w/xl47 (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`):
/// a G1 full-collection door is ONE HotSpot operation -- one `Safepoint`
/// line, one `sun.rt.safepoints` -- however many collection pauses it runs.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i47w_xl47_`.
#[cfg(test)]
mod gci_i47w_xl47_door_tests;

/// gcj j3/x2: a reading of the probe walk census (see [`STW_PROBE_WALKS`]).
/// Test-only since JIT round 22 wave 7 (lane review): nothing in the VM
/// reads the census back, only the gcj tests (`vm/tests/no_test_only_public_api.rs`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StwProbeCensus {
    pub(crate) walks: u64,
    pub(crate) abandoned: u64,
    pub(crate) visited: u64,
    pub(crate) walk_ns: u64,
}

/// gcj j3/x2: the probe walk census so far (process totals).
#[cfg(test)]
pub(crate) fn stw_probe_census() -> StwProbeCensus {
    use std::sync::atomic::Ordering::Relaxed;
    StwProbeCensus {
        walks: STW_PROBE_WALKS.load(Relaxed),
        abandoned: STW_PROBE_ABANDONED.load(Relaxed),
        visited: STW_PROBE_VISITED.load(Relaxed),
        walk_ns: STW_PROBE_WALK_NS.load(Relaxed),
    }
}

/// JIT round 22 wave 8, lane memory: the staged G1 compaction's byte-progress
/// rule (`CRATONVM_G1_STAGED_COMPACTION_BYTE_PROGRESS`).
#[cfg(test)]
mod r22_memory_staged_compaction_byte_progress_tests;

/// JIT round 22 wave 9, lane memory: the G1 full cycle's budget follows
/// progress (`CRATONVM_G1_FULL_CYCLE_BUDGET_FOLLOWS_PROGRESS`).
#[cfg(test)]
mod r22_memory_full_cycle_budget_tests;

#[cfg(test)]
mod r22_memory_oome_maximal_compaction_tests;

#[cfg(test)]
mod r22_memory_every_door_ladder_tests;

/// zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_JNI_WEAK_STRONG`): where the young
/// cycle's JNI weak-global roots join the pause.
#[cfg(test)]
mod zt11gn3_young_jni_weak_roots_tests;

/// zt t20/x16 (`CRATONVM_ZGC_SYSTEM_GC_NOTIFICATIONS_OFF_THREAD`, D10): the
/// explicit collection doors ask the ZGC `--compatible` hand-off after the
/// VM's own rule and before the inline forced drain; the hand-off reads its
/// default-ON switch and shares the ZGC / `WhenLockHeld` gate.
#[cfg(test)]
mod zt20x16_collection_door_notification_hand_off_tests;

/// zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in, and the census of
/// stage 1c): the door times its VM reference round and forgets the hidden
/// rows after the collection; the pre-collection pass hides exactly the rows
/// it would have nulled. The heap half is unit-tested in
/// `gc/src/zgc/concurrent_refs.rs`.
#[cfg(test)]
mod zt22zc3_reference_round_tests;

/// zt t23/hunt5: the two parity fixes of the ZGC concurrent profile's
/// mark-start and soft-clear paths. The behaviour is the rows'
/// (`docs/feature-designs/zgc-architecture.md`); these pin the
/// wiring and the switches.
#[cfg(test)]
mod zt23hunt5_tests;

/// zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): the clear-soft rung
/// with no soft reference to clear still collects, marking from scratch, when
/// a concurrent mark could be what the ladder rests on. The behaviour is the
/// rows' (`docs/feature-designs/zgc-architecture.md`, `h7_OOM*`);
/// these pin the wiring, the switch and the default ZGC control.
#[cfg(test)]
mod zt24hunt7_tests;

mod mtroots_on;
#[allow(unused_imports)]
pub use mtroots_on::*;
#[allow(unused_imports)]
use mtroots_on::*;
mod gc_notifications_go_to_delivery;
#[allow(unused_imports)]
pub use gc_notifications_go_to_delivery::*;
#[allow(unused_imports)]
use gc_notifications_go_to_delivery::*;
mod zgc_refusal_line;
#[allow(unused_imports)]
pub use zgc_refusal_line::*;
#[allow(unused_imports)]
use zgc_refusal_line::*;
mod finalizers_forced;
#[allow(unused_imports)]
pub use finalizers_forced::*;
#[allow(unused_imports)]
use finalizers_forced::*;
mod reference_round;
#[allow(unused_imports)]
pub use reference_round::*;
#[allow(unused_imports)]
use reference_round::*;
mod on_out_of_memory_error_commands;
#[allow(unused_imports)]
pub use on_out_of_memory_error_commands::*;
#[allow(unused_imports)]
use on_out_of_memory_error_commands::*;
mod frozen_peer_referents;
#[allow(unused_imports)]
pub use frozen_peer_referents::*;
#[allow(unused_imports)]
use frozen_peer_referents::*;
mod g1_force_full_cycle;
#[allow(unused_imports)]
pub use g1_force_full_cycle::*;
#[allow(unused_imports)]
use g1_force_full_cycle::*;

pub(crate) use reference_round::tlab_bump_object_fast;

#[cfg(test)]
pub(crate) fn read_gc_and_alloc_src_from_disk() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/runtime/interpreter/gc_and_alloc");
    // Keep this order in sync with GC_AND_ALLOC_SRC.
    [
        "mtroots_on.rs",
        "gc_notifications_go_to_delivery.rs",
        "zgc_refusal_line.rs",
        "finalizers_forced.rs",
        "reference_round.rs",
        "on_out_of_memory_error_commands.rs",
        "frozen_peer_referents.rs",
        "g1_force_full_cycle.rs",
    ]
    .into_iter()
    .map(|name| std::fs::read_to_string(dir.join(name)).expect("gc_and_alloc source"))
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
const GC_AND_ALLOC_SRC: &str = concat!(
    include_str!("mtroots_on.rs"),
    "\n",
    include_str!("gc_notifications_go_to_delivery.rs"),
    "\n",
    include_str!("zgc_refusal_line.rs"),
    "\n",
    include_str!("finalizers_forced.rs"),
    "\n",
    include_str!("reference_round.rs"),
    "\n",
    include_str!("on_out_of_memory_error_commands.rs"),
    "\n",
    include_str!("frozen_peer_referents.rs"),
    "\n",
    include_str!("g1_force_full_cycle.rs"),
    "\n",
);
